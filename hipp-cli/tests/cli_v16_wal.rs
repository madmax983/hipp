//! hipp-cli v0.16 differentials: WAL journal mode (leaf feature
//! `wal`) must render byte-identically to the pinned oracle —
//! `PRAGMA journal_mode` / `synchronous` / `wal_autocheckpoint` /
//! `journal_size_limit` / `wal_checkpoint` rows, the data read
//! through the log, and the errors' effect on the batch (one-shot
//! mode stops at the first). Each batch runs one-shot and over stdin;
//! afterwards both CLIs have closed the database the same way (the
//! log checkpointed and removed, the file still marked WAL), the
//! oracle's integrity check passes on hipp's file, and each CLI reads
//! the other's file. A generated corpus covers mixed workloads.

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16wal-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn side(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// One-shot: (exit success, stdout).
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

/// The files a closed CLI leaves agree: no `-wal` / `-shm`, the same
/// file-format bytes (when the oracle wrote the file at all).
fn closed_alike(tag: &str, a: &Path, b: &Path) {
    for sfx in ["-wal", "-shm", "-journal"] {
        assert_eq!(
            side(a, sfx).exists(),
            side(b, sfx).exists(),
            "{tag}: {sfx} presence"
        );
    }
    let (ha, hb) = (
        std::fs::read(a).unwrap_or_default(),
        std::fs::read(b).unwrap_or_default(),
    );
    if !hb.is_empty() {
        assert_eq!(ha.get(18..20), hb.get(18..20), "{tag}: file-format bytes");
    }
}

/// One batch, byte-compared one-shot (with the exit status) and over
/// stdin; then the closed files, the oracle's integrity check on
/// hipp's file, and both CLIs reading the other's file with `check`.
fn byte_identical(tag: &str, setup: &str, batch: &str, check: &str) {
    byte_identical_in(tag, setup, batch, check, true);
}

/// `byte_identical`, over stdin too when `stdin` (the oracle's shell
/// skips the rest of an input line after an error, hipp's runs it: a
/// batch with errors is compared one-shot only).
fn byte_identical_in(tag: &str, setup: &str, batch: &str, check: &str, stdin: bool) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    if !setup.is_empty() {
        assert!(run(&oracle(), &a, setup).0);
        assert!(run(&oracle(), &b, setup).0);
    }
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(
        String::from_utf8_lossy(&got.1),
        String::from_utf8_lossy(&run(&oracle(), &b, batch).1),
        "{tag}"
    );
    closed_alike(tag, &a, &b);
    if !stdin {
        assert_eq!(
            run(&oracle(), &a, "PRAGMA integrity_check;").1,
            b"ok\n",
            "{tag}"
        );
        return;
    }
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    if !setup.is_empty() {
        assert!(run(&oracle(), &a2, setup).0);
        assert!(run(&oracle(), &b2, setup).0);
    }
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a2, batch)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b2, batch)),
        "{tag} (stdin)"
    );
    closed_alike(&format!("{tag} (stdin)"), &a2, &b2);
    for f in [&a, &a2] {
        if std::fs::metadata(f).is_ok_and(|m| m.len() > 0) {
            assert_eq!(
                run(&oracle(), f, "PRAGMA integrity_check;").1,
                b"ok\n",
                "{tag}"
            );
        }
    }
    if !check.is_empty() {
        assert_eq!(
            run(&hipp_bin(), &b2, check),
            run(&oracle(), &a2, check),
            "{tag}: each CLI on the other's file"
        );
    }
}

