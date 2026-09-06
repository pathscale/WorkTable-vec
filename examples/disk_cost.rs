//! What a flush and an open cost against a real file.
//!
//! Bounded on purpose: fixed row counts, fixed reps, and the fixture is built
//! once. A benchmark without a ceiling is a hang.

use std::time::Instant;

use embedded_io_adapters::std::FromStd;
use worktable_vec::LinearTable;

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
    let path = std::env::temp_dir().join("worktable-vec-disk-cost.wtv");

    let mut wrote = Vec::new();
    let mut read = Vec::new();
    let mut bytes = 0u64;

    for _ in 0..REPS {
        let now = Instant::now();
        {
            let file = std::fs::File::create(&path)?;
            table.unload_to(&mut FromStd::new(file))?;
        }
        wrote.push(now.elapsed());
        bytes = std::fs::metadata(&path)?.len();

        let now = Instant::now();
        let back =
            LinearTable::<u64, String>::load_from(&mut FromStd::new(std::fs::File::open(&path)?))
                .map_err(|error| format!("{error}"))?;
        read.push(now.elapsed());
        assert_eq!(back.len(), ROWS);
    }

    wrote.sort();
    read.sort();
    let flush = wrote[REPS / 2];
    let open = read[REPS / 2];
    let mb = bytes as f64 / (1024.0 * 1024.0);

    println!(
        "\n{ROWS} rows, {mb:.1} MiB on disk, {} pages, median of {REPS}\n",
        bytes as usize / (4096 * 4)
    );
    println!(
        "  flush  {:>7.1} ms   {:>7.1} MiB/s   {:>6.0} ns/row",
        flush.as_secs_f64() * 1e3,
        mb / flush.as_secs_f64(),
        flush.as_secs_f64() * 1e9 / ROWS as f64
    );
    println!(
        "  open   {:>7.1} ms   {:>7.1} MiB/s   {:>6.0} ns/row",
        open.as_secs_f64() * 1e3,
        mb / open.as_secs_f64(),
        open.as_secs_f64() * 1e9 / ROWS as f64
    );

    // The floor: what the same rows cost with no pages, no checksum and no
    // fingerprint, just one archive straight to the file. Anything this codec
    // adds shows up as the gap.
    let now = Instant::now();
    let raw = rkyv::to_bytes::<rkyv::rancor::Error>(&table.rows().to_vec())?;
    let encode = now.elapsed();
    println!(
        "\n  rkyv alone, no pages   {:>7.1} ms encode, {} MiB",
        encode.as_secs_f64() * 1e3,
        raw.len() / (1024 * 1024)
    );

    let _ = std::fs::remove_file(&path);
    Ok(())
}
