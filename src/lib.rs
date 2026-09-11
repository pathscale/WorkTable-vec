//! **Deprecated. Use `worktable` instead.**
//!
//! New table development lives in WorkTable 1.9.0-alpha1. Existing releases
//! remain available for callers and historical benchmark baselines.
//!
//! | here | there |
//! |---|---|
//! | Vec storage and index choices | `worktable!` with `vec: true` and explicit `using` choices; the generated row API is not source-compatible with the old generic tables |
//! | `AtomicKeyTable` | `worktable::atomic_key_table::AtomicKeyTable`, identical, with the snapshot question answered in its documentation |
//! | the `hydrate` feature's page codec | `worktable::vec_hydrate`, reached through `unload`, `unload_appending` and `load`; callers own byte I/O |
//!
//! Snapshot format 3 requires regeneration or an old-reader export. WorkTable
//! does not generate the old embedded-I/O convenience methods. Freestanding
//! callers must check its documented no-default-features dependency boundary
//! before migrating. This notice does not publish or yank a crate version.
//!
//! Explicit, non-concurrent `Vec`-backed table baselines.
//!
//! This crate isolates the storage pattern applications otherwise tend to
//! grow ad hoc: ordered rows, linear point lookup, and an optional `BTreeMap`
//! side index. It is intentionally not presented as a durable or concurrent
//! WorkTable backend. Its purpose is to make the baseline a named component
//! with a stable contract so database comparisons use identical rows.

#![no_std]
// The crate is no_std. Tests link std so they can put a page run on a real
// disk, which is the only way to show the traits reach one.
#[cfg(test)]
extern crate std;

extern crate alloc;

#[cfg(feature = "hydrate")]
mod hydrate;
#[cfg(feature = "hydrate")]
pub use hydrate::{Codec, HydrateError, LoadError, PAGE_SIZE, RowTooLarge, UnloadError};

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

/// A fixed-capacity table whose rows are claimed without blocking and updated in place.
///
/// # What this is for, and why the other tables here cannot do it
///
/// [`LinearTable`] and its indexed variants take `&mut self` to insert, because they push onto a
/// `Vec` and a push can reallocate - which moves every row. That is the right shape for the
/// baselines this crate exists to name, and it is unusable from several threads at once.
///
/// The case that needs something else is a **counter table**: many writers, a small set of keys
/// that stabilises almost immediately, and an update that is a read-modify-write on the row
/// rather than a replacement of it. Performance measurement is the archetype - the first
/// consumer of this crate records timings from sixteen workers against a handful of named sites.
/// `performance_measurement`'s `PerformanceProfiler` is the same pattern one level up, and it
/// reaches for `lockfree::map::Map`, which is not `no_std`.
///
/// # How it avoids both a lock and a reallocation
///
/// Capacity is fixed at construction and every value is built then, so **no row ever moves** and
/// `&V` handed out stays valid for the life of the table. A key is claimed with one
/// `compare_exchange`; after that, finding it is a plain load. The value is updated through `&V`,
/// so `V` supplies its own interior mutability - atomics, typically - and this crate takes no
/// position on what the row contains.
///
/// The load-before-claim order is the point. Claiming with a `compare_exchange` on every lookup
/// takes the cache line exclusively even when nothing changes, so writers contending for a key
/// they all already own serialise on it. A relaxed load is a shared read.
///
/// # What it does not do
///
/// No removal, no resize, and no iteration order beyond slot order. A full table refuses rather
/// than growing, and [`AtomicKeyTable::len`] says how many slots are taken so a caller can
/// see it coming. Keys are `usize`, the same width as the slot arithmetic that indexes them,
/// and zero is the empty sentinel, so a caller whose key is a pointer, a hash or a `u64` maps
/// it in.
// A 64-bit `usize`, and it refuses rather than assuming one. `GOLDEN` below is
// a 64-bit constant, and truncating it to 32 bits leaves an even number, which
// is not invertible and quietly collapses keys onto the same slot. Nothing here
// is built or tested for a narrower target, so the honest answer is to say so
// at compile time instead of carrying a second constant nobody exercises.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("worktable-vec's AtomicKeyTable requires a 64-bit target");

