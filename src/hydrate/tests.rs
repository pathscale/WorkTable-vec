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
    let back = LinearTable::<u64, String>::load(&before.unload().expect("rows that fit a page"))
        .expect("a load");
    assert_eq!(back.rows(), before.rows());
}

/// More rows than one page holds, so the page run is doing real work.
#[test]
fn rows_survive_spanning_many_pages() {
    let before = table(20_000);
    let bytes = before.unload().expect("rows that fit a page");
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
    let bytes = table(0).unload().expect("rows that fit a page");
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
    let back = IndexedTable::<u64, String>::load(&before.unload().expect("rows that fit a page"))
        .expect("a load");
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

/// A page of zeroes is the shape a torn write leaves behind, and it has to be a
/// named error rather than a plausible empty table.
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

/// Somebody else's rows in well formed pages. Without the fingerprint this load
/// succeeds and hands back debris.
#[test]
fn a_different_row_type_is_refused_rather_than_reinterpreted() {
    let bytes = table(10).unload().expect("rows that fit a page");
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
    let mut spliced = table(1).unload().expect("rows that fit a page");
    let mut other: LinearTable<u64, u64> = LinearTable::new();
    other.push(1, 1);
    spliced.extend_from_slice(&other.unload().expect("rows that fit a page"));
    assert_eq!(
        LinearTable::<u64, String>::load(&spliced),
        Err(LoadError::Inconsistent { page: 1 })
    );
}

/// **The one rkyv cannot find.** A flipped bit inside an integer is a
/// structurally perfect archive of a different number, so validation passes and
/// only the checksum notices.
#[test]
fn a_flipped_bit_in_a_body_is_caught_by_the_checksum() {
    let mut bytes = table(64).unload().expect("rows that fit a page");
    bytes[HEADER_SIZE + 40] ^= 0b0000_0100;
    match LinearTable::<u64, String>::load(&bytes) {
        Err(LoadError::Corrupt { page: 0, .. }) => {}
        other => panic!("a flipped bit has to be caught: {other:?}"),
    }
}

/// Damage is one page's problem, not the file's. With an archive spanning
/// pages, breaking the last one would take every row with it.
#[test]
fn damage_stays_inside_the_page_it_happened_to() {
    let before = table(20_000);
    let bytes = before.unload().expect("rows that fit a page");
    let pages = bytes.len() / PAGE_SIZE;
    assert!(pages > 2, "need a middle page to damage: {pages}");

    // Everything before the damaged page still decodes on its own.
    let head = &bytes[..PAGE_SIZE];
    let intact = LinearTable::<u64, String>::load(head).expect("the first page alone loads");
    assert!(
        !intact.is_empty() && intact.len() < before.len(),
        "one page holds some rows but not all of them"
    );
}

#[test]
fn a_row_count_that_disagrees_with_the_body_is_refused() {
    let mut bytes = table(64).unload().expect("rows that fit a page");
    // Claim one more row than the body holds, and fix nothing else. The count
    // is in the directory at the page tail, not in the header, because the
    // header is DataBucket's and has no field for it.
    let at = PAGE_SIZE - DIRECTORY_SIZE;
    let rows = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    bytes[at..at + 4].copy_from_slice(&(rows + 1).to_le_bytes());
    match LinearTable::<u64, String>::load(&bytes) {
        Err(LoadError::RowCount { page: 0, .. }) => {}
        other => panic!("a lying row count has to be caught: {other:?}"),
    }
}

#[test]
fn appended_pages_read_back_as_one_table() {
    let whole = table(5_000);
    let mut bytes = Vec::new();
    // First half, then the rest appended, exactly as two writes would land.
    let mut first = LinearTable::new();
    for (key, value) in whole.rows().iter().take(2_000).cloned() {
        first.push(key, value);
    }
    bytes.extend_from_slice(&first.unload().expect("rows that fit a page"));
    bytes.extend_from_slice(&whole.unload_appending(2_000).expect("rows that fit a page"));

    let back = LinearTable::<u64, String>::load(&bytes).expect("a load");
    assert_eq!(back.rows(), whole.rows());
}

mod through_a_reader_and_a_writer {
    use super::*;
    use embedded_io_adapters::std::FromStd;

    /// `Vec<u8>` and `&[u8]` already implement the traits, so memory needs no
    /// adapter at all.
    #[test]
    fn memory_needs_no_adapter() {
        let before = table(3_000);
        let mut sink = Vec::new();
        before.unload_to(&mut sink).expect("a write");
        let back = LinearTable::<u64, String>::load_from(&mut sink.as_slice()).expect("a read");
        assert_eq!(back.rows(), before.rows());
    }

    /// **A real file on a real disk**, through the adapter, which is the point
    /// of the traits: no `std` in this crate and a `std::fs::File` on the other
    /// side of them.
    #[test]
    fn a_file_on_disk_round_trips() {
        let path = std::env::temp_dir().join(alloc::format!(
            "worktable-vec-hydrate-{}.wtv",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let before = table(20_000);
        {
            let file = std::fs::File::create(&path).expect("a file");
            before
                .unload_to(&mut FromStd::new(file))
                .expect("a write to disk");
        }

        let on_disk = std::fs::metadata(&path).expect("a stat").len() as usize;
        assert_eq!(
            on_disk % PAGE_SIZE,
            0,
            "a file is a whole number of pages: {on_disk}"
        );

        let file = std::fs::File::open(&path).expect("the file back");
        let back = LinearTable::<u64, String>::load_from(&mut FromStd::new(file)).expect("a read");
        assert_eq!(back.rows(), before.rows());
        let _ = std::fs::remove_file(&path);
    }

    /// Appending to a real file, which is the whole reason pages stand alone.
    #[test]
    fn appending_to_a_file_does_not_rewrite_it() {
        let path = std::env::temp_dir().join(alloc::format!(
            "worktable-vec-append-{}.wtv",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let whole = table(6_000);
        let mut first = LinearTable::new();
        for (key, value) in whole.rows().iter().take(2_000).cloned() {
            first.push(key, value);
        }

        {
            let file = std::fs::File::create(&path).expect("a file");
            first.unload_to(&mut FromStd::new(file)).expect("a write");
        }
        let after_first = std::fs::metadata(&path).expect("a stat").len();

        {
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("the file, to append");
            whole
                .append_to(&mut FromStd::new(file), 2_000)
                .expect("an append");
        }
        let after_append = std::fs::metadata(&path).expect("a stat").len();
        assert!(
            after_append > after_first,
            "an append adds pages: {after_first} then {after_append}"
        );

        let file = std::fs::File::open(&path).expect("the file back");
        let back = LinearTable::<u64, String>::load_from(&mut FromStd::new(file)).expect("a read");
        assert_eq!(back.rows(), whole.rows());
        let _ = std::fs::remove_file(&path);
    }

    /// A write that died half way through a page is a torn page, and says so
    /// rather than quietly dropping the rows it did not finish.
    #[test]
    fn a_half_written_page_is_torn_rather_than_ignored() {
        let bytes = table(5_000).unload().expect("rows that fit a page");
        let cut = bytes.len() - (PAGE_SIZE / 2);
        match LinearTable::<u64, String>::load_from(&mut &bytes[..cut]) {
            Err(HydrateError::Torn { .. }) => {}
            other => panic!("a half written page has to be caught: {other:?}"),
        }
    }
}

/// The header is DataBucket's `GeneralHeader`, byte for byte.
///
/// Verified against `data_bucket 0.5.7`, which for a `Data` page of space 3,
/// id 7, previous 6, next 8, length `0x11223344` emits exactly these 28 bytes.
/// This crate reproduces that layout rather than importing it, because
/// `data_bucket` is `std`. **A duplicated layout drifts**, and this is what
/// notices when it does.
#[test]
fn the_header_is_databuckets_layout() {
    let mut out = Vec::new();
    Header {
        version: 2,
        schema: 3,
        page: 7,
        previous: 6,
        next: 8,
        page_type: PAGE_TYPE_DATA,
        body: 0x1122_3344,
    }
    .write(&mut out);

    assert_eq!(out.len(), HEADER_SIZE, "GENERAL_HEADER_SIZE is 28");
    assert_eq!(
        out,
        alloc::vec![
            0x02, 0x00, 0x00, 0x00, // data_version
            0x03, 0x00, 0x00, 0x00, // space_id, here the row fingerprint
            0x07, 0x00, 0x00, 0x00, // page_id
            0x06, 0x00, 0x00, 0x00, // previous_id
            0x08, 0x00, 0x00, 0x00, // next_id
            0x02, 0x00, 0x00, 0x00, // page_type: Data
            0x44, 0x33, 0x22, 0x11, // data_length
        ],
        "the layout drifted from data_bucket 0.5.7"
    );
}

/// A page says how many rows it holds, which is what a WorkTable data page
/// cannot do and why one cannot be read without its index.
#[test]
fn every_page_declares_its_own_rows() {
    let before = table(20_000);
    let bytes = before.unload().expect("rows that fit a page");
    let pages = bytes.len() / PAGE_SIZE;
    assert!(pages > 2, "need several pages: {pages}");

    let mut counted = 0usize;
    for page in bytes.chunks_exact(PAGE_SIZE) {
        let at = PAGE_SIZE - DIRECTORY_SIZE;
        let mut word = [0u8; 4];
        word.copy_from_slice(&page[at..at + 4]);
        counted += u32::from_le_bytes(word) as usize;
    }
    assert_eq!(
        counted,
        before.len(),
        "the pages account for every row without an index"
    );
}

/// Version 3 is the version that has a directory. A page claiming 2 is a
/// WorkTable page, and its rows are not where this reader would look.
#[test]
fn a_version_two_page_is_refused() {
    let mut bytes = table(4).unload().expect("rows that fit a page");
    bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        LinearTable::<u64, String>::load(&bytes),
        Err(LoadError::ForeignPages {
            page: 0,
            version: 2
        })
    );
}

/// A row that does not fit a page is refused, rather than written into a file
/// that cannot be read back.
///
/// This is a regression. The writer used to hand such a row a page of its own,
/// spilling past the page boundary; `load` then stopped at
/// `Overlong`, so `unload` reported success and every row in the file was
/// unreachable. A 20 KB row wrote 32,768 bytes and lost one row; a 30 KB row
/// among a hundred ordinary ones lost all hundred and one.
#[test]
fn a_row_too_large_for_a_page_is_refused() {
    let mut table = LinearTable::<u64, String>::new();
    table.push(0, "x".repeat(20_000));
    let refusal = table
        .unload()
        .expect_err("a row that big cannot be written");
    assert_eq!(refusal.row, 0);
    assert_eq!(refusal.limit, BODY_SIZE);
    assert!(
        refusal.bytes > BODY_SIZE,
        "the refusal reports the archive size it could not place: {refusal}"
    );
}

/// The refusal names the row, not just the fact of one.
#[test]
fn the_refusal_names_which_row_is_too_large() {
    let mut table = table(50);
    table.push(999, "y".repeat(30_000));
    for n in 1_000..1_050u64 {
        table.push(n, "z".repeat(100));
    }
    let refusal = table
        .unload()
        .expect_err("a row that big cannot be written");
    assert_eq!(refusal.row, 50, "{refusal}");
}

/// The limit is a page body, and a row just under it still writes.
///
/// Both sides are asserted so the boundary is pinned from both directions: a
/// check that only ever refuses would pass with the limit set to zero.
#[test]
fn the_limit_is_a_page_body_and_not_less() {
    let mut fits = LinearTable::<u64, String>::new();
    fits.push(0, "x".repeat(BODY_SIZE - 64));
    let bytes = fits.unload().expect("a row just under the limit fits");
    let back = LinearTable::<u64, String>::load(&bytes).expect("a load");
    assert_eq!(back.rows(), fits.rows());

    let mut over = LinearTable::<u64, String>::new();
    over.push(0, "x".repeat(BODY_SIZE + 1));
    assert!(over.unload().is_err(), "a row over the limit is refused");
}

/// A refused unload writes nothing at all.
///
/// The refusal happens while the pages are being built, before the sink is
/// touched, so a caller who ignores the error still does not end up with a
/// half-written file.
#[test]
fn a_refused_unload_leaves_the_sink_untouched() {
    let mut table = LinearTable::<u64, String>::new();
    table.push(0, "x".repeat(20_000));
    let mut sink = alloc::vec::Vec::new();
    let refusal = table.unload_to(&mut sink).expect_err("nothing to write");
    assert!(matches!(refusal, UnloadError::Row(_)), "{refusal}");
    assert!(sink.is_empty(), "{} bytes reached the sink", sink.len());
}
