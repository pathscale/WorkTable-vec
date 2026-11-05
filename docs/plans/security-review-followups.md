# Security review disposition: WorkTable-vec retirement

The four findings below originated in the 2026-10-14 static review. I rechecked the cited current source on 2026-11-05. The code behaviors remain present; none was repaired by this review. The owner disposition is `RETIRED_REPO`: this crate is deprecated in favor of WorkTable, and this repository receives no security fix or release change in this task. The retirement notice is in [README.md](../../README.md) and at the top of `src/lib.rs`.

| Finding | Current-source evidence | Status and migration condition |
|---|---|---|
| WTV-1 — publishing credential scope and mutable action references | `.github/workflows/ci.yml`, `publish` job | The current workflow declares only `workflow_dispatch`, while `publish` is gated on a `push` to `master`; that job is unreachable through the configured trigger. Its token environment and mutable action references remain in source. Before any future release-path change, the release owner must review credential scope and pinning. No release or yank was performed. |
| WTV-2 — loaders have no configurable total byte, page, or row limits | `hydrate::io::read_rows`; `hydrate::from_pages` | Unresolved here. During migration, confirm the maintained loader has limits suitable for untrusted inputs; track any missing implementation in WorkTable. |
| WTV-3 — indexed loaders ignore insertion results for duplicate keys | `IndexedTable::load_from`; `IndexedTable::load` | Unresolved here. During migration, confirm and document consistent duplicate-key behavior in WorkTable. |
| WTV-4 — one-slot `AtomicKeyTable` computes a shift equal to the machine word width | `scatter`; `AtomicKeyTable::with_capacity` | Unresolved here. During migration, confirm the maintained single-slot table handles lookup and update safely. |

The source recheck was static only: no build, test, workflow, compiler, or runtime validation was performed. Do not describe the findings as fixed. The retirement decision preserves existing published versions for compatibility; it does not establish that old versions are safe. See [TODO.md](../TODO.md) for the remaining consumer-migration work.
