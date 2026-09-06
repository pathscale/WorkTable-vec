//! Load and unload a table as DataBucket data pages.
//!
//! # The whole interface
//!
//! ```ignore
//! let bytes = table.unload();               // rows out, as pages
//! let table = LinearTable::load(&bytes)?;   // rows back in
//! ```
//!
//! Rows live in a `Vec` while the table is in use and are pages only at rest.
//! That is the point: everything between a load and an unload runs at `Vec`
//! speed because it *is* a `Vec`, and the format is a codec at the two ends
//! rather than a storage engine underneath.
//!
//! # Why there are no I/O traits here
//!
//! A load takes `&[u8]` and an unload returns `Vec<u8>`. Nothing seeks and
//! nothing reads incrementally, so this needs no `Read`/`Seek`/`Write`
//! abstraction and adopts nobody's runtime. Whoever holds the bytes decides
//! how they got there. That is what keeps the crate `no_std` with no
//! dependency beyond the format itself.
//!
//! # What the bytes are, exactly
//!
//! A run of `PAGE_SIZE` pages. Each carries a `GeneralHeader` of
//! `PageType::Data` followed by up to `INNER_PAGE_SIZE` bytes of body, and the
//! bodies concatenate into one rkyv archive of the whole row vector.
//!
//! **Data pages only.** A WorkTable space file also opens with a
//! `SpaceInfoPage` carrying a name, a schema and a primary key list, and none
//! of that exists here: this is a `Vec<(K, V)>` with no declared schema. So
//! these are DataBucket pages and this is not a WorkTable space file, and a
//! reader expecting page 0 to describe a space will not find one.
//!
//! **A bulk codec, not random access.** A `Link` per row and a table of
//! contents to find it is what the parent format provides; a table that is
//! about to become a `Vec` anyway does not need one, and paying for it would
//! be the database overhead this crate exists to avoid.

use alloc::vec::Vec;

use data_bucket_format::{
    DATA_VERSION, GENERAL_HEADER_SIZE, GeneralHeader, INNER_PAGE_SIZE, PAGE_SIZE, PageType,
    Persistable, SpaceId, access_archived,
};
use rkyv::api::high::{HighDeserializer, HighValidator};
use rkyv::bytecheck::CheckBytes;
use rkyv::rancor::{Error as RkyvError, Strategy};
use rkyv::ser::Serializer;
use rkyv::ser::allocator::ArenaHandle;
use rkyv::ser::sharing::Share;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Deserialize, Serialize};

use crate::{IndexedTable, LinearTable};

/// What a load can refuse on.
///
/// Every variant is a statement about the bytes rather than about the caller,
/// because a load either recognises what it was handed or does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// The byte length is not a whole number of pages.
    NotWholePages {
        /// How many bytes arrived.
        found: usize,
    },
    /// A page header did not decode, or named a data version this build does
    /// not write.
    TornHeader {
        /// Which page, counting from zero.
        page: usize,
    },
    /// A header claimed a body longer than the page it sits in.
    Overlong {
        /// Which page, counting from zero.
        page: usize,
        /// What its header claimed.
        claimed: usize,
    },
    /// These are a different row type's bytes.
    ///
    /// Caught by a fingerprint rather than by deserialization, because
    /// deserialization does not catch it: rkyv validates a `(u64, String)`
    /// archive as a perfectly good `(u64, u64)` and hands back a `String`'s
    /// relative pointer as an integer. Keys look right, values are debris,
    /// and nothing errors.
    ForeignRows {
        /// The fingerprint these bytes were written with.
        found: u32,
        /// The fingerprint this row type expects.
        expected: u32,
    },
    /// The row bytes did not deserialize.
    ///
    /// Distinct from [`Self::TornHeader`] on purpose: the pages were readable
    /// and their contents were not, which usually means these are somebody
    /// else's rows rather than damaged ones.
    Rows,
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotWholePages { found } => write!(
                formatter,
                "{found} bytes is not a whole number of {PAGE_SIZE} byte pages"
            ),
            Self::TornHeader { page } => {
                write!(formatter, "page {page} has a torn or foreign header")
            }
            Self::Overlong { page, claimed } => write!(
                formatter,
                "page {page} claims a {claimed} byte body, more than a page holds"
            ),
            Self::ForeignRows { found, expected } => write!(
                formatter,
                "these are row type {found:#010x}, and this is row type {expected:#010x}"
            ),
            Self::Rows => write!(formatter, "the row bytes did not deserialize"),
        }
    }
}