#[test]
fn pragma_rows() {
    for (i, (setup, batch)) in [
        (
            "",
            "PRAGMA journal_mode; PRAGMA journal_mode = WAL; PRAGMA journal_mode; PRAGMA journal_mode = w; PRAGMA journal_mode = bogus; PRAGMA synchronous; PRAGMA wal_autocheckpoint; PRAGMA journal_size_limit;",
        ),
        (
            "CREATE TABLE t(a);",
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); PRAGMA wal_checkpoint(NOOP); PRAGMA wal_checkpoint; INSERT INTO t VALUES (3); PRAGMA wal_checkpoint(TRUNCATE); PRAGMA wal_checkpoint; SELECT * FROM t; PRAGMA journal_mode;",
        ),
        (
            "CREATE TABLE t(a);",
            "PRAGMA wal_checkpoint; PRAGMA journal_mode = wal; PRAGMA journal_mode = delete; PRAGMA wal_checkpoint; PRAGMA journal_mode = WAL; INSERT INTO t VALUES (1); PRAGMA journal_mode = DELETE; SELECT * FROM t;",
        ),
        (
            "",
            "PRAGMA synchronous = OFF; PRAGMA synchronous; PRAGMA synchronous = NORMAL; PRAGMA synchronous; PRAGMA synchronous = EXTRA; PRAGMA synchronous; PRAGMA synchronous = 7; PRAGMA synchronous; PRAGMA wal_autocheckpoint = 17; PRAGMA wal_autocheckpoint = -4; PRAGMA journal_size_limit = 4096; PRAGMA journal_size_limit = -9; SELECT * FROM pragma_journal_size_limit;",
        ),
        (
            "CREATE TABLE t(a, b);",
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 2; INSERT INTO t VALUES (1, 1); PRAGMA wal_checkpoint(noop); INSERT INTO t VALUES (2, zeroblob(5000)); PRAGMA wal_checkpoint(noop); BEGIN; INSERT INTO t VALUES (3, 3); SAVEPOINT s; UPDATE t SET b = 9 WHERE a <> 2; ROLLBACK TO s; COMMIT; PRAGMA wal_checkpoint(noop); SELECT a, length(b) FROM t;",
        ),
    ]
    .iter()
    .enumerate()
    {
        byte_identical(
            &format!("pr{i}"),
            setup,
            batch,
            "PRAGMA journal_mode; SELECT name FROM sqlite_schema;",
        );
    }
}

/// One-shot mode stops at the first error, as the oracle's does.
#[test]
fn errors_stop_the_batch() {
    for (i, batch) in [
        "BEGIN; PRAGMA journal_mode = WAL; SELECT 'not reached';",
        "PRAGMA journal_mode = WAL; BEGIN; PRAGMA journal_mode = DELETE; SELECT 'not reached';",
        "BEGIN; PRAGMA synchronous = OFF; SELECT 'not reached';",
        "PRAGMA journal_mode = WAL; BEGIN; INSERT INTO t VALUES (1); PRAGMA wal_checkpoint; SELECT 'not reached';",
        "PRAGMA journal_mode = WAL; BEGIN; INSERT INTO t VALUES (1); PRAGMA journal_mode = DELETE; COMMIT; SELECT * FROM t; PRAGMA journal_mode;",
    ]
    .iter()
    .enumerate()
    {
        byte_identical_in(
            &format!("er{i}"),
            "CREATE TABLE t(a);",
            batch,
            "SELECT count(*) FROM t;",
            false,
        );
        let a = temp_db(&format!("er{i}-st-h"));
        let b = temp_db(&format!("er{i}-st-o"));
        assert!(run(&oracle(), &a, "CREATE TABLE t(a);").0);
        assert!(run(&oracle(), &b, "CREATE TABLE t(a);").0);
        assert_eq!(
            run(&hipp_bin(), &a, batch).0,
            run(&oracle(), &b, batch).0,
            "er{i}: exit status"
        );
    }
}

/// The CLI reads a log the oracle left behind (killed mid-session,
/// its `-shm` stale) and closes it as the oracle would.
#[test]
fn reads_the_oracles_log() {
    let o = temp_db("ol");
    run_stdin(
        &oracle(),
        &o,
        "PRAGMA journal_mode = WAL;\nPRAGMA wal_autocheckpoint = 0;\nCREATE TABLE t(a, b);\n\
         INSERT INTO t VALUES (1, 'one'), (2, zeroblob(7000));\nUPDATE t SET b = 'uno' WHERE a = 1;\n\
         .system kill -9 $PPID\n",
    );
    assert!(side(&o, "-wal").exists(), "the oracle left its log");
    let h = temp_db("ol-h");
    let p = temp_db("ol-p");
    for d in [&h, &p] {
        std::fs::copy(&o, d).expect("db");
        std::fs::copy(side(&o, "-wal"), side(d, "-wal")).expect("wal");
        std::fs::copy(side(&o, "-shm"), side(d, "-shm")).expect("shm");
    }
    let q = "PRAGMA journal_mode; SELECT a, length(b), typeof(b) FROM t; INSERT INTO t VALUES (3, 'three'); PRAGMA wal_checkpoint(noop); SELECT count(*) FROM t;";
    let got = run(&hipp_bin(), &h, q);
    assert!(got.0);
    assert_eq!(got.1, run(&oracle(), &p, q).1);
    closed_alike("ol", &h, &p);
    assert_eq!(run(&oracle(), &h, "PRAGMA integrity_check;").1, b"ok\n");
}

