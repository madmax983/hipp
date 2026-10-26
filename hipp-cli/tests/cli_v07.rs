//! hipp-cli v0.7 differentials: write-side completion scripts
//! (OR REPLACE, UPDATE ... FROM, RETURNING, UPSERT) must
//! produce byte-identical output through the hipp CLI and the
//! pinned oracle CLI — RETURNING rows are CLI output, so the
//! corpus exercises them directly — and hipp's `.dump` after
//! v0.7 writes must replay into the oracle cleanly.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v07-{tag}-{}", std::process::id()));
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
CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, v INTEGER);
CREATE TABLE f (id INTEGER PRIMARY KEY, w INTEGER);
INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'c',30);
INSERT INTO f VALUES (1,100),(3,300);
INSERT OR REPLACE INTO t VALUES (2,'b2',22) RETURNING *;
INSERT INTO t VALUES (4,'d',40) ON CONFLICT DO NOTHING RETURNING id, u;
INSERT INTO t VALUES (4,'d',44) ON CONFLICT(id) DO UPDATE SET v = excluded.v RETURNING id, v;
INSERT INTO t VALUES (9,'c',90) ON CONFLICT(u) DO UPDATE SET v = v + excluded.v RETURNING *;
UPDATE t SET v = f.w FROM f WHERE f.id = t.id RETURNING id, v;
UPDATE OR REPLACE t SET u = 'a' WHERE id = 3 RETURNING id, u;
DELETE FROM t WHERE id = 4 RETURNING id, u, v;
SELECT * FROM t ORDER BY id;
";

#[test]
fn v07_corpus_byte_identical() {
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
fn dump_after_v07_writes_replays() {
    let a = temp_db("dump-a");
    let out = run(&hipp_bin(), &a, CORPUS);
    let _ = out;
    let dump = run_stdin(&hipp_bin(), &a, ".dump\n");
    let fresh = temp_db("dump-fresh");
    let replayed = run(&oracle(), &fresh, &dump);
    let _ = replayed;
    let check = "SELECT * FROM t ORDER BY id; SELECT * FROM f ORDER BY id;";
    assert_eq!(run(&oracle(), &fresh, check), run(&oracle(), &a, check));
}
