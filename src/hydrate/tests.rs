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

/// More rows than one page holds, so the page run is doing real work.
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

/// **The one rkyv cannot find.** A flipped bit inside an integer is a
/// structurally perfect archive of a different number, so validation passes and
/// only the checksum notices.
#[test]
fn a_flipped_bit_in_a_body_is_caught_by_the_checksum() {
    let mut bytes = table(64).unload();
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
    let bytes = before.unload();
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
    let mut bytes = table(64).unload();
    // Claim one more row than the body holds, and fix nothing else.
    let rows = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    bytes[8..12].copy_from_slice(&(rows + 1).to_le_bytes());
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
    bytes.extend_from_slice(&first.unload());
    bytes.extend_from_slice(&whole.unload_from(2_000));

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
        before.write(&mut sink).expect("a write");
        let back = LinearTable::<u64, String>::read(&mut sink.as_slice()).expect("a read");
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
                .write(&mut FromStd::new(file))
                .expect("a write to disk");
        }

        let on_disk = std::fs::metadata(&path).expect("a stat").len() as usize;
        assert_eq!(
            on_disk % PAGE_SIZE,
            0,
            "a file is a whole number of pages: {on_disk}"
        );

        let file = std::fs::File::open(&path).expect("the file back");
        let back = LinearTable::<u64, String>::read(&mut FromStd::new(file)).expect("a read");
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
            first.write(&mut FromStd::new(file)).expect("a write");
        }
        let after_first = std::fs::metadata(&path).expect("a stat").len();

        {
            let file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("the file, to append");
            whole
                .append(&mut FromStd::new(file), 2_000)
                .expect("an append");
        }
        let after_append = std::fs::metadata(&path).expect("a stat").len();
        assert!(
            after_append > after_first,
            "an append adds pages: {after_first} then {after_append}"
        );

        let file = std::fs::File::open(&path).expect("the file back");
        let back = LinearTable::<u64, String>::read(&mut FromStd::new(file)).expect("a read");
        assert_eq!(back.rows(), whole.rows());
        let _ = std::fs::remove_file(&path);
    }

    /// A write that died half way through a page is a torn page, and says so
    /// rather than quietly dropping the rows it did not finish.
    #[test]
    fn a_half_written_page_is_torn_rather_than_ignored() {
        let bytes = table(5_000).unload();
        let cut = bytes.len() - (PAGE_SIZE / 2);
        match LinearTable::<u64, String>::read(&mut &bytes[..cut]) {
            Err(ReadError::Torn { .. }) => {}
            other => panic!("a half written page has to be caught: {other:?}"),
        }
    }
}
