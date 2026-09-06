//! Load and unload a table as pages, and put those pages on a disk.
//!
//! # The interface
//!
//! Everything is a load or an unload, because what the two ends do is hydrate
//! a `Vec` and dehydrate it. `_to` and `_from` say where.
//!
//! ```ignore
//! table.unload_to(&mut sink)?;              // rows out, as pages
//! let table = LinearTable::load_from(&mut source)?;  // rows back into a Vec
//! table.append_to(&mut sink, from_row)?;    // more pages, no rewrite
//! ```
//!
//! The sink and the source are `embedded_io` traits, so a `std::fs::File`
//! wrapped in an adapter is a real file and a `Vec<u8>` is memory, with no
//! second code path and no `std` in this crate.
//!
//! When the bytes are already in hand there is no I/O to do:
//!
//! ```ignore
//! let bytes = table.unload();
//! let table = LinearTable::load(&bytes)?;
//! ```
//!
//! Rows live in a `Vec` while the table is in use and are pages only at rest.
//! Everything between a load and an unload runs at `Vec` speed because it *is*
//! a `Vec`. This is a codec plus a byte sink, not a storage engine underneath.
//!
//! # Page based, and each page stands alone
//!
//! A page is 16 KiB: a 24 byte header, then an rkyv archive of **the rows that
//! fit in that page**, and nothing spanning the boundary.
//!
//! That last part is the whole design. An archive split across pages means one
//! damaged page destroys every row in the file, and it means appending a row
//! rewrites everything. Self contained pages make damage local and appends
//! O(new rows), and cost only the few bytes of archive overhead repeated per
//! page.
//!
//! # What is checked
//!
//! Every page carries a CRC-32 of its body, and every field in the header is
//! validated rather than merely written. rkyv's own validation checks that an
//! archive is structurally sound, which is not the same as checking that these
//! are the bytes that were written: a flipped bit inside a `u64` passes
//! structural validation and reads back as a different number. The checksum is
//! what catches that.
//!
//! **These are not WorkTable space files.** A WorkTable space opens with a page
//! carrying a name, a schema and a primary key list, and a `Vec<(K, V)>`
//! declares no schema to put there.

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

/// DataBucket's `GENERAL_HEADER_SIZE`, which this page opens with.
pub const HEADER_SIZE: usize = 28;

/// The row directory at the page tail: a row count and a CRC-32.
pub const DIRECTORY_SIZE: usize = 8;

/// How much of a page is body, between the header and the directory.
pub const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE - DIRECTORY_SIZE;

/// `DATA_VERSION` 3: DataBucket's page framing, plus a row directory.
///
/// 2 is what WorkTable writes today, and a 2 page has no directory, so a reader
/// cannot find its rows without the index. 3 says the directory is there.
pub const PAGE_VERSION: u32 = 3;

/// `PageType::Data` in DataBucket's enum.
const PAGE_TYPE_DATA: u32 = 2;

