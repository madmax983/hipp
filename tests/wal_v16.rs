//! v0.16 write-ahead logging (leaf feature `wal`), differentially
//! vs the pinned oracle (SQLite 3.54.1, live). Probed on the pin
//! before implementation (STATUS.md, "WAL journal mode"); the matrix
//! the tests encode:
//!
//! - PRAGMA reporting: `journal_mode` (the pin's prefix match of the
//!   mode name, `wal` / `delete` switching both ways, the
//!   in-transaction rules), `synchronous` (every safety level, the
//!   in-transaction refusal), `wal_autocheckpoint`,
//!   `journal_size_limit`, `wal_checkpoint` (every mode, the
//!   `busy|log|checkpointed` row, `0|-1|-1` outside WAL mode, the
//!   "database table is locked" refusal) — the same rows as the pin,
//!   statement by statement, and the same `-wal` file sizes between
//!   statements (`.size` lines below: frames per transaction,
//!   restarts, `journal_size_limit`, TRUNCATE).
//! - Reading the pin's logs: a pin killed with frames left in its
//!   `-wal` (and a stale `-shm`) is read, extended and closed by
//!   hipp; logs rewritten with big-endian checksums, torn or
//!   corrupt frames, bad salts or a bad header read exactly as the
//!   pin reads them.
//! - The pin reading hipp's logs: hipp's `-wal` left by a dropped
//!   connection parses with the format's rules (checked here by an
//!   independent parser), and the pin reads, recovers,
//!   checkpoints and integrity-checks it.
//! - A generated corpus of WAL workloads (writes, savepoints,
//!   statement failures, checkpoints of every mode, mode switches,
//!   VACUUM) run on both engines with the outputs, file sizes and
//!   final contents compared.

#![cfg(feature = "wal")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};
use std::path::{Path, PathBuf};

type FsError = <FileStorage as hipp::Storage>::Error;

fn side(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The `-wal` file's size, `none` when it is absent or empty (the
/// pin creates the empty file at its first read in WAL mode, hipp at
/// its first frame).
fn wal_size(db: &Path) -> String {
    match std::fs::metadata(side(db, "-wal")) {
        Ok(m) if m.len() > 0 => m.len().to_string(),
        _ => "none".to_string(),
    }
}

fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => line.push_str(&format!("{f:?}")),
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
}

/// One step of a script: SQL, or `.size` (print the `-wal` file's
/// size, `none` when absent).
fn steps(script: &str) -> Vec<String> {
    script
        .split(";;")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The result of running a script: output lines (each statement's
/// rows, then `ERR` for a failed statement), and the file state
/// after the connection closed.
#[derive(Debug, PartialEq)]
struct Run {
    lines: Vec<String>,
}

/// Run `script` (steps separated by `;;`) on hipp the way the CLI
/// does — but carrying on past errors (an `ERR` line) — then close
/// the connection unless `close` is false (a crash: the log stays).
fn hipp_run(path: &Path, script: &str, close: bool) -> (Run, Vec<Error<FsError>>) {
    hipp_run_cfg(path, script, close, parity())
}

/// A page cache as large as the pin's (its default `cache_size` of
/// 2000 KiB): neither engine spills the corpus's transactions into
/// the log early, so their frame counts compare.
fn parity() -> Config {
    Config::default().cache_pages(500)
}

fn hipp_run_cfg(path: &Path, script: &str, close: bool, cfg: Config) -> (Run, Vec<Error<FsError>>) {
    block_on(async {
        // hipp's WAL claim is an `flock` on the file's open file
        // description. While another test in this process spawns the
        // pin, the child holds a copy of every descriptor until its
        // `exec` (`O_CLOEXEC` only closes them there), so a database
        // this test just closed can still look held for a moment and
        // the reopen sees `DatabaseBusy`. Retry briefly; the tests
        // that expect `DatabaseBusy` open the database directly.
        let mut tries = 0;
        let mut db = loop {
            match Connection::open(FileStorage::open(path).expect("fs"), cfg.clone()).await {
                Err(Error::DatabaseBusy) if tries < 100 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                r => break r.expect("open"),
            }
        };
        let mut lines = Vec::new();
        let mut errs = Vec::new();
        for s in steps(script) {
            if s == ".size" {
                lines.push(wal_size(path));
                continue;
            }
            let mut q = match db.prepare(&s).await {
                Ok(q) => q,
                Err(e) => {
                    lines.push("ERR".to_string());
                    errs.push(e);
                    continue;
                }
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        render(&q, &mut l);
                        lines.push(l);
                    }
                    Ok(Step::Done) => break,
                    Err(e) => {
                        lines.push("ERR".to_string());
                        errs.push(e);
                        break;
                    }
                }
            }
        }
        if close {
            db.close().await.expect("close");
        }
        (Run { lines }, errs)
    })
}

/// The same script on the pin's shell, over stdin: a failed
/// statement prints `ERR` (the shell reports it on stderr, naming
/// the statement's input line), and `.size` prints the `-wal` size.
/// With `close` false the shell is killed at the end, leaving its
/// log (and `-shm`) behind.
fn pin_run(path: &Path, script: &str, close: bool) -> Run {
    let mut input = String::new();
    let wal = side(path, "-wal");
    let mut line = 1usize;
    // Each step's input line (None for `.size`).
    let mut at = Vec::new();
    for s in steps(script) {
        if s == ".size" {
            input.push_str(&format!(
                ".system s=$(stat -c %s '{}' 2>/dev/null); [ \"${{s:-0}}\" = 0 ] && echo none || echo $s\n",
                wal.display()
            ));
            at.push(None);
            line += 1;
        } else {
            input.push_str(&s);
            input.push_str(";\n");
            at.push(Some(line));
            line += 1 + s.matches('\n').count();
        }
        input.push_str(".print @@END\n");
        line += 1;
    }
    if !close {
        input.push_str(".system kill -9 $PPID\n");
    }
    let mut child = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(input.as_bytes()).expect("write script");
    }
    let out = child.wait_with_output().expect("oracle output");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    // "Parse error near line N: ..." / "Runtime error near line N: ...".
    let failed: Vec<usize> = stderr
        .lines()
        .filter_map(|l| {
            let rest = &l[l.find("line ")? + 5..];
            rest[..rest.find(':')?].parse().ok()
        })
        .collect();
    let mut lines = Vec::new();
    let mut step = 0;
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if l == "@@END" {
            if let Some(Some(n)) = at.get(step)
                && failed.contains(n)
            {
                lines.push("ERR".to_string());
            }
            step += 1;
        } else {
            lines.push(l.to_string());
        }
    }
    assert_eq!(
        failed.len(),
        lines.iter().filter(|l| *l == "ERR").count(),
        "unattributed oracle errors: {stderr}"
    );
    Run { lines }
}