/// Scatter a key across the table.
///
/// # Why not the low bits, and why not a modulo
///
/// The first version shifted the key right by four and took it modulo the capacity, which is
/// wrong twice. The shift assumed a pointer key, whose low bits are alignment zeros; handed
/// small integers it maps every key under sixteen to slot zero, and a measured lookup over
/// sixty-four sequential keys walked a probe chain 9.3x slower than a linear scan of the same
/// rows. The modulo is an integer division on the hottest path in the crate.
///
/// Fibonacci hashing fixes the first: multiplying by the golden ratio spreads any input across
/// the whole word, and taking the **high** bits of the product is what reads that spread. A
/// power-of-two capacity fixes the second: the index is then a mask.
#[inline(always)]
const fn scatter(key: usize, shift: u32, mask: usize) -> usize {
    // 2^64 / phi, odd so the multiply is invertible and no input is lost.
    const GOLDEN: usize = 0x9E37_79B9_7F4A_7C15u64 as usize;
    (key.wrapping_mul(GOLDEN) >> shift) & mask
}
#[derive(Debug)]
pub struct AtomicKeyTable<V> {
    keys: Vec<core::sync::atomic::AtomicUsize>,
    values: Vec<V>,
    /// Capacity is a power of two, so the index is a mask rather than a division.
    mask: usize,
    shift: u32,
}

impl<V: Default> AtomicKeyTable<V> {
    /// A table with at least `capacity` slots, every value built now.
    ///
    /// Rounded up to a power of two so the slot index is a mask rather than a division. Size it
    /// generously: this is open addressed with linear probing, so a table much past half full
    /// costs a long probe on every miss.
    pub fn with_capacity(capacity: usize) -> Self {
        let slots = capacity.max(1).next_power_of_two();
        let mut keys = Vec::with_capacity(slots);
        let mut values = Vec::with_capacity(slots);
        for _ in 0..slots {
            keys.push(core::sync::atomic::AtomicUsize::new(0));
            values.push(V::default());
        }
        Self {
            keys,
            values,
            mask: slots - 1,
            shift: usize::BITS - slots.trailing_zeros(),
        }
    }
}

impl<V> AtomicKeyTable<V> {
    /// The row for this key, claiming a slot if it has none yet.
    ///
    /// Named for `WorkTable`'s `upsert`: it returns the existing row or creates one, and never
    /// replaces what is there. The row is then updated through `&V`, which is where the
    /// difference from a `WorkTable` upsert lies - the value carries its own interior mutability
    /// rather than being written back whole.
    ///
    /// `None` means the table is full. Zero is the empty sentinel and is rejected rather than
    /// silently colliding with an unclaimed slot.
    pub fn upsert(&self, key: usize) -> Option<&V> {
        if key == 0 || self.keys.is_empty() {
            return None;
        }
        let capacity = self.keys.len();
        let mut at = scatter(key, self.shift, self.mask);
        for _ in 0..capacity {
            match self.keys[at].load(core::sync::atomic::Ordering::Acquire) {
                existing if existing == key => return Some(&self.values[at]),
                0 => {
                    match self.keys[at].compare_exchange(
                        0,
                        key,
                        core::sync::atomic::Ordering::AcqRel,
                        core::sync::atomic::Ordering::Acquire,
                    ) {
                        Ok(_) => return Some(&self.values[at]),
                        Err(taken) if taken == key => return Some(&self.values[at]),
                        Err(_) => at = (at + 1) & self.mask,
                    }
                }
                _ => at = (at + 1) & self.mask,
            }
        }
        None
    }

    /// The row for this key, or `None` if no row has been created for it. Never creates one.
    ///
    /// The same name and shape as [`LinearTable::select`], so a caller moving between the two
    /// tables in this crate reads one vocabulary.
    pub fn select(&self, key: usize) -> Option<&V> {
        if key == 0 || self.keys.is_empty() {
            return None;
        }
        let capacity = self.keys.len();
        let mut at = scatter(key, self.shift, self.mask);
        for _ in 0..capacity {
            match self.keys[at].load(core::sync::atomic::Ordering::Acquire) {
                existing if existing == key => return Some(&self.values[at]),
                0 => return None,
                _ => at = (at + 1) & self.mask,
            }
        }
        None
    }

