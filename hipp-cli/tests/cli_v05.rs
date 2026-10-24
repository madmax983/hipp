//! hipp-cli v0.5 differentials: UPDATE / DELETE / DROP TABLE
//! scripts must produce byte-identical output through the hipp
//! CLI and the pinned oracle CLI, and hipp's `.dump` after the
//! write-side statements must replay into the oracle cleanly.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v05-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
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
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

#[test]
fn write_side_corpus_byte_identical() {
    let script = "\
CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER);
INSERT INTO t VALUES (1, 'one', 10), (2, 'two', 20), (3, 'three', 30);
CREATE UNIQUE INDEX ux_a ON t(a);
CREATE INDEX ix_b ON t(b);
UPDATE t SET b = b + 5 WHERE id <= 2;
SELECT * FROM t ORDER BY id;
UPDATE t SET a = upper(a) WHERE b >= 30;
SELECT * FROM t WHERE a = 'THREE';
DELETE FROM t WHERE id = 2;
SELECT * FROM t ORDER BY id;
UPDATE t SET b = 99;
SELECT count(*) FROM t;
DROP TABLE t;
SELECT name FROM sqlite_schema;
CREATE TABLE t (x TEXT);
INSERT INTO t VALUES ('back');
SELECT * FROM t;
";
    let hdb = temp_db("corpus-h");
    let odb = temp_db("corpus-o");
    let h = run_stdin(&hipp_bin(), &hdb, script);
    let o = run_stdin(&oracle(), &odb, script);
    assert_eq!(h, o, "hipp vs oracle stdout");
    assert_eq!(
        run(&oracle(), &hdb, "PRAGMA integrity_check;"),
        "ok\n",
        "oracle integrity on hipp output"
    );
}

#[test]
fn dump_after_write_side_replays_into_oracle() {
    let hdb = temp_db("dump-h");
    let setup = "\
CREATE TABLE keep (id INTEGER PRIMARY KEY, v TEXT);
CREATE TABLE gone (id INTEGER PRIMARY KEY, w TEXT);
INSERT INTO keep VALUES (1, 'a'), (2, 'b'), (3, 'c');
INSERT INTO gone VALUES (1, 'x'), (2, 'y');
CREATE INDEX ix_keep_v ON keep(v);
UPDATE keep SET v = 'z' WHERE id = 3;
DELETE FROM keep WHERE id = 1;
DROP TABLE gone;
";
    run_stdin(&hipp_bin(), &hdb, setup);
    // .dump and .schema agree with the oracle's view of the file.
    let dump = run_stdin(&hipp_bin(), &hdb, ".dump");
    let schema = run_stdin(&hipp_bin(), &hdb, ".schema");
    assert_eq!(schema, run(&oracle(), &hdb, ".schema"), "schema view");
    assert!(dump.contains("INSERT INTO keep VALUES(2,'b');"), "{dump}");
    assert!(dump.contains("INSERT INTO keep VALUES(3,'z');"), "{dump}");
    assert!(!dump.contains("gone"), "{dump}");
    // The dump replays into a fresh oracle database and reads
    // back the same rows.
    let odb = temp_db("dump-o");
    let _ = std::fs::remove_file(&odb);
    run_stdin(&oracle(), &odb, &dump);
    assert_eq!(
        run(&oracle(), &odb, "SELECT * FROM keep ORDER BY id;"),
        "2|b\n3|z\n"
    );
    assert_eq!(run(&oracle(), &odb, "PRAGMA integrity_check;"), "ok\n");
}