/// Both engines run `script` on fresh databases; their outputs must
/// agree, and so must the files they leave: the `-wal` / `-shm`
/// sidecars present, the file-format bytes, and the contents the pin
/// reads (with its integrity check) from hipp's file.
fn agree(tag: &str, setup: &str, script: &str) {
    agree_by(tag, setup, script, |l| l.to_vec());
}

/// The generated corpus's view of an output: hipp's B-trees are not
/// always laid out page for page as the pin's after deletes (a
/// pre-existing difference in rebalancing), so a transaction may
/// write a different number of frames. Frame counts become what
/// they mean — a checkpoint row `busy|log|checkpointed` reads as
/// busy, whether the log holds frames, and whether all are
/// checkpointed; a `.size` as whether the log holds any.
fn shape(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            let num = |x: &str| x.parse::<i64>().ok();
            match f.as_slice() {
                [b, lg, ck] if num(b).is_some() && num(lg).is_some() && num(ck).is_some() => {
                    format!("ckpt {b} {} {}", num(lg) > Some(0), lg == ck)
                }
                [x] if num(x).is_some() => "size>0".to_string(),
                _ => l.clone(),
            }
        })
        .collect()
}

fn agree_by(tag: &str, setup: &str, script: &str, view: impl Fn(&[String]) -> Vec<String>) {
    let h = common::temp_db(&format!("wal-{tag}-h"));
    let o = common::temp_db(&format!("wal-{tag}-o"));
    if !setup.is_empty() {
        common::cli(&h, setup);
        common::cli(&o, setup);
    }
    let (hr, _) = hipp_run(&h, script, true);
    let or = pin_run(&o, script, true);
    assert_eq!(
        view(&hr.lines),
        view(&or.lines),
        "{tag}: outputs differ for\n{script}"
    );
    files_agree(tag, &h, &o);
}

/// The files both engines left agree (see `agree`).
fn files_agree(tag: &str, h: &Path, o: &Path) {
    for sfx in ["-wal", "-shm", "-journal"] {
        assert_eq!(
            side(h, sfx).exists(),
            side(o, sfx).exists(),
            "{tag}: {sfx} presence"
        );
    }
    let hb = std::fs::read(h).expect("hipp file");
    let ob = std::fs::read(o).expect("pin file");
    // (The pin leaves a database it never wrote empty; hipp creates
    // page 1 at open.)
    if !ob.is_empty() {
        assert_eq!(hb.get(18..20), ob.get(18..20), "{tag}: file-format bytes");
    }
    assert_eq!(
        common::cli(h, "PRAGMA integrity_check;").trim(),
        "ok",
        "{tag}: integrity"
    );
    let dump = "SELECT name, sql FROM sqlite_schema ORDER BY name; \
                SELECT * FROM t ORDER BY 1;";
    let has_t = common::cli(o, "SELECT count(*) FROM sqlite_schema WHERE name = 't';");
    if has_t.trim() == "1" {
        assert_eq!(
            common::cli(h, dump),
            common::cli(o, dump),
            "{tag}: contents"
        );
    }
}

// ---------------------------------------------------------------
// WAL file parsing (an implementation independent of hipp's).
// ---------------------------------------------------------------

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().expect("4 bytes"))
}

fn cksum(be: bool, data: &[u8], mut s0: u32, mut s1: u32) -> (u32, u32) {
    for c in data.chunks(8) {
        let w = |x: &[u8]| {
            if be {
                u32::from_be_bytes(x.try_into().expect("4"))
            } else {
                u32::from_le_bytes(x.try_into().expect("4"))
            }
        };
        s0 = s0.wrapping_add(w(&c[0..4])).wrapping_add(s1);
        s1 = s1.wrapping_add(w(&c[4..8])).wrapping_add(s0);
    }
    (s0, s1)
}

/// A parsed `-wal`: page size, salts, and the valid frames as
/// (page, commit size).
#[derive(Debug)]
struct WalFile {
    page_size: usize,
    seq: u32,
    salt: (u32, u32),
    frames: Vec<(u32, u32)>,
}

fn parse_wal(d: &[u8]) -> Option<WalFile> {
    if d.len() < 32 {
        return None;
    }
    let magic = be32(d, 0);
    if magic & !1 != 0x377f_0682 || be32(d, 4) != 3_007_000 {
        return None;
    }
    let be = magic & 1 == 1;
    let ps = be32(d, 8) as usize;
    let (mut s0, mut s1) = cksum(be, &d[..24], 0, 0);
    if (s0, s1) != (be32(d, 24), be32(d, 28)) {
        return None;
    }
    let salt = (be32(d, 16), be32(d, 20));
    let mut frames = Vec::new();
    let mut off = 32;
    while off + 24 + ps <= d.len() {
        let h = &d[off..off + 24];
        if (be32(h, 8), be32(h, 12)) != salt || be32(h, 0) == 0 {
            break;
        }
        let c = cksum(be, &h[..8], s0, s1);
        let c = cksum(be, &d[off + 24..off + 24 + ps], c.0, c.1);
        if c != (be32(h, 16), be32(h, 20)) {
            break;
        }
        (s0, s1) = c;
        frames.push((be32(h, 0), be32(h, 4)));
        off += 24 + ps;
    }
    Some(WalFile {
        page_size: ps,
        seq: be32(d, 12),
        salt,
        frames,
    })
}

/// Rewrite a log: checksums in `be` order, frames cut to `keep`, the
/// salt of frame `bad_salt` or the checksum of frame `bad_ck` broken,
/// the header checksum broken with `bad_hdr`.
fn rewrite_wal(
    d: &[u8],
    be: bool,
    keep: Option<usize>,
    bad_salt: Option<usize>,
    bad_ck: Option<usize>,
    bad_hdr: bool,
) -> Vec<u8> {
    let ps = be32(d, 8) as usize;
    let mut out = d[..32].to_vec();
    out[0..4].copy_from_slice(&(0x377f_0682u32 | u32::from(be)).to_be_bytes());
    let (mut s0, mut s1) = cksum(be, &out[..24], 0, 0);
    out[24..28].copy_from_slice(&s0.to_be_bytes());
    out[28..32].copy_from_slice(&s1.to_be_bytes());
    let n = (d.len() - 32) / (24 + ps);
    let n = keep.map_or(n, |k| k.min(n));
    for i in 0..n {
        let off = 32 + i * (24 + ps);
        let mut h = d[off..off + 24].to_vec();
        let page = &d[off + 24..off + 24 + ps];
        if bad_salt == Some(i + 1) {
            h[8] ^= 1;
        }
        (s0, s1) = cksum(be, &h[..8], s0, s1);
        (s0, s1) = cksum(be, page, s0, s1);
        h[16..20].copy_from_slice(&s0.to_be_bytes());
        h[20..24].copy_from_slice(&s1.to_be_bytes());
        if bad_ck == Some(i + 1) {
            h[16] ^= 1;
        }
        out.extend_from_slice(&h);
        out.extend_from_slice(page);
    }
    if bad_hdr {
        out[24] ^= 1;
    }
    out
}

