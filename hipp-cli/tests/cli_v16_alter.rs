//! hipp-cli v0.16 differentials: ALTER TABLE RENAME TO / RENAME
//! COLUMN / DROP COLUMN, AUTOINCREMENT and TEMP objects must print
//! byte-identically to the pinned oracle — the stored schema text
//! after each ALTER (`.schema` and `sqlite_schema`), rows rewritten
//! by DROP COLUMN, the `sqlite_sequence` counter, and the temp
//! schema (`.schema` names temp objects `temp.x`, as the oracle
//! shell does). Each batch runs one-shot and over stdin (the shell
//! continues past errors there); hipp-written files must pass the
//! oracle's integrity check.

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
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16al-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> (bool, Vec<u8>) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (out.status.success(), out.stdout)
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("cli finishes").stdout
}

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let (hok, got) = run(&hipp_bin(), &a, batch);
    let (ook, want) = run(&oracle(), &b, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "{tag}"
    );
    assert_eq!(hok, ook, "{tag}");
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    if hok {
        assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;").1, b"ok\n");
    }
}

const ALTER: &str = "\
CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT DEFAULT 'd', b REAL UNIQUE, c AS (a || b), CHECK (t.b > 0));
CREATE INDEX ta ON t(a COLLATE NOCASE DESC, b);
CREATE VIEW v AS SELECT t.a, b, rowid FROM t WHERE b > 1 ORDER BY a;
INSERT INTO t(a, b) VALUES ('x', 1.5), ('y', 2.5), (NULL, 3);
ALTER TABLE t ADD COLUMN e DEFAULT 7;
ALTER TABLE t RENAME COLUMN a TO \"first name\";
ALTER TABLE t RENAME b TO bb;
ALTER TABLE t RENAME TO \"T 2\";
SELECT * FROM v;
ALTER TABLE \"T 2\" DROP COLUMN e;
ALTER TABLE \"T 2\" DROP COLUMN bb;
ALTER TABLE \"T 2\" DROP COLUMN id;
SELECT * FROM \"T 2\";
SELECT type, name, tbl_name, sql FROM sqlite_schema;
ALTER TABLE v RENAME TO w;
ALTER TABLE \"T 2\" RENAME TO sqlite_x;
";

const AUTOINC: &str = "\
CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b UNIQUE);
INSERT INTO t(b) VALUES ('x'), ('y');
DELETE FROM t WHERE b = 'y';
INSERT INTO t(b) VALUES ('z');
INSERT OR IGNORE INTO t(b) VALUES ('z');
INSERT INTO t(b) VALUES ('z') ON CONFLICT(b) DO UPDATE SET b = 'zz';
SELECT * FROM t;
SELECT rowid, * FROM sqlite_sequence;
UPDATE sqlite_sequence SET seq = 100;
INSERT INTO t(b) VALUES ('w') RETURNING *;
ALTER TABLE t RENAME TO u;
SELECT * FROM sqlite_sequence;
DROP TABLE u;
SELECT count(*) FROM sqlite_sequence;
CREATE TABLE f(a INTEGER PRIMARY KEY AUTOINCREMENT);
INSERT INTO f VALUES (9223372036854775807);
INSERT INTO f DEFAULT VALUES;
SELECT * FROM f;
";

const TEMP: &str = "\
CREATE TABLE m(a, b);
INSERT INTO m VALUES (1, 'main');
CREATE TEMP TABLE m(a, b UNIQUE);
CREATE INDEX mi ON m(a);
CREATE TEMP VIEW tv AS SELECT b FROM m;
INSERT INTO m VALUES (2, 'temp');
SELECT * FROM m;
SELECT * FROM main.m;
SELECT * FROM temp.m;
SELECT * FROM tv;
SELECT type, name, tbl_name, rootpage, sql FROM sqlite_temp_master;
PRAGMA table_list;
BEGIN;
INSERT INTO m VALUES (3, 'gone');
ROLLBACK;
SELECT count(*) FROM temp.m;
DROP TABLE m;
SELECT * FROM m;
SELECT name FROM sqlite_temp_schema;
";

/// The review fixes: DROP COLUMN past commas in comments, a quoted
/// `sqlite_temp_schema`, `temp.a` as a column of a table called
/// `temp`, connection-wide pragmas on TEMP tables, and "database or
/// disk is full" ending the transaction.
const REVIEW: &str = "\
CREATE TABLE d(a, b, /* x,y */ c, -- ,
 e, /* , */ UNIQUE(a));
