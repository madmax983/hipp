//! Differential acceptance tests vs the oracle CLI (SPEC §9.2):
//! A2 (hipp writes, oracle reads), A3 (oracle writes, hipp reads),
//! A4 (root split), A5 (10 KiB overflow, both directions),
//! A7 (format rejections, no panics).

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, MemStorage, Step, Value};

const A1_CREATE: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v REAL, b BLOB)";
const A1_ROWS: [&str; 5] = [
    "INSERT INTO t VALUES (1, 'one', 1.5, x'00ff')",
    "INSERT INTO t VALUES (9007199254740993, 'big', NULL, x'')",
    "INSERT INTO t VALUES (3, 'three', -0.5, x'deadbeef')",
    "INSERT INTO t VALUES (4, '', 0.0, NULL)",
    "INSERT INTO t VALUES (5, 'five', 2.5, x'00')",
];
/// Expected oracle list-mode rendering of the A1 table:
/// id|name|v|hex(b), in rowid order. Reals render as SQLite prints
/// them (1.5, -0.5, 0.0, 2.5).
const A1_EXPECTED: &str = "\
1|one|1.5|00FF
3|three|-0.5|DEADBEEF
4||0.0|
5|five|2.5|00
9007199254740993|big||\n";

async fn hipp_exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str)
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    assert_eq!(st.step().await.expect("step"), Step::Done);
}

fn a2_hipp_writes_oracle_reads() {
    block_on(async {
        let path = common::temp_db("a2");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            hipp_exec(&mut db, A1_CREATE).await;
            for r in A1_ROWS {
                hipp_exec(&mut db, r).await;
            }
        }
        let chk = common::cli(&path, "PRAGMA integrity_check;");
        assert_eq!(chk.trim(), "ok");
        let out = common::cli(&path, "SELECT id, name, v, hex(b) FROM t;");
        assert_eq!(out, A1_EXPECTED, "oracle version: {}", common::oracle_version());
    });
}

fn a3_oracle_writes_hipp_reads() {
    block_on(async {
        // Default page size, then a 512-byte page size variant.
        for (tag, pragma) in [("a3", ""), ("a3-512", "PRAGMA page_size=512;")] {
            let path = common::temp_db(tag);
            let script = format!(
                "{pragma} {A1_CREATE}; {}",
                A1_ROWS.join("; ")
            );
            let out = common::cli(&path, &script);
            assert_eq!(out, "");
            // hipp reads it back and renders the same values.
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            let mut q = db.prepare("SELECT id, name, v, b FROM t").await.expect("prep");
            let mut rendered = String::new();
            while q.step().await.expect("step") == Step::Row {
                let id = q.column_i64(0).unwrap();
                let name = q.column_bytes(1).map(|b| String::from_utf8_lossy(b).into_owned());
                let v = match q.column_value(2) {
                    Some(Value::Real(f)) => format!("{f}"),
                    Some(Value::Null) => String::new(),
                    other => panic!("v: {other:?}"),
                };
                // SQLite prints integral reals with .0
                let v = if v.contains('.') || v.is_empty() { v } else { format!("{v}.0") };
                let b = match q.column_value(3) {
                    Some(Value::Blob(bytes)) => bytes.iter().map(|x| format!("{x:02X}")).collect(),
                    Some(Value::Null) => String::new(),
                    other => panic!("b: {other:?}"),
                };
                rendered.push_str(&format!("{}|{}|{}|{}\n", id, name.as_deref().unwrap_or(""), v, b));
            }
            assert_eq!(rendered, A1_EXPECTED, "tag {tag}");
        }
    });
}

fn a4_root_split() {
    block_on(async {
        let path = common::temp_db("a4");
        let payload = "x".repeat(80);
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            hipp_exec(&mut db, "CREATE TABLE big (id INTEGER PRIMARY KEY, data TEXT)").await;
            for i in 1..=500 {
                hipp_exec(
                    &mut db,
                    &format!("INSERT INTO big VALUES ({i}, '{payload}')"),
                )
                .await;
            }
            // hipp-side scan: all 500 in order.
            let mut q = db.prepare("SELECT id FROM big").await.expect("prep");
            let mut expect = 1i64;
            while q.step().await.expect("step") == Step::Row {
                assert_eq!(q.column_i64(0), Some(expect));
                expect += 1;
            }
            assert_eq!(expect, 501);
        }
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        assert_eq!(common::cli(&path, "SELECT count(*) FROM big;").trim(), "500");
        // Root page of `big` must be an interior table page (0x05).
        let bytes = std::fs::read(&path).expect("read db");
        let root = common::cli(&path, "SELECT rootpage FROM sqlite_schema WHERE name='big';")
            .trim()
            .parse::<usize>()
            .expect("rootpage");
        let page_size = 4096usize;
        assert_eq!(bytes[(root - 1) * page_size], 0x05, "root is interior");
    });
}