/// `.open` closes the previous database (its log checkpointed and
/// removed) as the oracle's does.
#[test]
fn dot_open_closes() {
    let a = temp_db("op-h");
    let b = temp_db("op-o");
    for (bin, db) in [(hipp_bin(), &a), (oracle(), &b)] {
        let other = side(db, "-other");
        let _ = std::fs::remove_file(&other);
        run_stdin(
            &bin,
            db,
            &format!(
                "PRAGMA journal_mode = WAL;\nCREATE TABLE t(a);\nINSERT INTO t VALUES (1);\n.open {}\nSELECT 1;\n",
                other.display()
            ),
        );
    }
    closed_alike("op", &a, &b);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Small transactions (no cache spills at the CLI's default cache)
/// with checkpoints and mode switches, every row compared byte for
/// byte.
#[test]
fn generated_corpus() {
    let n: usize = std::env::var("HIPP_WAL_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let mut rng = Rng(0x5eed_0016_0c11);
    for i in 0..n {
        let mut s = String::from("PRAGMA journal_mode = WAL; ");
        s.push_str(&format!(
            "PRAGMA wal_autocheckpoint = {}; ",
            [0, 1, 3, 1000][rng.below(4) as usize]
        ));
        for _ in 0..(4 + rng.below(14)) {
            let id = rng.below(30);
            let stmt = match rng.below(12) {
                0..=3 => format!(
                    "INSERT OR REPLACE INTO t VALUES ({id}, 'v{}');",
                    rng.below(1000)
                ),
                4 => format!(
                    "UPDATE t SET b = b || '+' WHERE a % {} = 0;",
                    2 + rng.below(4)
                ),
                5 => format!("DELETE FROM t WHERE a = {id};"),
                6 => format!(
                    "PRAGMA wal_checkpoint{};",
                    ["", "(FULL)", "(RESTART)", "(TRUNCATE)", "(NOOP)"][rng.below(5) as usize]
                ),
                7 => format!(
                    "BEGIN; INSERT OR REPLACE INTO t VALUES ({id}, 'tx'); {} ",
                    if rng.below(2) == 0 {
                        "COMMIT;"
                    } else {
                        "ROLLBACK;"
                    }
                ),
                8 => "PRAGMA journal_mode = DELETE; PRAGMA journal_mode = WAL;".to_string(),
                9 => format!("PRAGMA journal_size_limit = {};", rng.below(9000)),
                10 => "SELECT count(*), group_concat(a) FROM t;".to_string(),
                _ => "PRAGMA wal_checkpoint(NOOP);".to_string(),
            };
            s.push_str(&stmt);
            s.push(' ');
        }
        s.push_str("SELECT * FROM t ORDER BY a; PRAGMA wal_checkpoint(NOOP);");
        byte_identical(
            &format!("gen{i}"),
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b);",
            &s,
            "SELECT * FROM t ORDER BY a;",
        );
    }
}

/// `.open` of the WAL database already open (v0.16 review fix): the
/// oracle closes the old connection before opening the new one, so
/// the new one's log is the file on disk. hipp opened first, and the
/// old connection's close then checkpointed and unlinked the log the
/// new one went on appending to: killed, its committed rows were
/// gone. Both CLIs are killed after the second insert; the oracle
/// then reads rows 1 and 2 from each one's files.
#[test]
fn dot_open_same_wal_db_then_killed() {
    use std::io::{BufRead, Write};
    let h = temp_db("os-h");
    let o = temp_db("os-o");
    let script = |db: &Path| {
        format!(
            "PRAGMA journal_mode = WAL;\nCREATE TABLE t(a);\nINSERT INTO t VALUES (1);\n.open {}\nINSERT INTO t VALUES (2);\nSELECT 'done';\n",
            db.display()
        )
    };
    // hipp-cli has no `.system`: killed once it has printed `done`.
    let mut child = Command::new(hipp_bin())
        .arg(&h)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("cli spawns");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(script(&h).as_bytes()).expect("write stdin");
    let mut out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    let mut seen = String::new();
    while !seen.contains("done") {
        let mut line = String::new();
        if out.read_line(&mut line).expect("read stdout") == 0 {
            break;
        }
        seen.push_str(&line);
    }
    child.kill().expect("kill");
    let _ = child.wait();
    drop(stdin);
    assert_eq!(seen, "wal\ndone\n");
    run_stdin(
        &oracle(),
        &o,
        &format!("{}.system kill -9 $PPID\n", script(&o)),
    );
    for db in [&h, &o] {
        assert_eq!(
            run(
                &oracle(),
                db,
                "SELECT a FROM t ORDER BY a; PRAGMA integrity_check;"
            )
            .1,
            b"1\n2\nok\n",
            "{}",
            db.display()
        );
    }
    // Run to the end, the checkpoint rows agree too.
    let a = temp_db("os2-h");
    let b = temp_db("os2-o");
    let tail = "INSERT INTO t VALUES (3);\nPRAGMA wal_checkpoint;\nSELECT a FROM t;\n";
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(
            &hipp_bin(),
            &a,
            &format!("{}{tail}", script(&a))
        )),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b, &format!("{}{tail}", script(&b)))),
    );
    closed_alike("os2", &a, &b);
}

