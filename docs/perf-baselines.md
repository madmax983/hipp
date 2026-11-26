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

## index_insert_core parses every cell of every page on the descent (baseline at 8d78815)

`index_insert_core` (every index entry written: INSERT index maintenance
and CREATE INDEX) turns each interior page on the descent and the leaf
into owned `IndexCell`s (`index_leaf_cells` / `index_interior_cells`: one
heap copy per cell) although the binary search visits O(log cells) of
them, then rebuilds the whole leaf image from those copies.

| n    | total Ir      |
|------|---------------|
| 500  | 531,491,467   |
| 1000 | 1,221,031,532 |
| 2000 | 2,791,295,949 |

n=1000 profile: `index_insert` inclusive 552,561,282 (45.25%) of which
`index_leaf_cells` 250,165,910 (20.49%). malloc/free (self) ~45% of all
instructions. `valgrind --tool=dhat` (n=1000): 1,885,182 blocks,
321,242,993 bytes total.

Reproduce dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: search only the visited cells, rebuild the leaf from page bytes

`index_search_raw` parses just the cells the binary search visits (every
cell's layout is still validated, allocation-free); a leaf that still fits
is rewritten by `write_index_leaf_inserted`, copying cell bytes straight
from the page image. A leaf that overflows falls back to the old
parse-everything path. Workload checksums identical; a 5 MB index-heavy
database (512 B pages, overflow keys, DESC/UNIQUE/NOCASE indexes,
deletes/updates) is byte-identical (sha256) before/after.

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 531,491,467   | 412,739,173   | -22.3%  |
| 1000 | 1,221,031,532 | 935,461,204   | -23.4%  |
| 2000 | 2,791,295,949 | 2,136,959,519 | -23.4%  |

n=1000: index_insert inclusive 552,561,282 -> 266,949,278 (-51.7%);
dhat 1,885,182 -> 1,289,425 blocks (-31.6%), 321,242,993 -> 201,527,480
bytes (-37.3%).

Size gates: tiny unchanged (76,960); full +872 B, max +984 B, xl +976 B;
ratchets raised by those amounts (each well under `justify_over`).

## parse_real big-integer conversion (baseline at 7cfaf1e)

`bolt_workload 1000` (callgrind, release + debuginfo). The workload's
2n INSERT statements carry `{i}.5` REAL literals; each one goes through
`int_pow10_to_f64`'s big-integer division although the value is exactly
representable (mantissa < 2^53, |exp10| <= 22).

| n    | total Ir    | parse_real inclusive Ir |
|------|-------------|-------------------------|
| 1000 | 937,058,403 | 70,732,757 (7.55%)      |

### After: Clinger exact fast path in `int_pow10_to_f64`

| n    | total Ir    | parse_real inclusive Ir | Δ total |
|------|-------------|-------------------------|---------|
| 500  | 378,111,682 | n/a                     | n/a     |
| 1000 | 866,107,908 | 1,333,026 (0.15%)       | -7.57%  |
| 2000 | 1,993,289,422 | n/a                   | n/a     |

## btree_insert (table leaf) parse-all-and-clone (baseline at 10b5549)

`bolt_workload` (callgrind, release + debuginfo). Every table INSERT
parses all cells of the target leaf into owned `LeafCell`s (`leaf_cells`),
clones them all into a merged list, and rebuilds the page from that list
(`build_leaf_image`), although only one cell is added and the leaf nearly
always still fits.

| n    | total Ir      |
|------|---------------|
| 500  | 378,136,050   |
| 1000 | 866,107,919   |
| 2000 | 1,993,376,790 |

n=1000 profile: `btree_insert` inclusive 203,601,500 (23.51%) of which
`leaf_cells` 97,076,273 (11.21%) and `build_leaf_image` 90,835,172 (10.49%).
malloc+free (self) ~35% of all instructions. dhat (n=1000): 1,041,475
blocks, 198,910,292 bytes total.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: insert into the leaf image without parsing/cloning every cell

`btree_insert` validates the leaf with `leaf_cells_meta` (no per-cell
allocation) and, when the leaf still fits, builds the new page image
straight from the page bytes (`build_leaf_image_inserted`). Only a leaf
that overflows re-parses with `leaf_cells` and takes the old split path.
Workload checksum identical (657610); the test suite's pass/fail set is
identical before/after (950 tests; the 823 failures are the absent
`sqlite3` oracle, same on the baseline commit).

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 378,136,050   | 268,236,272   | -29.1%  |
| 1000 | 866,107,919   | 626,660,907   | -27.6%  |
| 2000 | 1,993,376,790 | 1,526,832,320 | -23.4%  |

dhat (n=1000): 1,041,475 -> 535,229 blocks (-48.6%); 198,910,292 ->
143,513,712 bytes (-27.8%).

Size gates: tiny +828 B, full +776 B, max +752 B, xl +744 B; ratchets
raised by those amounts (each well under `justify_over`).

## index_insert validates every leaf cell twice (baseline at 0e4687f)

`bolt_workload` (callgrind, release + debuginfo). In the common case
(the new cell fits), `index_insert_core` computes every leaf cell's
layout in `index_search_raw` (to validate the page and sum the cell
lengths) and then again in `write_index_leaf_inserted` (to copy each
cell into the new image): `index_cell_layout` runs ~2x per cell.

| n    | total Ir      |
|------|---------------|
| 500  | 268,235,071   |
| 1000 | 626,649,402   |
| 2000 | 1,526,801,879 |

n=1000 profile: `index_insert_core` inclusive 265,845,678 (42.42%);
`index_cell_layout` inclusive 99,904,355 (15.94%), of which
48,768,787 (7.78%, 566,692 calls) from `index_search_raw` and
48,094,812 (7.67%, 559,242 calls) from `write_index_leaf_inserted`
(inlined into `index_insert_core`). dhat (n=1000): 535,229 blocks,
143,513,712 bytes.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: reuse the leaf's validated cell spans

`index_search_raw` keeps each cell's `(offset, on-page length)` (as
`u16`s, reserved once per page) while it validates the page, and
`write_index_leaf_inserted` copies the cells from those spans instead
of laying every cell out again. Workload checksums identical; an
index-heavy file (DESC / UNIQUE NOCASE / multi-column indexes, keys
with overflow, deletes, updates, a late CREATE INDEX, INSERT ...
SELECT, a WITHOUT ROWID table with a secondary index) at 512 B and
4096 B pages is byte-identical (cmp) before/after. Test pass/fail set
identical (1002 tests; failures are the absent `sqlite3` oracle).

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 268,235,071   | 250,102,694   | -6.76%  |
| 1000 | 626,649,402   | 582,822,372   | -6.99%  |
| 2000 | 1,526,801,879 | 1,425,425,432 | -6.64%  |