fn a5_overflow_both_directions() {
    block_on(async {
        let big = "y".repeat(10 * 1024);
        // hipp writes, oracle reads.
        let path = common::temp_db("a5a");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            hipp_exec(&mut db, "CREATE TABLE o (id INTEGER PRIMARY KEY, data TEXT)").await;
            hipp_exec(&mut db, &format!("INSERT INTO o VALUES (1, '{big}')")).await;
            let mut q = db.prepare("SELECT data FROM o").await.expect("prep");
            assert_eq!(q.step().await.expect("step"), Step::Row);
            assert_eq!(q.column_bytes(0).map(|b| b.len()), Some(10240));
        }
        assert_eq!(common::cli(&path, "PRAGMA integrity_check;").trim(), "ok");
        assert_eq!(
            common::cli(&path, "SELECT length(data) FROM o;").trim(),
            "10240"
        );
        assert_eq!(
            common::cli(&path, "SELECT count(*) FROM o WHERE data GLOB '*[^y]*';").trim(),
            "0",
            "every byte of the overflow value is 'y' per the oracle"
        );
        assert_eq!(
            common::cli(&path, "SELECT substr(data, 10239, 2) FROM o;").trim(),
            "yy"
        );
        // oracle writes, hipp reads.
        let path2 = common::temp_db("a5b");
        common::cli(
            &path2,
            &format!("CREATE TABLE o (id INTEGER PRIMARY KEY, data TEXT); INSERT INTO o VALUES (1, '{big}');"),
        );
        let fs = FileStorage::open(&path2).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open");
        let mut q = db.prepare("SELECT data FROM o").await.expect("prep");
        assert_eq!(q.step().await.expect("step"), Step::Row);
        let bytes = q.column_bytes(0).expect("data");
        assert_eq!(bytes.len(), 10240);
        assert!(bytes.iter().all(|b| *b == b'y'));
    });
}

fn a7_rejections() {
    block_on(async {
        // WAL fixture → WalUnsupported.
        let wal = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wal.db");
        let fs = FileStorage::open(&wal).expect("fs open");
        let r = Connection::open(fs, Config::default()).await;
        assert!(
            matches!(r, Err(Error::WalUnsupported)),
            "wal: {:?}", r.err(),
        );

        // UTF-16 file (oracle-created) → UnsupportedEncoding.
        let path = common::temp_db("a7-utf16");
        common::cli(&path, "PRAGMA encoding='UTF-16le'; CREATE TABLE u (a TEXT);");
        let fs = FileStorage::open(&path).expect("fs open");
        let r = Connection::open(fs, Config::default()).await;
        assert!(
            matches!(r, Err(Error::UnsupportedEncoding)),
            "utf16: {:?}", r.err(),
        );

        // Garbage → BadMagic.
        let mem = MemStorage::new();
        mem.set_snapshot(vec![0xAB; 8192]);
        let r = Connection::open(mem, Config::default()).await;
        assert!(matches!(r, Err(Error::BadMagic)), "garbage: {:?}", r.err());

        // Truncated header → TruncatedHeader.
        let mem = MemStorage::new();
        mem.set_snapshot(vec![0u8; 50]);
        let r = Connection::open(mem, Config::default()).await;
        assert!(
            matches!(r, Err(Error::TruncatedHeader)),
            "truncated: {:?}", r.err(),
        );

        // Corrupt a data page of a valid file: errors, never panic.
        let path = common::temp_db("a7-corrupt");
        {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default()).await.expect("open");
            hipp_exec(&mut db, "CREATE TABLE c (id INTEGER PRIMARY KEY, x TEXT)").await;
            hipp_exec(&mut db, "INSERT INTO c VALUES (1, 'ok')").await;
        }
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[4096] = 0x77; // page 2 kind byte → nonsense
        std::fs::write(&path, &bytes).expect("write");
        let fs = FileStorage::open(&path).expect("fs open");
        let mut db = Connection::open(fs, Config::default()).await.expect("open opens");
        let mut q = db.prepare("SELECT x FROM c").await.expect("prep");
        let r = q.step().await;
        assert!(matches!(r, Err(Error::CorruptPage { page: 2 })), "corrupt scan: {:?}", r.err());
    });
}

#[test]
fn a2() {
    a2_hipp_writes_oracle_reads();
}
#[test]
fn a3() {
    a3_oracle_writes_hipp_reads();
}
#[test]
fn a4() {
    a4_root_split();
}
#[test]
fn a5() {
    a5_overflow_both_directions();
}
#[test]
fn a7() {
    a7_rejections();
}