INSERT INTO d VALUES (1, 2, 3, 4);
ALTER TABLE d DROP COLUMN e;
ALTER TABLE d DROP COLUMN c;
SELECT * FROM d;
SELECT sql FROM sqlite_schema WHERE name = 'd';
CREATE TABLE temp(a);
INSERT INTO temp VALUES (5);
SELECT temp.a FROM temp;
CREATE TEMP TABLE tt(a);
INSERT INTO tt VALUES ('ABC');
SELECT name FROM \"sqlite_temp_schema\";
SELECT count(*) FROM [sqlite_temp_master];
PRAGMA case_sensitive_like = ON;
SELECT count(*) FROM tt WHERE a LIKE 'abc';
PRAGMA temp.case_sensitive_like = OFF;
SELECT count(*) FROM tt WHERE a LIKE 'abc';
CREATE TABLE f(id INTEGER PRIMARY KEY AUTOINCREMENT, b);
INSERT INTO f VALUES (9223372036854775807, 0);
BEGIN;
INSERT INTO d VALUES (8, 9);
INSERT INTO f(b) VALUES (1);
COMMIT;
SELECT * FROM d;
CREATE TEMP VIEW tv AS SELECT a, b FROM d -- a comment
;
SELECT * FROM tv;
SELECT sql FROM sqlite_temp_schema WHERE name = 'tv';
CREATE TEMP INDEX ti ON tt(a);
SELECT name FROM sqlite_temp_schema;
";

#[test]
fn review_fixes_byte_identical() {
    byte_identical("review", REVIEW);
}

#[test]
fn alter_table_byte_identical() {
    byte_identical("alter", ALTER);
}

#[test]
fn autoincrement_byte_identical() {
    byte_identical("autoinc", AUTOINC);
}

#[test]
fn temp_byte_identical() {
    byte_identical("temp", TEMP);
}

/// `.schema` after ALTER, with AUTOINCREMENT and with temp objects
/// (`temp.` names), over stdin (dot-commands are no one-shot SQL).
#[test]
fn schema_dot_command_byte_identical() {
    let batch = "\
CREATE TABLE p(id INTEGER PRIMARY KEY AUTOINCREMENT, a, b);
CREATE INDEX pa ON p(a);
CREATE VIEW pv AS SELECT a, b FROM p;
ALTER TABLE p RENAME COLUMN a TO aa;
ALTER TABLE p RENAME TO q;
ALTER TABLE q DROP COLUMN b;
CREATE TEMP TABLE x(a, b UNIQUE);
CREATE INDEX xi ON x(a);
CREATE TEMP VIEW xv AS SELECT a FROM x;
.schema
.schema x
.schema q
";
    let a = temp_db("schema-hipp");
    let b = temp_db("schema-ora");
    let got = run_stdin(&hipp_bin(), &a, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b, batch))
    );
    assert!(!got.is_empty());
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;").1, b"ok\n");
}

/// `.tables` lists TEMP objects as `temp.x` and leaves out the
/// `sqlite_*` tables, as the oracle shell does (its column layout
/// is its own: compared here as sorted name sets).
#[test]
fn tables_dot_command_names_temp_objects() {
    let batch = "\
CREATE TABLE m(id INTEGER PRIMARY KEY AUTOINCREMENT);
CREATE VIEW mv AS SELECT 1;
CREATE TEMP TABLE x(a INTEGER PRIMARY KEY AUTOINCREMENT);
CREATE TEMP VIEW xv AS SELECT a FROM x;
.tables
";
    let names = |out: Vec<u8>| {
        let mut v: Vec<String> = String::from_utf8_lossy(&out)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        v.sort();
        v
    };
    let got = names(run_stdin(&hipp_bin(), &temp_db("tables-hipp"), batch));
    let want = names(run_stdin(&oracle(), &temp_db("tables-ora"), batch));
    assert_eq!(got, want);
    assert_eq!(got, ["m", "mv", "temp.x", "temp.xv"]);
}

/// `.dump` of an AUTOINCREMENT database replays (into hipp and into
/// the oracle) with its counter: `sqlite_sequence` is written last as
/// `DELETE` + `INSERT`s, as the oracle shell writes it, not as a
/// CREATE of the reserved name.
#[test]
fn dump_keeps_the_autoincrement_counter() {
    let src = temp_db("dump-src");
    let (ok, _) = run(
        &hipp_bin(),
        &src,
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b); \
         INSERT INTO t(b) VALUES (1), (2), (3); DELETE FROM t WHERE a > 1;",
    );
    assert!(ok);
    let dump = String::from_utf8(run_stdin(&hipp_bin(), &src, ".dump\n")).expect("utf8");
    assert!(
        dump.contains("DELETE FROM sqlite_sequence;\nINSERT INTO sqlite_sequence VALUES('t',3);\n")
    );
    assert!(!dump.contains("CREATE TABLE sqlite_sequence"));
    let check = "INSERT INTO t(b) VALUES (9); SELECT * FROM t; SELECT * FROM sqlite_sequence;";
    let mut outs = Vec::new();
    for (tag, bin) in [("h", hipp_bin()), ("o", oracle())] {
        let db = temp_db(&format!("dump-replay-{tag}"));
        run_stdin(&bin, &db, &dump);
        outs.push(run(&bin, &db, check).1);
        assert_eq!(run(&oracle(), &db, "PRAGMA integrity_check;").1, b"ok\n");
    }
    assert_eq!(outs[0], b"1|1\n4|9\nt|4\n");
    assert_eq!(outs[0], outs[1]);
}
