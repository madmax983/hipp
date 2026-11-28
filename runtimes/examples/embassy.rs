//! Driving hipp from embassy, the embedded async executor.
//!
//! ```text
//! cargo run -p hipp-runtimes --example embassy
//! ```
//!
//! This runs embassy's executor on the host (`platform-std`), with the
//! same task model as on a microcontroller: statically allocated tasks,
//! one thread, no heap required by the executor. On a chip, swap the
//! platform feature (`platform-cortex-m`, `platform-riscv32`, ...) and
//! back [`DmaFlash`] with your HAL's flash or SD driver. The hipp side
//! stays the same.
//!
//! What it shows:
//!
//! - **A `Storage` whose operations complete later**, as a DMA transfer
//!   does. [`DmaFlash`] returns `Pending` for a few polls on every
//!   write, sync and delete, so the executor runs other tasks meanwhile.
//! - **A sensor task and a database task** cooperating through a
//!   queue: readings keep arriving while a commit waits on the flash.
//! - **Keeping a task small.** embassy sizes each task's static memory
//!   at compile time from its future. Boxing hipp's work inside the
//!   task keeps that static at the size of a pointer.

use embassy_executor::{Executor, Spawner};
use hipp::storage::{MemError, MemSidecar, MemStorage, Sidecar, SidecarFile, Storage};
use hipp::{Config, Connection, Step};
use hipp_runtimes::{exec, rows};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

/// How many polls one simulated flash operation takes.
const DMA_POLLS: u32 = 3;
/// How many readings the sensor produces.
const READINGS: i64 = 40;

/// State shared by the tasks (all on one executor thread, so `Rc` and
/// `Cell` are enough).
#[derive(Default)]
struct Shared {
    /// Readings waiting to be stored: (sequence number, millidegrees).
    queue: RefCell<VecDeque<(i64, i64)>>,
    sensor_done: Cell<bool>,
    /// A flash operation is in flight.
    dma_busy: Cell<bool>,
    /// Readings the sensor produced while a flash operation was in flight.
    produced_during_dma: Cell<u32>,
    /// Polls that found the flash still busy.
    dma_waits: Cell<u32>,
}

/// Set when the database task finishes; the executor then returns.
static DONE: AtomicBool = AtomicBool::new(false);

/// Wait as a DMA transfer would: `Pending` for a few polls, waking the
/// task each time so the executor comes back to it after the others.
async fn dma_transfer(shared: &Shared) {
    shared.dma_busy.set(true);
    for _ in 0..DMA_POLLS {
        shared.dma_waits.set(shared.dma_waits.get() + 1);
        embassy_futures::yield_now().await;
    }
    shared.dma_busy.set(false);
}

/// Simulated flash: the bytes live in a `MemStorage`, and every
/// mutating operation takes a few polls to complete.
struct DmaFlash {
    mem: MemStorage,
    shared: Rc<Shared>,
}

struct DmaFlashSidecar {
    inner: MemSidecar,
    shared: Rc<Shared>,
}

impl Storage for DmaFlash {
    type Error = MemError;
    type Sidecar = DmaFlashSidecar;

