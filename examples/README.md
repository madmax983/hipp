# Examples

| example | profile | what it shows |
|---|---|---|
| [`tiny.rs`](tiny.rs) | tiny | everything the tiny profile does: tables, INSERT, scans, rowid lookups, transactions, overflow values, arena limits, what tiny refuses ([walkthrough](../docs/profiles/tiny.md)) |
| [`full.rs`](full.rs) | full | the core SQL dialect: indexes, expressions, joins, aggregates, writes, upsert, subqueries, views, CTEs, functions, math, parameters, savepoints, PRAGMA, ALTER, TEMP ([walkthrough](../docs/profiles/full.md)) |
| [`max.rs`](max.rs) | max | window functions, triggers, foreign keys, JSON, expression indexes, WITHOUT ROWID, date / time, virtual tables, R*Tree, FTS5, EXPLAIN QUERY PLAN, ATTACH, WAL, UTF-16, auto-vacuum ([walkthrough](../docs/profiles/max.md)) |
| [`xl.rs`](xl.rs) | xl | the join planner, automatic indexes, Bloom filters, MULTI-INDEX OR, skip-scans, flattening, FTS5 deletes ([walkthrough](../docs/profiles/xl.md)) |
| [`custom_storage.rs`](custom_storage.rs) | tiny | implementing `Storage`, and power loss at every point of a commit |
| [`file_db.rs`](file_db.rs) | `std` | a database file on disk with `FileStorage`, and `VACUUM INTO` |
| [`params.rs`](params.rs) | `params` | prepare once, bind / step / reset per row |
| [`mem_demo.rs`](mem_demo.rs) | tiny | the smallest end-to-end program |
| [`bolt_workload.rs`](bolt_workload.rs) | full | the profiling workload behind [`docs/perf-baselines.md`](../docs/perf-baselines.md) |
| [`crash_writer.rs`](crash_writer.rs) | `std` | the crash-injection harness `tests/crash.rs` drives |

The async-runtime examples live in their own crate, [`../runtimes`](../runtimes),
so the core library keeps zero dependencies:

| example | what it shows |
|---|---|
| [`tokio.rs`](../runtimes/examples/tokio.rs) | an async `Storage` over `tokio::fs`; tasks sharing a connection on a `LocalSet`; a database thread serving a multi-threaded runtime through a `Send + Clone` handle |
| [`smol.rs`](../runtimes/examples/smol.rs) | a `Storage` over `std::fs` run on smol's blocking pool (`smol::unblock`); tasks sharing a connection on a `LocalExecutor` |
| [`embassy.rs`](../runtimes/examples/embassy.rs) | embassy's executor; a simulated DMA flash `Storage` that yields while it programs; sensor and database tasks |

Run one in its profile:

```bash
cargo run --example tiny --no-default-features
```

```bash
cargo run --example full --no-default-features --features full
```

```bash
cargo run --example max --no-default-features --features max
```

```bash
cargo run --example xl --no-default-features --features xl
```

The profile examples check every answer they print, and panic on a wrong
one. CI runs each example in its own profile and in every larger one, and
the runtime examples too:

```bash
ci/run-examples.sh
```

When editing an example, `HIPP_EXAMPLE_KEEP_GOING=1` reports every mismatch
instead of stopping at the first. `examples/common/mod.rs` holds the helpers
(`check`, `plan`, `fails`, `exec`, `rows`).
