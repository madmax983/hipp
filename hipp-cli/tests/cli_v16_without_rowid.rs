//! hipp-cli v0.16 differentials: WITHOUT ROWID tables (leaf feature
//! `without_rowid`) must render byte-identically to the pinned oracle
//! — the stored schema texts and `.schema`, PRAGMA table_info /
//! table_list / index_list / index_info / index_xinfo of the clustered
//! key and of secondary indexes, the rows every write path leaves, the
//! rows' order in scans (the pin's key order and its covering-index
//! choices), ALTER TABLE, VACUUM, and a generated corpus of random
//! WITHOUT ROWID schemas and writes. Each batch runs one-shot and
//! over stdin, and the hipp-written file must pass the oracle's
//! integrity check.

use std::path::{Path, PathBuf};
use std::process::Command;

fn hipp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hipp"))
}

fn oracle() -> PathBuf {
    if let Ok(p) = std::env::var("HIPP_ORACLE") {
        return PathBuf::from(p);
    }
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hipp-design/oracle/sqlite3");
    if pinned.exists() {
        return pinned;
    }
    PathBuf::from("/usr/bin/sqlite3")
}

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16wr-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> Vec<u8> {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run(&oracle(), &b, batch)),
        "{tag}"
    );
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a2, batch)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b2, batch)),
        "{tag} (stdin)"
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// Definitions, introspection and the schema as the shell shows it.
const SCHEMA: &str = r#"
CREATE TABLE t(a INT, b TEXT COLLATE NOCASE, c, d AS (a * 2), PRIMARY KEY(b DESC, a), UNIQUE(c)) WITHOUT ROWID;
CREATE INDEX ti ON t(c DESC, b);
CREATE TABLE "odd name"(k INTEGER PRIMARY KEY, v) without rowid;
CREATE TABLE s(x INT PRIMARY KEY, y TEXT) STRICT, WITHOUT ROWID;
INSERT INTO t(a, b, c) VALUES (1, 'x', 3), (2, 'Y', 1), (3, 'y', 2), (1, 'a', NULL);
INSERT INTO "odd name" VALUES ('k', 1), (2, 2.5);
INSERT INTO s VALUES ('7', 'q');
SELECT name, sql FROM sqlite_schema ORDER BY name;
PRAGMA table_info(t);
PRAGMA table_xinfo(t);
SELECT name, ncol, wr, strict FROM pragma_table_list WHERE schema = 'main' ORDER BY name;
PRAGMA index_list(t);
PRAGMA index_info(sqlite_autoindex_t_1);
PRAGMA index_xinfo(sqlite_autoindex_t_1);
PRAGMA index_xinfo(sqlite_autoindex_t_2);
PRAGMA index_xinfo(ti);
SELECT * FROM t;
SELECT * FROM "odd name";
SELECT x, typeof(x) FROM s;
PRAGMA integrity_check;
"#;

/// Every write path, and the order rows come back in.
const WRITES: &str = r#"
CREATE TABLE t(a PRIMARY KEY, b UNIQUE, c) WITHOUT ROWID;
INSERT INTO t VALUES (3, 30, 'c'), (1, 10, 'a'), (2, 20, 'b');
INSERT OR IGNORE INTO t VALUES (1, 99, 'dup'), (4, 10, 'dupb'), (5, 50, 'e');
REPLACE INTO t VALUES (1, 20, 'both');
INSERT INTO t VALUES (3, 0, 'x') ON CONFLICT(a) DO UPDATE SET c = c || excluded.c RETURNING *;
INSERT INTO t VALUES (9, 50, 'x') ON CONFLICT(b) DO UPDATE SET a = a + 100 RETURNING *;
SELECT * FROM t;
UPDATE t SET a = a + 10 RETURNING *;
UPDATE OR REPLACE t SET a = 13 WHERE a = 11 RETURNING a, b;
DELETE FROM t WHERE b > 25 RETURNING *;
SELECT * FROM t;
SELECT last_insert_rowid(), changes();
CREATE TABLE d(a, b, c, PRIMARY KEY(a DESC, b)) WITHOUT ROWID;
CREATE INDEX dc ON d(c);
INSERT INTO d VALUES (1, 1, 'z'), (2, 2, 'a'), (1, 3, 'm'), (3, 0, 'q');
SELECT * FROM d;
SELECT a, b FROM d;
SELECT c, a FROM d;
SELECT * FROM d WHERE a = 1;
SELECT * FROM d ORDER BY c DESC LIMIT 2;
SELECT a, group_concat(b) FROM d GROUP BY a;
ALTER TABLE d ADD COLUMN e DEFAULT 'new';
DROP INDEX dc;
ALTER TABLE d DROP COLUMN c;
ALTER TABLE d RENAME TO dd;
SELECT * FROM dd;
SELECT sql FROM sqlite_schema WHERE name = 'dd';
VACUUM;
SELECT * FROM dd;
PRAGMA integrity_check;
"#;

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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