    async fn len(&self) -> Result<u64, MemError> {
        self.mem.len().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        // Reads are memory-mapped on many parts: immediate.
        self.mem.read_at(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), MemError> {
        dma_transfer(&self.shared).await;
        self.mem.write_at(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Result<(), MemError> {
        self.mem.set_len(len).await
    }
    async fn sync(&mut self) -> Result<(), MemError> {
        // The durability barrier: wait for the flash to finish programming.
        dma_transfer(&self.shared).await;
        self.mem.sync().await
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<DmaFlashSidecar, MemError> {
        Ok(DmaFlashSidecar {
            inner: self.mem.sidecar(kind).await?,
            shared: self.shared.clone(),
        })
    }
}

impl SidecarFile for DmaFlashSidecar {
    type Error = MemError;

    fn existed(&self) -> bool {
        self.inner.existed()
    }
    async fn len(&self) -> Result<u64, MemError> {
        self.inner.len().await
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        self.inner.read_at(offset, buf).await
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), MemError> {
        dma_transfer(&self.shared).await;
        self.inner.write_at(offset, buf).await
    }
    async fn set_len(&mut self, len: u64) -> Result<(), MemError> {
        self.inner.set_len(len).await
    }
    async fn sync(&mut self) -> Result<(), MemError> {
        dma_transfer(&self.shared).await;
        self.inner.sync().await
    }
    async fn delete(&mut self) -> Result<(), MemError> {
        dma_transfer(&self.shared).await;
        self.inner.delete().await
    }
}

/// The sensor: a reading every few polls, whatever the database is
/// doing.
#[embassy_executor::task]
async fn sensor(shared: Rc<Shared>) {
    for seq in 0..READINGS {
        let millideg = 21_000 + (seq * 37) % 900;
        shared.queue.borrow_mut().push_back((seq, millideg));
        if shared.dma_busy.get() {
            shared
                .produced_during_dma
                .set(shared.produced_during_dma.get() + 1);
        }
        // The next reading is a few polls away.
        for _ in 0..4 {
            embassy_futures::yield_now().await;
        }
    }
    shared.sensor_done.set(true);
}

/// The database task: drain the queue in batches, one transaction per
/// batch, with one prepared INSERT.
#[embassy_executor::task]
async fn database(shared: Rc<Shared>, medium: MemStorage) {
    let work = store_readings(shared, medium);
    println!(
        "the database work's future is {} bytes; boxed, the task's static holds a pointer",
        core::mem::size_of_val(&work)
    );
    Box::pin(work).await;
    DONE.store(true, Ordering::Release);
}

async fn store_readings(shared: Rc<Shared>, medium: MemStorage) {
    let storage = DmaFlash {
        mem: medium,
        shared: shared.clone(),
    };
    // The embedded reference configuration: a 4-page cache, small arenas.
    let mut db = Connection::open(storage, Config::tiny())
        .await
        .expect("open database");
    exec(
        &mut db,
        "CREATE TABLE reading (seq INTEGER PRIMARY KEY, millideg INTEGER)",
    )
    .await;

    let mut batches = 0;
    loop {
        let batch: Vec<(i64, i64)> = shared.queue.borrow_mut().drain(..).collect();
        if batch.is_empty() {
            if shared.sensor_done.get() {
                break;
            }
            embassy_futures::yield_now().await;
            continue;
        }
        batches += 1;
        exec(&mut db, "BEGIN").await;
        {
            let mut ins = db
                .prepare("INSERT INTO reading (seq, millideg) VALUES (?1, ?2)")
                .await
                .expect("prepare");
            for (seq, millideg) in batch {
                ins.bind_i64(1, seq).expect("bind");
                ins.bind_i64(2, millideg).expect("bind");
                assert_eq!(ins.step().await.expect("insert"), Step::Done);
                ins.reset();
            }
        }
        // The commit's journal writes and syncs wait on the flash;
        // the sensor keeps producing meanwhile.
        exec(&mut db, "COMMIT").await;
    }

    let summary = rows(
        &mut db,
        "SELECT count(*), min(seq), max(seq), round(avg(millideg) / 1000.0, 2) FROM reading",
    )
    .await;
    println!("stored in {batches} transactions: count|min|max|avg °C = {summary:?}");
    assert_eq!(summary, ["40|0|39|21.38"]);
    assert_eq!(rows(&mut db, "PRAGMA integrity_check").await, ["ok"]);
    db.close().await.expect("close");
}

fn main() {
    let shared = Rc::new(Shared::default());
    let medium = MemStorage::new();

    // `run_until` needs a `&'static mut Executor`; leaking a box gives
    // one without `static mut`. (On a chip: `#[embassy_executor::main]`.)
    let executor: &'static mut Executor = Box::leak(Box::new(Executor::new()));
    let (s1, s2, m) = (shared.clone(), shared.clone(), medium.clone());
    executor.run_until(
        move |spawner: Spawner| {
            spawner.spawn(sensor(s1).expect("spawn sensor"));
            spawner.spawn(database(s2, m).expect("spawn database"));
        },
        || DONE.load(Ordering::Acquire),
    );

    println!(
        "flash busy for {} polls; the sensor produced {} readings while a flash operation was in flight",
        shared.dma_waits.get(),
        shared.produced_during_dma.get()
    );
    assert!(shared.dma_waits.get() > 0);
    assert!(shared.produced_during_dma.get() > 0);

    // The flash image is a SQLite database.
    let image = medium.snapshot();
    assert_eq!(&image[..16], b"SQLite format 3\0");
    assert!(!medium.journal_exists());
    println!("flash image: {} bytes, a SQLite 3 database", image.len());
    println!("\nembassy: ok");
}
