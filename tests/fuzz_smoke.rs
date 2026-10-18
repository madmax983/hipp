//! A8: fuzz smoke (SPEC §9.2). Deterministic mutations of the
//! golden fixtures fed to open/scan/insert, and byte mutations of
//! SQL fed to the parser: the decoders must never panic or hang.
//! (Spec target is 10^6 inputs; this debug-profile smoke runs
//! 2×20k file mutations + 20k parser mutations. The 10^6 run is a
//! release-profile pass recorded in STATUS.md.)

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("fixture")
}

fn mutate(rng: &mut Rng, buf: &mut [u8]) {
    let n = 1 + (rng.next() % 8) as usize;
    for _ in 0..n {
        let i = (rng.next() as usize) % buf.len();
        buf[i] = rng.next() as u8;
    }
    // Occasionally truncate.
    if rng.next().is_multiple_of(23) && buf.len() > 200 {
        let keep = 100 + (rng.next() as usize) % (buf.len() - 100);
        // Truncation is applied by the caller via resize; here we
        // emulate by zeroing the tail (same decoder surface).
        for b in &mut buf[keep..] {
            *b = 0;
        }
    }
}

/// Per-fixture iteration counts: probe.db is tiny and cheap;
/// many.db scans are capped shallow (decoder surface is in the
/// first pages; deep mutated scans are slow, not more thorough).
fn file_iters(name: &str) -> usize {
    // HIPP_FUZZ_BIG=1: the SPEC §9.2 10^6-input pass over the
    // small fixture (release profile; see STATUS.md).
    if std::env::var("HIPP_FUZZ_BIG").is_ok() {
        return match name {
            "probe.db" => 1_000_000,
            _ => 0,
        };
    }
    match name {
        "probe.db" => 20_000,
        _ => 3_000,
    }
}

#[test]
fn a8_fuzz_fixtures() {
    block_on(async {
        for name in ["probe.db", "many.db"] {
            let golden = fixture(name);
            let mut rng = Rng(fixture_seed(name));
            let mut opened = 0usize;
            for _iter in 0..file_iters(name) {
                let mut bytes = golden.clone();
                mutate(&mut rng, &mut bytes);
                let mem = MemStorage::new();
                mem.set_snapshot(bytes);
                let Ok(mut db) = Connection::open(mem, Config::default()).await else {
                    continue;
                };
                opened += 1;
                // Scan the fixture's table with a step cap.
                if let Ok(mut q) = db.prepare("SELECT * FROM t").await {
                    for _ in 0..60 {
                        match q.step().await {
                            Ok(Step::Row) => continue,
                            _ => break,
                        }
                    }
                }
                drop(db);
            }
            // Sanity: the mutator must produce *some* openable files
            // (otherwise the smoke proves nothing).
            if file_iters(name) > 0 {
                assert!(opened > 100, "{name}: only {opened} opens");
            }
        }
    });
}

fn fixture_seed(name: &str) -> u64 {
    // Distinct deterministic seed per fixture.
    match name {
        "probe.db" => 0x1111_2222_3333_4444,
        _ => 0x5555_6666_7777_8888,
    }
}

const SQL_ITERS: usize = 20_000;

#[test]
fn a8_fuzz_parser() {
    block_on(async {
        let corpus: &[&[u8]] = &[
            b"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB)",
            b"INSERT INTO t VALUES (9007199254740993, 'x', 1.5e10, x'00ff')",
            b"SELECT id, name FROM t",
            b"CREATE TABLE \"weird name\" ([a b] TEXT DEFAULT 'it''s', c VARCHAR(12) NOT NULL, PRIMARY KEY(c))",
            b"INSERT INTO t (name, id) VALUES ('', NULL)",
        ];
        let mem = MemStorage::new();
        let mut db = Connection::open(mem, Config::default())
            .await
            .expect("open");
        let mut st = db
            .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB)")
            .await
            .expect("create");
        st.step().await.expect("step");
        drop(st);
        let mut rng = Rng(0xabcd_ef01_2345_6789);
        for _ in 0..SQL_ITERS {
            let base = corpus[(rng.next() as usize) % corpus.len()];
            let mut sql = base.to_vec();
            mutate(&mut rng, &mut sql);
            let Ok(text) = core::str::from_utf8(&sql) else {
                continue;
            };
            // Ok or Err — never a panic. Successful prepares are
            // dropped unstepped (mutations that parse as writes must
            // not run: a mutated INSERT could be valid but huge).
            let _ = db.prepare(text).await;
        }
    });
}
