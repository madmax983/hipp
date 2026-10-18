//! A10 in-process gates (SPEC §9.2, §4.1): a counting global
//! allocator proves the scan loop's steady state is
//! allocation-free at the cursor layer after warm-up, and the
//! tiny reference configuration's measured arena high-waters +
//! page pool stay inside the 64 KiB engine-RAM budget.
//!
//! The linked-binary size/static gates are measured on the
//! thumbv7em build (see README/STATUS for the numbers).

use hipp::btree::ScanCursor;
use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Both gate tests share one process (and one global counter):
/// serialize them so the other test's churn can't pollute the
/// steady-state measurement.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn allocs() -> usize {
    ALLOCS.load(Ordering::Relaxed)
}

#[test]
fn a10_scan_loop_zero_steady_state_allocations() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    block_on(async {
        // Phase 1: one big leaf page (64 KiB) — the steady window
        // is entirely inside one warm page, so the per-row cost
        // must be exactly zero allocations.
        let mem = MemStorage::new();
        let cfg = Config::default().page_size(65536);
        let mut db = Connection::open(mem.clone(), cfg).await.expect("open");
        {
            let mut st = db
                .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT)")
                .await
                .expect("prep");
            st.step().await.expect("create");
        }
        for i in 1..=300 {
            let mut st = db
                .prepare(&format!(
                    "INSERT INTO t VALUES ({i}, 'payload-{i:04}-0123456789abcdef')"
                ))
                .await
                .expect("prep");
            st.step().await.expect("insert");
            drop(st);
        }
        let pager = db.pager_mut();
        assert_eq!(pager.page_count(), 2, "single leaf page, no splits");
        let mut cursor = ScanCursor::new(2);
        let mut buf = hipp::arena::AVec::new();
        for _ in 0..10 {
            assert!(
                cursor
                    .next_row_into(pager, &mut buf)
                    .await
                    .expect("row")
                    .is_some()
            );
        }
        let before = allocs();
        let mut steady = 0usize;
        while cursor
            .next_row_into(pager, &mut buf)
            .await
            .expect("row")
            .is_some()
        {
            steady += 1;
        }
        assert_eq!(steady, 290);
        assert_eq!(allocs() - before, 0, "steady scan allocated");
        drop(cursor);

        // Phase 2: page-refill cost is bounded and amortised —
        // a full 300-row scan over 4 KiB pages allocates only a
        // small constant per page turned (snapshot + metadata).
        let mem = MemStorage::new();
        let mut db = Connection::open(mem.clone(), Config::default())
            .await
            .expect("open");
        {
            let mut st = db
                .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT)")
                .await
                .expect("prep");
            st.step().await.expect("create");
        }
        for i in 1..=300 {
            let mut st = db
                .prepare(&format!(
                    "INSERT INTO t VALUES ({i}, 'payload-{i:04}-0123456789abcdef')"
                ))
                .await
                .expect("prep");
            st.step().await.expect("insert");
            drop(st);
        }
        let pager = db.pager_mut();
        let pages = pager.page_count();
        let mut cursor = ScanCursor::new(2);
        let mut buf = hipp::arena::AVec::new();
        let before = allocs();
        let mut n = 0usize;
        while cursor
            .next_row_into(pager, &mut buf)
            .await
            .expect("row")
            .is_some()
        {
            n += 1;
        }
        assert_eq!(n, 300);
        let total = allocs() - before;
        assert!(
            total <= 6 * pages as usize,
            "full scan allocated {total} times over {pages} pages"
        );
    });
}

#[test]
fn a10_tiny_config_ram_budget() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    block_on(async {
        let mem = MemStorage::new();
        let cfg = Config::tiny();
        let mut db = Connection::open(mem.clone(), cfg).await.expect("open");
        let mut stmt_hw = 0usize;
        {
            let mut st = db
                .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT)")
                .await
                .expect("prep");
            st.step().await.expect("create");
            stmt_hw = stmt_hw.max(st.arena_high_water());
        }
        let payload = "z".repeat(80);
        for i in 1..=500 {
            let mut st = db
                .prepare(&format!("INSERT INTO t VALUES ({i}, '{payload}')"))
                .await
                .expect("prep");
            st.step().await.expect("insert");
            stmt_hw = stmt_hw.max(st.arena_high_water());
            drop(st);
        }
        {
            let mut q = db.prepare("SELECT id, payload FROM t").await.expect("prep");
            let mut n = 0usize;
            while q.step().await.expect("step") == Step::Row {
                n += 1;
            }
            assert_eq!(n, 500);
            stmt_hw = stmt_hw.max(q.arena_high_water());
        }
        let stats = db.arena_stats();
        // §4.1 tiny budget: pool 16K + catalog 16K + stmt 16K +
        // txn 8K = 56K of the 64K engine-RAM gate (statics are the
        // linked-binary measurement, reported in STATUS).
        assert_eq!(stats.pool_bytes, 16 * 1024);
        assert!(
            stats.catalog_high_water <= 16 * 1024,
            "catalog hw {}",
            stats.catalog_high_water
        );
        assert!(
            stats.txn_high_water <= 8 * 1024,
            "txn hw {}",
            stats.txn_high_water
        );
        assert!(stmt_hw <= 16 * 1024, "stmt hw {stmt_hw}");
        let total = stats.pool_bytes + stats.catalog_high_water + stats.txn_high_water + stmt_hw;
        assert!(total <= 64 * 1024, "total engine RAM {total}");
    });
}
