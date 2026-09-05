//! Explicit, non-concurrent `Vec`-backed table baselines.
//!
//! This crate isolates the storage pattern applications otherwise tend to
//! grow ad hoc: ordered rows, linear point lookup, and an optional `BTreeMap`
//! side index. It is intentionally not presented as a durable or concurrent
//! WorkTable backend. Its purpose is to make the baseline a named component
//! with a stable contract so database comparisons use identical rows.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
#[cfg(feature = "congee")]
use alloc::sync::Arc;
use alloc::vec::Vec;

#[cfg(feature = "arctic")]
use arctic::{ConcurrentMap, Key};
#[cfg(feature = "congee")]
use congee::Congee;
#[cfg(feature = "wti")]
use wti::concurrent::map::BTreeMap as WtiMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertError<K> {
    DuplicateKey(K),
    OutOfMemory(K),
}

/// Ordered rows with the same linear point lookup used by an unindexed
/// application `Vec`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct LinearTable<K, V> {
    rows: Vec<(K, V)>,
}

/// A name for [`LinearTable`] that emphasizes its exact Vec-backed shape.
pub type VecTable<K, V> = LinearTable<K, V>;

impl<K, V> LinearTable<K, V> {
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
        }
    }

    /// Append without checking key uniqueness, with the same amortized O(1)
    /// behavior as [`Vec::push`].
    pub fn push(&mut self, key: K, value: V) -> usize {
        let row = self.rows.len();
        self.rows.push((key, value));
        row
    }

    pub fn get_row(&self, row: usize) -> Option<&(K, V)> {
        self.rows.get(row)
    }

    pub fn get_row_mut(&mut self, row: usize) -> Option<&mut (K, V)> {
        self.rows.get_mut(row)
    }

    pub fn as_slice(&self) -> &[(K, V)] {
        self.rows.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [(K, V)] {
        self.rows.as_mut_slice()
    }

    pub fn iter(&self) -> core::slice::Iter<'_, (K, V)> {
        self.rows.iter()
    }

    pub fn iter_mut(&mut self) -> core::slice::IterMut<'_, (K, V)> {
        self.rows.iter_mut()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.rows.reserve(additional);
    }

    pub fn capacity(&self) -> usize {
        self.rows.capacity()
    }

    pub fn into_rows(self) -> Vec<(K, V)> {
        self.rows
    }

    pub fn rows(&self) -> &[(K, V)] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<K, V> LinearTable<K, V>
where
    K: Eq,
{
    pub fn insert(&mut self, key: K, value: V) -> Result<usize, InsertError<K>> {
        if self.rows.iter().any(|(present, _)| present == &key) {
            return Err(InsertError::DuplicateKey(key));
        }
        let row = self.rows.len();
        self.rows.push((key, value));
        Ok(row)
    }

    #[inline]
    pub fn select(&self, key: &K) -> Option<&V> {
        self.rows
            .iter()
            .find(|(present, _)| present == key)
            .map(|(_, value)| value)
    }
}

impl<K, V> From<Vec<(K, V)>> for LinearTable<K, V> {
    fn from(rows: Vec<(K, V)>) -> Self {
        Self { rows }
    }
}

impl<K, V> AsRef<[(K, V)]> for LinearTable<K, V> {
    fn as_ref(&self) -> &[(K, V)] {
        self.as_slice()
    }
}

/// Ordered `Vec` storage with a separately maintained ordered primary index.
///
/// Values remain in insertion order; the map contains only key-to-row
/// positions. This is the common application-level `Vec + BTreeMap` pattern,
/// made explicit so its build/query/memory cost can be measured honestly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexedTable<K, V> {
    rows: Vec<(K, V)>,
    primary: BTreeMap<K, usize>,
}

/// Ordered `Vec` storage indexed by Arctic row offsets.
///
/// The row payload stays byte-for-byte the same as [`IndexedTable`]. Arctic
/// holds only `K -> u64 row offset`, so an A/B attributes the difference to
/// the index rather than to a different row representation.
#[cfg(feature = "arctic")]
pub struct ArcticTable<K: Key, V> {
    rows: Vec<(K, V)>,
    primary: ConcurrentMap<K, u64>,
}

