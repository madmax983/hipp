//! hipp-cli v0.16 foreign keys (leaf feature `foreign_keys`):
//! byte-identical stdout against the pinned oracle (SQLite 3.54.1)
//! for scripts that enforce foreign keys — the rows a cascade leaves,
//! `PRAGMA foreign_key_check` / `foreign_key_list` output, the
//! COMMIT that deferred violations refuse (the transaction staying
//! open), RETURNING rows before a deferred failure, and a `.dump` of
//! a file with keys replayed under `PRAGMA foreign_keys = ON`. Errors
//! are compared by count only: hipp's messages are not the pin's.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16fk-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("t.db")
}

/// Run `input` over stdin against `db`; (stdout, error lines).
fn run_stdin(bin: &Path, db: &Path, input: &str) -> (String, usize) {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    let err = String::from_utf8_lossy(&out.stderr);
    let errors = err
        .lines()
        .filter(|l| l.starts_with("Error") || l.starts_with("Parse error"))
        .count();
    (String::from_utf8(out.stdout).expect("utf8 stdout"), errors)
}

/// Run one-shot SQL against `db`: (stdout, succeeded).
fn run_arg(bin: &Path, db: &Path, sql: &str) -> (String, bool) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        out.status.success(),
    )
}

/// Both engines over stdin on fresh files: same stdout and errors.
fn same_stdin(tag: &str, input: &str) -> String {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let hr = run_stdin(&hipp_bin(), &h, input);
    let or = run_stdin(&oracle(), &o, input);
    assert_eq!(hr, or, "{tag}:\n{input}");
    hr.0
}

#[test]
fn cascades_and_checks_byte_identical() {
    let out = same_stdin(
        "cascade",
        "PRAGMA foreign_keys = ON;
CREATE TABLE artist(id INTEGER PRIMARY KEY, name TEXT UNIQUE COLLATE NOCASE);
CREATE TABLE album(id INTEGER PRIMARY KEY, artist REFERENCES artist ON DELETE CASCADE ON UPDATE CASCADE,
  title TEXT, by_name TEXT REFERENCES artist(name) ON UPDATE CASCADE ON DELETE SET NULL);
CREATE TABLE track(album REFERENCES album ON DELETE CASCADE, n INTEGER, title TEXT DEFAULT 'untitled',
  FOREIGN KEY (title) REFERENCES album(title) DEFERRABLE INITIALLY DEFERRED);
CREATE UNIQUE INDEX album_title ON album(title);
INSERT INTO artist VALUES (1, 'Ada'), (2, 'Brook'), (3, 'Cy');
INSERT INTO album VALUES (10, 1, 'First', 'ada'), (11, 1, 'Second', 'ADA'), (20, 2, 'Third', 'Brook');
INSERT INTO track VALUES (10, 1, 'First'), (10, 2, 'First'), (11, 1, 'Second'), (20, 1, 'Third');
INSERT INTO track VALUES (99, 1, 'First');
UPDATE artist SET id = 100, name = 'ADA' WHERE id = 1;
SELECT * FROM album;
UPDATE artist SET name = 'Ada Lovelace' WHERE id = 100;
SELECT * FROM album;
DELETE FROM artist WHERE id = 100;
SELECT changes(), total_changes();
SELECT * FROM album;
SELECT * FROM track;
PRAGMA foreign_key_list(track);
PRAGMA foreign_key_list(album);
PRAGMA foreign_key_check;
PRAGMA foreign_key_check(track);
BEGIN;
INSERT INTO track VALUES (20, 2, 'Fourth');
COMMIT;
SELECT 'still open', count(*) FROM track;
UPDATE track SET title = 'Third' WHERE n = 2;
COMMIT;
SELECT * FROM track;
",
    );
    // NOCASE 'Ada' IS 'ADA': no cascade for that update.
    assert!(out.contains("10|100|First|ada"), "{out}");
}