/// What a load can refuse on.
///
/// Every variant is a statement about the bytes rather than about the caller,
/// and every one of them names the page, because a file that will not load is
/// a question about which page went wrong.
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
    /// The body does not match the checksum written with it.
    ///
    /// This is the one rkyv cannot find. A flipped bit inside an integer is a
    /// structurally perfect archive of the wrong number.
    Corrupt {
        /// Which page, counting from zero.
        page: usize,
        /// The checksum in the header.
        expected: u32,
        /// The checksum of the bytes actually there.
        found: u32,
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
    /// A page's rows did not deserialize.
    Rows {
        /// Which page, counting from zero.
        page: usize,
    },
    /// A page's header promised a row count its body did not contain.
    RowCount {
        /// Which page, counting from zero.
        page: usize,
        /// What the header promised.
        expected: usize,
        /// What the body held.
        found: usize,
    },
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
            Self::Corrupt {
                page,
                expected,
                found,
            } => write!(
                formatter,
                "page {page} checksums {found:#010x} and its header says {expected:#010x}"
            ),
            Self::Inconsistent { page } => {
                write!(formatter, "page {page} names a different row type")
            }
            Self::ForeignRows { found, expected } => write!(
                formatter,
                "these are row type {found:#010x}, and this is row type {expected:#010x}"
            ),
            Self::Rows { page } => write!(formatter, "page {page} did not deserialize"),
            Self::RowCount {
                page,
                expected,
                found,
            } => write!(
                formatter,
                "page {page} promised {expected} rows and held {found}"
            ),
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
/// The one thing [`Codec::decode`] can say. Which page it happened on is the
/// caller's to add, because a codec does not know it is reading a page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotAnArchive;

pub trait Codec: Sized {
    /// Rows to bytes.
    fn encode(&self) -> AlignedVec<16>;
    /// Bytes back to rows.
    ///
    /// # Errors
    ///
    /// Fails when the bytes are not this type's archive.
    fn decode(bytes: &[u8]) -> Result<Self, NotAnArchive>;
}

impl<T> Codec for T
where
    T: Archive
        + for<'a> Serialize<Strategy<Serializer<AlignedVec<16>, ArenaHandle<'a>, Share>, RkyvError>>,
    <T as Archive>::Archived: Deserialize<T, HighDeserializer<RkyvError>>
        + for<'a> CheckBytes<HighValidator<'a, RkyvError>>,
{
    fn encode(&self) -> AlignedVec<16> {
        // Infallible in practice: the only failure rkyv reports here is an
        // allocator refusing, which on this path means the process is already
        // out of memory.
        rkyv::to_bytes::<RkyvError>(self).expect("rows serialize")
    }

    fn decode(bytes: &[u8]) -> Result<Self, NotAnArchive> {
        rkyv::from_bytes::<T, RkyvError>(bytes).map_err(|_| NotAnArchive)
    }
}

/// What row type wrote these bytes.
///
/// FNV-1a over `core::any::type_name`, which is neither stable across compiler
/// versions nor guaranteed unique. That is fine for what it is for: refusing an
/// obvious mismatch, not authenticating a schema. A false match is possible and
/// a false mismatch is a rebuild, so it fails toward refusing to load rather
/// than toward reinterpreting.
pub(crate) fn fingerprint<T: ?Sized>() -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in core::any::type_name::<T>().as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// CRC-32, the usual reversed polynomial, computed a nibble at a time.
///
/// Sixteen entries rather than a 256 entry table: this runs once per 16 KiB
/// page, so the table is cache noise and the loop is not the cost of anything.
fn crc32(bytes: &[u8]) -> u32 {
    const NIBBLE: [u32; 16] = [
        0x0000_0000,
        0x1db7_1064,
        0x3b6e_20c8,
        0x26d9_30ac,
        0x76dc_4190,
        0x6b6b_51f4,
        0x4db2_6158,
        0x5005_713c,
        0xedb8_8320,
        0xf00f_9344,
        0xd6d6_a3e8,
        0xcb61_b38c,
        0x9b64_c2b0,
        0x86d3_d2d4,
        0xa00a_e278,
        0xbdbd_f21c,
    ];
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        crc = (crc >> 4) ^ NIBBLE[(crc & 0x0f) as usize];
        crc = (crc >> 4) ^ NIBBLE[(crc & 0x0f) as usize];
    }
    !crc
}

/// DataBucket's `GeneralHeader`, byte for byte.
///
/// Seven little-endian `u32`s in declaration order, which is what
/// `rkyv::to_bytes` of that struct actually produces: no relative pointers, and
/// `page_type` padded from `u16` to four bytes. Verified against
/// `data_bucket 0.5.7`, which for a `Data` page of space 3, id 7, previous 6,
/// next 8, length `0x11223344` emits:
///
/// ```text
/// 02000000 03000000 07000000 06000000 08000000 02000000 44332211
/// version  space    page     previous next     type     length
/// ```
///
/// It is reproduced here rather than imported because `data_bucket` is `std`
/// (tokio for file access, eyre through its signatures) and this crate is not.
/// **That is a real duplication and the risk that comes with it is a layout
/// drifting apart in two places**, which is why the bytes above are written
/// down and `the_header_matches_databuckets_layout` checks them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Header {
    /// `DATA_VERSION`. See [`PAGE_VERSION`].
    version: u32,
    /// Carries the row type fingerprint rather than a space id.
    ///
    /// A `Vec<(K, V)>` belongs to no space, and the field is a `u32` sitting in
    /// the right place, so it holds the one identity these pages do have. A
    /// WorkTable reader will see a space id it does not recognise, which is the
    /// honest outcome: these are not its rows.
    schema: u32,
    page: u32,
    previous: u32,
    next: u32,
    /// `PageType::Data`, which is 2.
    page_type: u32,
    /// Bytes of row archive in this page, before the directory.
    body: u32,
}

impl Header {
    fn write(self, out: &mut Vec<u8>) {
        for field in [
            self.version,
            self.schema,
            self.page,
            self.previous,
            self.next,
            self.page_type,
            self.body,
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
            previous: at(3),
            next: at(4),
            page_type: at(5),
            body: at(6),
        }
    }
}

/// The row directory, at the tail of every page.
///
/// **This is the slotted part.** The header is DataBucket's and has nowhere to
/// say how many rows a page holds, which is exactly the gap that makes a
/// WorkTable data page unreadable without its index. Putting the count in the
/// page means the page describes itself.
///
/// Eight bytes at the very end: the row count, then a CRC-32 of the body. The
/// checksum lives here rather than in the header for the same reason the count
/// does, and because a header this crate did not design has no spare field for
/// it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Directory {
    rows: u32,
    crc: u32,
}