/// Copy a database and its sidecars to a fresh path.
fn copy_db(from: &Path, tag: &str) -> PathBuf {
    let to = common::temp_db(tag);
    std::fs::copy(from, &to).expect("copy db");
    for sfx in ["-wal", "-shm"] {
        if side(from, sfx).exists() {
            std::fs::copy(side(from, sfx), side(&to, sfx)).expect("copy sidecar");
        }
    }
    to
}

fn hipp_query(path: &Path, sql: &str) -> String {
    let (r, errs) = hipp_run(path, sql, false);
    assert!(errs.is_empty(), "hipp {sql}: {errs:?}");
    r.lines.join("\n")
}

fn pin_query(path: &Path, sql: &str) -> String {
    common::cli(path, sql).trim_end().to_string()
}

// ---------------------------------------------------------------
// PRAGMA reporting and mode switches.
// ---------------------------------------------------------------

#[test]
fn journal_mode_values_and_switches() {
    for (i, s) in [
        "PRAGMA journal_mode;; PRAGMA journal_mode = WAL;; PRAGMA journal_mode;; PRAGMA main.journal_mode",
        "PRAGMA journal_mode = w;; PRAGMA journal_mode = d;; PRAGMA journal_mode = wa;; PRAGMA journal_mode = '';; PRAGMA journal_mode",
        "PRAGMA journal_mode = walx;; PRAGMA journal_mode = bogus;; PRAGMA journal_mode = 1;; PRAGMA journal_mode(wal);; PRAGMA journal_mode = DELETE;; PRAGMA journal_mode = wAl",
        "CREATE TABLE t(a);; PRAGMA journal_mode = wal;; INSERT INTO t VALUES (1);; .size;; PRAGMA journal_mode = delete;; .size;; SELECT * FROM t",
        "CREATE TABLE t(a);; PRAGMA journal_mode = wal;; INSERT INTO t VALUES (1);; PRAGMA journal_mode = delete;; INSERT INTO t VALUES (2);; PRAGMA journal_mode = wal;; INSERT INTO t VALUES (3);; .size;; SELECT * FROM t",
        // In a transaction: a written one keeps the mode silently;
        // one that has not written refuses the change.
        "CREATE TABLE t(a);; BEGIN;; INSERT INTO t VALUES (1);; PRAGMA journal_mode = WAL;; COMMIT;; PRAGMA journal_mode",
        "CREATE TABLE t(a);; BEGIN;; PRAGMA journal_mode = WAL;; SELECT * FROM t;; PRAGMA journal_mode = WAL;; COMMIT;; PRAGMA journal_mode",
        "CREATE TABLE t(a);; PRAGMA journal_mode = WAL;; BEGIN;; INSERT INTO t VALUES (1);; PRAGMA journal_mode = DELETE;; COMMIT;; PRAGMA journal_mode",
        "CREATE TABLE t(a);; PRAGMA journal_mode = WAL;; BEGIN;; PRAGMA journal_mode = DELETE;; ROLLBACK;; PRAGMA journal_mode = delete;; PRAGMA journal_mode",
        "PRAGMA journal_mode = WAL;; PRAGMA journal_mode = WAL;; BEGIN;; PRAGMA journal_mode = WAL;; PRAGMA journal_mode;; COMMIT",
        "SELECT * FROM pragma_journal_mode;; PRAGMA journal_mode = WAL;; SELECT * FROM pragma_journal_mode;; SELECT * FROM pragma_journal_mode('main')",
        // The TEMP database never enters WAL mode.
        "CREATE TEMP TABLE x(a);; PRAGMA temp.journal_mode = wal;; PRAGMA journal_mode",
    ]
    .iter()
    .enumerate()
    {
        agree(&format!("jm{i}"), "", s);
    }
}

#[test]
fn safety_levels_and_limits() {
    for (i, s) in [
        "PRAGMA synchronous;; PRAGMA synchronous = OFF;; PRAGMA synchronous;; PRAGMA synchronous = NORMAL;; PRAGMA synchronous;; PRAGMA synchronous = EXTRA;; PRAGMA synchronous",
        "PRAGMA synchronous = 7;; PRAGMA synchronous;; PRAGMA synchronous = bogus;; PRAGMA synchronous;; PRAGMA synchronous = 4;; PRAGMA synchronous;; PRAGMA synchronous = -1;; PRAGMA synchronous;; PRAGMA synchronous = full;; PRAGMA synchronous",
        "PRAGMA synchronous = on;; PRAGMA synchronous;; PRAGMA synchronous = no;; PRAGMA synchronous;; PRAGMA synchronous = 0;; PRAGMA synchronous;; SELECT * FROM pragma_synchronous",
        // The safety level cannot change inside a transaction.
        "CREATE TABLE t(a);; BEGIN;; PRAGMA synchronous = OFF;; PRAGMA synchronous;; COMMIT;; PRAGMA synchronous",
        "PRAGMA wal_autocheckpoint;; PRAGMA wal_autocheckpoint = 5;; PRAGMA wal_autocheckpoint;; PRAGMA wal_autocheckpoint = -3;; PRAGMA wal_autocheckpoint;; PRAGMA wal_autocheckpoint = 'abc';; PRAGMA wal_autocheckpoint = 99999999999;; PRAGMA wal_autocheckpoint = 2.7;; PRAGMA wal_autocheckpoint(7);; PRAGMA temp.wal_autocheckpoint",
        "PRAGMA journal_size_limit;; PRAGMA journal_size_limit = 100;; PRAGMA journal_size_limit;; PRAGMA journal_size_limit = -5;; PRAGMA journal_size_limit = -1;; PRAGMA journal_size_limit = 'x';; PRAGMA journal_size_limit;; PRAGMA journal_size_limit = 99999999999999;; PRAGMA journal_size_limit = 0x10;; PRAGMA journal_size_limit = '  12abc';; SELECT * FROM pragma_journal_size_limit",
        "BEGIN;; PRAGMA wal_autocheckpoint = 5;; PRAGMA journal_size_limit = 7;; COMMIT;; PRAGMA wal_autocheckpoint;; PRAGMA journal_size_limit",
        // Settings are not stored in the file.
        "PRAGMA journal_mode = WAL;; PRAGMA synchronous = OFF;; PRAGMA wal_autocheckpoint = 9;; PRAGMA journal_size_limit = 4",
    ]
    .iter()
    .enumerate()
    {
        agree(&format!("sl{i}"), "", s);
    }
    // ... a new connection reads the defaults.
    let h = common::temp_db("wal-sl-reopen");
    hipp_run(
        &h,
        "PRAGMA journal_mode = WAL;; PRAGMA synchronous = OFF;; PRAGMA wal_autocheckpoint = 9",
        true,
    );
    assert_eq!(
        hipp_query(
            &h,
            "PRAGMA synchronous;; PRAGMA wal_autocheckpoint;; PRAGMA journal_mode"
        ),
        "2\n1000\nwal"
    );
}