/// `.open` of a file that cannot be opened (v0.16): the oracle closes
/// the current database first and carries on with a substitute
/// in-memory one; so does hipp (it kept the old database).
#[test]
fn dot_open_failure_substitutes_memory() {
    let a = temp_db("of-h");
    let b = temp_db("of-o");
    let batch = "CREATE TABLE t(a);\nINSERT INTO t VALUES (1);\n.open /nonexistent-hipp-dir/x.db\nSELECT * FROM t;\nPRAGMA database_list;\nPRAGMA journal_mode;\nSELECT 1;\n";
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a, batch)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b, batch)),
    );
    closed_alike("of", &a, &b);
}

/// An in-memory database's journal mode is `memory` whatever is asked
/// (v0.16 review fix: hipp's `:memory:` reported `delete`, entered
/// WAL mode and ran checkpoints); `wal_checkpoint` is `0|-1|-1`, the
/// TEMP database's mode `delete`, the other WAL pragmas their
/// defaults.
#[test]
fn memory_database_stays_memory() {
    let q = "PRAGMA journal_mode; PRAGMA journal_mode = WAL; PRAGMA journal_mode = DELETE; PRAGMA journal_mode = MEMORY; PRAGMA journal_mode = OFF; PRAGMA journal_mode = TRUNCATE; PRAGMA journal_mode = PERSIST; PRAGMA journal_mode = bogus; PRAGMA main.journal_mode; CREATE TABLE t(a); INSERT INTO t VALUES (1); PRAGMA wal_checkpoint; PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode; PRAGMA temp.journal_mode; PRAGMA wal_autocheckpoint; PRAGMA journal_size_limit; PRAGMA synchronous; SELECT * FROM pragma_journal_mode; BEGIN; INSERT INTO t VALUES (2); PRAGMA journal_mode = WAL; COMMIT; SELECT * FROM t;";
    let mem = Path::new(":memory:");
    let got = run(&hipp_bin(), mem, q);
    let want = run(&oracle(), mem, q);
    assert_eq!(got.0, want.0);
    assert_eq!(
        String::from_utf8_lossy(&got.1),
        String::from_utf8_lossy(&want.1)
    );
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), mem, &q.replace("; ", ";\n"))),
        String::from_utf8_lossy(&run_stdin(&oracle(), mem, &q.replace("; ", ";\n"))),
    );
}

