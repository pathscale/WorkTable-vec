//! Load and unload a table as pages.
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
//! speed because it *is* a `Vec`. This is a codec at the two ends, not a
//! storage engine underneath, which is the thing this crate exists not to be.
//!
//! # Self contained on purpose
//!
//! Nothing here reaches for a page format from another crate. The container
//! `data_bucket` provides is `std`: `tokio` for file access and `eyre` in its
//! signatures, and a table that is `no_std` and alloc-only cannot take that on
//! just to serialize a `Vec`.
//!
//! So the pages are this module's own, and they are deliberately modest: a
//! fixed 24 byte header of little-endian integers, then a body. No rkyv in the
//! header, because rkyv puts an archive's root at the end of its buffer, and a
//! header that has to be found at a fixed offset should not have to care. Rows
//! use rkyv, where it earns its place.
//!
//! **These are not WorkTable space files.** A WorkTable space opens with a
//! page carrying a name, a schema and a primary key list, and none of that
//! exists here, because a `Vec<(K, V)>` declares no schema. Reading one format
//! with the other fails, and the fingerprint below is what makes it fail rather
//! than silently succeed.

use alloc::vec::Vec;

use rkyv::api::high::{HighDeserializer, HighValidator};
use rkyv::bytecheck::CheckBytes;
use rkyv::rancor::{Error as RkyvError, Strategy};
use rkyv::ser::Serializer;
use rkyv::ser::allocator::ArenaHandle;
use rkyv::ser::sharing::Share;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Deserialize, Serialize};

use crate::{IndexedTable, LinearTable};

/// One page, header included.
pub const PAGE_SIZE: usize = 4096 * 4;

/// The fixed header every page opens with.
pub const HEADER_SIZE: usize = 24;

/// How much of a page is body.
pub const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE;

/// Bumped when the page layout changes, so an older file is refused instead of
/// being read through the new shape.
pub const PAGE_VERSION: u32 = 1;

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
    /// A page carries a version this build does not write.
    ///
    /// Also what a page of zeroes looks like, which is the shape a torn write
    /// leaves behind.
    ForeignPages {
        /// Which page, counting from zero.
        page: usize,
        /// The version that page claims.
        version: u32,
    },
    /// A header claimed a body longer than a page holds.
    Overlong {
        /// Which page, counting from zero.
        page: usize,
        /// What its header claimed.
        claimed: usize,
    },
    /// The pages disagree with each other about the row type.
    Inconsistent {
        /// Which page disagreed.
        page: usize,
    },
    /// These are a different row type's bytes.
    ///
    /// Caught by a fingerprint rather than by deserialization, because
    /// deserialization does not catch it: rkyv validates a `(u64, String)`
    /// archive as a perfectly good `(u64, u64)` and hands back a `String`'s
    /// relative pointer as an integer. Keys look right, values are debris, and
    /// nothing errors.
    ForeignRows {
        /// The fingerprint these bytes were written with.
        found: u32,
        /// The fingerprint this row type expects.
        expected: u32,
    },
    /// The row bytes did not deserialize.
    Rows,
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotWholePages { found } => write!(
                formatter,
                "{found} bytes is not a whole number of {PAGE_SIZE} byte pages"
            ),
            Self::ForeignPages { page, version } => write!(
                formatter,
                "page {page} is version {version}, and this build writes {PAGE_VERSION}"
            ),
            Self::Overlong { page, claimed } => write!(
                formatter,
                "page {page} claims a {claimed} byte body, more than a page holds"
            ),
            Self::Inconsistent { page } => {
                write!(formatter, "page {page} names a different row type")
            }
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
/// FNV-1a over `core::any::type_name`, which is neither stable across compiler
/// versions nor guaranteed unique. That is fine for what it is for: refusing an
/// obvious mismatch, not authenticating a schema. A false match is possible and
/// a false mismatch is a rebuild, so it fails toward refusing to load rather
/// than toward reinterpreting.
fn fingerprint<T: ?Sized>() -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in core::any::type_name::<T>().as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// A page header: six little-endian `u32`s, in this order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Header {
    version: u32,
    /// The row type every page in this run carries.
    schema: u32,
    page: u32,
    /// How many pages the run has, so a truncated one is visible from page zero.
    pages: u32,
    body: u32,
    /// Zero for now. A layout change that needs a flag has somewhere to put it
    /// without moving anything else.
    reserved: u32,
}

impl Header {
    fn write(self, out: &mut Vec<u8>) {
        for field in [
            self.version,
            self.schema,
            self.page,
            self.pages,
            self.body,
            self.reserved,
        ] {
            out.extend_from_slice(&field.to_le_bytes());
        }
    }

    fn read(raw: &[u8]) -> Self {
        let at = |n: usize| {
            let mut word = [0u8; 4];
            word.copy_from_slice(&raw[n * 4..n * 4 + 4]);
            u32::from_le_bytes(word)
        };
        Self {
            version: at(0),
            schema: at(1),
            page: at(2),
            pages: at(3),
            body: at(4),
            reserved: at(5),
        }
    }
}