#[test]
fn checkpoints_match_the_pin() {
    let setup = "CREATE TABLE t(a, b);";
    for (i, s) in [
        // Outside WAL mode.
        "PRAGMA wal_checkpoint;; PRAGMA wal_checkpoint(TRUNCATE);; PRAGMA wal_checkpoint(bogus);; PRAGMA wal_checkpoint(noop)",
        // Every mode, with the log restarting after each.
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; INSERT INTO t VALUES (3, 'x');; INSERT INTO t VALUES (4, 'y');; .size;; \
         PRAGMA wal_checkpoint(noop);; PRAGMA wal_checkpoint(PASSIVE);; .size;; INSERT INTO t VALUES (5, 'z');; .size;; PRAGMA wal_checkpoint;; \
         PRAGMA wal_checkpoint(FULL);; INSERT INTO t VALUES (6, 'w');; PRAGMA wal_checkpoint(RESTART);; PRAGMA wal_checkpoint;; \
         INSERT INTO t VALUES (7, 'v');; PRAGMA wal_checkpoint;; PRAGMA wal_checkpoint(TRUNCATE);; .size;; PRAGMA wal_checkpoint;; \
         INSERT INTO t VALUES (8, 'u');; .size;; PRAGMA wal_checkpoint(passive);; PRAGMA wal_checkpoint(bogus);; PRAGMA main.wal_checkpoint;; \
         PRAGMA wal_checkpoint = full;; PRAGMA wal_checkpoint(NOOP);; PRAGMA wal_checkpoint('')",
        // The automatic checkpoint at its threshold.
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 3;; INSERT INTO t VALUES (1, 1);; PRAGMA wal_checkpoint(noop);; \
         INSERT INTO t VALUES (2, 2);; INSERT INTO t VALUES (3, 3);; PRAGMA wal_checkpoint(noop);; .size;; INSERT INTO t VALUES (4, 4);; \
         PRAGMA wal_checkpoint(noop);; INSERT INTO t VALUES (5, 5);; INSERT INTO t VALUES (6, 6);; INSERT INTO t VALUES (7, 7);; \
         PRAGMA wal_checkpoint(noop);; .size",
        // journal_size_limit trims the log at the first commit after
        // a restart.
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; INSERT INTO t VALUES (1, zeroblob(9000));; INSERT INTO t VALUES (2, 2);; .size;; \
         PRAGMA wal_checkpoint;; INSERT INTO t VALUES (3, 3);; .size;; PRAGMA journal_size_limit = 5000;; PRAGMA wal_checkpoint;; \
         INSERT INTO t VALUES (4, 4);; .size;; INSERT INTO t VALUES (5, 5);; .size;; PRAGMA journal_size_limit = 0;; PRAGMA wal_checkpoint;; \
         INSERT INTO t VALUES (6, 6);; .size;; PRAGMA wal_checkpoint(TRUNCATE);; .size;; INSERT INTO t VALUES (7, 7);; .size",
        // Refused inside a transaction that read or wrote, in either
        // mode; allowed right after BEGIN.
        "BEGIN;; INSERT INTO t VALUES (1, 1);; PRAGMA wal_checkpoint;; COMMIT",
        "BEGIN;; PRAGMA wal_checkpoint;; COMMIT;; BEGIN;; SELECT * FROM t;; PRAGMA wal_checkpoint;; COMMIT",
        "PRAGMA journal_mode = WAL;; BEGIN;; INSERT INTO t VALUES (1, 1);; PRAGMA wal_checkpoint(TRUNCATE);; ROLLBACK;; PRAGMA wal_checkpoint(TRUNCATE);; .size",
        "PRAGMA journal_mode = WAL;; BEGIN;; PRAGMA wal_checkpoint;; SELECT count(*) FROM t;; PRAGMA wal_checkpoint;; COMMIT;; PRAGMA wal_checkpoint",
        // Big transactions: overflow pages, splits, the cache spilling
        // into the log mid-transaction.
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; BEGIN;; \
         INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 400) SELECT i, printf('%0300d', i) FROM c;; \
         UPDATE t SET b = b || 'x' WHERE a % 3 = 0;; COMMIT;; PRAGMA wal_checkpoint(noop);; .size;; DELETE FROM t WHERE a > 50;; \
         PRAGMA wal_checkpoint(noop);; PRAGMA wal_checkpoint(TRUNCATE);; SELECT count(*), sum(length(b)) FROM t",
        // VACUUM in WAL mode keeps the page size; VACUUM INTO writes a
        // rollback-mode file with the pending one.
        "PRAGMA journal_mode = WAL;; INSERT INTO t VALUES (1, zeroblob(5000));; DELETE FROM t;; PRAGMA page_size = 1024;; VACUUM;; \
         PRAGMA page_size;; PRAGMA wal_checkpoint(noop);; PRAGMA page_count;; PRAGMA journal_mode",
    ]
    .iter()
    .enumerate()
    {
        agree(&format!("ck{i}"), setup, s);
    }
}

#[test]
fn vacuum_into_from_wal() {
    let h = common::temp_db("wal-vi-h");
    let o = common::temp_db("wal-vi-o");
    for p in [&h, &o] {
        let _ = std::fs::remove_file(side(p, "-into"));
    }
    let s = |p: &Path| {
        format!(
            "PRAGMA journal_mode = WAL;; CREATE TABLE t(a);; INSERT INTO t VALUES (1);; PRAGMA page_size = 1024;; \
             VACUUM INTO '{}';; PRAGMA journal_mode",
            side(p, "-into").display()
        )
    };
    let (hr, _) = hipp_run(&h, &s(&h), true);
    let or = pin_run(&o, &s(&o), true);
    assert_eq!(hr.lines, or.lines);
    let (hi, oi) = (side(&h, "-into"), side(&o, "-into"));
    let q = "PRAGMA page_size; PRAGMA journal_mode; PRAGMA integrity_check; SELECT * FROM t;";
    assert_eq!(common::cli(&hi, q), common::cli(&oi, q));
    assert_eq!(
        std::fs::read(&hi).expect("into").get(18..20),
        Some(&[1u8, 1][..])
    );
}