/// The pages an UPDATE leaves clean (v0.16 review fix): an index no
/// assigned column reaches was rewritten unchanged, and so was a row
/// whose record did not change, each a frame too many in
/// `wal_checkpoint`'s `log|checkpointed`. Probed page by page on the
/// oracle: a plain index an assigned column (or a generated column
/// computed from one, a partial index's WHERE, the rowid) reaches is
/// rewritten even when its entry is unchanged; one with an expression
/// or VIRTUAL generated key part only when its entry changes; the row
/// only when its record changes or its key is assigned; a record of
/// the same size is overwritten in place, its unchanged overflow
/// pages not written.
#[test]
fn update_frames_match() {
    let base = "CREATE TABLE t(a, b, c); INSERT INTO t VALUES (1, 'x', 3), (2, NULL, 4);";
    let mut cases: Vec<(String, String)> = Vec::new();
    for ix in [
        "t(a)",
        "t(a+0)",
        "t(a, b)",
        "t(b, a)",
        "t(c) WHERE a > 0",
        "t(c) WHERE a > 5",
        "t(lower(a))",
        "t(a, b || '')",
        "t(a) WHERE c > 0",
        "t(a+0) WHERE a > 0",
        "t(c+0) WHERE a > 0",
    ] {
        for set in ["a = a", "a = 1", "a = 2", "c = c", "b = b", "b = 'y'"] {
            cases.push((
                format!("{base} CREATE INDEX i ON {ix};"),
                format!("UPDATE t SET {set} WHERE rowid = 1"),
            ));
        }
    }
    for g in ["VIRTUAL", "STORED"] {
        for set in ["a = a", "a = 2", "b = b"] {
            cases.push((
                format!("CREATE TABLE t(a, b, c AS (a + 1) {g}); CREATE INDEX i ON t(c); INSERT INTO t(a, b) VALUES (1, 'x'), (2, NULL);"),
                format!("UPDATE t SET {set} WHERE rowid = 1"),
            ));
        }
    }
    for ix in ["t(b)", "t(b+0)", "t(c, a)"] {
        for set in ["a = a", "a = 1", "b = b"] {
            cases.push((
                format!("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX i ON {ix}; INSERT INTO t VALUES (1, 'x', 3), (2, NULL, 4);"),
                format!("UPDATE t SET {set} WHERE a = 1"),
            ));
        }
    }
    for set in ["b = b", "b = 'y'", "a = a", "c = c"] {
        cases.push((
            "CREATE TABLE t(a PRIMARY KEY, b, c) WITHOUT ROWID; CREATE INDEX i ON t(c); INSERT INTO t VALUES (1, 'x', 3), (2, NULL, 4);".to_string(),
            format!("UPDATE t SET {set} WHERE a = 1"),
        ));
    }
    // A WITHOUT ROWID key that changes moves every entry (they end in
    // the key), one assigned unchanged leaves expression entries alone.
    for set in [
        "a = a + 10",
        "a = a",
        "b = b || '!'",
        "c = c",
        "c = 5",
        "a = 1, c = c",
    ] {
        cases.push((
            "CREATE TABLE t(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID; CREATE INDEX i ON t(c * 2) WHERE c > 0; CREATE INDEX j ON t(b || ''); CREATE INDEX k ON t(c); INSERT INTO t VALUES (1, 'x', 3), (2, 'y', 1);".to_string(),
            format!("UPDATE t SET {set} WHERE a = 1"),
        ));
    }
    for set in ["a = 'X'", "a = a", "a = 'x'"] {
        cases.push((
            "CREATE TABLE t(a TEXT COLLATE NOCASE PRIMARY KEY, b) WITHOUT ROWID; CREATE INDEX i ON t(b + 0); INSERT INTO t VALUES ('x', 1);".to_string(),
            format!("UPDATE t SET {set}"),
        ));
    }
    for (setup, up) in [
        (
            "CREATE TABLE t(a PRIMARY KEY, b); INSERT INTO t VALUES (1, 'x'), (2, NULL);",
            "UPDATE t SET b = 'y' WHERE a = 1",
        ),
        (
            "CREATE TABLE t(a, b); CREATE INDEX ia ON t(a); CREATE INDEX ib ON t(b); INSERT INTO t VALUES (1, 'x'), (2, NULL);",
            "UPDATE t SET b = 'y' WHERE rowid = 1",
        ),
        (
            "CREATE TABLE t(a, b); INSERT INTO t VALUES (1, 2); ALTER TABLE t ADD COLUMN d DEFAULT 5;",
            "UPDATE t SET a = 1",
        ),
        // (Not a REAL column's integral value: the oracle stores it
        // as an integer, hipp as an 8-byte real, so a row the oracle
        // wrote is a different record to hipp; STATUS.md gaps.)
        (
            "CREATE TABLE t(a, b); INSERT INTO t VALUES (zeroblob(5000), 'y');",
            "UPDATE t SET b = 'z'",
        ),
        (
            "CREATE TABLE t(a, b); INSERT INTO t VALUES (zeroblob(9000), 'y');",
            "UPDATE t SET a = zeroblob(9000)",
        ),
        (
            "CREATE TABLE t(a PRIMARY KEY, b, c); CREATE INDEX i ON t(c); INSERT INTO t VALUES (1, 'x', 3);",
            "INSERT INTO t VALUES (1, 'z', 9) ON CONFLICT(a) DO UPDATE SET b = 'x'",
        ),
        (
            "CREATE TABLE t(a PRIMARY KEY, b, c); CREATE INDEX i ON t(c); INSERT INTO t VALUES (1, 'x', 3);",
            "INSERT INTO t VALUES (1, 'z', 9) ON CONFLICT(a) DO UPDATE SET c = 3",
        ),
        (
            "PRAGMA encoding = 'UTF-16le'; CREATE TABLE t(a, b); INSERT INTO t VALUES ('x', 'y');",
            "UPDATE t SET a = 'x'",
        ),
    ] {
        cases.push((setup.to_string(), up.to_string()));
    }
    // Generated: small tables (one leaf each), indexes of every kind,
    // assignments drawn from a small domain so many leave values as
    // they were.
    let mut rng = Rng(0x0016_0a1e_u64);
    let n: usize = std::env::var("HIPP_WAL_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let kinds = [
        "a",
        "b",
        "c",
        "a, b",
        "c, a",
        "a + 0",
        "b || ''",
        "g",
        "s",
        "a) WHERE b > 1 AND (1",
        "c + 0) WHERE a < 3 AND (1",
        "b) WHERE c IS NOT NULL AND (1",
    ];
    let vals = ["1", "2", "3", "NULL", "'x'", "a", "b", "c"];
    for _ in 0..n {
        let mut setup =
            String::from("CREATE TABLE t(a, b, c, g AS (a + 1) VIRTUAL, s AS (b || 'q') STORED);");
        for k in 0..(1 + rng.below(3)) {
            let unique = if rng.below(4) == 0 { "UNIQUE " } else { "" };
            let kind = kinds[rng.below(kinds.len() as u64) as usize];
            // A UNIQUE index of rows sharing keys cannot be built.
            let unique = if kind.contains("WHERE") { "" } else { unique };
            setup.push_str(&format!("CREATE {unique}INDEX i{k} ON t({kind});"));
        }
        setup.push_str("INSERT OR IGNORE INTO t(a, b, c) VALUES ");
        for r in 0..(2 + rng.below(6)) {
            if r > 0 {
                setup.push(',');
            }
            setup.push_str(&format!("({r}, {}, {})", rng.below(3), rng.below(3)));
        }
        setup.push(';');
        let mut sets = Vec::new();
        for col in ["a", "b", "c"] {
            if rng.below(2) == 0 {
                sets.push(format!(
                    "{col} = {}",
                    vals[rng.below(vals.len() as u64) as usize]
                ));
            }
        }
        if sets.is_empty() {
            sets.push("b = b".to_string());
        }
        let up = format!(
            "UPDATE OR IGNORE t SET {} WHERE rowid % {} = {}",
            sets.join(", "),
            1 + rng.below(3),
            rng.below(2)
        );
        cases.push((setup, up));
    }
    for (i, (setup, up)) in cases.iter().enumerate() {
        // Every column the table has, in an order both engines agree
        // on (a statement error would end an oracle stdin line).
        let three = setup.split(';').next().is_some_and(|t| t.contains(", c"));
        let sel = if three {
            "SELECT a, b, c FROM t ORDER BY 1, 2, 3"
        } else {
            "SELECT * FROM t ORDER BY 1, 2"
        };
        byte_identical(
            &format!("up{i}"),
            setup,
            &format!(
                "PRAGMA journal_mode = WAL; {up}; PRAGMA wal_checkpoint; {sel}; PRAGMA integrity_check;"
            ),
            "SELECT count(*) FROM t;",
        );
    }
}