/// Split one run of row bytes across pages.
fn to_pages(body: &[u8], schema: u32) -> Vec<u8> {
    // An empty table still writes one page. A zero byte file would be
    // indistinguishable from a missing one, and a load has to be able to tell
    // "no rows" from "nothing landed".
    let chunks: Vec<&[u8]> = if body.is_empty() {
        alloc::vec![body]
    } else {
        body.chunks(BODY_SIZE).collect()
    };

    let pages = u32::try_from(chunks.len()).expect("a page count inside u32");
    let mut out = Vec::with_capacity(chunks.len() * PAGE_SIZE);
    for (index, chunk) in chunks.iter().enumerate() {
        Header {
            version: PAGE_VERSION,
            schema,
            page: u32::try_from(index).expect("a page index inside u32"),
            pages,
            body: u32::try_from(chunk.len()).expect("a chunk inside u32"),
            reserved: 0,
        }
        .write(&mut out);
        out.extend_from_slice(chunk);
        out.resize((index + 1) * PAGE_SIZE, 0);
    }
    out
}

/// Walk the pages back into the row type and one run of row bytes.
///
/// The body comes back in an `AlignedVec` because rkyv reads an archive in
/// place and needs it aligned. A plain `Vec<u8>` is aligned to 1, and whether
/// the allocator happened to hand back more is not something to rest on.
fn from_pages(bytes: &[u8]) -> Result<(u32, AlignedVec), LoadError> {
    if bytes.is_empty() || bytes.len() % PAGE_SIZE != 0 {
        return Err(LoadError::NotWholePages { found: bytes.len() });
    }

    let mut schema = None;
    let mut body = AlignedVec::with_capacity(bytes.len());
    for (index, raw) in bytes.chunks_exact(PAGE_SIZE).enumerate() {
        let header = Header::read(&raw[..HEADER_SIZE]);
        if header.version != PAGE_VERSION {
            return Err(LoadError::ForeignPages {
                page: index,
                version: header.version,
            });
        }
        match schema {
            None => schema = Some(header.schema),
            // Every page names the row type, so a run spliced together from two
            // files is caught rather than concatenated.
            Some(first) if first != header.schema => {
                return Err(LoadError::Inconsistent { page: index });
            }
            Some(_) => {}
        }
        let take = header.body as usize;
        if take > BODY_SIZE {
            return Err(LoadError::Overlong {
                page: index,
                claimed: take,
            });
        }
        body.extend_from_slice(&raw[HEADER_SIZE..HEADER_SIZE + take]);
    }
    Ok((schema.unwrap_or_default(), body))
}

/// Check the row type, then the rows.
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
    /// Every row, as pages.
    #[must_use]
    pub fn unload(&self) -> Vec<u8> {
        to_pages(self.rows.encode().as_ref(), fingerprint::<Vec<(K, V)>>())
    }

    /// Rows back from pages.
    ///
    /// # Errors
    ///
    /// Refuses bytes that are not whole pages, a page from another version or
    /// another run, a header claiming more body than a page holds, another row
    /// type, or rows that do not deserialize.
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
    /// Every row, as pages.
    ///
    /// The index is not written. It is derived from the rows, so rebuilding it
    /// on load costs one pass, where storing it would cost bytes at rest and a
    /// second thing that can disagree with the rows.
    #[must_use]
    pub fn unload(&self) -> Vec<u8> {
        to_pages(self.rows.encode().as_ref(), fingerprint::<Vec<(K, V)>>())
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

    /// The interesting case: more rows than one page body holds, so the page
    /// run is doing real work rather than describing a single page.
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
    fn an_empty_table_is_one_page_and_comes_back_empty() {
        let bytes = table(0).unload();
        assert_eq!(bytes.len(), PAGE_SIZE);
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
    fn a_zeroed_page_is_not_an_empty_table() {
        let bytes = alloc::vec![0u8; PAGE_SIZE];
        assert_eq!(
            LinearTable::<u64, String>::load(&bytes),
            Err(LoadError::ForeignPages {
                page: 0,
                version: 0
            })
        );
    }

    /// Somebody else's rows in well formed pages. Without the fingerprint this
    /// load succeeds and hands back debris.
    #[test]
    fn a_different_row_type_is_refused_rather_than_reinterpreted() {
        let bytes = table(10).unload();
        assert_eq!(
            LinearTable::<u64, u64>::load(&bytes),
            Err(LoadError::ForeignRows {
                found: fingerprint::<Vec<(u64, String)>>(),
                expected: fingerprint::<Vec<(u64, u64)>>(),
            })
        );
    }

    /// Two files spliced together are not one longer file.
    #[test]
    fn pages_from_two_runs_are_refused() {
        let mut spliced = table(1).unload();
        let mut other: LinearTable<u64, u64> = LinearTable::new();
        other.push(1, 1);
        spliced.extend_from_slice(&other.unload());
        assert_eq!(
            LinearTable::<u64, String>::load(&spliced),
            Err(LoadError::Inconsistent { page: 1 })
        );
    }
}