#[cfg(feature = "arctic")]
impl<K: Key, V> core::fmt::Debug for ArcticTable<K, V> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ArcticTable")
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "arctic")]
impl<K: Key, V> Default for ArcticTable<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "arctic")]
impl<K: Key, V> ArcticTable<K, V> {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            primary: ConcurrentMap::default(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            primary: ConcurrentMap::default(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Result<usize, InsertError<K>> {
        let row = self.rows.len();
        if self.primary.insert(key.as_insert(), row as u64).is_err() {
            return Err(InsertError::DuplicateKey(key));
        }
        self.rows.push((key, value));
        Ok(row)
    }

    #[inline]
    pub fn select(&self, key: &K::Borrowed) -> Option<&V> {
        let row = *self.primary.get(key)? as usize;
        self.rows.get(row).map(|(_, value)| value)
    }

    pub fn rows(&self) -> &[(K, V)] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Ordered `Vec` storage indexed by Congee row offsets.
///
/// Congee's public key API is fixed-width: `K` must be copyable and round-trip
/// through `usize`. The row payload remains in the Vec; Congee stores only the
/// row offset.
#[cfg(feature = "congee")]
pub struct CongeeTable<K, V>
where
    K: From<usize> + Copy,
    usize: From<K>,
{
    rows: Vec<(K, V)>,
    primary: Congee<K, usize>,
}

#[cfg(feature = "congee")]
impl<K, V> core::fmt::Debug for CongeeTable<K, V>
where
    K: From<usize> + Copy,
    usize: From<K>,
{
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CongeeTable")
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "congee")]
impl<K, V> Default for CongeeTable<K, V>
where
    K: From<usize> + Copy,
    usize: From<K>,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "congee")]
impl<K, V> CongeeTable<K, V>
where
    K: From<usize> + Copy,
    usize: From<K>,
{
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            primary: Congee::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            primary: Congee::new(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Result<usize, InsertError<K>> {
        let guard = self.primary.pin();
        if self.primary.get(key, &guard).is_some() {
            return Err(InsertError::DuplicateKey(key));
        }

        let row = self.rows.len();
        match self.primary.insert(key, Arc::new(row), &guard) {
            Ok(None) => {
                self.rows.push((key, value));
                Ok(row)
            }
            Ok(Some(_)) => Err(InsertError::DuplicateKey(key)),
            Err(_) => Err(InsertError::OutOfMemory(key)),
        }
    }

    #[inline]
    pub fn select(&self, key: K) -> Option<&V> {
        let guard = self.primary.pin();
        let row = *self.primary.get(key, &guard)?;
        self.rows.get(row).map(|(_, value)| value)
    }

    pub fn rows(&self) -> &[(K, V)] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Ordered `Vec` storage indexed by WorkTablesIndex row offsets.
///
/// The row payload remains in the Vec. WTI stores only a cloned key and the
/// row offset in its concurrent index.
#[cfg(feature = "wti")]
pub struct WtiTable<K, V>
where
    K: core::fmt::Debug + Send + Ord + Clone + 'static,
{
    rows: Vec<(K, V)>,
    primary: WtiMap<K, usize>,
}

#[cfg(feature = "wti")]
impl<K, V> core::fmt::Debug for WtiTable<K, V>
where
    K: core::fmt::Debug + Send + Ord + Clone + 'static,
{
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("WtiTable")
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "wti")]
impl<K, V> Default for WtiTable<K, V>
where
    K: core::fmt::Debug + Send + Ord + Clone + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "wti")]