impl Directory {
    fn write(self, page: &mut [u8]) {
        let at = page.len() - DIRECTORY_SIZE;
        page[at..at + 4].copy_from_slice(&self.rows.to_le_bytes());
        page[at + 4..].copy_from_slice(&self.crc.to_le_bytes());
    }

    fn read(page: &[u8]) -> Self {
        let at = page.len() - DIRECTORY_SIZE;
        let word = |n: usize| {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&page[n..n + 4]);
            u32::from_le_bytes(bytes)
        };
        Self {
            rows: word(at),
            crc: word(at + 4),
        }
    }
}

/// The most rows of `rows` whose archive fits one page body.
///
/// **Bounded probes.** The obvious version binary searches over the whole
/// remaining slice, which re-serializes every row still to be written on
/// every probe, for every page. That measured 2.3 seconds to write what rkyv
/// alone encodes in 6.7 ms, because the work is quadratic in the row count.
///
/// So the search is bounded to roughly two pages of rows: one sample encode
/// gives bytes per row, the estimate from that sets the ceiling, and the
/// binary search runs under it. Every probe serializes about a page, never a
/// file. Uniform rows land in a probe or two and wildly variable rows still
/// terminate, because the ceiling is only a ceiling.
///
/// Always returns at least one for a non-empty slice, so the caller always
/// makes progress. A single row too large for a page is written as an
/// oversized page rather than looping forever.
fn rows_per_page<K, V>(rows: &[(K, V)], hint: usize) -> usize
where
    Vec<(K, V)>: Codec,
    (K, V): Clone,
{
    if rows.is_empty() {
        return 0;
    }

    let fits = |take: usize| rows[..take].to_vec().encode().len() <= BODY_SIZE;

    // A page holds about what the last one held, so start there and walk.
    // Uniform rows settle in a probe or two; only the first page, or a run
    // whose rows change size, pays for a search.
    if hint > 0 && hint <= rows.len() && fits(hint) {
        let mut take = hint;
        while take < rows.len() && fits(take + 1) {
            take += 1;
        }
        return take;
    }

    // No usable hint, or the rows grew. One sample gives bytes per row, and
    // the estimate from it bounds the search to about two pages of rows.
    let sample = rows.len().min(64);
    let sampled = rows[..sample].to_vec().encode().len();
    let estimate = (BODY_SIZE * sample)
        .checked_div(sampled)
        .map_or(rows.len(), |estimate| estimate.max(1));
    let mut low = 1usize;
    let mut high = rows.len().min(estimate.saturating_mul(2)).max(1);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// Rows to pages, each page standing alone.
pub(crate) fn to_pages<K, V>(rows: &[(K, V)], schema: u32) -> Vec<u8>
where
    Vec<(K, V)>: Codec,
    (K, V): Clone,
{
    let mut out = Vec::new();
    let mut rest = rows;
    let mut hint = 0usize;

    // An empty table still writes one page. A zero byte file is
    // indistinguishable from a missing one, and a load has to tell "no rows"
    // from "nothing landed".
    loop {
        // Zero for an empty table, which still writes its one page and stops.
        // At least one for anything else, so this always makes progress.
        let take = rows_per_page(rest, hint);
        hint = take;
        let archive = rest[..take].to_vec().encode();
        let body = archive.as_ref();
        let page = u32::try_from(out.len() / PAGE_SIZE).expect("a page index inside u32");
        let last = rest.len() == take;
        Header {
            version: PAGE_VERSION,
            schema,
            page,
            previous: page.saturating_sub(1),
            // A last page points at itself, so a chain walker stops rather than
            // running off the end.
            next: if last { page } else { page + 1 },
            page_type: PAGE_TYPE_DATA,
            body: u32::try_from(body.len()).expect("a body inside u32"),
        }
        .write(&mut out);
        out.extend_from_slice(body);
        out.resize(out.len().next_multiple_of(PAGE_SIZE), 0);

        // The directory goes in last, into the tail of the page just written.
        let start = out.len() - PAGE_SIZE;
        Directory {
            rows: u32::try_from(take).expect("a row count inside u32"),
            crc: crc32(body),
        }
        .write(&mut out[start..]);

        rest = &rest[take..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// One page back into rows, with every header field checked.
pub(crate) fn page_rows<K, V>(
    raw: &[u8],
    index: usize,
    schema: &mut Option<u32>,
) -> Result<Vec<(K, V)>, LoadError>
where
    Vec<(K, V)>: Codec,
{
    let header = Header::read(&raw[..HEADER_SIZE]);
    if header.version != PAGE_VERSION {
        return Err(LoadError::ForeignPages {
            page: index,
            version: header.version,
        });
    }
    match schema {
        None => *schema = Some(header.schema),
        // Every page names the row type, so a file spliced onto another is
        // caught where they stop agreeing rather than concatenated.
        Some(first) if *first != header.schema => {
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
    let directory = Directory::read(raw);
    let body = &raw[HEADER_SIZE..HEADER_SIZE + take];
    let found = crc32(body);
    if found != directory.crc {
        return Err(LoadError::Corrupt {
            page: index,
            expected: directory.crc,
            found,
        });
    }

    // Copied into an AlignedVec because rkyv reads an archive in place and
    // needs it aligned. A page body sits at a header's offset into a Vec<u8>,
    // which is aligned to nothing in particular.
    let mut aligned = AlignedVec::<16>::with_capacity(take);
    aligned.extend_from_slice(body);
    let rows =
        Vec::<(K, V)>::decode(&aligned).map_err(|NotAnArchive| LoadError::Rows { page: index })?;
    if rows.len() != directory.rows as usize {
        return Err(LoadError::RowCount {
            page: index,
            expected: directory.rows as usize,
            found: rows.len(),
        });
    }
    Ok(rows)
}

/// Every page back into one row vector.
fn from_pages<K, V>(bytes: &[u8]) -> Result<Vec<(K, V)>, LoadError>
where
    Vec<(K, V)>: Codec,
{
    if bytes.is_empty() || bytes.len() % PAGE_SIZE != 0 {
        return Err(LoadError::NotWholePages { found: bytes.len() });
    }

    let mut schema = None;
    let mut rows = Vec::new();
    for (index, raw) in bytes.chunks_exact(PAGE_SIZE).enumerate() {
        rows.append(&mut page_rows(raw, index, &mut schema)?);
    }
    let expected = fingerprint::<Vec<(K, V)>>();
    match schema {
        Some(found) if found != expected => Err(LoadError::ForeignRows { found, expected }),
        _ => Ok(rows),
    }
}

impl<K, V> LinearTable<K, V>
where
    Vec<(K, V)>: Codec,
    (K, V): Clone,
{
    /// Every row, as pages.
    #[must_use]
    pub fn unload(&self) -> Vec<u8> {
        to_pages(&self.rows, fingerprint::<Vec<(K, V)>>())
    }

    /// The rows from `first` on, as pages, ready to append to a file that
    /// already holds the ones before it.
    ///
    /// Appending is possible at all because pages stand alone: the existing
    /// file is untouched and these pages are simply more of them.
    #[must_use]
    pub fn unload_appending(&self, first: usize) -> Vec<u8> {
        let first = first.min(self.rows.len());
        to_pages(&self.rows[first..], fingerprint::<Vec<(K, V)>>())
    }

    /// Rows back from pages.
    ///
    /// # Errors
    ///
    /// Refuses bytes that are not whole pages, a page from another version, a
    /// header claiming more body than a page holds, a body that fails its
    /// checksum, pages that disagree about the row type, another row type, or
    /// rows that do not deserialize.
    pub fn load(bytes: &[u8]) -> Result<Self, LoadError> {
        Ok(Self {
            rows: from_pages(bytes)?,
        })
    }
}

impl<K, V> IndexedTable<K, V>
where
    Vec<(K, V)>: Codec,
    (K, V): Clone,
    K: Ord + Clone,
{
    /// Every row, as pages.
    ///
    /// The index is not written. It is derived from the rows, so rebuilding it
    /// on load costs one pass, where storing it would cost bytes at rest and a
    /// second thing that can disagree with the rows.
    #[must_use]
    pub fn unload(&self) -> Vec<u8> {
        to_pages(&self.rows, fingerprint::<Vec<(K, V)>>())
    }

    /// The rows from `first` on, as pages.
    #[must_use]
    pub fn unload_appending(&self, first: usize) -> Vec<u8> {
        let first = first.min(self.rows.len());
        to_pages(&self.rows[first..], fingerprint::<Vec<(K, V)>>())
    }

    /// Rows back from pages, with the index rebuilt.
    ///
    /// # Errors
    ///
    /// As [`LinearTable::load`].
    pub fn load(bytes: &[u8]) -> Result<Self, LoadError> {
        let rows = from_pages(bytes)?;
        let mut table = Self::with_capacity(rows.len());
        for (key, value) in rows {
            let _ = table.insert(key, value);
        }
        Ok(table)
    }
}

mod io;
pub use io::HydrateError;

#[cfg(test)]
mod tests;