#[test]
fn foreign_key_check_output_byte_identical() {
    same_stdin(
        "check",
        "CREATE TABLE p(a INTEGER PRIMARY KEY, b UNIQUE);
CREATE TABLE c(x REFERENCES P, y REFERENCES p(b), z REFERENCES gone);
INSERT INTO p VALUES (1, 'one');
INSERT INTO c VALUES (1, 'one', NULL), (2, 'one', 3), (1, 'two', 4), (NULL, 'zz', NULL);
PRAGMA foreign_key_check;
PRAGMA foreign_key_check(c);
PRAGMA foreign_key_check(p);
CREATE TABLE q(k TEXT PRIMARY KEY);
CREATE TABLE r(x INTEGER REFERENCES q);
INSERT INTO q VALUES ('1');
INSERT INTO r VALUES (1), ('1'), (1.0), (2);
PRAGMA foreign_key_check(r);
PRAGMA foreign_keys = ON;
DELETE FROM r WHERE x = 2;
DELETE FROM q;
PRAGMA foreign_key_check(r);
",
    );
}

#[test]
fn deferred_and_returning_byte_identical() {
    same_stdin(
        "deferred",
        "PRAGMA foreign_keys = ON;
CREATE TABLE p(a INTEGER PRIMARY KEY);
CREATE TABLE c(x REFERENCES p DEFERRABLE INITIALLY DEFERRED, v);
INSERT INTO c VALUES (1, 'a') RETURNING *;
SELECT count(*) FROM c;
BEGIN;
INSERT INTO c VALUES (1, 'a'), (2, 'b') RETURNING v, x;
SAVEPOINT s;
INSERT INTO p VALUES (1);
RELEASE s;
COMMIT;
INSERT INTO p VALUES (2);
COMMIT;
SELECT * FROM c;
PRAGMA defer_foreign_keys = ON;
DELETE FROM p WHERE a = 2 RETURNING a;
SELECT * FROM p;
CREATE TABLE d(x REFERENCES p);
INSERT INTO d VALUES (5), (1) RETURNING *;
SELECT count(*) FROM d;
",
    );
}

#[test]
fn dump_replays_with_keys_on() {
    // A file with keys, dumped by each engine, replays in each and
    // is then checked and written under `foreign_keys = ON` (the dump
    // writes the tables in schema order).
    let setup = "CREATE TABLE p(a INTEGER PRIMARY KEY, b TEXT UNIQUE);
CREATE TABLE c(x REFERENCES p ON DELETE CASCADE, y REFERENCES p(b));
INSERT INTO p VALUES (1, 'x'), (2, 'y');
INSERT INTO c VALUES (1, 'y'), (2, NULL), (3, 'x');";
    let h = temp_db("dump-h");
    let o = temp_db("dump-o");
    assert!(run_arg(&hipp_bin(), &h, setup).1);
    assert!(run_arg(&oracle(), &o, setup).1);
    let (hd, ok_h) = run_arg(&hipp_bin(), &h, ".dump");
    let (od, ok_o) = run_arg(&oracle(), &o, ".dump");
    assert!(ok_h && ok_o);
    // The pin's dump starts by turning the keys off; hipp's does not
    // write that line (a fresh connection has them off).
    assert_eq!(hd, od.trim_start_matches("PRAGMA foreign_keys=OFF;\n"));
    let script = format!(
        "{hd}PRAGMA foreign_keys = ON;\nPRAGMA foreign_key_check;\nDELETE FROM p WHERE a = 1;\nSELECT * FROM c;\n"
    );
    let h2 = temp_db("dump-h2");
    let o2 = temp_db("dump-o2");
    let hr = run_stdin(&hipp_bin(), &h2, &script);
    let or = run_stdin(&oracle(), &o2, &script);
    assert_eq!(hr, or);
    // Row 3's x (fkid 1) has no parent; deleting p 1 fails (c 3 still
    // names its 'x').
    assert_eq!((hr.0.as_str(), hr.1), ("c|3|p|1\n1|y\n2|\n3|x\n", 1));
}

