# Performance baselines

Deterministic instruction counts (valgrind --tool=callgrind, release +
debuginfo) from `examples/bolt_workload`. Reproduce:

    CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example bolt_workload
    valgrind --tool=callgrind --callgrind-out-file=cg.out target/release/examples/bolt_workload N
    callgrind_annotate --inclusive=yes cg.out

## join_rows nested-loop join (baseline at ecf225b)

| n    | total Ir       | join_rows inclusive Ir |
|------|----------------|------------------------|
| 500  | 1,621,576,341  | 631,232,295 (38.9%)    |
| 1000 | 5,072,335,510  | 2,500,305,421 (49.3%)  |
| 2000 | 16,771,502,721 | 9,993,981,984 (59.6%)  |

At n=1000 malloc/free/realloc account for ~40% of all instructions; 18.6%
of the total is Vec allocation/growth inside `join_rows` (it clones the
accumulator row for every outer x inner pair).

## index_insert descent/leaf scan (baseline at 2d6f730)

Linear scan decodes every cell on the descent path (copy + assemble +
decode into owned values: several allocations per cell).

| n    | total Ir      | index_insert inclusive Ir |
|------|---------------|---------------------------|
| 500  | 1,197,934,791 | 553,198,487 (46.2%)       |
| 1000 | 3,371,808,347 | 1,334,609,167 (39.6%)     |
| 2000 | 9,811,562,547 | 2,846,547,273 (29.0%)     |

### After: binary search over sorted cells in index_insert

| n    | total Ir      | Δ total | index_insert Ir | Δ index_insert |
|------|---------------|---------|-----------------|----------------|
| 500  | 975,531,342   | -18.6%  | 330,646,286     | -40.2%         |
| 1000 | 2,822,970,068 | -16.3%  | 785,336,396     | -41.2%         |
| 2000 | 8,629,951,563 | -12.0%  | 1,665,455,821   | -41.5%         |

## index_insert leaf merge clones (baseline at dddcf9d)

`index_insert` parses every leaf cell into an owned `IndexCell` (one
heap copy each) and then `clone()`s each again while merging in the new
cell, although the parsed leaf cells are never used afterwards.

| n    | total Ir      | index_insert inclusive Ir |
|------|---------------|---------------------------|
| 1000 | 2,745,910,617 | 826,540,662 (30.1%)       |

### After: reuse the parsed leaf cells vector (insert in place)

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 931,842,438   | 818,053,256   | -12.2%  |
| 1000 | 2,745,910,617 | 2,470,276,502 | -10.0%  |
| 2000 | 8,674,819,320 | 8,022,700,508 | -7.5%   |

Full no_std size gate: 256,353 B (ratchet 256,465 B); tiny unchanged at 81,680 B.

## IndexCursor::seek_found decodes every cell on the descent (baseline at da5922d)

`seek_found` copies each page on the root-to-leaf path and decodes
every cell into an owned `IndexEntry` (several allocations per cell),
then binary-searches the decoded entries: O(cells per page) work and
allocations per level for an O(log) search. The join probe
(`SELECT t.id FROM t JOIN u ON u.t_id = t.id`) and index point lookups
go through it once per outer row.

| n    | total Ir      | seek_found inclusive Ir |
|------|---------------|-------------------------|
| 500  | 948,492,506   | 368,768,027 (38.9%)     |
| 1000 | 2,365,103,763 | 1,036,197,347 (43.8%)   |
| 2000 | 5,255,054,649 | 2,125,990,952 (40.5%)   |

### After: lazy per-page entries in IndexCursor (decode only visited cells)

Cursor frames keep the page image and decode a cell on first use
(binary search visits O(log cells) per page; iteration decodes what it
yields). Workload output (checksum) is identical before/after.

| n    | total before  | total after   | Δ total | seek_found before | seek_found after |
|------|---------------|---------------|---------|-------------------|------------------|
| 500  | 948,492,506   | 551,845,699   | -41.8%  | 368,768,027       | 17,216,698 (3.1%)  |
| 1000 | 2,365,103,763 | 1,267,512,629 | -46.4%  | 1,036,197,347     | 38,821,008 (3.1%)  |
| 2000 | 5,255,054,649 | 2,895,279,141 | -44.9%  | 2,125,990,952     | 85,679,128 (3.0%)  |

Size gates: tiny unchanged (`indexes` not in tiny); full 500,200 -> 500,944 B
(+744), max 815,792 -> 816,576 B (+784); ratchets raised by those amounts
(each well under `justify_over`).