// ---------------------------------------------------------------
// Reading the pin's logs.
// ---------------------------------------------------------------

/// A database the pin left in WAL mode with committed frames in its
/// `-wal` (killed: no checkpoint, a stale `-shm` beside it).
fn pin_left_log(tag: &str, script: &str) -> PathBuf {
    let o = common::temp_db(tag);
    pin_run(&o, script, false);
    assert!(side(&o, "-wal").exists(), "{tag}: the pin left its log");
    o
}

#[test]
fn hipp_reads_and_extends_the_pins_log() {
    let o = pin_left_log(
        "wal-rd",
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; CREATE TABLE t(a INTEGER PRIMARY KEY, b);; \
         CREATE INDEX tb ON t(b);; INSERT INTO t VALUES (1, 'one');; BEGIN;; INSERT INTO t VALUES (2, zeroblob(6000));; \
         INSERT INTO t VALUES (3, 'three');; COMMIT;; UPDATE t SET b = 'uno' WHERE a = 1;; DELETE FROM t WHERE a = 3",
    );
    let wal = parse_wal(&std::fs::read(side(&o, "-wal")).expect("wal")).expect("pin's log");
    assert!(wal.frames.len() >= 5, "{wal:?}");
    assert!(
        std::fs::read(&o).expect("db").len() <= 4096,
        "frames not checkpointed"
    );
    let want = "SELECT a, length(b), typeof(b) FROM t ORDER BY a";
    // hipp reads the committed frames (the database file alone holds
    // an empty schema).
    let h = copy_db(&o, "wal-rd-h1");
    let p = copy_db(&o, "wal-rd-p1");
    assert_eq!(hipp_query(&h, want), pin_query(&p, &format!("{want};")));
    assert_eq!(hipp_query(&h, "PRAGMA journal_mode"), "wal");
    let h = copy_db(&o, "wal-rd-h");
    let p = copy_db(&o, "wal-rd-p");
    // ... extends the log after the pin's frames, closes (checkpoint,
    // log and stale -shm removed), and the pin reads the result.
    let more = "INSERT INTO t VALUES (4, 'four');; UPDATE t SET b = 'deux' WHERE a = 2;; PRAGMA wal_checkpoint(noop)";
    let (hr, errs) = hipp_run(&h, more, true);
    assert!(errs.is_empty(), "{errs:?}");
    let or = pin_run(&p, more, true);
    assert_eq!(hr.lines, or.lines);
    files_agree("wal-rd", &h, &p);
    assert!(!side(&h, "-shm").exists());
    assert_eq!(
        pin_query(&h, &format!("{want};")),
        pin_query(&p, &format!("{want};"))
    );
}

#[test]
fn torn_and_corrupt_logs_read_as_on_the_pin() {
    let o = pin_left_log(
        "wal-tc",
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; CREATE TABLE t(a);; INSERT INTO t VALUES (1);; \
         INSERT INTO t VALUES (2);; BEGIN;; INSERT INTO t VALUES (3);; INSERT INTO t VALUES (zeroblob(5000));; COMMIT;; \
         INSERT INTO t VALUES (5)",
    );
    let log = std::fs::read(side(&o, "-wal")).expect("wal");
    let n = parse_wal(&log).expect("log").frames.len();
    let q = "SELECT count(*), group_concat(typeof(a)) FROM t";
    let mut cases = Vec::new();
    for be in [false, true] {
        cases.push(rewrite_wal(&log, be, None, None, None, false));
        cases.push(rewrite_wal(&log, be, None, None, None, true));
        for k in 0..=n {
            cases.push(rewrite_wal(&log, be, Some(k), None, None, false));
        }
        for k in 1..=n {
            cases.push(rewrite_wal(&log, be, None, Some(k), None, false));
            cases.push(rewrite_wal(&log, be, None, None, Some(k), false));
        }
    }
    // A torn last frame (a crash mid-write) and trailing garbage.
    let mut torn = log.clone();
    torn.truncate(log.len() - 100);
    cases.push(torn);
    let mut junk = log.clone();
    junk.extend_from_slice(&[0x5a; 4200]);
    cases.push(junk);
    for (i, c) in cases.iter().enumerate() {
        let h = copy_db(&o, &format!("wal-tc{i}-h"));
        let p = copy_db(&o, &format!("wal-tc{i}-p"));
        std::fs::write(side(&h, "-wal"), c).expect("log");
        std::fs::write(side(&p, "-wal"), c).expect("log");
        let pin = std::process::Command::new(common::oracle())
            .arg(&p)
            .arg(format!("{q};"))
            .output()
            .expect("oracle");
        let pin_out = if pin.status.success() {
            String::from_utf8_lossy(&pin.stdout).trim_end().to_string()
        } else {
            "ERR".to_string()
        };
        let (hr, _) = hipp_run(&h, q, true);
        assert_eq!(hr.lines.join("\n"), pin_out, "case {i}");
        // Closing checkpointed what was read: the pin agrees.
        if pin_out != "ERR" {
            assert_eq!(
                pin_query(&h, &format!("{q};")),
                pin_out,
                "case {i} after close"
            );
            assert_eq!(pin_query(&h, "PRAGMA integrity_check;"), "ok", "case {i}");
        }
    }
}

#[test]
fn log_beside_rollback_header_and_empty_logs() {
    // A non-empty log beside a file whose header says rollback mode:
    // the pin reads it (and enters WAL mode); so does hipp.
    let o = pin_left_log(
        "wal-hd",
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; CREATE TABLE t(a);; INSERT INTO t VALUES (1), (2)",
    );
    let mut db = std::fs::read(&o).expect("db");
    db[18] = 1;
    db[19] = 1;
    std::fs::write(&o, &db).expect("db");
    let h = copy_db(&o, "wal-hd-h");
    let p = copy_db(&o, "wal-hd-p");
    let s = "SELECT group_concat(a) FROM t;; PRAGMA journal_mode";
    assert_eq!(hipp_run(&h, s, true).0, pin_run(&p, s, true));
    files_agree("wal-hd", &h, &p);
    // An empty (0-byte) log beside a rollback-mode file: rollback mode,
    // the log left alone; beside a WAL-mode file: WAL mode, removed at
    // close.
    for (i, v) in [1u8, 2].iter().enumerate() {
        let h = common::temp_db(&format!("wal-em{i}-h"));
        let p = common::temp_db(&format!("wal-em{i}-p"));
        for f in [&h, &p] {
            common::cli(f, "CREATE TABLE t(a); INSERT INTO t VALUES (1);");
            let mut b = std::fs::read(f).expect("db");
            b[18] = *v;
            b[19] = *v;
            std::fs::write(f, &b).expect("db");
            std::fs::write(side(f, "-wal"), b"").expect("log");
        }
        let s = "PRAGMA journal_mode;; INSERT INTO t VALUES (2);; SELECT count(*) FROM t";
        assert_eq!(hipp_run(&h, s, true).0, pin_run(&p, s, true), "v={v}");
        files_agree(&format!("wal-em{i}"), &h, &p);
    }
    // A log beside an empty database file is deleted.
    let h = common::temp_db("wal-ed-h");
    std::fs::write(&h, b"").expect("db");
    std::fs::write(
        side(&h, "-wal"),
        std::fs::read(side(&o, "-wal")).expect("log"),
    )
    .expect("log");
    assert_eq!(hipp_query(&h, "SELECT count(*) FROM sqlite_schema"), "0");
    assert!(!side(&h, "-wal").exists());
}