#[test]
fn one_shot_errors_and_exit_status() {
    let h = temp_db("oneshot-h");
    let o = temp_db("oneshot-o");
    let setup = "CREATE TABLE p(a INTEGER PRIMARY KEY); CREATE TABLE c(x REFERENCES p);";
    assert!(run_arg(&hipp_bin(), &h, setup).1);
    assert!(run_arg(&oracle(), &o, setup).1);
    for (sql, ok) in [
        ("PRAGMA foreign_keys = ON; INSERT INTO c VALUES (1);", false),
        (
            "PRAGMA foreign_keys = ON; INSERT INTO c VALUES (NULL);",
            true,
        ),
        ("INSERT INTO c VALUES (7);", true),
        ("PRAGMA foreign_keys = ON; SELECT count(*) FROM c;", true),
        ("PRAGMA foreign_key_check;", true),
        (
            "PRAGMA foreign_keys = ON; DELETE FROM c WHERE x = 7; SELECT count(*) FROM c;",
            true,
        ),
    ] {
        let hr = run_arg(&hipp_bin(), &h, sql);
        let or = run_arg(&oracle(), &o, sql);
        assert_eq!(hr, or, "{sql}");
        assert_eq!(hr.1, ok, "{sql}");
    }
}

/// With `triggers`, a cascade fires the child's triggers in the pin's
/// order.
#[test]
fn cascade_fires_triggers_byte_identical() {
    same_stdin(
        "triggers",
        "PRAGMA foreign_keys = ON;
CREATE TABLE log(n INTEGER PRIMARY KEY, m);
CREATE TABLE p(a INTEGER PRIMARY KEY);
CREATE TABLE c(x REFERENCES p ON DELETE CASCADE ON UPDATE SET NULL, v);
CREATE TRIGGER cb BEFORE DELETE ON c BEGIN
  INSERT INTO log(m) VALUES ('before delete ' || old.v || ' changes=' || changes());
END;
CREATE TRIGGER ca AFTER UPDATE ON c BEGIN
  INSERT INTO log(m) VALUES ('after update ' || quote(old.x) || '->' || quote(new.x));
END;
CREATE TRIGGER pa AFTER DELETE ON p BEGIN
  INSERT INTO log(m) VALUES ('parent gone ' || old.a);
END;
INSERT INTO p VALUES (1), (2);
INSERT INTO c VALUES (1, 'one'), (1, 'uno'), (2, 'two');
DELETE FROM p WHERE a = 1;
UPDATE p SET a = 3 WHERE a = 2;
SELECT * FROM log;
SELECT * FROM c;
",
    );
}

/// Review fixes: writes made by INSTEAD OF trigger bodies are
/// checked (the view's statement fails, its RETURNING rows withheld
/// on an immediate key), the rowid conflict wins over the key in a
/// single-row INSERT OR ROLLBACK (the transaction ends), ADD COLUMN
/// of a REFERENCES column with a default on an empty table, and a
/// cascade from a trigger deleting the row an upsert updates.
#[test]
fn review_fixes_byte_identical() {
    same_stdin(
        "review",
        "PRAGMA foreign_keys = ON;
CREATE TABLE p(a PRIMARY KEY, b);
CREATE TABLE c(id INTEGER PRIMARY KEY, x REFERENCES p ON DELETE CASCADE);
CREATE VIEW v AS SELECT * FROM c;
CREATE TRIGGER tv INSTEAD OF INSERT ON v BEGIN INSERT INTO c VALUES (new.id, new.x); END;
INSERT INTO v VALUES (1, 5) RETURNING id;
SELECT count(*) FROM c;
PRAGMA foreign_key_check;
INSERT INTO p VALUES (1, 0);
INSERT INTO v VALUES (1, 1) RETURNING id, x;
BEGIN;
INSERT INTO p VALUES (2, 0);
INSERT OR ROLLBACK INTO c VALUES (1, 9);
INSERT INTO p VALUES (3, 0);
COMMIT;
SELECT a FROM p;
CREATE TABLE e(x REFERENCES p);
ALTER TABLE e ADD COLUMN z REFERENCES p DEFAULT 1;
SELECT sql FROM sqlite_schema WHERE name = 'e';
CREATE TRIGGER tc BEFORE UPDATE ON c BEGIN DELETE FROM p WHERE a = old.x; END;
INSERT INTO c VALUES (1, 3) ON CONFLICT (id) DO UPDATE SET x = 3;
SELECT * FROM c;
SELECT a FROM p;
",
    );
}
