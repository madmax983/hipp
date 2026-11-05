//! hipp-cli v0.17 FTS5 delete writes (leaf `fts5_delete`):
//! byte-for-byte the pinned oracle's stdout for `contentless_delete=1`
//! tables (DELETE / UPDATE / INSERT OR REPLACE writing tombstones, the
//! queries and fts5vocab rows that skip them, the shadow tables as
//! hex) and `secure-delete=1` tables (deletes removing entries in
//! place, the `version` and cookie, highlight / snippet / bm25 after
//! them), with the pin's error lines on the refused UPDATEs.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17fd-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
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
    let out = child.wait_with_output().expect("cli finishes");
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// `script` on fresh twin files: identical stdout.
fn same(tag: &str, script: &str) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let a = run_stdin(&oracle(), &o, script);
    let b = run_stdin(&hipp_bin(), &h, script);
    assert_eq!(b, a, "script:\n{script}");
}

const TOMBSTONES: &str = "\
CREATE VIRTUAL TABLE notes USING fts5(title, body, tag UNINDEXED, content='', contentless_delete=1);
INSERT INTO notes(notes, rank) VALUES('pgsz', 64);
INSERT INTO notes(rowid, title, body, tag) SELECT value, 'note ' || value, 'w' || (value % 4) || ' shared text', 'x' FROM generate_series(1, 40);
DELETE FROM notes WHERE rowid IN (3, 7, 11);
DELETE FROM notes WHERE rowid > 30;
SELECT count(*), group_concat(rowid) FROM notes WHERE notes MATCH 'w3';
SELECT rowid, bm25(notes) FROM notes WHERE notes MATCH 'shared' ORDER BY rank LIMIT 4;
UPDATE notes SET title = 'renamed', body = 'fresh words' WHERE rowid = 5;
UPDATE notes SET tag = 'only the unindexed column' WHERE rowid = 6;
SELECT changes();
UPDATE notes SET title = 'half' WHERE rowid = 8;
INSERT OR REPLACE INTO notes(rowid, title, body) VALUES(9, 'replaced', 'w9'), (5000000000, 'far', 'away');
DELETE FROM notes WHERE rowid = 5000000000;
SELECT rowid FROM notes WHERE notes MATCH 'renamed OR replaced OR far OR note';
CREATE VIRTUAL TABLE nv USING fts5vocab(notes, row);
SELECT * FROM nv WHERE term IN ('fresh', 'shared', 'w1', 'renamed');
SELECT id, hex(block) FROM notes_data WHERE id = 10 OR id > 9000000000000000;
SELECT id, hex(sz), origin FROM notes_docsize WHERE id < 6;
INSERT INTO notes(notes) VALUES('optimize');
SELECT id, hex(block) FROM notes_data WHERE id = 10;
INSERT INTO notes(notes) VALUES('integrity-check');
";

const SECURE: &str = "\
CREATE VIRTUAL TABLE docs USING fts5(title, body, prefix='2');
INSERT INTO docs(docs, rank) VALUES('pgsz', 40);
INSERT INTO docs(docs, rank) VALUES('secure-delete', 1);
INSERT INTO docs SELECT 'Doc ' || value, 'alpha beta w' || (value % 5) || '. Second sentence here.' FROM generate_series(1, 30);
SELECT k, v FROM docs_config;
DELETE FROM docs WHERE rowid % 3 = 0;
SELECT k, v FROM docs_config;
SELECT hex(block) FROM docs_data WHERE id = 10;
UPDATE docs SET body = 'gamma delta. Replaced text' WHERE rowid = 4;
INSERT OR REPLACE INTO docs(rowid, title, body) VALUES(5, 'Five', 'alpha only');
SELECT rowid, highlight(docs, 1, '[', ']') FROM docs WHERE docs MATCH 'gamma OR only';
SELECT rowid, snippet(docs, 1, '<', '>', '...', 3) FROM docs WHERE docs MATCH 'w1' ORDER BY rank;
SELECT rowid, round(bm25(docs), 6) FROM docs WHERE docs MATCH 'al*' ORDER BY rowid DESC LIMIT 5;
DELETE FROM docs WHERE rowid BETWEEN 10 AND 25;
SELECT segid, hex(term), pgno FROM docs_idx;
SELECT count(*) FROM docs WHERE docs MATCH 'alpha';
INSERT INTO docs(docs) VALUES('integrity-check');
";

#[test]
fn contentless_delete_output() {
    same("tomb", TOMBSTONES);
}

#[test]
fn secure_delete_output() {
    same("secure", SECURE);
}

#[test]
fn delete_errors_and_contentless_updates() {
    same(
        "errors",
        "\
CREATE VIRTUAL TABLE c USING fts5(a, b UNINDEXED, content='');
INSERT INTO c(rowid, a, b) VALUES(1, 'x', 'y');
UPDATE c SET b = 'z' WHERE rowid = 1;
SELECT 'after', changes();
UPDATE c SET a = 'z' WHERE rowid = 1;
SELECT 'after2';
CREATE VIRTUAL TABLE d USING fts5(a, b, content='', contentless_delete=1);
INSERT INTO d VALUES('p q', 'r');
UPDATE d SET a = 'only a' WHERE rowid = 1;
UPDATE d SET rowid = 2 WHERE rowid = 1;
INSERT INTO d(d, rowid, a, b) VALUES('delete', 1, 'p q', 'r');
UPDATE d SET rowid = NULL, a = 's', b = 't' WHERE rowid = 1;
SELECT rowid FROM d('p');
",
    );
}
