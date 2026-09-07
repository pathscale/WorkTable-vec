//! Reading and writing pages, without knowing what they are stored on.
//!
//! # no_std and real I/O at the same time
//!
//! The I/O is a pair of traits, not a filesystem. A caller with an operating
//! system wraps a `std::fs::File` and gets real files; a caller without one
//! implements two methods over whatever it has. Neither costs this crate `std`,
//! and there is no second code path for the two cases.
//!
//! ```ignore
//! // a real file, through the adapter
//! let file = std::fs::File::create("rows.wtv")?;
//! table.unload_to(&mut embedded_io_adapters::std::FromStd::new(file))?;
//!
//! // memory, because Vec<u8> and &[u8] already implement the traits
//! let mut bytes = Vec::new();
//! table.unload_to(&mut bytes)?;
//! ```
//!
//! # Streaming, one page at a time
//!
//! A write emits a page and moves on; a read consumes a page and moves on.
//! Neither holds the whole file, which is the other half of why pages stand
//! alone: a reader that had to see the last page before trusting the first
//! could not stream at all.
//!
//! Appending needs no support here. Pages are self contained, so appending is
//! opening the sink in append mode and writing more of them.

use alloc::vec::Vec;

use embedded_io::{Read, Write};

use super::{Codec, LoadError, PAGE_SIZE, RowTooLarge, fingerprint, page_rows, to_pages};
use crate::{IndexedTable, LinearTable};

/// A read that failed, either at the transport or at the page.
///
/// The two are kept apart on purpose. A disk that would not answer and a page
/// that was not what it claimed are different problems with different fixes,
/// and collapsing them into one string loses which one happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HydrateError<E> {
    /// The reader failed.
    Io(E),
    /// The reader worked and the bytes were wrong.
    Page(LoadError),
    /// The last page stopped part way through.
    ///
    /// Distinct from a bad page: this is a write that did not finish, not a
    /// page that was damaged after it did.
    Torn {
        /// Which page, counting from zero.
        page: usize,
        /// How many bytes of it arrived.
        found: usize,
    },
}

impl<E> From<LoadError> for HydrateError<E> {
    fn from(error: LoadError) -> Self {
        Self::Page(error)
    }
}

impl<E: core::fmt::Display> core::fmt::Display for HydrateError<E> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "the reader failed: {error}"),
            Self::Page(error) => error.fmt(formatter),
            Self::Torn { page, found } => write!(
                formatter,
                "page {page} stops after {found} of {PAGE_SIZE} bytes"
            ),
        }
    }
}

impl<E: core::fmt::Debug + core::fmt::Display> core::error::Error for HydrateError<E> {}

/// A write that failed, either at the transport or at the rows.
///
/// The mirror of [`HydrateError`], and split for the same reason: a sink that
/// would not take the bytes and a row that cannot be written at all are
/// different problems.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnloadError<E> {
    /// The writer failed.
    Io(E),
    /// The rows could not be made into pages.
    Row(RowTooLarge),
}

impl<E> From<RowTooLarge> for UnloadError<E> {
    fn from(error: RowTooLarge) -> Self {
        Self::Row(error)
    }
}

impl<E: core::fmt::Display> core::fmt::Display for UnloadError<E> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "the writer failed: {error}"),
            Self::Row(error) => error.fmt(formatter),
        }
    }
}

impl<E: core::fmt::Debug + core::fmt::Display> core::error::Error for UnloadError<E> {}

/// Fill `page` from `source`, or say how far it got.
///
/// Returns `Ok(false)` at a clean end of input, which is the only case where a
/// short read is not a problem.
fn fill<R: Read>(
    source: &mut R,
    page: &mut [u8],
    index: usize,
) -> Result<bool, HydrateError<R::Error>> {
    let mut filled = 0;
    while filled < page.len() {
        match source.read(&mut page[filled..]).map_err(HydrateError::Io)? {
            0 if filled == 0 => return Ok(false),
            0 => {
                return Err(HydrateError::Torn {
                    page: index,
                    found: filled,
                });
            }
            read => filled += read,
        }
    }
    Ok(true)
}