impl core::error::Error for LoadError {}

/// What a row set has to be able to do to make the trip.
///
/// The bounds are rkyv's and there are five lines of them, so they are stated
/// once here and every signature below asks only for `Codec`. The blanket impl
/// means a caller never names this trait either: any row pair whose key and
/// value already derive rkyv's traits satisfies it.
pub trait Codec: Sized {
    /// Rows to bytes.
    fn encode(&self) -> AlignedVec;
    /// Bytes back to rows.
    ///
    /// # Errors
    ///
    /// [`LoadError::Rows`] when the bytes are not this type's archive.
    fn decode(bytes: &[u8]) -> Result<Self, LoadError>;
}

impl<T> Codec for T
where
    T: Archive
        + for<'a> Serialize<Strategy<Serializer<AlignedVec, ArenaHandle<'a>, Share>, RkyvError>>,
    <T as Archive>::Archived: Deserialize<T, HighDeserializer<RkyvError>>
        + for<'a> CheckBytes<HighValidator<'a, RkyvError>>,
{
    fn encode(&self) -> AlignedVec {
        // Infallible in practice: the only failure rkyv reports here is an
        // allocator refusing, which on this path means the process is already
        // out of memory.
        rkyv::to_bytes::<RkyvError>(self).expect("rows serialize")
    }

    fn decode(bytes: &[u8]) -> Result<Self, LoadError> {
        rkyv::from_bytes::<T, RkyvError>(bytes).map_err(|_| LoadError::Rows)
    }
}

/// What row type wrote these bytes.
///
/// FNV-1a over `core::any::type_name`, which is neither stable across
/// compiler versions nor guaranteed unique. That is fine for what it is for:
/// refusing an obvious mismatch, not authenticating a schema. A false match
/// is possible and a false mismatch is a rebuild, so this fails toward
/// refusing to load rather than toward reinterpreting them.
fn fingerprint<T: ?Sized>() -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in core::any::type_name::<T>().as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// One page of header plus body, zero padded to `PAGE_SIZE`.
fn page(header: &GeneralHeader, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PAGE_SIZE);
    out.extend_from_slice(header.as_bytes().as_ref());
    out.resize(GENERAL_HEADER_SIZE, 0);
    out.extend_from_slice(body);
    out.resize(PAGE_SIZE, 0);
    out
}

/// Split one run of row bytes across pages, behind a page that says what row
/// type wrote them.
fn to_pages(body: &[u8], space: SpaceId, schema: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(PAGE_SIZE * (2 + body.len() / INNER_PAGE_SIZE));

    // Page 0 carries the fingerprint and no rows. It is what makes a load able
    // to refuse somebody else's bytes instead of reinterpreting them.
    let mut head = GeneralHeader::new(0.into(), PageType::SpaceInfo, space);
    head.data_length = 4;
    head.next_id = 1.into();
    out.extend_from_slice(&page(&head, &schema.to_le_bytes()));

    // An empty table still writes a data page. A file of one page would be
    // ambiguous with a header-only write, and a load has to tell "no rows"
    // from "nothing landed".
    let chunks: Vec<&[u8]> = if body.is_empty() {
        alloc::vec![body]
    } else {
        body.chunks(INNER_PAGE_SIZE).collect()
    };

    let last = chunks.len();
    for (index, chunk) in chunks.iter().enumerate() {
        let id = u32::try_from(index + 1).expect("a page count inside u32");
        let mut header = GeneralHeader::new(id.into(), PageType::Data, space);
        header.data_length = u32::try_from(chunk.len()).expect("a chunk inside u32");
        header.previous_id = (id - 1).into();
        header.next_id = if index + 1 == last { id } else { id + 1 }.into();
        out.extend_from_slice(&page(&header, chunk));
    }
    out
}

