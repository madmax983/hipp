//! Implementing `Storage` yourself, and what crash safety means.
//!
//! hipp never touches a file system on its own: everything goes
//! through the `Storage` trait (the main file) and `SidecarFile`
//! (the `-journal`, and with `wal` the `-wal` / `-shm`). On a
//! microcontroller you implement them over flash, an SD card or a
//! block device; on a host `FileStorage` (feature `std`) is provided.
//!
//! This example wraps `MemStorage` in a `PowerCut` storage that
//! "loses power" after a chosen number of writes: every later write,
//! truncate, sync or delete fails. It cuts an INSERT's commit at
//! every possible point and checks that reopening always finds
//! either the whole transaction or none of it — never a torn file.
//!
//! Works in every profile, tiny included:
//!
//! ```text
//! cargo run --example custom_storage --no-default-features
//! ```

mod common;

use common::{check, exec, rows};
use hipp::storage::{MemError, MemSidecar, MemStorage, Sidecar, SidecarFile, Storage, block_on};
use hipp::{Config, Connection};
use std::cell::Cell;
use std::rc::Rc;

/// The storage error type is yours to choose. hipp hands it back
/// inside `hipp::Error::Storage(..)` (wrapped, never inspected).
#[derive(Debug)]
enum PowerError {
    /// The power is off: nothing reaches the medium any more.
    Off,
    /// The wrapped storage failed.
    Mem(MemError),
}

/// Counts mutating operations; after `budget` of them, the power
/// is cut. Shared between the database file and its sidecars.
#[derive(Clone)]
struct Power {
    budget: Rc<Cell<Option<usize>>>,
    used: Rc<Cell<usize>>,
}

impl Power {
    fn on() -> Power {
        Power {
            budget: Rc::new(Cell::new(None)),
            used: Rc::new(Cell::new(0)),
        }
    }
    fn cut_after(&self, ops: usize) {
        self.budget.set(Some(ops));
        self.used.set(0);
    }
    /// Called before every mutating operation.
    fn spend(&self) -> Result<(), PowerError> {
        if let Some(b) = self.budget.get()
            && self.used.get() >= b
        {
            return Err(PowerError::Off);
        }
        self.used.set(self.used.get() + 1);
        Ok(())
    }
}

/// The main database file: reads always work, writes spend power.
struct PowerCut {
    mem: MemStorage,
    power: Power,
}

/// A sidecar file (the rollback journal): the same rule.
struct PowerCutSidecar {
    inner: MemSidecar,
    power: Power,
}

impl Storage for PowerCut {
    type Error = PowerError;
    type Sidecar = PowerCutSidecar;

    async fn len(&self) -> Result<u64, PowerError> {
        self.mem.len().await.map_err(PowerError::Mem)
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), PowerError> {
        self.mem.read_at(offset, buf).await.map_err(PowerError::Mem)
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), PowerError> {
        self.power.spend()?;
        self.mem
            .write_at(offset, buf)
            .await
            .map_err(PowerError::Mem)
    }
    async fn set_len(&mut self, len: u64) -> Result<(), PowerError> {
        self.power.spend()?;
        self.mem.set_len(len).await.map_err(PowerError::Mem)
    }
    async fn sync(&mut self) -> Result<(), PowerError> {
        self.power.spend()?;
        self.mem.sync().await.map_err(PowerError::Mem)
    }
    async fn sidecar(&mut self, kind: Sidecar) -> Result<PowerCutSidecar, PowerError> {
        Ok(PowerCutSidecar {
            inner: self.mem.sidecar(kind).await.map_err(PowerError::Mem)?,
            power: self.power.clone(),
        })
    }
    // The other trait methods (`file_name`, `log_beside`, ...) have
    // defaults that suit storage with no file system around it.
}

impl SidecarFile for PowerCutSidecar {
    type Error = PowerError;

