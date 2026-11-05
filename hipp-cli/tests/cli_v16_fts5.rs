//! hipp-cli v0.16 FTS5 (leaf `fts5`): byte-for-byte the pinned
//! oracle's stdout for full-text tables -- MATCH queries, ORDER BY
//! rank, bm25() / highlight() / snippet() text (REAL rendering
//! included), fts5vocab tables, the special INSERT commands,
//! external-content tables kept by triggers, trigram LIKE / GLOB
//! patterns, error lines, and `.schema` listing the shadow tables
//! (list mode: the output modes are the shell's, tested elsewhere).

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16fts-{tag}"));
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

const BASIC: &str = "\
CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize='porter unicode61 remove_diacritics 2', prefix='2 3');
INSERT INTO docs VALUES('SQLite FTS5', 'Full-text search in SQLite is provided by the FTS5 extension. Searching is fast.');
INSERT INTO docs VALUES('Caf\u{e9} culture', 'The caf\u{e9} serves r\u{e9}sum\u{e9}-writing workshops. Caf\u{e9}s are searched for.');
INSERT INTO docs VALUES('Running', 'Runners run; they ran and are running. Search the runs!');
INSERT INTO docs(rowid, title, body) VALUES(10, NULL, 'search search search');
SELECT rowid, title FROM docs WHERE docs MATCH 'search' ORDER BY rank;
SELECT rowid, rank FROM docs WHERE docs MATCH 'cafe OR run*' ORDER BY rank;
SELECT rowid, bm25(docs, 10.0, 1.0) FROM docs('search NOT title:sqlite');
SELECT highlight(docs, 1, '[', ']') FROM docs WHERE docs MATCH '\"full text\" OR runner';
SELECT snippet(docs, -1, '<b>', '</b>', '...', 6) FROM docs WHERE docs = 'search' ORDER BY rowid DESC;
SELECT rowid FROM docs WHERE docs MATCH 'NEAR(search fast, 5)';
SELECT rowid FROM docs WHERE title MATCH '^caf*';
SELECT count(*) FROM docs WHERE docs MATCH '{title body}: sqlite';
SELECT rowid, title, rank FROM docs WHERE docs MATCH 'sqlite' ORDER BY rank LIMIT 1;
SELECT rowid, title FROM docs WHERE docs MATCH 'run' ORDER BY rowid;
.schema docs
";

#[test]
fn basic_queries_output() {
    same("basic", BASIC);
}

const VOCAB_COMMANDS: &str = "\
CREATE VIRTUAL TABLE t USING fts5(a, b, detail=column);
CREATE VIRTUAL TABLE vr USING fts5vocab(t, row);
CREATE VIRTUAL TABLE vc USING fts5vocab(t, col);
INSERT INTO t(t, rank) VALUES('pgsz', 64);
INSERT INTO t SELECT 'w' || (value % 9) || ' common', 'v' || (value % 4) FROM generate_series(1, 60);
SELECT * FROM vr WHERE cnt > 10;
SELECT * FROM vc WHERE term = 'common';
INSERT INTO t(t, rank) VALUES('merge', 100);
INSERT INTO t(t) VALUES('optimize');
SELECT id, length(block) FROM t_data ORDER BY id;
DELETE FROM t WHERE rowid % 2 = 0;
INSERT INTO t(t) VALUES('integrity-check');
INSERT INTO t(t, rank) VALUES('automerge', 0);
SELECT * FROM t_config;
INSERT INTO t(t) VALUES('rebuild');
SELECT term, doc FROM vr ORDER BY doc DESC, term LIMIT 4;
INSERT INTO t(t) VALUES('delete-all');
SELECT count(*) FROM vr;
SELECT hex(block) FROM t_data;
";

#[test]
fn vocab_and_commands_output() {
    same("vocab", VOCAB_COMMANDS);
}

const EXTERNAL: &str = "\
CREATE TABLE notes(id INTEGER PRIMARY KEY, txt);
CREATE VIRTUAL TABLE nf USING fts5(txt, content=notes, content_rowid=id, tokenize=trigram);
CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN INSERT INTO nf(rowid, txt) VALUES(new.id, new.txt); END;
CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN INSERT INTO nf(nf, rowid, txt) VALUES('delete', old.id, old.txt); END;
INSERT INTO notes(txt) VALUES('buy milk and bread'), ('call the plumber'), ('Milkshake recipe');
SELECT rowid, txt FROM nf WHERE nf MATCH 'milk' ORDER BY rowid;
SELECT rowid, highlight(nf, 0, '*', '*') FROM nf WHERE txt LIKE '%lumb%';
DELETE FROM notes WHERE id = 1;
BEGIN;
INSERT INTO notes(txt) VALUES('almond milk');
SELECT rowid FROM nf WHERE nf MATCH 'ilk';
ROLLBACK;
SELECT rowid FROM nf WHERE nf MATCH 'ilk';
INSERT INTO nf(nf) VALUES('integrity-check');
CREATE VIRTUAL TABLE c USING fts5(x, content='');
INSERT INTO c(rowid, x) VALUES(4, 'gone text');
SELECT rowid, x, bm25(c) FROM c WHERE c MATCH 'text';
.schema
";

#[test]
fn external_content_output() {
    same("external", EXTERNAL);
}

const ERRORS: &str = "\
CREATE VIRTUAL TABLE t USING fts5(a);
INSERT INTO t VALUES('x y');
SELECT * FROM t WHERE t MATCH 'x AND';
SELECT * FROM t WHERE t MATCH 'nocol: x';
CREATE VIRTUAL TABLE u USING fts5(a, tokenize='nosuch');
INSERT INTO t(t) VALUES('nosuch');
SELECT snippet(t) FROM t WHERE t MATCH 'x';
SELECT 'still running';
SELECT a FROM t WHERE t MATCH 'y';
";

#[test]
fn errors_keep_the_shell_going() {
    same("errors", ERRORS);
}