/// Every page from a reader, back into rows.
fn read_rows<K, V, R: Read>(source: &mut R) -> Result<Vec<(K, V)>, HydrateError<R::Error>>
where
    Vec<(K, V)>: Codec,
{
    let mut page = alloc::vec![0u8; PAGE_SIZE];
    let mut schema = None;
    let mut rows = Vec::new();
    let mut index = 0;

    while fill(source, &mut page, index)? {
        rows.append(&mut page_rows(&page, index, &mut schema)?);
        index += 1;
    }

    if index == 0 {
        return Err(HydrateError::Page(LoadError::NotWholePages { found: 0 }));
    }
    let expected = fingerprint::<Vec<(K, V)>>();
    match schema {
        Some(found) if found != expected => Err(HydrateError::Page(LoadError::ForeignRows {
            found,
            expected,
        })),
        _ => Ok(rows),
    }
}

impl<K, V> LinearTable<K, V>
where
    Vec<(K, V)>: Codec,
    (K, V): Clone,
{
    /// Write every row, as pages.
    ///
    /// # Errors
    ///
    /// Whatever the writer reports, or a row too large for a page.
    pub fn unload_to<W: Write>(&self, sink: &mut W) -> Result<(), UnloadError<W::Error>> {
        let pages = to_pages(&self.rows, fingerprint::<Vec<(K, V)>>())?;
        sink.write_all(&pages).map_err(UnloadError::Io)?;
        sink.flush().map_err(UnloadError::Io)
    }

    /// Write the rows from `first` on, for a sink already holding the rest.
    ///
    /// Appending works because pages stand alone: what is already written is
    /// untouched and these are simply more pages.
    ///
    /// # Errors
    ///
    /// Whatever the writer reports, or a row too large for a page.
    pub fn append_to<W: Write>(
        &self,
        sink: &mut W,
        first: usize,
    ) -> Result<(), UnloadError<W::Error>> {
        let first = first.min(self.rows.len());
        if first == self.rows.len() {
            return sink.flush().map_err(UnloadError::Io);
        }
        let pages = to_pages(&self.rows[first..], fingerprint::<Vec<(K, V)>>())?;
        sink.write_all(&pages).map_err(UnloadError::Io)?;
        sink.flush().map_err(UnloadError::Io)
    }

    /// Read a table back from a reader.
    ///
    /// # Errors
    ///
    /// The reader's own errors, a page that stops part way, or any of the
    /// refusals in [`LoadError`].
    pub fn load_from<R: Read>(source: &mut R) -> Result<Self, HydrateError<R::Error>> {
        Ok(Self {
            rows: read_rows(source)?,
        })
    }
}

impl<K, V> IndexedTable<K, V>
where
    Vec<(K, V)>: Codec,
    (K, V): Clone,
    K: Ord + Clone,
{
    /// Write every row, as pages.
    ///
    /// The index is not written, because it is derived from the rows.
    ///
    /// # Errors
    ///
    /// Whatever the writer reports, or a row too large for a page.
    pub fn unload_to<W: Write>(&self, sink: &mut W) -> Result<(), UnloadError<W::Error>> {
        let pages = to_pages(&self.rows, fingerprint::<Vec<(K, V)>>())?;
        sink.write_all(&pages).map_err(UnloadError::Io)?;
        sink.flush().map_err(UnloadError::Io)
    }

    /// Read a table back from a reader, rebuilding the index.
    ///
    /// # Errors
    ///
    /// As [`LinearTable::read`].
    pub fn load_from<R: Read>(source: &mut R) -> Result<Self, HydrateError<R::Error>> {
        let rows = read_rows(source)?;
        let mut table = Self::with_capacity(rows.len());
        for (key, value) in rows {
            let _ = table.insert(key, value);
        }
        Ok(table)
    }
}
