# TODO

This crate is retired in favor of [WorkTable](https://github.com/pathscale/WorkTable). The security findings below remain unresolved in this historical source; retirement is not a repair.

- [ ] Identify remaining deployed `WorkTable-vec` callers and plan migration to WorkTable, including compatibility differences documented in [README.md](../README.md).
- [ ] Before switching callers, export or regenerate their format-3 snapshots with the old reader; WorkTable uses a deliberate snapshot-format cutover.
- [ ] During migration, verify that the maintained replacement provides the required loader limits, consistent duplicate-key handling, and safe single-slot key-table behavior. Track any missing fixes in WorkTable itself.
- [ ] Have the release owner review the retired crate's publishing credential and action-pinning concerns before any future release action. No release or yank is part of this retirement follow-up.

See [the source review disposition](plans/security-review-followups.md) for the specific unresolved behaviors and evidence. No build, test, workflow, or runtime validation was performed for this documentation update.
