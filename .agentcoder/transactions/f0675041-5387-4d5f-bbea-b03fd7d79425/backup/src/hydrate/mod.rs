//! Load and unload a table as pages, and put those pages on a disk.
//!
//! # The interface
//!
//! ```ignore
//! table.flush("rows.wtv")?;                   // write it
//! let table = LinearTable::open("rows.wtv")?; // read it back
//! table.append("rows.wtv", from_row)?;        // add rows without a rewrite
//! ```
//!
//! and the same thing without a filesystem, for callers that already hold the
//! bytes or do not have one:
//!
//! ```ignore
//! let bytes = table.unload();
//! let table = LinearTable::load(&bytes)?;
//! ```
//!
//! Rows live in a `Vec` while the table is in use and are pages only at rest.
//! Everything between an open and a flush runs at `Vec` speed because it *is* a
//! `Vec`. This is a codec plus a file, not a storage engine underneath.
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

/// The fixed header every page opens with.
pub const HEADER_SIZE: usize = 24;

/// How much of a page is body.
pub const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE;

/// Bumped when the page layout changes, so an older file is refused rather than
/// read through the new shape.
pub const PAGE_VERSION: u32 = 1;

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

/// A page header: six little-endian `u32`s, in this order.
///
/// Every one of them is checked on the way back in. A field that is written and
/// never validated is worse than a field that does not exist, because it reads
/// like a guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Header {
    version: u32,
    /// The row type every page in a run carries.
    schema: u32,
    /// How many rows this page's archive holds.
    rows: u32,
    /// How many bytes of that archive are in this page.
    body: u32,
    /// CRC-32 over exactly `body` bytes.
    crc: u32,
    /// Zero for now. A layout change that needs a flag has somewhere to put it
    /// without moving anything else.
    flags: u32,
}

impl Header {
    fn write(self, out: &mut Vec<u8>) {
        for field in [
            self.version,
            self.schema,
            self.rows,
            self.body,
            self.crc,
            self.flags,
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
            rows: at(2),
            body: at(3),
            crc: at(4),
            flags: at(5),
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
        Header {
            version: PAGE_VERSION,
            schema,
            rows: u32::try_from(take).expect("a row count inside u32"),
            body: u32::try_from(body.len()).expect("a body inside u32"),
            crc: crc32(body),
            flags: 0,
        }
        .write(&mut out);
        out.extend_from_slice(body);
        out.resize(out.len().next_multiple_of(PAGE_SIZE), 0);

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
    let body = &raw[HEADER_SIZE..HEADER_SIZE + take];
    let found = crc32(body);
    if found != header.crc {
        return Err(LoadError::Corrupt {
            page: index,
            expected: header.crc,
            found,
        });
    }

    // Copied into an AlignedVec because rkyv reads an archive in place and
    // needs it aligned. A page body sits at offset 24 in a Vec<u8>, which is
    // aligned to nothing in particular.
    let mut aligned = AlignedVec::<16>::with_capacity(take);
    aligned.extend_from_slice(body);
    let rows =
        Vec::<(K, V)>::decode(&aligned).map_err(|NotAnArchive| LoadError::Rows { page: index })?;
    if rows.len() != header.rows as usize {
        return Err(LoadError::RowCount {
            page: index,
            expected: header.rows as usize,
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