n=1000: `index_insert_core` inclusive 265,845,678 -> 221,704,757
(-16.6%); `index_cell_layout` inclusive 99,904,355 -> 51,809,543
(-48.1%). dhat (n=1000): 535,229 -> 538,328 blocks (+0.58%, one
span buffer per index insert), 143,513,712 -> 145,792,820 bytes
(+1.59%); peak (t-gmax) unchanged at 852,156 B. Storing the spans as
`usize` pairs measured -7.69% Ir but +6.35% bytes; `u16` was kept.

Size gates: tiny unchanged; full +184 B, max +24 B, xl +40 B;
ratchets raised by those amounts (each well under `justify_over`).

## vec_zeroed builds page images a byte at a time (baseline at c261574)

`bolt_workload` (callgrind, release + debuginfo). `vec_zeroed(n)`, the
zeroed buffer behind every page image `build_leaf_image`,
`build_leaf_image_inserted`, `build_interior_image` and
`build_index_image` start from, pushes `n` zero bytes one by one into
an empty `AVec`: ~4 instructions of capacity check per byte plus ~10
`RawVec<u8>::grow_one` reallocations (8 -> 4096 B) per page.

| n    | total Ir      |
|------|---------------|
| 500  | 249,920,143   |
| 1000 | 582,896,114   |
| 2000 | 1,425,375,525 |

n=1000 profile: `build_leaf_image_inserted` (the table-INSERT fast
path, 1,996 calls) inclusive 88,174,993 (15.13%), ~44.2k Ir per call
for a 4096 B page; its inlined `Vec::push` loop 42,146,318 (7.23%)
inclusive, and 19,960 `grow_one` calls (10 per image) costing
9,383,974 (1.61%). dhat (n=1000): 538,328 blocks, 145,799,068 bytes;
t-gmax 852,164 B.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: one zeroed allocation per page image

`vec_zeroed(n)` is `AVec::filled(0, n)` (a new `AVec` constructor in
`arena.rs` over `vec![v; n]`), so each page image is one zeroed
allocation instead of `n` pushes and ~10 regrowths. Every image
builder that calls it benefits; the workload exercises the table
INSERT fast path. Workload checksum identical (657610). Test pass/fail
set identical before/after (1022 tests; the 893 failures are the
absent `sqlite3` oracle, `expect("oracle runs")`, same on the
baseline commit).

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 249,920,143   | 215,916,996   | -13.6%  |
| 1000 | 582,896,114   | 511,583,798   | -12.2%  |
| 2000 | 1,425,375,525 | 1,276,264,443 | -10.5%  |