// ---------------------------------------------------------------
// The pin reading hipp's logs.
// ---------------------------------------------------------------

#[test]
fn pin_reads_and_recovers_hipps_log() {
    let h = common::temp_db("wal-hl");
    let script = "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; CREATE TABLE t(a INTEGER PRIMARY KEY, b);; \
                  CREATE INDEX tb ON t(b);; INSERT INTO t VALUES (1, 'one');; BEGIN;; INSERT INTO t VALUES (2, zeroblob(6000));; \
                  INSERT INTO t VALUES (3, 'three');; COMMIT;; UPDATE t SET b = 'uno' WHERE a = 1;; DELETE FROM t WHERE a = 3;; \
                  BEGIN;; INSERT INTO t VALUES (9, 'rolled back');; ROLLBACK";
    let (_, errs) = hipp_run(&h, script, false);
    assert!(errs.is_empty(), "{errs:?}");
    // The log, by an independent parser: hipp's header, salts and
    // cumulative checksums, commit frames ending each transaction.
    let log = std::fs::read(side(&h, "-wal")).expect("hipp left its log");
    let w = parse_wal(&log).expect("valid header");
    assert_eq!(w.page_size, 4096);
    assert_eq!(w.seq, 0);
    assert_ne!(w.salt, (0, 0));
    assert_eq!(&log[0..4], &0x377f_0682u32.to_be_bytes());
    assert!(w.frames.len() >= 6, "{w:?}");
    assert_ne!(
        w.frames.last().expect("frames").1,
        0,
        "ends at a commit frame"
    );
    let commits = w.frames.iter().filter(|f| f.1 != 0).count();
    assert_eq!(commits, 6, "one commit frame per transaction: {w:?}");
    assert!(std::fs::read(&h).expect("db").len() <= 4096 * 2);
    // The pin reads it (a copy each time: its close checkpoints).
    let want = "SELECT a, length(b) FROM t ORDER BY a;";
    let p = copy_db(&h, "wal-hl-p");
    assert_eq!(pin_query(&p, want), "1|3\n2|6000");
    assert_eq!(pin_query(&p, "PRAGMA integrity_check;"), "ok");
    assert!(
        !side(&p, "-wal").exists(),
        "the pin checkpointed and removed it"
    );
    let p = copy_db(&h, "wal-hl-p2");
    assert_eq!(
        pin_query(
            &p,
            "PRAGMA journal_mode; PRAGMA wal_checkpoint(TRUNCATE); PRAGMA integrity_check;"
        ),
        format!("wal\n0|0|0\nok")
    );
    assert_eq!(pin_query(&p, want), "1|3\n2|6000");
    // The pin extends hipp's log, then hipp reads the pin's frames.
    let p = copy_db(&h, "wal-hl-p3");
    pin_run(
        &p,
        "PRAGMA wal_autocheckpoint = 0;; INSERT INTO t VALUES (5, 'five');; UPDATE t SET b = 'dos' WHERE a = 2",
        false,
    );
    assert_eq!(hipp_query(&p, &want.replace(';', "")), "1|3\n2|3\n5|4");
}

#[test]
fn hipp_log_survives_reopen_and_big_endian_logs_extend() {
    // A big-endian log (as a big-endian host's pin writes) is read and
    // extended with big-endian checksums.
    let o = pin_left_log(
        "wal-be",
        "PRAGMA journal_mode = WAL;; PRAGMA wal_autocheckpoint = 0;; CREATE TABLE t(a);; INSERT INTO t VALUES (1)",
    );
    let log = std::fs::read(side(&o, "-wal")).expect("log");
    std::fs::write(
        side(&o, "-wal"),
        rewrite_wal(&log, true, None, None, None, false),
    )
    .expect("log");
    let (_, errs) = hipp_run(
        &o,
        "INSERT INTO t VALUES (2);; INSERT INTO t VALUES (3)",
        false,
    );
    assert!(errs.is_empty());
    let log = std::fs::read(side(&o, "-wal")).expect("log");
    assert_eq!(&log[0..4], &0x377f_0683u32.to_be_bytes());
    assert_eq!(parse_wal(&log).expect("log").frames.len(), 5);
    let p = copy_db(&o, "wal-be-p");
    assert_eq!(pin_query(&p, "SELECT group_concat(a) FROM t;"), "1,2,3");
    // hipp reopens its own log, any number of times.
    for _ in 0..3 {
        hipp_run(&o, "INSERT INTO t VALUES (4)", false);
    }
    assert_eq!(hipp_query(&o, "SELECT count(*) FROM t"), "6");
    assert_eq!(
        pin_query(&o, "SELECT count(*) FROM t; PRAGMA integrity_check;"),
        "6\nok"
    );
}

#[test]
fn close_removes_the_log_and_a_stale_shm() {
    let h = common::temp_db("wal-cl");
    hipp_run(
        &h,
        "PRAGMA journal_mode = WAL;; CREATE TABLE t(a);; INSERT INTO t VALUES (1)",
        false,
    );
    assert!(side(&h, "-wal").exists());
    std::fs::write(side(&h, "-shm"), [0u8; 32768]).expect("stale shm");
    hipp_run(&h, "SELECT * FROM t", true);
    assert!(!side(&h, "-wal").exists());
    assert!(!side(&h, "-shm").exists());
    let b = std::fs::read(&h).expect("db");
    assert_eq!(&b[18..20], &[2, 2], "still WAL mode");
    assert_eq!(
        hipp_query(&h, "PRAGMA journal_mode;; SELECT * FROM t"),
        "wal\n1"
    );
}