fn val(r: &mut Rng) -> String {
    match r.below(10) {
        0 => "NULL".into(),
        1..=4 => format!("{}", r.below(12) as i64 - 3),
        5 => format!("{}.5", r.below(9)),
        6 => format!("x'0{}'", r.below(9)),
        _ => format!("'{}'", r.pick(&["a", "A", "b", "B ", "c", "10"])),
    }
}

/// A batch of random WITHOUT ROWID tables, writes and reads.
fn corpus(seed: u64, n: usize) -> String {
    let mut r = Rng(seed);
    let mut out = String::new();
    for t in 0..n {
        let cols = ["p", "q", "s"];
        let ty = |r: &mut Rng| r.pick(&["", " INT", " TEXT", " REAL", " BLOB"]).to_string();
        let coll = |r: &mut Rng| r.pick(&["", "", " COLLATE NOCASE"]).to_string();
        let (t1, t2, t3) = (ty(&mut r), ty(&mut r), ty(&mut r));
        let (c1, c2) = (coll(&mut r), coll(&mut r));
        let key = match r.below(4) {
            0 => format!("p{}", r.pick(&["", " DESC"])),
            1 => format!("q{}, p", r.pick(&["", " DESC"])),
            2 => format!("p, s{}", r.pick(&["", " DESC"])),
            _ => "s DESC, q".to_string(),
        };
        out.push_str(&format!(
            "CREATE TABLE w{t}(p{t1}{c1}, q{t2}{c2}, s{t3}, PRIMARY KEY({key})) WITHOUT ROWID;\n"
        ));
        if r.below(2) == 0 {
            out.push_str(&format!(
                "CREATE INDEX w{t}i ON w{t}({}{});\n",
                r.pick(&cols),
                r.pick(&["", " DESC"])
            ));
        }
        for _ in 0..8 {
            // REPLACE of a NULL key column is the NOT NULL error,
            // which would end the batch: those rows go through IGNORE.
            let v = [val(&mut r), val(&mut r), val(&mut r)];
            let verb = if v.iter().any(|x| x == "NULL") || r.below(3) == 0 {
                "IGNORE"
            } else {
                "REPLACE"
            };
            out.push_str(&format!(
                "INSERT OR {verb} INTO w{t} VALUES ({}, {}, {});\n",
                v[0], v[1], v[2]
            ));
        }
        out.push_str(&format!(
            "UPDATE OR IGNORE w{t} SET {} = {} WHERE {} > {};\n",
            r.pick(&cols),
            val(&mut r),
            r.pick(&cols),
            val(&mut r)
        ));
        out.push_str(&format!(
            "DELETE FROM w{t} WHERE {} = {};\n",
            r.pick(&cols),
            val(&mut r)
        ));
        out.push_str(&format!("SELECT * FROM w{t};\n"));
        out.push_str(&format!("SELECT {} FROM w{t};\n", r.pick(&cols)));
        out.push_str(&format!(
            "SELECT * FROM w{t} ORDER BY {} LIMIT 3;\n",
            r.pick(&cols)
        ));
    }
    out.push_str("PRAGMA integrity_check;\n");
    out
}

#[test]
fn cli_v16_without_rowid_schema_byte_identical() {
    byte_identical("schema", SCHEMA);
}

#[test]
fn cli_v16_without_rowid_writes_byte_identical() {
    byte_identical("writes", WRITES);
}

#[test]
fn cli_v16_without_rowid_corpus_byte_identical() {
    for (i, seed) in [
        0x9e37_79b9_7f4a_7c15u64,
        0x0123_4567_89ab_cdef,
        0xfeed_beef_0000_0001,
    ]
    .iter()
    .enumerate()
    {
        byte_identical(&format!("corpus{i}"), &corpus(*seed, 25));
    }
}
