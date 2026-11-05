# Security review follow-ups

These source-owned follow-ups come from a local static review dated 2026-10-14. The review is historical evidence, not proof about current master. Every item remains **unverified** until its source anchor is rechecked. This summary omits exploit sequences, credential values, and local paths.

| Review ID | Source anchor | Required invariant | Status |
|---|---|---|---|
| WTV-1 | .github/workflows/ci.yml (publish job, lines 54-73) | Publishing credentials must be absent during dependency verification; release inputs must be pinned. | Unverified against current master; confirm before closing. |
| WTV-2 | src/hydrate/io.rs (read_rows), src/hydrate/mod.rs (from_pages) | Untrusted loading must offer explicit byte, row, and page limits. | Unverified against current master; confirm before closing. |
| WTV-3 | src/hydrate/mod.rs (IndexedTable::load), src/hydrate/io.rs region (IndexedTable::load_from, let _ = table.insert(..)) | All table loaders must report duplicate keys consistently instead of dropping rows. | Unverified against current master; confirm before closing. |
| WTV-4 | src/lib.rs (scatter), shift computed at src/lib.rs | Single-slot key-table operations must avoid shifting by the integer width. | Unverified against current master; confirm before closing. |