/// A database a live pin connection holds in WAL mode (its `-shm`
/// shared) is refused, never read beside it.
#[cfg(target_os = "linux")]
#[test]
fn a_live_pin_connection_is_refused() {
    let o = common::temp_db("wal-busy");
    common::cli(&o, "PRAGMA journal_mode = WAL; CREATE TABLE t(a);");
    let mut child = std::process::Command::new(common::oracle())
        .arg(&o)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("oracle");
    {
        use std::io::{BufRead, Write};
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(b"SELECT 'ready' FROM t UNION ALL SELECT 'ready';\n")
            .expect("w");
        stdin.flush().expect("flush");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.as_mut().expect("stdout"))
            .read_line(&mut line)
            .expect("ready");
    }
    let r = block_on(async {
        Connection::open(FileStorage::open(&o).expect("fs"), Config::default())
            .await
            .map(|_| ())
    });
    assert!(matches!(r, Err(Error::DatabaseBusy)), "{:?}", r.err());
    drop(child.stdin.take());
    let _ = child.wait();
    // Once it has gone, the database opens.
    assert_eq!(hipp_query(&o, "PRAGMA journal_mode"), "wal");
}

/// Two hipp connections on one WAL database in one process (v0.16
/// review fix): each would keep its own heap wal-index, and the
/// first one's close would checkpoint and unlink the log the second
/// appends to, its commits then on no file at all. The second is
/// refused (`DatabaseBusy`) while the first holds the database in WAL
/// mode — at open, or when it would enter WAL mode — and opens once
/// the first has closed or left WAL mode. Every commit stays on disk.
#[cfg(unix)]
#[test]
fn a_second_hipp_connection_is_refused() {
    let h = common::temp_db("wal-two");
    block_on(async {
        let open = || async {
            Connection::open(FileStorage::open(&h).expect("fs"), Config::default()).await
        };
        let exec = |db: &mut Connection<FileStorage>, sql: &'static str| {
            let fut = async move {
                let mut q = db.prepare(sql).await?;
                while q.step().await? == Step::Row {}
                Ok::<(), Error<FsError>>(())
            };
            block_on(fut)
        };
        let mut a = open().await.expect("first");
        exec(&mut a, "PRAGMA journal_mode = WAL").expect("wal");
        exec(&mut a, "CREATE TABLE t(a)").expect("create");
        exec(&mut a, "INSERT INTO t VALUES (1)").expect("insert");
        assert!(matches!(open().await.map(|_| ()), Err(Error::DatabaseBusy)));
        a.close().await.expect("close");
        let mut b = open().await.expect("second, after the first closed");
        exec(&mut b, "INSERT INTO t VALUES (2)").expect("insert");
        // The log is the one on disk: a crash (the connection
        // dropped, nothing checkpointed) loses nothing.
        drop(b);
        assert_eq!(pin_query(&h, "SELECT a FROM t ORDER BY a"), "1\n2");
        // A rollback-mode connection may not enter WAL mode while
        // another holds the database in it; once that one has left
        // WAL mode, it may.
        let mut c = open().await.expect("third");
        exec(&mut c, "PRAGMA journal_mode = DELETE").expect("delete");
        let mut d = open().await.expect("rollback-mode open");
        exec(&mut c, "PRAGMA journal_mode = WAL").expect("wal again");
        assert!(matches!(
            exec(&mut d, "PRAGMA journal_mode = WAL"),
            Err(Error::DatabaseBusy)
        ));
        exec(&mut c, "PRAGMA journal_mode = DELETE").expect("leave");
        drop(c);
        drop(d);
    });
    assert_eq!(
        pin_query(&h, "SELECT a FROM t ORDER BY a; PRAGMA integrity_check"),
        "1\n2\nok"
    );
}

/// `MemStorage::memory_db` is an in-memory database (v0.16 review
/// fix): its journal mode is `memory` whatever is asked, as the pin's
/// `:memory:` — no WAL mode, `wal_checkpoint` `0|-1|-1`. A plain
/// `MemStorage` models a file and switches as one.
#[test]
fn memory_db_stays_memory() {
    use hipp::MemStorage;
    fn rows(storage: MemStorage, sql: &[&str]) -> Vec<String> {
        block_on(async {
            let mut db = Connection::open(storage, Config::default())
                .await
                .expect("open");
            let mut out = Vec::new();
            for s in sql {
                let mut q = db.prepare(s).await.expect("prepare");
                while q.step().await.expect("step") == Step::Row {
                    let mut line = String::new();
                    for i in 0..q.column_count() {
                        if i > 0 {
                            line.push('|');
                        }
                        match q.column_value(i) {
                            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
                            Some(hipp::Value::Text(b)) => {
                                line.push_str(&String::from_utf8_lossy(b))
                            }
                            _ => {}
                        }
                    }
                    out.push(line);
                }
            }
            out
        })
    }
    let sql = [
        "PRAGMA journal_mode",
        "PRAGMA journal_mode = WAL",
        "PRAGMA journal_mode = OFF",
        "CREATE TABLE t(a)",
        "INSERT INTO t VALUES (1)",
        "PRAGMA wal_checkpoint",
        "PRAGMA journal_mode",
    ];
    assert_eq!(
        rows(MemStorage::memory_db(), &sql),
        ["memory", "memory", "memory", "0|-1|-1", "memory"]
    );
    let file = MemStorage::new();
    let got = rows(file.clone(), &sql[..2]);
    assert_eq!(got, ["delete", "wal"]);
    assert_eq!(file.snapshot().get(18..20), Some(&[2u8, 2][..]));
}

#[test]
fn errors_are_the_pins() {
    let h = common::temp_db("wal-err");
    let (_, errs) = hipp_run(
        &h,
        "CREATE TABLE t(a);; BEGIN;; PRAGMA journal_mode = WAL;; PRAGMA synchronous = 1;; SELECT 1;; \
         ROLLBACK;; PRAGMA journal_mode = WAL;; BEGIN;; PRAGMA journal_mode = DELETE;; SELECT * FROM t;; PRAGMA wal_checkpoint",
        true,
    );
    let kinds: Vec<_> = errs
        .iter()
        .map(|e| match e {
            Error::Schema { kind } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            Some(SchemaKind::WalModeInTransaction),
            Some(SchemaKind::SafetyLevelInTransaction),
            Some(SchemaKind::WalModeInTransaction),
            Some(SchemaKind::CheckpointLocked),
        ]
    );
}

// ---------------------------------------------------------------
// Savepoints and statement rollback inside WAL transactions.
// ---------------------------------------------------------------

