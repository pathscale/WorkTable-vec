//! What an open actually spends its time on, before anyone parallelises it.
//!
//! A speedup on a synthetic loop says nothing about this path. Page decode is
//! the only embarrassingly parallel part, and it is only worth threads if it is
//! most of the time.

use std::hint::black_box;
use std::time::Instant;

use worktable_vec::{LinearTable, PAGE_SIZE};

const ROWS: usize = 200_000;
const REPS: usize = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut table = LinearTable::new();
    for n in 0..ROWS as u64 {
        table.push(
            n,
            format!("row {n} with enough text to be worth serializing"),
        );
    }
    let bytes = table.unload();
    let pages = bytes.len() / PAGE_SIZE;

    let mut whole = Vec::new();
    let mut per_page = Vec::new();
    for _ in 0..REPS {
        let now = Instant::now();
        let back = LinearTable::<u64, String>::load(black_box(&bytes))?;
        whole.push(now.elapsed());
        black_box(back.len());

        // Every page decoded on its own, which is what a worker would do, with
        // no vector to append into and no ordering to preserve.
        let now = Instant::now();
        let mut n = 0usize;
        for page in bytes.chunks_exact(PAGE_SIZE) {
            n += LinearTable::<u64, String>::load(black_box(page))?.len();
        }
        per_page.push(now.elapsed());
        black_box(n);
    }
    whole.sort();
    per_page.sort();
    let (w, p) = (whole[REPS / 2], per_page[REPS / 2]);
    println!("\n{ROWS} rows, {pages} pages, median of {REPS}\n");
    println!(
        "  load, whole file        {:>7.1} ms",
        w.as_secs_f64() * 1e3
    );
    println!(
        "  page decode alone       {:>7.1} ms   {:.0}% of it",
        p.as_secs_f64() * 1e3,
        100.0 * p.as_secs_f64() / w.as_secs_f64()
    );
    println!("\n  Amdahl ceiling at 16 cores, if only decode parallelises:");
    let s = 1.0 - p.as_secs_f64() / w.as_secs_f64();
    println!("    {:.2}x", 1.0 / (s + (1.0 - s) / 16.0));
    Ok(())
}