/// Read one page's header, or say which page would not read.
fn header_at(raw: &[u8], page: usize) -> Result<GeneralHeader, LoadError> {
    let archived =
        access_archived::<<GeneralHeader as Archive>::Archived>(&raw[..GENERAL_HEADER_SIZE])
            .map_err(|_| LoadError::TornHeader { page })?;
    let header: GeneralHeader =
        rkyv::deserialize::<_, RkyvError>(archived).map_err(|_| LoadError::TornHeader { page })?;
    if header.data_version != DATA_VERSION {
        return Err(LoadError::TornHeader { page });
    }
    Ok(header)
}

/// Walk the pages back into the fingerprint and one run of row bytes.
///
/// The body comes back in an `AlignedVec` because rkyv reads an archive in
/// place and needs it aligned. A plain `Vec<u8>` is aligned to 1, and whether
/// the allocator happened to hand back more is not something to rest on.
fn from_pages(bytes: &[u8]) -> Result<(u32, AlignedVec), LoadError> {
    if bytes.is_empty() || bytes.len() % PAGE_SIZE != 0 {
        return Err(LoadError::NotWholePages { found: bytes.len() });
    }

    let head = header_at(&bytes[..PAGE_SIZE], 0)?;
    if head.data_length != 4 {
        return Err(LoadError::TornHeader { page: 0 });
    }
    let mut fingerprint = [0u8; 4];
    fingerprint.copy_from_slice(&bytes[GENERAL_HEADER_SIZE..GENERAL_HEADER_SIZE + 4]);
    let schema = u32::from_le_bytes(fingerprint);

    let mut body = AlignedVec::with_capacity(bytes.len());
    for (index, raw) in bytes.chunks_exact(PAGE_SIZE).enumerate().skip(1) {
        let header = header_at(raw, index)?;
        let take = header.data_length as usize;
        if take > INNER_PAGE_SIZE {
            return Err(LoadError::Overlong {
                page: index,
                claimed: take,
            });
        }
        body.extend_from_slice(&raw[GENERAL_HEADER_SIZE..GENERAL_HEADER_SIZE + take]);
    }
    Ok((schema, body))
}

/// Check the fingerprint, then the rows.
fn rows_from<T: Codec>(bytes: &[u8]) -> Result<T, LoadError> {
    let (found, body) = from_pages(bytes)?;
    let expected = fingerprint::<T>();
    if found != expected {
        return Err(LoadError::ForeignRows { found, expected });
    }
    T::decode(&body)
}

impl<K, V> LinearTable<K, V>
where
    Vec<(K, V)>: Codec,
{
    /// Every row, as DataBucket data pages.
    #[must_use]
    pub fn unload(&self) -> Vec<u8> {
        self.unload_to(SpaceId::from(0))
    }

    /// The same, tagged with a space id.
    #[must_use]
    pub fn unload_to(&self, space: SpaceId) -> Vec<u8> {
        to_pages(
            self.rows.encode().as_ref(),
            space,
            fingerprint::<Vec<(K, V)>>(),
        )
    }

    /// Rows back from pages.
    ///
    /// # Errors
    ///
    /// Refuses bytes that are not whole pages, a torn or foreign page header,
    /// a header claiming more body than a page holds, or rows that do not
    /// deserialize.
    pub fn load(bytes: &[u8]) -> Result<Self, LoadError> {
        let rows: Vec<(K, V)> = rows_from(bytes)?;
        Ok(Self { rows })
    }
}