impl<K, V> WtiTable<K, V>
where
    K: core::fmt::Debug + Send + Ord + Clone + 'static,
{
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            primary: WtiMap::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            primary: WtiMap::new(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Result<usize, InsertError<K>> {
        let row = self.rows.len();
        if self.primary.checked_insert(key.clone(), row).is_none() {
            return Err(InsertError::DuplicateKey(key));
        }
        self.rows.push((key, value));
        Ok(row)
    }

    #[inline]
    pub fn select(&self, key: &K) -> Option<&V> {
        let row = self.primary.lookup_for_select(key)?;
        self.rows.get(row).map(|(_, value)| value)
    }

    pub fn rows(&self) -> &[(K, V)] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<K, V> IndexedTable<K, V>
where
    K: Clone + Ord,
{
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            primary: BTreeMap::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            primary: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) -> Result<usize, InsertError<K>> {
        if self.primary.contains_key(&key) {
            return Err(InsertError::DuplicateKey(key));
        }
        let row = self.rows.len();
        self.rows.push((key.clone(), value));
        self.primary.insert(key, row);
        Ok(row)
    }

    #[inline]
    pub fn select(&self, key: &K) -> Option<&V> {
        self.primary
            .get(key)
            .and_then(|row| self.rows.get(*row))
            .map(|(_, value)| value)
    }

    pub fn rows(&self) -> &[(K, V)] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_and_indexed_tables_have_the_same_row_contract() {
        let mut linear = LinearTable::new();
        let mut indexed = IndexedTable::new();
        for (key, value) in [(7_u64, "a"), (2, "b"), (11, "c")] {
            linear.insert(key, value).unwrap();
            indexed.insert(key, value).unwrap();
        }

        assert_eq!(linear.rows(), indexed.rows());
        for key in [2_u64, 7, 11, 99] {
            assert_eq!(linear.select(&key), indexed.select(&key));
        }
        assert_eq!(
            linear.insert(7, "duplicate"),
            Err(InsertError::DuplicateKey(7_u64))
        );
        assert_eq!(
            indexed.insert(7, "duplicate"),
            Err(InsertError::DuplicateKey(7_u64))
        );
    }

    #[test]
    fn vec_table_exposes_the_vec_fast_path_without_extra_storage() {
        assert_eq!(
            core::mem::size_of::<VecTable<u64, u64>>(),
            core::mem::size_of::<alloc::vec::Vec<(u64, u64)>>()
        );

        let mut table = VecTable::with_capacity(3);
        assert_eq!(table.push(7, "a"), 0);
        assert_eq!(table.push(7, "duplicate allowed"), 1);
        assert_eq!(table.get_row(1), Some(&(7, "duplicate allowed")));
        assert_eq!(table.as_slice().len(), 2);
        assert!(table.capacity() >= 3);
    }

    #[cfg(feature = "arctic")]
    #[test]
    fn arctic_table_has_the_same_row_contract() {
        let mut linear = LinearTable::new();
        let mut arctic = ArcticTable::new();
        for (key, value) in [(7_u64, "a"), (2, "b"), (11, "c")] {
            linear.insert(key, value).unwrap();
            arctic.insert(key, value).unwrap();
        }

        assert_eq!(linear.rows(), arctic.rows());
        for key in [2_u64, 7, 11, 99] {
            assert_eq!(linear.select(&key), arctic.select(&key));
        }
        assert_eq!(
            arctic.insert(7, "duplicate"),
            Err(InsertError::DuplicateKey(7_u64))
        );
    }

    #[cfg(feature = "congee")]
    #[test]
    fn congee_table_has_the_same_row_contract() {
        let mut linear = LinearTable::new();
        let mut congee = CongeeTable::new();
        for (key, value) in [(7_usize, "a"), (2, "b"), (11, "c")] {
            linear.insert(key, value).unwrap();
            congee.insert(key, value).unwrap();
        }

        assert_eq!(linear.rows(), congee.rows());
        for key in [2_usize, 7, 11, 99] {
            assert_eq!(linear.select(&key), congee.select(key));
        }
        assert_eq!(
            congee.insert(7, "duplicate"),
            Err(InsertError::DuplicateKey(7_usize))
        );
    }

    #[cfg(feature = "wti")]
    #[test]
    fn wti_table_has_the_same_row_contract() {
        let mut linear = LinearTable::new();
        let mut wti = WtiTable::new();
        for (key, value) in [(7_usize, "a"), (2, "b"), (11, "c")] {
            linear.insert(key, value).unwrap();
            wti.insert(key, value).unwrap();
        }

        assert_eq!(linear.rows(), wti.rows());
        for key in [2_usize, 7, 11, 99] {
            assert_eq!(linear.select(&key), wti.select(&key));
        }
        assert_eq!(
            wti.insert(7, "duplicate"),
            Err(InsertError::DuplicateKey(7_usize))
        );
    }
}