    fn existed(&self) -> bool {
        self.inner.existed()
    }
    async fn len(&self) -> Result<u64, PowerError> {
        self.inner.len().await.map_err(PowerError::Mem)
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), PowerError> {
        self.inner
            .read_at(offset, buf)
            .await
            .map_err(PowerError::Mem)
    }
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), PowerError> {
        self.power.spend()?;
        self.inner
            .write_at(offset, buf)
            .await
            .map_err(PowerError::Mem)
    }
    async fn set_len(&mut self, len: u64) -> Result<(), PowerError> {
        self.power.spend()?;
        self.inner.set_len(len).await.map_err(PowerError::Mem)
    }
    async fn sync(&mut self) -> Result<(), PowerError> {
        self.power.spend()?;
        self.inner.sync().await.map_err(PowerError::Mem)
    }
    async fn delete(&mut self) -> Result<(), PowerError> {
        self.power.spend()?;
        self.inner.delete().await.map_err(PowerError::Mem)
    }
}

/// The ids in table `t`, read from whatever the medium now holds.
async fn ids(medium: &MemStorage) -> Vec<String> {
    // `MemStorage` clones share their bytes, so this opens the very
    // "device" the crashed connection was writing to. Opening finds
    // the hot journal (if any) and rolls the torn commit back.
    let mut db = Connection::open(medium.clone(), Config::tiny())
        .await
        .expect("reopen after the crash");
    rows(&mut db, "SELECT id FROM t").await
}

fn main() {
    block_on(async {
        let medium = MemStorage::new();
        {
            let mut db = Connection::open(medium.clone(), Config::tiny())
                .await
                .expect("open");
            exec(
                &mut db,
                "CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)",
            )
            .await;
            exec(
                &mut db,
                "INSERT INTO t VALUES (1, 'committed before the test')",
            )
            .await;
            check(
                &mut db,
                "SELECT id, note FROM t",
                &["1|committed before the test"],
            )
            .await;
        }
        let before = medium.snapshot();

        // How many mutating operations does the next commit take?
        let power = Power::on();
        let total = {
            let mut db = Connection::open(
                PowerCut {
                    mem: MemStorage::from_bytes(before.clone()),
                    power: power.clone(),
                },
                Config::tiny(),
            )
            .await
            .expect("open");
            power.cut_after(usize::MAX);
            exec(&mut db, "INSERT INTO t VALUES (2, 'two'), (3, 'three')").await;
            power.used.get()
        };
        println!("\nthe commit takes {total} mutating storage operations");

        // Now cut the power after 0, 1, ..., total-1 of them.
        let (mut none, mut all) = (0, 0);
        for k in 0..total {
            let medium = MemStorage::from_bytes(before.clone());
            let power = Power::on();
            let mut db = Connection::open(
                PowerCut {
                    mem: medium.clone(),
                    power: power.clone(),
                },
                Config::tiny(),
            )
            .await
            .expect("open");
            power.cut_after(k);
            let res = match db
                .prepare("INSERT INTO t VALUES (2, 'two'), (3, 'three')")
                .await
            {
                Ok(mut st) => st.step().await.map(|_| ()),
                Err(e) => Err(e),
            };
            // The failure surfaces as `Error::Storage(PowerError::Off)`.
            let err = res.expect_err("the power was cut");
            assert!(format!("{err:?}").contains("Off"), "{err:?}");
            drop(db); // no cleanup: the "device" just lost power

            let got = ids(&medium).await;
            match got.as_slice() {
                [a] if a == "1" => none += 1,
                [a, b, c] if a == "1" && b == "2" && c == "3" => all += 1,
                other => panic!("torn commit after {k} ops: {other:?}"),
            }
        }
        println!(
            "cut at each of {total} points: {none} recovered to the old state, \
             {all} found the commit complete, 0 torn"
        );
        assert_eq!(none + all, total);
        // The commit point is the journal's deletion, its last
        // operation: cut before it and the next open rolls back.
        // With the whole budget, the commit lands.
        let medium = MemStorage::from_bytes(before.clone());
        let power = Power::on();
        {
            let mut db = Connection::open(
                PowerCut {
                    mem: medium.clone(),
                    power: power.clone(),
                },
                Config::tiny(),
            )
            .await
            .expect("open");
            power.cut_after(total);
            exec(&mut db, "INSERT INTO t VALUES (2, 'two'), (3, 'three')").await;
        }
        assert_eq!(ids(&medium).await, ["1", "2", "3"]);
        println!("with {total} operations of power the commit completes: ids 1, 2, 3");
    });
    println!("\ncustom_storage: ok");
}