    /// Every claimed row, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &V)> {
        self.keys
            .iter()
            .zip(self.values.iter())
            .filter_map(
                |(k, v)| match k.load(core::sync::atomic::Ordering::Acquire) {
                    0 => None,
                    key => Some((key, v)),
                },
            )
    }

    /// How many rows the table holds.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Whether any row has been created. Matches [`LinearTable::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many rows the table can hold. Fixed at construction.
    pub fn capacity(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
mod atomic_key_table_tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Counter(AtomicU64);

    #[test]
    fn a_claimed_row_is_found_by_a_plain_load_and_never_reclaimed() {
        let table: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(64);
        let first = table.upsert(7).expect("capacity");
        first.0.fetch_add(1, Ordering::Relaxed);
        let again = table.upsert(7).expect("already claimed");
        again.0.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            again.0.load(Ordering::Relaxed),
            2,
            "the second call found the same row"
        );
        assert_eq!(table.len(), 1, "one key claimed one slot");
    }

    #[test]
    fn zero_is_the_empty_sentinel_and_is_refused_rather_than_colliding() {
        let table: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(8);
        assert!(
            table.upsert(0).is_none(),
            "zero would be indistinguishable from empty"
        );
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn a_full_table_refuses_rather_than_growing() {
        let table: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(4);
        for key in 1..=4 {
            assert!(table.upsert(key).is_some(), "slot {key} fits");
        }
        assert_eq!(table.len(), 4);
        assert!(table.upsert(5).is_none(), "the fifth has nowhere to go");
        assert!(
            table.upsert(3).is_some(),
            "a claimed key is still reachable when full"
        );
    }

    #[test]
    fn select_never_creates_a_row() {
        let table: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(8);
        assert!(table.select(9).is_none());
        assert_eq!(table.len(), 0, "find must not take a slot");
        table.upsert(9).expect("capacity");
        assert!(table.select(9).is_some());
    }

    #[test]
    fn every_claimed_row_is_iterated_and_no_empty_one_is() {
        let table: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(32);
        for key in [11usize, 22, 33] {
            table
                .upsert(key)
                .expect("capacity")
                .0
                .store(key as u64, Ordering::Relaxed);
        }
        let mut seen: Vec<(usize, u64)> = table
            .iter()
            .map(|(k, v)| (k, v.0.load(Ordering::Relaxed)))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, alloc::vec![(11usize, 11u64), (22, 22), (33, 33)]);
    }

    #[test]
    fn concurrent_writers_agree_on_one_row_per_key() {
        extern crate std;
        let table: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(512);
        let shared = &table;
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(move || {
                    for round in 0..1_000usize {
                        let key = (round % 16) + 1;
                        shared
                            .upsert(key)
                            .expect("capacity")
                            .0
                            .fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(
            table.len(),
            16,
            "sixteen keys, sixteen slots, whatever the interleaving"
        );
        let total: u64 = table.iter().map(|(_, v)| v.0.load(Ordering::Relaxed)).sum();
        assert_eq!(
            total,
            8 * 1_000,
            "no update was lost and none was double counted"
        );
    }
}

#[cfg(test)]
mod atomic_key_table_cost {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Counter(AtomicU64);

    /// A claimed lookup must cost what a `Vec` index costs, because that is the whole claim.
    ///
    /// The comparison is against this crate's own [`LinearTable`], which is the baseline it
    /// exists to name. `select` there is a linear scan, so the interesting result is not that
    /// the atomic table wins - it has to, at any size past a handful - but that it does not
    /// *lose* at the small sizes a counter table actually runs at.
    ///
    /// Timed with the platform clock and reported as a ratio, so the assertion is about shape
    /// rather than about this machine. Generous bound: the point is to catch a regression that
    /// makes lookup linear or takes a lock, not to police a few percent.
    #[test]
    fn a_claimed_lookup_costs_about_what_a_vec_index_costs() {
        extern crate std;
        const KEYS: usize = 64;
        const ROUNDS: usize = 200_000;

        let atomic: AtomicKeyTable<Counter> = AtomicKeyTable::with_capacity(KEYS * 4);
        let mut linear: LinearTable<usize, u64> = LinearTable::new();
        for key in 1..=KEYS {
            atomic.upsert(key).expect("capacity");
            linear.push(key, key as u64);
        }

        let start = std::time::Instant::now();
        let mut sink = 0u64;
        for round in 0..ROUNDS {
            let key = (round % KEYS) + 1;
            sink += atomic
                .select(key)
                .expect("claimed")
                .0
                .load(Ordering::Relaxed);
        }
        let atomic_ns = start.elapsed().as_nanos().max(1);

        let start = std::time::Instant::now();
        for round in 0..ROUNDS {
            let key = (round % KEYS) + 1;
            sink += *linear.select(&key).expect("present");
        }
        let linear_ns = start.elapsed().as_nanos().max(1);
        std::hint::black_box(sink);

        let ratio = atomic_ns as f64 / linear_ns as f64;
        std::eprintln!(
            "lookup over {KEYS} keys: atomic {atomic_ns} ns, linear scan {linear_ns} ns, ratio {ratio:.3}"
        );
        assert!(
            ratio < 1.0,
            "a hashed lookup should not be slower than a linear scan over {KEYS} keys, \
             but the atomic table took {atomic_ns} ns against {linear_ns} ns"
        );
    }
}
