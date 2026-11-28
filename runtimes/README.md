# hipp under async runtimes

hipp's API is `async` and has no runtime of its own: it never spawns, sleeps
or blocks on an executor, so any executor can drive it. These examples show
how with **tokio**, **smol** and **embassy**. Each includes a `Storage`
implementation suited to its runtime.

They are in a crate of their own (`hipp-runtimes`, not published) so the core
library keeps zero dependencies, dev-dependencies included. CI runs all three
(`ci/run-examples.sh runtimes`).

```bash
cargo run -p hipp-runtimes --example tokio
```

```bash
cargo run -p hipp-runtimes --example smol
```

```bash
cargo run -p hipp-runtimes --example embassy
```

| example | storage | sharing the connection |
|---|---|---|
| [`tokio.rs`](examples/tokio.rs) | `TokioFile`: async I/O over `tokio::fs` | part 1: `Rc<tokio::sync::Mutex<Connection>>` between `spawn_local` tasks on a current-thread runtime. Part 2: a database thread with an `mpsc` / `oneshot` request channel, so tasks on a multi-threaded runtime hold a `Send + Clone` handle. |
| [`smol.rs`](examples/smol.rs) | `UnblockFile`: `std::fs` on smol's blocking pool via `smol::unblock` | `Rc<smol::lock::Mutex<Connection>>` between tasks on a `LocalExecutor`. A ticker task shows the executor stays free during I/O. |
| [`embassy.rs`](examples/embassy.rs) | `DmaFlash`: simulated flash whose writes and syncs stay `Pending` for a few polls, as DMA does | a sensor task and a database task on embassy's executor (std platform), passing readings through a queue. The database work is boxed to keep the task's static small. |

The rules they follow:

- A `Connection` is `!Send`. Keep it on one thread.
- One statement at a time per connection. To share one between tasks, use an
  async mutex and hold it for a statement or a whole transaction.
- A `Storage` must make `sync` a real durability barrier, must create
  sidecars lazily and delete them idempotently, and must tolerate being
  cancelled at any await. See the [usage guide §10](../docs/GUIDE.md#10-implementing-storage).
- If rustc reports "queries overflow the depth limit" for an executor's
  future, box the future you hand the executor.

`src/lib.rs` holds the helpers the three share (`exec`, `rows`, `try_rows`,
sidecar file names).
