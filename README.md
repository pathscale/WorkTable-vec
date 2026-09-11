# WorkTable-vec

> **Deprecated. Use [`worktable`](https://github.com/pathscale/WorkTable) instead.**
>
> Everything here lives in `worktable` now, and the last piece that did not,
> `AtomicKeyTable`, has been ported. Nothing in this crate is maintained.
>
> | here | there |
> |---|---|
> | `LinearTable`, `IndexedTable`, `ArcticTable` | `worktable!` with `vec: true`, whose `using` clause picks the same backends |
> | `AtomicKeyTable` | `worktable::atomic_key_table::AtomicKeyTable` |
> | the `hydrate` feature | `worktable::vec_hydrate`, through `unload` and `load` |
>
> It stays published rather than yanked: yanking does not delete, and it would
> break anyone pinned to it for nothing.

Tiny, explicit `Vec`-backed tables for WorkTable-shaped workloads.

Applications often grow an implicit table out of a `Vec` plus bespoke scans
or side indexes. That is difficult to audit and easy to compare unfairly with
a database. This crate isolates those choices behind one row contract:

- `LinearTable`: ordered `Vec<(K, V)>` with linear point lookup;
- `IndexedTable`: the same ordered rows plus `BTreeMap<K, row_offset>`;
- `ArcticTable`: the same ordered rows plus Arctic `K -> row_offset`, behind
  the optional `arctic` feature.
- `CongeeTable`: the same ordered rows plus Congee `K -> row_offset`, behind
  the optional `congee` feature and limited to keys that round-trip through
  `usize`.
- `WtiTable`: the same ordered rows plus WorkTablesIndex `K -> row_offset`,
  behind the optional `wti` feature.

The default crate is `#![no_std]` and uses only `alloc`. Enable Arctic when a
runtime with its synchronization support is available:

```toml
worktable-vec = { version = "^0.1", features = ["arctic"] }
```

Use `features = ["congee"]` for fixed-width Congee keys, or enable both to
compare the two indexes over the identical Vec row representation.

The default `VecTable`/`LinearTable` is one `Vec<(K, V)>` field. Its `push`,
row-offset access, slices, iteration, capacity, reserve, and `into_rows` paths
retain ordinary Vec behavior and storage. `insert` is the explicitly stronger
operation: it performs an O(n) uniqueness check before appending.

It is deliberately not a persistence engine and does not claim the generated
schema, queries, concurrency, version publication, or durability of WorkTable.
Its role is to make those missing capabilities and their performance cost
measurable instead of leaving a homegrown implementation inside an
application. It is a standalone crate so applications and benchmark suites
can share the exact same baseline without copying it into either repository.

```rust
use worktable_vec::ArcticTable;

let mut rows = ArcticTable::new();
rows.insert(7_u64, "seven")?;
assert_eq!(rows.select(&7), Some(&"seven"));
# Ok::<(), worktable_vec::InsertError<u64>>(())
```

## MoE resident lookup result

The paired benchmark is
`../wt-benchmarks/src/bin/moe-resident-index-ab.rs`. It uses 1,528 provenance
rows, one million deterministic successful point lookups, nine samples, and an
identical five-field row payload in every arm. Every arm must produce the same
checksum before timings are reported.

On the first local release run:

| implementation | median ns/query |
|---|---:|
| linear Vec | 208.17 |
| Vec + BTreeMap | 32.34 |
| Vec + Arctic | 5.25 |
| generated WorkTable + Arctic | 26.07 |

A second immediate run produced 208.49, 32.18, 5.55, and 26.37 ns/query,
respectively. This makes Arctic about 5.8–6.2x faster than BTreeMap as the
isolated row-offset index. Full WorkTable is about 7.9–8.0x faster than the
application's linear scan and about 1.22–1.24x faster than the BTreeMap arm,
while the stripped Vec+Arctic arm remains about 4.8–5.0x faster than the full
generated table.

Build/load and persisted-attach measurements are separate work; these numbers
are warm in-process point lookups and do not imply that the Vec arms provide
WorkTable semantics.
