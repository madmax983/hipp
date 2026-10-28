//! hipp-cli v0.10 differentials: NOT NULL enforcement on the
//! successful paths (OR IGNORE skips, OR REPLACE default
//! substitution, upsert proposal resolution) must produce
//! byte-identical output through the hipp CLI and the pinned
//! oracle CLI, and hipp's `.dump` after v0.10 writes must
//! replay into the oracle cleanly. (Error statements are
//! covered by the core differentials in
//! `tests/not_null_v10.rs`; the two CLIs format errors
//! differently by design.)

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v10-{tag}-{}", std::process::id()));
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

const CORPUS: &str = "\
CREATE TABLE t (a INTEGER UNIQUE, b TEXT NOT NULL DEFAULT 'D');
INSERT INTO t VALUES (1,'one'),(2,'two');
INSERT OR IGNORE INTO t VALUES (3,NULL),(4,'four');
INSERT OR REPLACE INTO t VALUES (5,NULL);
REPLACE INTO t VALUES (6,NULL);
UPDATE OR IGNORE t SET b = NULL WHERE a = 1;
UPDATE OR REPLACE t SET b = NULL WHERE a = 2;
INSERT OR REPLACE INTO t VALUES (1, NULL) ON CONFLICT(a) DO UPDATE SET b = 'z';
INSERT OR IGNORE INTO t VALUES (1, NULL) ON CONFLICT(a) DO UPDATE SET b = 'w';
SELECT rowid, * FROM t ORDER BY rowid;
SELECT b, typeof(b) FROM t ORDER BY a;
CREATE TABLE n (id INTEGER PRIMARY KEY NOT NULL, v INT NOT NULL DEFAULT '7');
INSERT INTO n VALUES (NULL, 5) ON CONFLICT(id) DO NOTHING;
INSERT OR REPLACE INTO n VALUES (NULL, NULL);
SELECT rowid, *, typeof(v) FROM n;
";

#[test]
fn v10_corpus_byte_identical() {
    let a = temp_db("oneshot-a");
    let b = temp_db("oneshot-b");
    assert_eq!(
        run(&hipp_bin(), &a, CORPUS),
        run(&oracle(), &b, CORPUS),
        "one-shot corpus diverged"
    );
    let a2 = temp_db("stdin-a");
    let b2 = temp_db("stdin-b");
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, CORPUS),
        run_stdin(&oracle(), &b2, CORPUS),
        "stdin corpus diverged"
    );
}

#[test]
fn dump_after_v10_writes_replays() {
    let a = temp_db("dump-a");
    let _ = run(&hipp_bin(), &a, CORPUS);
    let dump = run_stdin(&hipp_bin(), &a, ".dump\n");
    let fresh = temp_db("dump-fresh");
    let replayed = run(&oracle(), &fresh, &dump);
    let _ = replayed;
    // The replayed file must answer the same query the same
    // way through both CLIs.
    let q = "SELECT rowid, * FROM t ORDER BY rowid;";
    assert_eq!(run(&hipp_bin(), &a, q), run(&oracle(), &fresh, q));
}