n=1000: `build_leaf_image_inserted` inclusive 88,174,993 ->
22,614,443 (-74.4%; 44.2k -> 11.3k Ir per call). dhat (n=1000):
538,328 -> 519,770 blocks (-3.4%), 145,799,068 -> 137,369,612 bytes
(-5.8%); t-gmax unchanged at 852,164 B.

Size gates: tiny -48 B, full -48 B, max -40 B, xl -32 B; ratchets
lowered by those amounts.

## index_search_raw lays every leaf cell out out-of-line (baseline at 1bde36f)

`bolt_workload` (callgrind, release + debuginfo). On every index
insert, `index_search_raw` walks all cells of the target page to
validate them, sum their lengths and record their spans, calling the
general `index_cell_layout` (an out-of-line call returning a
`Result<IndexCellLayout>` through memory, plus an out-of-line
`decode_varint` and `index_local`) for each one, although nearly every
index cell is a 1-byte payload-length varint with no overflow.

| n    | total Ir      |
|------|---------------|
| 500  | 215,947,232   |
| 1000 | 512,085,990   |
| 2000 | 1,275,323,631 |

n=1000 profile: `index_insert_core` inclusive 220,249,518 (43.01%);
`index_search_raw` inclusive 126,399,681 (24.68%); `index_cell_layout`
inclusive 51,809,543 (10.12%), of which 48,768,787 (9.52%, 566,692
calls, ~86 Ir per cell) from the walk in `index_search_raw`.
`index_cell_layout` is the top self-cost entry (31,892,921, 6.23%).

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`

### After: lay out 1-byte-header local cells inline

The validation walk in `index_search_raw` reads the byte after the
cell's child pointer: when it is a 1-byte varint (`< 0x80`) no larger
than X (`index_max_local`, now shared with `index_local`) and the
payload ends on the page, the cell's length is `child + 1 + b`, which
is exactly what `index_cell_layout` would return. Every other cell,
and every error, still goes through `index_cell_layout`, so corrupt
pages fail with the same error as before. Workload checksums identical
(176089 / 657610 / 4083309 at n=500/1000/2000). Test pass/fail set
identical before/after (1157 tests; the 1027 failures are the absent
`sqlite3` oracle, same on the baseline commit).

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 215,947,232   | 197,576,305   | -8.51%  |
| 1000 | 512,085,990   | 466,197,976   | -8.96%  |
| 2000 | 1,275,323,631 | 1,168,340,905 | -8.39%  |

n=1000: `index_search_raw` inclusive 126,399,681 -> 80,513,948
(-36.3%); `index_insert_core` inclusive 220,249,518 -> 174,363,785
(-20.8%); calls to `index_cell_layout` from the walk 566,692 -> 0
(the 34,896 left come from `parse_index_cell` in the binary search).
No allocation change (the walk allocated nothing before or after).

Size gates: tiny unchanged (no `indexes`); full +104 B, max +88 B,
xl +88 B; ratchets raised by those amounts (each well under
`justify_over`).

## index_search_raw decodes each binary-search probe into owned copies (baseline at 9c007e6)

`bolt_workload` (callgrind, release + debuginfo). Every cell the
binary search in `index_search_raw` visits (17,103 visits over 3,101
pages at n=1000, all from `index_insert_core`: CREATE INDEX and
index maintenance) goes through `parse_index_cell` (a heap copy of the
cell), `index_cell_payload` (a second copy, through
`assemble_payload`'s byte-at-a-time push) and `decode_index_entry` (a
`Value` vector, then an `OwnedValue` vector with a heap copy of every
text / blob field), ~1,890 Ir per visit, to compare one key, although
nearly every index cell is entirely local on the page.

| n    | total Ir      |
|------|---------------|
| 500  | 197,547,305   |
| 1000 | 465,696,010   |
| 2000 | 1,169,278,333 |

n=1000 profile: `index_insert_core` inclusive 174,172,870 (37.40%);
`index_search_raw` inclusive 80,486,203 (17.28%), of which the binary
search's per-probe decode is 32,208,535 (6.92%): `decode_index_entry`
16,761,743 (3.60%), `index_cell_payload` 9,704,854 (2.08%),
`parse_index_cell` 4,026,254 (0.86%), `compare_entries` 1,715,684
(0.37%). dhat (n=1000): 519,770 blocks, 137,369,612 bytes; t-gmax
852,164 B.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: compare local probes in place

The binary search lays out the visited cell (`index_cell_layout`) and,
when its payload is entirely local (`local_len == payload_len`),
decodes the record straight from the page image into a reused
borrowed `Value` vector (`decode_index_values_into`, the checks
`decode_index_entry` makes, which now calls it) and compares it with
`compare_entry_values` (`compare_entries` over borrowed fields). A
cell that spills to overflow pages, and every cell of a UTF-16
database (whose records `assemble_payload` transcodes to UTF-8 before
they are compared), takes the old copy + reassemble + owned-decode
path; `tests/utf16_index_insert.rs` pins that (without the UTF-16
guard, `integrity_check` finds rows missing from both indexes). Workload checksums identical (176089 / 657610 /
4083309 at n=500/1000/2000). Test pass/fail set identical
before/after (1033 tests; the 899 failures are the absent `sqlite3`
oracle, same on the baseline commit), plus the 3 new tests passing.

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 197,547,305   | 186,678,623   | -5.50%  |
| 1000 | 465,696,010   | 440,999,107   | -5.30%  |
| 2000 | 1,169,278,333 | 1,114,283,597 | -4.70%  |

n=1000: `index_search_raw` inclusive 80,486,203 -> 55,806,678
(-30.7%); `index_insert_core` inclusive 174,172,870 -> 149,489,452
(-14.2%); the per-probe compare 32,208,535 -> ~3.4M
(`index_cell_layout` 1,502,183 + `compare_entry_values` 1,655,487
plus the inlined record decode). dhat (n=1000): 519,770 -> 437,354
blocks (-15.86%), 137,369,612 -> 133,470,749 bytes (-2.84%); t-gmax
unchanged at 852,164 B.

Size gates: tiny unchanged (no `indexes`); full +288 B, max +464 B,
xl +472 B; ratchets raised by those amounts (each well under
`justify_over`).

## leaf_cells_meta grows its cell table one regrowth at a time (baseline at b0c69c5)

`bolt_workload` (callgrind, release + debuginfo; dhat). Every table
INSERT validates the target leaf with `leaf_cells_meta`, which pushes
one 56-byte `CellMeta` per cell into an `AVec::new()`: a leaf of ~124
cells regrows 4 -> 8 -> ... -> 128 (6 reallocations, each copying the
table so far), and dhat counts every one of those blocks' bytes.

| n    | total Ir      |
|------|---------------|
| 500  | 187,442,516   |
| 1000 | 440,999,108   |
| 2000 | 1,114,110,743 |

n=1000 profile: `btree_insert` inclusive 69,096,129 (15.67%);
`leaf_cells_meta` inclusive 36,477,188 (8.27%; 2,004 calls from
`btree_insert`, 249,028 cells, ~146 Ir per cell), of which the cell
table's regrowth (`grow_one` -> `finish_grow`, 12,035 calls) is
~6.1M (1.38%). dhat (n=1000): 437,354 blocks, 133,470,797 bytes;
t-gmax 852,164 B.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: reserve the cell table once

`leaf_cells_meta` reserves `min(ncell, pointers that fit on the page)`
entries up front, so the table is one allocation and a corrupt `ncell`
reserves no more than the loop could have pushed. Every caller
benefits (table INSERT, the scan cache, row rewrites); the workload
exercises table INSERT. Workload checksums identical (176089 / 657610
/ 4083309 at n=500/1000/2000). Test pass/fail set identical
before/after (1036 tests; the 899 failures are the absent `sqlite3`
oracle, same on the baseline commit).

| n    | total before  | total after   | Δ total |
|------|---------------|---------------|---------|
| 500  | 187,442,516   | 184,216,372   | -1.72%  |
| 1000 | 440,999,108   | 435,412,115   | -1.27%  |
| 2000 | 1,114,110,743 | 1,099,772,573 | -1.29%  |

dhat (n=1000): 133,470,797 -> 106,515,532 bytes (**-20.2%**);
437,354 -> 427,368 blocks (-2.28%); t-gmax unchanged at 852,164 B.
n=1000: `leaf_cells_meta` inclusive 36,477,188 -> 30,218,092
(-17.2%); `btree_insert` inclusive 69,096,129 -> 63,178,295. The
gate cleared is allocation bytes (>= 10%); the instruction delta is
below the 5% floor and is not claimed as the win.

Negative result, not shipped: decoding the 1- and 2-byte varints
(payload length, rowid) inline in `leaf_cells_meta` on top of the
reservation brings n=1000 to 428,109,167 Ir (a further -1.68%;
`leaf_cells_meta` inclusive 22,488,993). Below the floor on its own.

Size gates: tiny +24 B, full +16 B, max +16 B, xl +16 B; ratchets
raised by those amounts (each well under `justify_over`).

## prepare lexes every statement three times (baseline at 53303df)

`bolt_workload` (callgrind, release + debuginfo; dhat). Every
`Connection::prepare` lexes the statement text once in
`route_attach` (attached-database routing), once in `route` (TEMP
routing), and once more in `parse_params` (the parse itself): 3
`lex_inner` calls per prepare (7,230 at n=1000 for 2,410 prepares).
The two routing lexes only read the tokens to decide the statement
stays on main; the parse lexes the same bytes again.

| n    | total Ir      | prepare inclusive Ir   | lex_inner inclusive Ir |
|------|---------------|------------------------|------------------------|
| 500  | 183,768,300   | 47,175,161 (25.67%)    | 12,882,265 (7.01%)     |
| 1000 | 435,845,467   | 94,235,444 (21.62%)    | 25,921,167 (5.95%)     |
| 2000 | 1,100,189,123 | 191,555,872 (17.41%)   | 54,504,500 (4.95%)     |

n=1000: `lex_inner` called from `route_attach` 8,887,954, `route`
8,526,274, `parse_params` 8,327,492 (2,410 calls each). dhat
(n=1000): 427,368 blocks, 106,515,485 bytes; t-gmax 852,164 B.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 1000`;
`callgrind_annotate --inclusive=yes cg.out | grep -E 'prepare::\{closure#0\} \[|lex_inner'`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 1000`

### After: lex once in prepare

`prepare` lexes the statement once (`parser::lex_statement`: the
tokens and the parameter table `parse_params` would build), hands
the tokens to `route_attach` and `route` (which now take
`Option<&[Token]>`; `None` is the old "the lex failed" path), and,
when the statement stays on main as written (no `MainAs` rewrite),
hands the same tokens to `parse_params_lexed` — the length check,
then the lex error if there was one, then the parse, in the order
`parse_params` reports them. Routers that rewrite the text
(TEMP / attached statements) still lex the rewrite themselves.
Workload checksums identical (176089 / 657610 / 4083309). Test
pass/fail set identical before/after (1165 tests; the 1027
failures are the absent `sqlite3` oracle, same on the baseline
commit); 61 TEMP / ATTACH / parameter / lex-error / parse-error
statements give byte-identical `hipp` CLI output on both commits.

| n    | total before  | total after   | Δ total | prepare before | prepare after | Δ prepare |
|------|---------------|---------------|---------|----------------|---------------|-----------|
| 500  | 183,768,300   | 174,965,156   | -4.79%  | 47,175,161     | 38,558,319    | -18.27%   |
| 1000 | 435,845,467   | 417,555,413   | -4.20%  | 94,235,444     | 76,553,223    | -18.76%   |
| 2000 | 1,100,189,123 | 1,063,193,605 | -3.36%  | 191,555,872    | 154,355,839   | -19.42%   |

`lex_inner` inclusive: 12,882,265 -> 4,677,398 / 25,921,167 ->
9,136,892 / 54,504,500 -> 18,551,678 (-64% to -66%; one call per
prepare instead of three). dhat (n=1000): 427,368 -> 408,906 blocks
(-4.32%), 106,515,485 -> 103,243,613 bytes (-3.07%); t-gmax
unchanged at 852,164 B. The gate cleared is the prepare path (21.6%
of the workload at n=1000) at -18.8%; the whole-workload delta is
below 5% and not claimed as the win.

Size gates: tiny unchanged; full +640 B, max +448 B, xl +600 B;
ratchets raised by those amounts (each well under `justify_over`).

### Re-measured on main at 8ce2df6 (after PRs #33 / #35)

Main now lexes from the arena copy of the text and rewinds the
statement arena past the routers' token copies (`mark`). The shared
lex sits before `mark`, so the rewinds keep its tokens and handles.
Workload checksums identical (176089 / 657610 / 4083309).

| n    | main total    | merged total  | Δ total | main prepare | merged prepare | Δ prepare |
|------|---------------|---------------|---------|--------------|----------------|-----------|
| 500  | 183,157,068   | 175,063,936   | -4.42%  | 47,747,141   | 39,338,652     | -17.61%   |
| 1000 | 433,774,893   | 417,604,509   | -3.73%  | 95,097,022   | 78,228,635     | -17.74%   |
| 2000 | 1,097,949,736 | 1,063,272,983 | -3.16%  | 193,616,622  | 157,639,394    | -18.58%   |

n=1000: `lex_inner` inclusive 23,671,745 -> 8,527,423 (-63.98%).
dhat (n=1000): 429,827 -> 411,365 blocks (-4.30%), 106,617,844 ->
103,345,965 bytes (-3.07%); t-gmax unchanged at 853,148 B.

Size gates on that base: tiny unchanged; full +400 B, max +632 B,
xl +632 B; ratchets set to the measured figures.

## index leaf balance clones every gathered cell ~5 times (baseline at 450a1a7)

`bolt_workload` (callgrind, release + debuginfo; dhat). When an index
leaf overflows (CREATE INDEX over 1000+ rows: `t_g` keys land
mid-tree), `ins_balance_index_leaf_window` gathers the window's
cells and copies each one (its own heap `bytes`) repeatedly: the
trigger leaf's preset list is cloned, every gathered cell is cloned
into `all`, `index_min_pages_ins` runs the greedy partition (cloning
every cell) only to count its pages, `partition_index_ins`'s
`distribute_insert` clones every cell into groups, and each group is
then cloned again. The page count and every capacity check depend
only on cell lengths.

| n    | total Ir      | leaf balance inclusive Ir | calls | dhat blocks | under leaf balance |
|------|---------------|---------------------------|-------|-------------|--------------------|
| 500  | 175,293,229   | 0                         | 0     | 164,546     | 0                  |
| 1000 | 417,936,209   | 26,875,004 (6.43%)        | 12    | 411,769     | 53,064 (12.89%)    |
| 2000 | 1,064,276,249 | 176,896,238 (16.62%)      | 56    | 1,171,020   | 341,564 (29.17%)   |

n=2000: `partition_index_ins` 49,250,091 (4.63%), `index_min_pages_ins`
32,038,647 (3.01%). dhat bytes: 44,035,063 / 104,120,517 / 262,975,768;
t-gmax 483,169 / 854,068 / 1,623,372 B.

Reproduce: `valgrind --tool=callgrind target/release/examples/bolt_workload 2000`;
`callgrind_annotate --inclusive=yes cg.out | grep ins_balance_index_leaf_window`;
dhat: `valgrind --tool=dhat target/release/examples/bolt_workload 2000`

### After: plan the partition from weights, move each cell once

`partition_index_ins`, its greedy fallback and `index_min_pages_ins`
compute the page / divider role of every item from the cells'
weights (`bytes.len() + 2`) alone, then move the gathered cells
into place once (`apply_index_plan`); `partition_index_ins` takes the
cells by value. The window gathers (leaf and interior) move the
loaded cells and the trigger's preset instead of cloning them (the
preset is still cloned if its page recurred in the window). The
partition rules and capacity checks are unchanged. Workload checksums
identical (176089 / 657610 / 4083309); an index-heavy script (DESC /
UNIQUE / NOCASE multi-column indexes, 900-byte overflow keys,
deletes, updates, a WITHOUT ROWID table with a secondary index, a
late CREATE INDEX, INSERT ... SELECT; 6,000+ rows) writes
byte-identical database files (cmp) at 512 B and 4096 B pages, and
`PRAGMA integrity_check` is ok on both.

| n    | total before  | total after   | Δ total | leaf balance before | leaf balance after | Δ balance |
|------|---------------|---------------|---------|---------------------|--------------------|-----------|
| 500  | 175,293,229   | 175,148,361   | -0.08%  | 0                   | 0                  | n/a       |
| 1000 | 417,936,209   | 397,504,007   | -4.89%  | 26,875,004          | 7,145,333          | -73.4%    |
| 2000 | 1,064,276,249 | 933,976,949   | -12.24% | 176,896,238         | 49,390,132         | -72.1%    |

dhat blocks: 164,546 -> 162,720 (-1.11%) / 411,769 -> 362,955
(**-11.85%**) / 1,171,020 -> 874,766 (**-25.30%**); bytes 44,035,063
-> 43,969,549 / 104,120,517 -> 101,598,408 (-2.42%) / 262,975,768 ->
246,747,587 (-6.17%); t-gmax unchanged (483,169 / 854,068 / 1,623,372 B).

Size gates: tiny unchanged; full +592 B, max +544 B, xl +544 B;
ratchets raised by those amounts (each well under `justify_over`).

## index-entry decode copies on the read path (baseline at 3387927)

`bolt_workload` (callgrind, release + debuginfo; dhat). Every index
entry an `IndexCursor` yields (`next_entry`: the `COUNT(*) WHERE g =
?` scans, the join's `u_t` probes) is decoded by `LazyPage::fill`
with three heap copies of the cell before a value is read --
`parse_index_cell` copies the cell image, `index_cell_payload`
copies its local part, `assemble_payload` copies that again (one
byte at a time) -- then cached in the page's slot and cloned out
(`next_entry`'s `.clone()`; the slot is never read again on a
scan). Underneath, every `decode_record_into` (all record decodes,
not only index entries) collects the header's serial types into a
temporary heap list (`serials`) before walking the body.

| n    | total Ir    | dhat blocks | dhat bytes  | t-gmax      |
|------|-------------|-------------|-------------|-------------|
| 500  | 175,161,140 | 162,720     | 43,969,516  | 483,169 B   |
| 1000 | 397,647,237 | 362,955     | 101,598,375 | 854,068 B   |
| 2000 | 933,981,509 | 874,766     | 246,747,554 | 1,623,372 B |

n=2000: `IndexCursor::next_entry` inclusive 51,838,641 Ir (5.55%),
`LazyPage::entry` 52,327,670 (5.60%, seeks included),
`decode_record_into` 43,908,303 (4.70%). dhat blocks: the `serials`
list (record.rs:271) 76,391 (8.73%), `next_entry`'s clone 18,441
(2.11%), and per filled slot (21,528) one block each for the cell
copy, the local copy and the assembled payload (~7.4%).

Both binaries are run from one path (`./bw`) in one session:
callgrind's count moves ~0.1% with the binary's path.

Reproduce: `CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example bolt_workload`;
`cp target/release/examples/bolt_workload bw`;
`valgrind --tool=callgrind --callgrind-out-file=cg.out ./bw 2000`;
`callgrind_annotate --inclusive=yes cg.out | grep -E 'next_entry|decode_record_into'`;
`valgrind --tool=dhat ./bw 2000`

### After: decode in place, take instead of clone, no serials list

`LazyPage::decode` reads a cell whose payload is all on the page
straight from the page image (`index_cell_layout`, then
`decode_index_entry` over the borrowed local bytes, as
`index_search_raw` compares it); a cell that spills to overflow
pages, and every cell of a UTF-16 database, keeps the
`parse_index_cell` / `index_cell_payload` path. `next_entry` takes
the entry (`take_entry`: the decode a seek cached, or a fresh one
left uncached) instead of cloning the slot. `decode_record_into`
counts and checks the serial types in the same first pass, then
reads them again from the header for the body instead of from a
heap list. Workload checksums identical (176089 / 657610 / 4083309);
an index-heavy script (overflow keys up to 1.4 KB, DESC / NOCASE /
multi-column indexes, a WITHOUT ROWID table with a secondary index,
ALTER TABLE ADD COLUMN, deletes, updates, joins, min / max) prints
byte-identical results at UTF-8, UTF-16le and UTF-16be, and
`PRAGMA integrity_check` is ok.

| n    | total before | total after | Δ total | dhat blocks before | after   | Δ blocks    |
|------|--------------|-------------|---------|--------------------|---------|-------------|
| 500  | 175,161,140  | 169,833,321 | -3.04%  | 162,720            | 140,544 | **-13.63%** |
| 1000 | 397,647,237  | 384,541,960 | -3.30%  | 362,955            | 306,207 | **-15.63%** |
| 2000 | 933,981,509  | 893,806,518 | -4.30%  | 874,766            | 715,350 | **-18.22%** |

n=2000: `next_entry` inclusive 51,838,641 -> 27,433,220 (-47.1%);
`decode_record_into` 43,908,303 -> 30,771,218 (-29.9%). dhat bytes
-1.31% / -1.44% / -1.61%; t-gmax +40 B (483,209 / 854,108 /
1,623,412 B).

Ablation at n=2000 (each part alone; run from the build path, base
934,279,328 Ir / 874,766 blocks): the cursor path alone 906,631,225
Ir (-2.96%), 791,741 blocks (-9.49%); the serials list alone
920,472,065 Ir (-1.48%), 798,375 blocks (-8.73%). Neither clears the
allocation floor alone; together they remove the decode chain's
temporary blocks. A third candidate, bulk copies in
place of byte-at-a-time `push` loops (`copy_vec`,
`assemble_payload_into`, `OwnedValue::from_value`, `encode_with`),
measured -1.97% Ir and -3.31% blocks: below the floor, not included.

Size gates: tiny +24 B, full +344 B, max +272 B, xl +272 B;
ratchets raised by those amounts (each well under `justify_over`).

## statement arena first chunk (baseline at b58ac93)

`bolt_workload` (callgrind, release + debuginfo; dhat). Every
`Connection::prepare` builds a new statement `Arena`. Its first
`alloc_copy_handle` (the statement text) allocates the arena's
first chunk, 4096 B, through `vec![0u8; 4096]`: a `calloc` that
glibc must `memset`, because the block is a recycled one. It also
allocates the arena's three bookkeeping lists (`chunks`, `owned`,
`offs`). The `Statement` frees all of it when it drops, and the
next `prepare` allocates it again. In this workload almost every
statement fits in that one chunk: 4,816 zeroed chunk allocations
for 4,810 prepares at n=2000.

| n    | total Ir    | `prepare` inclusive  | `alloc_copy_handle` from `prepare` | dhat blocks | dhat bytes  | t-gmax      |
|------|-------------|----------------------|------------------------------------|-------------|-------------|-------------|
| 500  | 169,843,764 | 39,768,113 (23.41%)  | 6,602,261 (3.89%), 1,210 calls     | 140,544     | 43,393,950  | 483,209 B   |
| 1000 | 384,541,425 | 79,490,294 (20.67%)  | 12,885,725 (3.35%), 2,410 calls    | 306,207     | 100,138,649 | 854,108 B   |
| 2000 | 893,809,665 | 158,066,910 (17.68%) | 26,186,058 (2.93%), 4,810 calls    | 715,350     | 242,771,890 | 1,623,412 B |

n=2000, by self cost: memset is 6.05% of all Ir. Of that, 4.19% is
called from `calloc` (9,086 calls). `__rust_alloc_zeroed` callers:
`Arena::alloc_pos` 23,765,355 Ir (2.66%, 4,816 calls), then
`build_leaf_image_inserted` 2.00%. The statement chunks are
4,810 × 4,096 B = 19.7 MB of the 242.8 MB that dhat counts (8.1%):
11.4% at n=500 and 9.9% at n=1000.

Reproduce: `CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release --example bolt_workload`;
`cp target/release/examples/bolt_workload bw`;
`valgrind --tool=callgrind --callgrind-out-file=cg.out ./bw 2000`;
`callgrind_annotate --inclusive=yes cg.out | grep -E 'Connection<.*>>::prepare::|alloc_copy_handle'`;
`valgrind --tool=dhat ./bw 2000`

### After: a dropped statement's arena waits for the next prepare

The `Connection` holds an `ArenaSlot` (`Rc<Cell<Option<Arena>>>`). A
`Statement`'s arena is a `HomedArena`: it derefs to the `Arena`, and
when it drops it parks the arena in the slot if the arena kept to its
first chunk. An arena that grew is freed as before. `prepare` takes
the parked arena when it has exactly the chunk that a fresh arena's
first allocation (the statement text) would add (`Arena::renew`,
through the `next_cap` growth rule that `alloc_pos` now shares).
Allocations, growth, `ArenaExhausted` and the high-water stat are
therefore those of a fresh arena; only the chunk's stale bytes
differ, and every arena allocation writes its slice before reading
it.

The drop logic sits on the lifetime-free field, not on `Statement`.
A `Drop` impl on `Statement` would keep its `&mut Connection` borrow
live to the end of scope. That fails to compile existing callers
(E0499 in `tests/dialect.rs` and others), so it would be an API
break. No new `unsafe`: `reusable` and `renew` reach the arena's
state through `UnsafeCell::get_mut`.

Workload checksums are identical (176089 / 657610 / 4083309).

| n    | total before | total after | Δ total | `prepare` before | `prepare` after | Δ prepare   |
|------|--------------|-------------|---------|------------------|-----------------|-------------|
| 500  | 169,843,764  | 162,959,071 | -4.05%  | 39,768,113       | 33,560,466      | **-15.61%** |
| 1000 | 384,541,425  | 370,415,783 | -3.67%  | 79,490,294       | 66,833,272      | **-15.92%** |
| 2000 | 893,809,665  | 867,463,500 | -2.95%  | 158,066,910      | 134,114,303     | **-15.15%** |

dhat:

| n    | bytes before | bytes after | Δ bytes     | blocks before | blocks after | Δ blocks |
|------|--------------|-------------|-------------|---------------|--------------|----------|
| 500  | 43,393,950   | 38,248,574  | **-11.86%** | 140,544       | 135,709      | -3.44%   |
| 1000 | 100,138,649  | 89,886,073  | **-10.24%** | 306,207       | 296,572      | -3.15%   |
| 2000 | 242,771,890  | 222,304,914 | -8.43%      | 715,350       | 696,115      | -2.69%   |

That is 4 blocks fewer per prepare: the chunk and the arena's three
bookkeeping lists. t-gmax is +128 B (483,337 / 854,236 / 1,623,540 B):
the slot's `Rc` and the parked chunk while it waits. memset falls
from 6.05% to 3.95% of all Ir at n=2000 (54.0 M -> 34.3 M).

Size gates: tiny +556 B, full +744 B, max +296 B, xl +376 B. Ratchets
are raised by those amounts, each well under `justify_over`.