#[test]
fn savepoints_inside_wal_transactions() {
    let setup = "PRAGMA journal_mode = WAL; CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE);";
    for (i, s) in [
        "SAVEPOINT a;; INSERT INTO t VALUES (1, 'x');; SAVEPOINT b;; INSERT INTO t VALUES (2, 'y');; ROLLBACK TO b;; \
         INSERT INTO t VALUES (3, 'z');; RELEASE a;; SELECT * FROM t;; PRAGMA wal_checkpoint(noop)",
        "BEGIN;; INSERT INTO t VALUES (1, 'x');; INSERT INTO t VALUES (2, 'y'), (3, 'x');; SELECT * FROM t;; COMMIT;; SELECT * FROM t",
        "INSERT INTO t VALUES (1, 'x');; INSERT INTO t VALUES (2, 'y'), (3, 'x');; SELECT * FROM t;; PRAGMA wal_checkpoint(noop)",
        "BEGIN;; INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 300) SELECT i, printf('%0200d', i) FROM c;; \
         SAVEPOINT s;; UPDATE t SET b = b || 'q';; DELETE FROM t WHERE a % 2 = 0;; ROLLBACK TO s;; \
         UPDATE t SET b = 'v' || b WHERE a % 7 = 0;; COMMIT;; SELECT count(*), sum(length(b)) FROM t;; PRAGMA wal_checkpoint(noop);; .size",
        "BEGIN;; INSERT INTO t VALUES (1, 'x');; ROLLBACK;; INSERT INTO t VALUES (2, 'y');; SELECT * FROM t;; PRAGMA wal_checkpoint(noop)",
    ]
    .iter()
    .enumerate()
    {
        agree(&format!("sp{i}"), setup, s);
    }
}

// ---------------------------------------------------------------
// A generated corpus.
// ---------------------------------------------------------------

/// A deterministic PRNG (splitmix64), as in tests/fuzz_smoke.rs.
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

fn gen_script(rng: &mut Rng) -> String {
    let mut s = vec![
        "PRAGMA journal_mode = WAL".to_string(),
        format!(
            "PRAGMA wal_autocheckpoint = {}",
            [0, 1][rng.below(2) as usize]
        ),
    ];
    if rng.below(3) == 0 {
        s.push(format!("PRAGMA journal_size_limit = {}", rng.below(20_000)));
    }
    let mut in_txn = false;
    let mut sps = 0;
    for _ in 0..(6 + rng.below(22)) {
        let k = rng.below(100);
        let id = rng.below(60);
        let width = [1, 50, 700, 3000, 9000][rng.below(5) as usize];
        let stmt = match k {
            0..=24 => format!(
                "INSERT OR REPLACE INTO t VALUES ({id}, printf('%0{width}d', {id}), {})",
                rng.below(9)
            ),
            25..=34 => format!(
                "INSERT INTO t WITH RECURSIVE c(i) AS (SELECT {id} * 100 UNION ALL SELECT i + 1 FROM c WHERE i < {id} * 100 + {}) \
                 SELECT i, printf('%0{width}d', i), i % 9 FROM c",
                rng.below(40)
            ),
            35..=44 => format!(
                "UPDATE t SET b = b || 'u', c = c + 1 WHERE a % {} = 0",
                2 + rng.below(5)
            ),
            45..=51 => format!("DELETE FROM t WHERE a % {} = 1", 2 + rng.below(6)),
            // A statement that fails half-way (UNIQUE on c's index).
            52..=55 => format!("INSERT INTO u VALUES ({id}), ({id})"),
            56..=61 => {
                in_txn = !in_txn;
                if in_txn {
                    "BEGIN".to_string()
                } else if rng.below(3) == 0 {
                    sps = 0;
                    "ROLLBACK".to_string()
                } else {
                    sps = 0;
                    "COMMIT".to_string()
                }
            }
            62..=66 => {
                sps += 1;
                format!("SAVEPOINT s{sps}")
            }
            67..=70 if sps > 0 => format!("ROLLBACK TO s{}", 1 + rng.below(sps)),
            71..=73 if sps > 0 => {
                let n = 1 + rng.below(sps);
                let r = format!("RELEASE s{n}");
                sps = n - 1;
                if sps == 0 && !in_txn {
                    // Releasing the outermost savepoint outside BEGIN
                    // commits.
                }
                r
            }
            74..=81 => format!(
                "PRAGMA wal_checkpoint{}",
                [
                    "",
                    "(PASSIVE)",
                    "(FULL)",
                    "(RESTART)",
                    "(TRUNCATE)",
                    "(NOOP)"
                ][rng.below(6) as usize]
            ),
            82..=84 => ".size".to_string(),
            85..=86 => "PRAGMA journal_mode = DELETE".to_string(),
            87..=88 => "PRAGMA journal_mode = WAL".to_string(),
            89 => "VACUUM".to_string(),
            90..=92 => "SELECT 'n', count(*), sum(length(b)), sum(c) FROM t".to_string(),
            _ => "PRAGMA wal_checkpoint(NOOP)".to_string(),
        };
        s.push(stmt);
    }
    s.push("SELECT 'n', count(*), sum(length(b)), sum(c) FROM t".to_string());
    s.push(".size".to_string());
    s.join(";; ")
}

#[test]
fn generated_wal_workloads() {
    let n: usize = std::env::var("HIPP_WAL_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let mut rng = Rng(0x5eed_0016_00a1);
    let setup = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tc ON t(c); CREATE TABLE u(x UNIQUE);";
    for i in 0..n {
        let s = gen_script(&mut rng);
        agree_by(&format!("gen{i}"), setup, &s, shape);
    }
}

/// The corpus again with the connection dropped (hipp) or killed
/// (the pin) at the end: each engine reads the other's log.
#[test]
fn generated_logs_cross_read() {
    let n: usize = std::env::var("HIPP_WAL_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        / 2;
    let mut rng = Rng(0x5eed_0016_00a2);
    let setup = "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c); CREATE INDEX tc ON t(c); CREATE TABLE u(x UNIQUE);";
    let q = "SELECT count(*), sum(length(b)), sum(c), group_concat(a) FROM t";
    for i in 0..n {
        let s = gen_script(&mut rng);
        let h = common::temp_db(&format!("wal-x{i}-h"));
        let o = common::temp_db(&format!("wal-x{i}-o"));
        common::cli(&h, setup);
        common::cli(&o, setup);
        let (hr, _) = hipp_run(&h, &s, false);
        let or = pin_run(&o, &s, false);
        assert_eq!(shape(&hr.lines), shape(&or.lines), "x{i}: outputs\n{s}");
        // The pin reads hipp's log; hipp reads the pin's.
        let hp = copy_db(&h, &format!("wal-x{i}-hp"));
        let oh = copy_db(&o, &format!("wal-x{i}-oh"));
        assert_eq!(
            pin_query(&hp, &format!("{q}; PRAGMA integrity_check;")),
            format!(
                "{}\nok",
                pin_query(&copy_db(&o, &format!("wal-x{i}-oo")), &format!("{q};"))
            ),
            "x{i}: the pin reading hipp's log\n{s}"
        );
        assert_eq!(
            hipp_query(&oh, q),
            hipp_query(&h, q),
            "x{i}: hipp reading the pin's log\n{s}"
        );
    }
}
