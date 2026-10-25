# Bolt profile: mixed workload on MemStorage (baseline, no change applied)

Harness: `examples/bolt_workload.rs`. Counters only (wall-clock is unreliable here).

Reproduce:

    CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example bolt_workload
    valgrind --tool=callgrind target/release/examples/bolt_workload load
    valgrind --tool=dhat      target/release/examples/bolt_workload load

## Baseline

| phase | callgrind Ir | dhat |
|-------|--------------|------|
| `load` (2 tables, 2 indexes, 2000 + 6000 INSERTs in one txn) | 11,914,361,322 | 25,745,900 allocs, 2,520,371,359 B total, 2,081,732 B peak |
| `point` (`load` + 1000 point SELECTs) | 12,513,330,310 | not run |

The other phases (`scan`, `join`, `agg`, `write`, `subq`) were not profiled.
`point` minus `load` is only ~0.6G Ir, so `load` dominates.

## Profile of `load` (inclusive)

- `session::Statement::step` 96.1%
  - `btree::index_insert` **84.2%**
    - `decode_index_entry` 30.2% (self-contained allocs, per cell)
    - `index_cell_payload` 19.5% (copies each cell payload)
    - `btree_insert` 11.4%
- malloc/free (all callers): ~40% of Ir (`_int_free` 12.8%, `_int_malloc` 10.4%,
  `malloc` 9.2%, `free` 5.9% self)
- `decode_record_into` 3.5% self

## Observation (hypothesis, not tested)

`index_insert` visits each leaf cell up to twice per insert: once in the
duplicate-check loop (all cells, `index_cell_payload` + `decode_index_entry`
each) and again in the merge loop (until the insertion point). On a sorted
leaf both can be a single pass that stops at the first cell greater than the
probe, erroring on Equal. That would cut leaf decodes/allocs per insert to
roughly a third. Not implemented; needs before/after callgrind + dhat on
this harness and must clear the impact floor before a PR.
