//! What the query set in `agentcode/docs/worktable-vec-query-requirements.md`
//! costs, in the three representations that could serve it.
//!
//! # The arms
//!
//! | arm | point lookup | range |
//! |---|---|---|
//! | `LinearTable` (today) | linear scan | filtered scan |
//! | `IndexedTable` (today) | `BTreeMap::get` | `BTreeMap::range`, then random access into rows |
//! | sorted `Vec` | binary search | two binary searches, then **a contiguous slice** |
//!
//! # Sizes
//!
//! 68,172 rows, which is the document's real `TextLexeme` count from ADR
//! 0011's dogfooding inventory, not the synthetic ladder. Keys are `u128`
//! composed the way `compose_lexeme_key` composes them, so a prefix is a
//! contiguous key range.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

const ROWS: usize = 68_172;
const REPS: usize = 5;

/// A posting row, shaped like the document describes: key-to-blob-reference,
/// with the columns a collision check reads.
#[derive(Clone)]
struct Posting {
    snapshot_key: u128,
    records_blob_high: u128,
    records_blob_low: u128,
    normalized_term: String,
}

/// `[ discriminator : 32 ][ order : 32 ][ tail : 64 ]`, so a shared
/// discriminator and order prefix is a contiguous `u128` range.
fn compose(discriminator: u32, order: u32, tail: u64) -> u128 {
    (u128::from(discriminator) << 96) | (u128::from(order) << 64) | u128::from(tail)
}

fn rows() -> Vec<(u128, Posting)> {
    (0..ROWS)
        .map(|n| {
            let key = compose(7, (n / 64) as u32, (n % 64) as u64);
            (
                key,
                Posting {
                    snapshot_key: 42,
                    records_blob_high: n as u128,
                    records_blob_low: 0,
                    normalized_term: format!("term{n}"),
                },
            )
        })
        .collect()
}

fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    times[times.len() / 2]
}

fn time(mut body: impl FnMut()) -> f64 {
    median(
        (0..REPS)
            .map(|_| {
                let start = Instant::now();
                body();
                start.elapsed().as_secs_f64()
            })
            .collect(),
    )
}

fn main() {
    let source = rows();
    // Probe keys spread across the space, and a prefix range that selects the
    // 64 rows sharing one order field, which is what a prefix search asks for.
    let probes: Vec<u128> = (0..1_000).map(|n| source[n * 61 % ROWS].0).collect();
    let (low, high) = (compose(7, 500, 0), compose(7, 500, u64::MAX));

    println!("{ROWS} rows, median of {REPS}\n");

    // ---- build
    let linear_build = time(|| {
        let mut table = worktable_vec::LinearTable::new();
        for (key, value) in source.iter().take(4_000) {
            let _ = table.insert(*key, value.clone());
        }
        black_box(&table);
    });
    let indexed_build = time(|| {
        let mut table = worktable_vec::IndexedTable::new();
        for (key, value) in &source {
            let _ = table.insert(*key, value.clone());
        }
        black_box(&table);
    });
    let sorted_build = time(|| {
        let mut rows = source.clone();
        rows.sort_unstable_by_key(|(key, _)| *key);
        black_box(&rows);
    });
    println!("build");
    println!(
        "  LinearTable   {:>9.1} ms   (only 4,000 rows: insert is O(n), so {ROWS} would be ~{:.0} s)",
        linear_build * 1e3,
        linear_build * (ROWS as f64 / 4_000.0).powi(2)
    );
    println!("  IndexedTable  {:>9.1} ms", indexed_build * 1e3);
    println!("  sorted Vec    {:>9.1} ms", sorted_build * 1e3);

    // ---- the tables
    let mut indexed = worktable_vec::IndexedTable::new();
    for (key, value) in &source {
        let _ = indexed.insert(*key, value.clone());
    }
    let mut sorted = source.clone();
    sorted.sort_unstable_by_key(|(key, _)| *key);
    let mut index: BTreeMap<u128, usize> = BTreeMap::new();
    for (row, (key, _)) in source.iter().enumerate() {
        index.insert(*key, row);
    }
    let mut linear = worktable_vec::LinearTable::new();
    for (key, value) in source.iter().take(4_000) {
        let _ = linear.insert(*key, value.clone());
    }

    // ---- Q1, unique point lookup
    println!("\nQ1  point lookup, 1,000 probes");
    let t = time(|| {
        for key in &probes {
            black_box(indexed.select(key));
        }
    });
    println!(
        "  IndexedTable  {:>9.0} ns a lookup",
        t / probes.len() as f64 * 1e9
    );
    let t = time(|| {
        for key in &probes {
            let found = sorted
                .binary_search_by_key(key, |(k, _)| *k)
                .ok()
                .map(|row| &sorted[row].1);
            black_box(found);
        }
    });
    println!(
        "  sorted Vec    {:>9.0} ns a lookup",
        t / probes.len() as f64 * 1e9
    );
    let small: Vec<u128> = probes.iter().take(50).copied().collect();
    let t = time(|| {
        for key in &small {
            black_box(linear.select(key));
        }
    });
    println!(
        "  LinearTable   {:>9.0} ns a lookup   (over 4,000 rows, not {ROWS})",
        t / small.len() as f64 * 1e9
    );

    // ---- Q2, inclusive range
    println!("\nQ2  inclusive range over a prefix (64 rows)");
    let t = time(|| {
        let hits: usize = indexed
            .rows()
            .iter()
            .filter(|(key, _)| *key >= low && *key <= high)
            .count();
        black_box(hits);
    });
    println!("  scan of rows  {:>9.0} ns   (what the crate can do today)", t * 1e9);
    let t = time(|| {
        let hits: usize = index
            .range(low..=high)
            .map(|(_, row)| black_box(&source[*row].1))
            .count();
        black_box(hits);
    });
    println!("  BTreeMap      {:>9.0} ns   (log n, then random access per row)", t * 1e9);
    let t = time(|| {
        let start = sorted.partition_point(|(key, _)| *key < low);
        let end = sorted.partition_point(|(key, _)| *key <= high);
        black_box(&sorted[start..end]);
    });
    println!("  sorted Vec    {:>9.0} ns   (two searches, then a slice)", t * 1e9);

    // ---- Q3, non-unique
    println!("\nQ3  non-unique lookup, all rows sharing one key");
    let duplicated = {
        let mut rows: Vec<(u128, Posting)> = source
            .iter()
            .map(|(key, value)| (key >> 6 << 6, value.clone()))
            .collect();
        rows.sort_unstable_by_key(|(key, _)| *key);
        rows
    };
    let target = duplicated[ROWS / 2].0;
    let t = time(|| {
        let start = duplicated.partition_point(|(key, _)| *key < target);
        let end = duplicated.partition_point(|(key, _)| *key <= target);
        black_box(&duplicated[start..end]);
    });
    println!(
        "  sorted Vec    {:>9.0} ns   (equal_range: the same two searches, no second index)",
        t * 1e9
    );

    // ---- memory
    println!("\nmemory for the index alone");
    println!(
        "  BTreeMap<u128, usize>   about {:>5.1} MB",
        (ROWS * (16 + 8) * 2) as f64 / 1e6
    );
    println!("  sorted Vec              0 MB   (the rows are the index)");
}