impl<K, V> IndexedTable<K, V>
where
    Vec<(K, V)>: Codec,
    K: Ord + Clone,
{
    /// Every row, as DataBucket data pages.
    ///
    /// The index is not written. It is derived from the rows, so rebuilding it
    /// on load costs one pass, where storing it would cost bytes at rest and a
    /// second thing that can disagree with the rows.
    #[must_use]
    pub fn unload(&self) -> Vec<u8> {
        to_pages(
            self.rows.encode().as_ref(),
            SpaceId::from(0),
            fingerprint::<Vec<(K, V)>>(),
        )
    }

    /// Rows back from pages, with the index rebuilt.
    ///
    /// # Errors
    ///
    /// As [`LinearTable::load`].
    pub fn load(bytes: &[u8]) -> Result<Self, LoadError> {
        let rows: Vec<(K, V)> = rows_from(bytes)?;
        let mut table = Self::with_capacity(rows.len());
        for (key, value) in rows {
            let _ = table.insert(key, value);
        }
        Ok(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};

    fn table(rows: usize) -> LinearTable<u64, String> {
        let mut table = LinearTable::new();
        for n in 0..rows {
            table.push(n as u64, alloc::format!("row {n}"));
        }
        table
    }

    #[test]
    fn rows_survive_the_round_trip() {
        let before = table(1_000);
        let back = LinearTable::<u64, String>::load(&before.unload()).expect("a load");
        assert_eq!(back.rows(), before.rows());
    }

    /// The interesting case: more rows than one page body holds, so the header
    /// chain is doing real work rather than describing a single page.
    #[test]
    fn rows_survive_spanning_many_pages() {
        let before = table(20_000);
        let bytes = before.unload();
        assert!(
            bytes.len() / PAGE_SIZE > 1,
            "the fixture has to span pages: {} pages",
            bytes.len() / PAGE_SIZE
        );
        let back = LinearTable::<u64, String>::load(&bytes).expect("a load");
        assert_eq!(back.rows(), before.rows());
    }

    #[test]
    fn an_empty_table_still_writes_a_data_page() {
        let bytes = table(0).unload();
        // Two: the fingerprint page, then an empty data page. One page alone
        // would be ambiguous with a write that landed only its header.
        assert_eq!(bytes.len(), PAGE_SIZE * 2);
        assert!(
            LinearTable::<u64, String>::load(&bytes)
                .expect("a load")
                .is_empty()
        );
    }

    #[test]
    fn the_index_is_rebuilt_rather_than_stored() {
        let mut before = IndexedTable::new();
        for n in 0..500u64 {
            before.insert(n, n.to_string()).expect("a row");
        }
        let back = IndexedTable::<u64, String>::load(&before.unload()).expect("a load");
        assert_eq!(back.len(), 500);
        assert_eq!(back.select(&37), Some(&"37".to_string()));
    }

    #[test]
    fn bytes_that_are_not_whole_pages_are_refused() {
        assert_eq!(
            LinearTable::<u64, String>::load(&[0u8; 17]),
            Err(LoadError::NotWholePages { found: 17 })
        );
    }

    /// A whole page of zeroes is the shape a torn write leaves behind, and it
    /// has to be a named error rather than a plausible empty table.
    #[test]
    fn a_zeroed_page_is_a_torn_header() {
        let bytes = alloc::vec![0u8; PAGE_SIZE];
        assert_eq!(
            LinearTable::<u64, String>::load(&bytes),
            Err(LoadError::TornHeader { page: 0 })
        );
    }

    /// Somebody else's rows in well formed pages: the pages read, the contents
    /// do not, and the error says which.
    #[test]
    fn foreign_rows_in_good_pages_are_a_row_error() {
        let bytes = table(10).unload();
        assert_eq!(
            LinearTable::<u64, u64>::load(&bytes),
            Err(LoadError::ForeignRows {
                found: fingerprint::<Vec<(u64, String)>>(),
                expected: fingerprint::<Vec<(u64, u64)>>(),
            }),
            "without this the load succeeds and hands back a String's relative \
             pointer as an integer"
        );
    }
}
