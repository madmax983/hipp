//! v0.17 FTS5 delete writes (leaf `fts5_delete`), differentially vs
//! the pinned oracle (SQLite 3.54.1):
//!
//! - `contentless_delete=1` tables: DELETE, UPDATE (every indexed
//!   column; UNINDEXED-only updates are no-ops) and INSERT OR REPLACE
//!   write the pin's tombstone hash pages (32-slot first page, growth,
//!   multi-page rebuilds, 4- and 8-byte keys, the rowid-0 flag), the
//!   structure's tombstone counters (written when a hash is rebuilt,
//!   and at the flush), the tombstones counted as automerge work and
//!   the `deletemerge` merges they drive.
//! - `secure-delete=1`: every delete (DELETE, UPDATE, OR REPLACE, the
//!   'delete' command on contentless and external-content tables)
//!   removes the row's entries from the segments in place, in every
//!   detail mode, with prefix indexes and small pages: terms removed
//!   or replaced by the next one, emptied pages, overflowing position
//!   lists cut, page indexes and `%_idx` rows rewritten, `version`
//!   moving to 5 with the cookie.
//! - Every shadow table compared byte for byte, and the queries
//!   (MATCH, AND / OR / phrases / prefixes, bm25, highlight,
//!   fts5vocab), after the hand-written cases and generated write
//!   sequences; each engine's integrity-check passes on the other's
//!   file; files move between the engines mid-way both ways.

#![cfg(feature = "fts5_delete")]

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// One row as the pin's shell prints it in list mode.
fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
    for i in 0..q.column_count() {
        if i > 0 {
            line.push('|');
        }
        match q.column_value(i) {
            Some(hipp::Value::Null) | None => {}
            Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
            Some(hipp::Value::Real(f)) => {
                let mut out = hipp::arena::AVec::new();
                hipp::record::render_real(f, &mut out);
                line.push_str(&String::from_utf8_lossy(out.as_slice()));
            }
            Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                line.push_str(&String::from_utf8_lossy(b));
            }
        }
    }
}

/// hipp on `path`: every statement, continuing past errors as the
/// shell does. The output lines, per statement whether it failed, and
/// the first error.
fn hipp_run(
    path: &std::path::Path,
    stmts: &[String],
) -> (Vec<String>, Vec<bool>, Option<Error<FsError>>) {
    block_on(async {
        let mut db = open(path).await;
        let mut lines = Vec::new();
        let mut failed = Vec::new();
        let mut first = None;
        for stmt in stmts {
            let mut q = match db.prepare(stmt).await {
                Ok(q) => q,
                Err(e) => {
                    failed.push(true);
                    first.get_or_insert(e);
                    continue;
                }
            };
            let mut bad = false;
            loop {
                match q.step().await {
                    Ok(Step::Row) => {
                        let mut l = String::new();
                        render(&q, &mut l);
                        for part in l.split('\n') {
                            lines.push(part.to_string());
                        }
                    }
                    Ok(Step::Done) => break,
                    Err(e) => {
                        bad = true;
                        first.get_or_insert(e);
                        break;
                    }
                }
            }
            failed.push(bad);
        }
        (lines, failed, first)
    })
}

/// The pin's shell on `path`: its stdout lines and error count.
fn pin_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, usize) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            let mut sin = c.stdin.take().expect("stdin");
            for stmt in stmts {
                writeln!(sin, "{stmt};")?;
            }
            drop(sin);
            c.wait_with_output()
        })
        .expect("oracle runs");
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let errs = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| {
            (l.starts_with("Error")
                || l.starts_with("Parse error")
                || l.starts_with("Runtime error"))
                && l.contains(" near line ")
        })
        .count();
    (lines, errs)
}

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// `stmts` on fresh twin files: the same output lines (statement
/// markers name a difference) and the same failing statements.
fn same(tag: &str, stmts: &[String]) -> Option<Error<FsError>> {
    let h = common::temp_db(&format!("v17fd-{tag}-h"));
    let o = common::temp_db(&format!("v17fd-{tag}-o"));
    same_on(&h, &o, stmts)
}

fn same_on(h: &std::path::Path, o: &std::path::Path, stmts: &[String]) -> Option<Error<FsError>> {
    let mut marked = Vec::new();
    for (i, s) in stmts.iter().enumerate() {
        marked.push(format!("SELECT '#{i}'"));
        marked.push(s.clone());
    }
    let _ = std::fs::write(h.with_extension("sql"), marked.join(";\n"));
    let (hl, hf, he) = hipp_run(h, &marked);
    let (ol, oerr) = pin_run(o, &marked);
    if hl != ol {
        let at = hl
            .iter()
            .zip(&ol)
            .position(|(a, b)| a != b)
            .unwrap_or(hl.len().min(ol.len()));
        let stmt = ol[..at.min(ol.len())]
            .iter()
            .rev()
            .find_map(|l| l.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()))
            .and_then(|i| stmts.get(i));
        panic!(
            "{h:?}: output differs at line {at} (statement {stmt:?})\npin : {:?}\nhipp: {:?}\nhipp error: {he:?}",
            ol.get(at..(at + 3).min(ol.len())),
            hl.get(at..(at + 3).min(hl.len())),
        );
    }
    let herr = hf.iter().filter(|b| **b).count();
    assert_eq!(
        herr, oerr,
        "{h:?}: failing statement count differs; hipp error: {he:?}"
    );
    he
}

/// Every shadow table's rows.
fn shadow() -> Vec<String> {
    v(&[
        "SELECT id, hex(block) FROM t_data",
        "SELECT segid, hex(term), pgno FROM t_idx",
        "SELECT k, v FROM t_config",
    ])
}

const CHECK: &str = "INSERT INTO t(t) VALUES('integrity-check')";

/// A query that reads the structure record: after a rollback or a
/// schema reparse the generated workloads run one, closing the window
/// in which the pin's connection holds a cookie other than the file's
/// (a rolled-back one, or -1 after a reparse; an honest gap: a
/// 'rebuild' or 'delete-all' there writes the pin's cookie, hipp the
/// file's).
const REREAD: &str = "SELECT count(*) FROM t WHERE t MATCH 'alpha'";

/// The pin's `integrity-check` (and PRAGMA integrity_check) on a file.
fn pin_checks(db: &std::path::Path) -> Vec<String> {
    pin_run(
        db,
        &v(&["PRAGMA integrity_check", CHECK, "SELECT 'checked'"]),
    )
    .0
}

/// hipp's on a file.
fn hipp_checks(db: &std::path::Path) -> Vec<String> {
    let (l, f, e) = hipp_run(
        db,
        &v(&["PRAGMA integrity_check", CHECK, "SELECT 'checked'"]),
    );
    assert!(f.iter().all(|b| !b), "{db:?}: {e:?}");
    l
}

/// Both files pass the other engine's checks.
fn cross_check(tag: &str, h: &std::path::Path, o: &std::path::Path) {
    let ok = v(&["ok", "checked"]);
    assert_eq!(pin_checks(h), ok, "{tag}: the pin checks hipp's file");
    assert_eq!(hipp_checks(o), ok, "{tag}: hipp checks the pin's file");
}

/// `stmts` (then the shadow tables and the integrity checks) on twin
/// files.
fn same_checked(tag: &str, stmts: &[String]) {
    let h = common::temp_db(&format!("v17fd-{tag}-h"));
    let o = common::temp_db(&format!("v17fd-{tag}-o"));
    let mut all = stmts.to_vec();
    all.extend(shadow());
    all.push("SELECT id, hex(sz) FROM t_docsize".into());
    same_on(&h, &o, &all);
    cross_check(tag, &h, &o);
}

/// Statements with the shadow tables after each write.
fn dumped(xs: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for x in xs {
        out.push(x.to_string());
        let u = x.trim_start().to_ascii_uppercase();
        if !u.starts_with("SELECT") && !u.starts_with("CREATE") {
            out.extend(shadow());
        }
    }
    out
}

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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

// ---------------- Tombstones (contentless_delete=1) ----------------

/// The first tombstone page (32 slots), its growth to a page of
/// `nElem * 4` slots, past `pgsz` into `2n + 1` pages, 8-byte keys for
/// rowids past 2^32, the rowid-0 flag, negative rowids; deletes over
/// several segments of different origins; the structure written when
/// a hash is rebuilt (visible inside a transaction) and at the flush.
#[test]
fn tombstone_pages_alike() {
    for (tag, pgsz, detail) in [
        ("t1", 4050, "full"),
        ("t2", 64, "column"),
        ("t3", 128, "none"),
    ] {
        let create = format!(
            "CREATE VIRTUAL TABLE t USING fts5(a, b, content='', contentless_delete=1, detail={detail})"
        );
        let pg = format!("INSERT INTO t(t, rank) VALUES('pgsz', {pgsz})");
        let s = dumped(&[
            &create,
            &pg,
            "INSERT INTO t(rowid, a, b) SELECT value, 'w' || (value % 7) || ' common', 'x' || value FROM generate_series(1, 200)",
            "INSERT INTO t(rowid, a, b) SELECT value, 'late ' || value, 'common' FROM generate_series(201, 260)",
            "DELETE FROM t WHERE rowid = 5",
            "SELECT rowid FROM t WHERE t MATCH 'w5' ORDER BY rowid LIMIT 3",
            "DELETE FROM t WHERE rowid IN (6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17)",
            "DELETE FROM t WHERE rowid BETWEEN 20 AND 120",
            "DELETE FROM t WHERE rowid % 5 = 0",
            "SELECT count(*), sum(rowid) FROM t WHERE t MATCH 'common'",
            "INSERT INTO t(rowid, a, b) VALUES(0, 'zero', 'row'), (-7, 'negative', 'row'), (9000000000, 'big', 'row')",
            "DELETE FROM t WHERE rowid IN (0, -7)",
            "DELETE FROM t WHERE rowid = 9000000000",
            "SELECT rowid FROM t WHERE t MATCH 'row OR zero OR big OR negative'",
            "BEGIN",
            "DELETE FROM t WHERE rowid = 130",
            "SELECT id, hex(block) FROM t_data WHERE id = 10",
            "DELETE FROM t WHERE rowid = 131",
            "SELECT rowid FROM t WHERE t MATCH 'w4' AND rowid BETWEEN 125 AND 140",
            "COMMIT",
            "SELECT rowid FROM t WHERE t MATCH 'late' ORDER BY rowid DESC LIMIT 4",
            "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'w3 common' ORDER BY rank LIMIT 5",
            "CREATE VIRTUAL TABLE v USING fts5vocab(t, row)",
            "SELECT * FROM v",
            CHECK,
        ]);
        same_checked(tag, &s);
    }
}

/// DELETE / UPDATE / INSERT OR REPLACE semantics on a
/// contentless_delete table: the totals and `%_docsize` follow, an
/// UPDATE must set every indexed column (the pin's "cannot UPDATE a
/// subset of columns"), one of UNINDEXED columns alone is a no-op,
/// 'delete' stays refused, a missing rowid deletes nothing, a row
/// still pending is flushed before its tombstone.
#[test]
fn contentless_delete_writes_alike() {
    let s = dumped(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b, c UNINDEXED, content='', contentless_delete=1)",
        "INSERT INTO t VALUES('x y', 'p', 'u1'), ('y z', 'q', 'u2'), ('z w', 'r', 'u3')",
        "UPDATE t SET a = 'new' WHERE rowid = 1",
        "UPDATE t SET a = 'n2', b = 'm2' WHERE rowid = 1",
        "SELECT rowid FROM t('n2 OR new OR x')",
        "UPDATE t SET c = 'zz' WHERE rowid = 2",
        "SELECT changes()",
        "UPDATE t SET rowid = 5 WHERE rowid = 2",
        "UPDATE t SET rowid = 6, a = 'six', b = 'b' WHERE rowid = 2",
        "SELECT rowid FROM t('six OR y')",
        "UPDATE t SET rowid = NULL, a = 'n', b = 'n' WHERE rowid = 6",
        "INSERT OR REPLACE INTO t(rowid, a, b) VALUES(6, 'r', 's'), (7, 'seven', 's')",
        "SELECT rowid FROM t('r OR six OR seven')",
        "INSERT INTO t(t, rowid, a, b) VALUES('delete', 6, 'r', 's')",
        "DELETE FROM t WHERE rowid = 99",
        "SELECT changes()",
        "BEGIN",
        "INSERT INTO t(rowid, a, b) VALUES(20, 'pending', 'row')",
        "DELETE FROM t WHERE rowid = 20",
        "INSERT INTO t(rowid, a, b) VALUES(21, 'pending', 'again')",
        "UPDATE t SET a = 'moved', b = 'on' WHERE rowid = 21",
        "COMMIT",
        "SELECT rowid FROM t('pending OR moved')",
        "SELECT id, hex(sz), origin FROM t_docsize",
        "SELECT hex(block) FROM t_data WHERE id = 1",
        CHECK,
    ]);
    same_checked("cdel-writes", &s);
}

/// Tombstones drive merges: `deletemerge` picks the level whose
/// tombstoned share is largest, automerge counts each tombstone as a
/// leaf of work, optimize and merge drop tombstoned entries and the
/// hash pages, and an incremental merge in progress gets tombstones
/// in both its input and output segments.
#[test]
fn tombstone_merges_alike() {
    for (tag, dm, am) in [("m1", 10, 4), ("m2", 1, 2), ("m3", 0, 8), ("m4", 50, 0)] {
        let dms = format!("INSERT INTO t(t, rank) VALUES('deletemerge', {dm})");
        let ams = format!("INSERT INTO t(t, rank) VALUES('automerge', {am})");
        let mut s = vec![
            "CREATE VIRTUAL TABLE t USING fts5(a, content='', contentless_delete=1)".to_string(),
            "INSERT INTO t(t, rank) VALUES('pgsz', 48)".into(),
            dms,
            ams,
        ];
        for b in 0..6 {
            s.push(format!(
                "INSERT INTO t(rowid, a) SELECT value, 'k' || (value % 5) || ' batch{b}' FROM generate_series({}, {})",
                b * 30 + 1,
                b * 30 + 30
            ));
            s.push(format!(
                "DELETE FROM t WHERE rowid % {} = 0 AND rowid <= {}",
                b + 2,
                b * 30 + 30
            ));
            s.extend(shadow());
        }
        s.push("INSERT INTO t(t, rank) VALUES('merge', 3)".into());
        s.push("DELETE FROM t WHERE rowid BETWEEN 40 AND 70".into());
        s.extend(shadow());
        s.push("INSERT INTO t(t, rank) VALUES('merge', -40)".into());
        s.extend(shadow());
        s.push("INSERT INTO t(t) VALUES('optimize')".into());
        s.push("SELECT count(*) FROM t WHERE t MATCH 'k1'".into());
        s.push(CHECK.into());
        same_checked(tag, &s);
    }
}

// ---------------- Secure delete ----------------

/// In-place deletes in every detail mode: a term's only entry (the
/// term goes, the next term on the page takes its key), entries first
/// on their page (the header), the last term of a page, doclists
/// spanning pages (pages emptied, `%_idx` rows deleted), position
/// lists overflowing onto later pages, prefix indexes; `version` 5
/// and the cookie after the first.
#[test]
fn secure_delete_alike() {
    for (tag, detail, pgsz, prefix) in [
        ("s1", "full", 32, ""),
        ("s2", "column", 40, ", prefix='1'"),
        ("s3", "none", 32, ", prefix='1 2'"),
        ("s4", "full", 4050, ", prefix='2'"),
        ("s5", "none", 64, ""),
        ("s6", "column", 100, ""),
    ] {
        let create = format!("CREATE VIRTUAL TABLE t USING fts5(a, b, detail={detail}{prefix})");
        let pg = format!("INSERT INTO t(t, rank) VALUES('pgsz', {pgsz})");
        let s = dumped(&[
            &create,
            &pg,
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "INSERT INTO t VALUES('only', 'x'), ('alpha beta', 'y'), ('beta gamma', 'z')",
            "DELETE FROM t WHERE rowid = 1",
            "DELETE FROM t WHERE rowid = 2",
            "INSERT INTO t SELECT 'w' || (value % 9) || ' common t' || value, 'tail' FROM generate_series(1, 120)",
            "INSERT INTO t SELECT 'common ' || group_concat('long', ' '), 'big' FROM generate_series(1, 150)",
            "INSERT INTO t SELECT 'common ' || group_concat('long', ' '), 'bigger' FROM generate_series(1, 400)",
            "DELETE FROM t WHERE rowid = 10",
            "DELETE FROM t WHERE rowid IN (11, 12, 13, 50, 51)",
            "DELETE FROM t WHERE rowid BETWEEN 60 AND 100",
            "DELETE FROM t WHERE rowid = 124",
            "SELECT rowid FROM t WHERE t MATCH 'long'",
            "UPDATE t SET a = 'replaced common' WHERE rowid % 7 = 3",
            "INSERT OR REPLACE INTO t(rowid, a, b) VALUES(20, 'twenty', 'w'), (21, 'common', 'w')",
            "SELECT count(*), sum(rowid) FROM t WHERE t MATCH 'common'",
            "SELECT rowid FROM t WHERE t MATCH 'w2 AND common' AND rowid > 30",
            "DELETE FROM t WHERE rowid % 2 = 0",
            "SELECT rowid, highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'w1' LIMIT 5",
            "INSERT INTO t(t) VALUES('optimize')",
            "DELETE FROM t WHERE rowid > 30",
            "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'common OR tail' ORDER BY rank",
            "CREATE VIRTUAL TABLE v USING fts5vocab(t, row)",
            "SELECT * FROM v",
            CHECK,
        ]);
        same_checked(tag, &s);
    }
}

/// secure-delete on other content modes and histories: the 'delete'
/// command of a contentless table, an external-content table kept by
/// triggers, delete markers written before the setting was turned on
/// (the newest entry of a row is removed, a marker stripped of its
/// positions), the setting turned off again, and 'rebuild' /
/// 'delete-all' resetting `version` to 4 within the statement.
#[test]
fn secure_delete_histories_alike() {
    let s = dumped(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, content='')",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
        "INSERT INTO t(rowid, a) VALUES(1, 'x y'), (2, 'y z'), (3, 'z w')",
        "INSERT INTO t(t, rowid, a) VALUES('delete', 1, 'x y')",
        "INSERT INTO t(t, rowid, a) VALUES('delete', 1, 'x y')",
        "INSERT INTO t(t, rowid, a) VALUES('delete', 3, 'z w')",
        "SELECT rowid FROM t('y OR z OR w')",
        "INSERT INTO t(t) VALUES('delete-all')",
        "INSERT INTO t(rowid, a) VALUES(4, 'again')",
        "INSERT INTO t(t, rowid, a) VALUES('delete', 4, 'again')",
    ]);
    same_checked("sd-contentless", &s);
    let s = dumped(&[
        "CREATE TABLE ext(id INTEGER PRIMARY KEY, a, b)",
        "CREATE VIRTUAL TABLE t USING fts5(a, b, content=ext, content_rowid=id, prefix='1')",
        "CREATE TRIGGER ai AFTER INSERT ON ext BEGIN INSERT INTO t(rowid, a, b) VALUES(new.id, new.a, new.b); END",
        "CREATE TRIGGER ad AFTER DELETE ON ext BEGIN INSERT INTO t(t, rowid, a, b) VALUES('delete', old.id, old.a, old.b); END",
        "CREATE TRIGGER au AFTER UPDATE ON ext BEGIN INSERT INTO t(t, rowid, a, b) VALUES('delete', old.id, old.a, old.b); INSERT INTO t(rowid, a, b) VALUES(new.id, new.a, new.b); END",
        "INSERT INTO t(t, rank) VALUES('pgsz', 40)",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
        "INSERT INTO ext(a, b) SELECT 'w' || (value % 6), 'v' || value FROM generate_series(1, 90)",
        "DELETE FROM ext WHERE id % 3 = 0",
        "UPDATE ext SET a = 'changed' WHERE id % 5 = 1",
        "SELECT rowid FROM t WHERE t MATCH 'changed OR w2' LIMIT 8",
        "INSERT INTO t(t, rank) VALUES('integrity-check', 1)",
    ]);
    same_checked("sd-external", &s);
    for (tag, detail) in [
        ("sd-old-f", "full"),
        ("sd-old-c", "column"),
        ("sd-old-n", "none"),
    ] {
        let create = format!("CREATE VIRTUAL TABLE t USING fts5(a, detail={detail})");
        let s = dumped(&[
            &create,
            "INSERT INTO t(t, rank) VALUES('pgsz', 32)",
            "INSERT INTO t SELECT 'k' || (value % 4) || ' all', 'x' FROM generate_series(1, 60)",
            "DELETE FROM t WHERE rowid % 3 = 0",
            "UPDATE t SET a = 'k1 all again' WHERE rowid % 7 = 0",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "DELETE FROM t WHERE rowid % 7 = 0",
            "DELETE FROM t WHERE rowid % 4 = 1",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 0)",
            "DELETE FROM t WHERE rowid % 5 = 2",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "UPDATE t SET a = 'final' WHERE rowid < 20",
            "INSERT INTO t(t) VALUES('rebuild')",
            "DELETE FROM t WHERE rowid = 2",
            "SELECT count(*) FROM t WHERE t MATCH 'all'",
            CHECK,
        ]);
        same_checked(tag, &s);
    }
}

/// Transactions, savepoints and statement rollbacks over pending
/// tombstones and secure deletes: ROLLBACK TO drops them (the cached
/// structure with them), RELEASE keeps them, a failing statement in a
/// transaction restores its start.
#[test]
fn delete_transactions_alike() {
    for (tag, create, sd) in [
        (
            "tx-cdel",
            "CREATE VIRTUAL TABLE t USING fts5(a, content='', contentless_delete=1)",
            "SELECT 1",
        ),
        (
            "tx-sd",
            "CREATE VIRTUAL TABLE t USING fts5(a)",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
        ),
    ] {
        let s = dumped(&[
            create,
            sd,
            "INSERT INTO t(t, rank) VALUES('pgsz', 64)",
            "INSERT INTO t(rowid, a) SELECT value, 'w' || (value % 5) FROM generate_series(1, 50)",
            "BEGIN",
            "DELETE FROM t WHERE rowid = 3",
            "SAVEPOINT a",
            "DELETE FROM t WHERE rowid IN (4, 5, 6)",
            "SELECT rowid FROM t('w4') LIMIT 3",
            "ROLLBACK TO a",
            "SELECT rowid FROM t('w4') LIMIT 3",
            "DELETE FROM t WHERE rowid % 9 = 0",
            "RELEASE a",
            "INSERT INTO t(rowid, a) VALUES(51, 'w1'), (52, 'w2')",
            "DELETE FROM t WHERE rowid = 52",
            "COMMIT",
            "BEGIN",
            "DELETE FROM t WHERE rowid > 40",
            "ROLLBACK",
            "SELECT count(*) FROM t('w1 OR w2')",
            CHECK,
        ]);
        same_checked(tag, &s);
    }
}

// ---------------- Regressions ----------------

/// One case per bug the fuzzing found (several were v0.16 paths the
/// delete writes exposed).
#[test]
fn regressions() {
    // A row with no tokens counts into the next segment's nEntry
    // (`nPendingRow` outlives a commit that flushed nothing).
    same_checked(
        "r-pendingrow",
        &dumped(&[
            "CREATE VIRTUAL TABLE t USING fts5(c0, content='', contentless_delete=1)",
            "INSERT INTO t(rowid, c0) VALUES (3, '')",
            "DELETE FROM t WHERE rowid = 3",
            "INSERT INTO t(rowid, c0) VALUES (4, 'iota eta')",
        ]),
    );
    // A rebuilt tombstone hash writes the structure at once.
    same_checked(
        "r-rebuildwrite",
        &dumped(&[
            "CREATE VIRTUAL TABLE t USING fts5(c0, content='', contentless_delete=1)",
            "INSERT INTO t(rowid, c0) VALUES (4294967302, 'aa')",
            "BEGIN",
            "UPDATE t SET c0 = 'nu' WHERE rowid = 4294967302",
            "SELECT id, hex(block) FROM t_data",
            "COMMIT",
        ]),
    );
    // `rowid IN (x)` is `rowid = x`: no statement-journal flush.
    for create in [
        "CREATE VIRTUAL TABLE t USING fts5(c0, c1, content='', contentless_delete=1)",
        "CREATE VIRTUAL TABLE t USING fts5(c0, c1)",
    ] {
        same_checked(
            "r-inlist",
            &dumped(&[
                create,
                "INSERT INTO t(rowid, c0, c1) VALUES (21, 'gamma sig', 'rho'), (29, 'iota eps', '')",
                "BEGIN",
                "DELETE FROM t WHERE rowid IN (21)",
                "DELETE FROM t WHERE rowid IN (22)",
                "INSERT INTO t(rowid, c0, c1) VALUES (30, 'a', 'b'), (31, 'c', 'd')",
                "DELETE FROM t WHERE rowid IN (+30)",
                "COMMIT",
            ]),
        );
    }
    // integrity-check of an external-content table checks the content
    // only with rank 1.
    same(
        "r-extcheck",
        &v(&[
            "CREATE TABLE ext(id INTEGER PRIMARY KEY, c0)",
            "CREATE TRIGGER ai AFTER INSERT ON ext BEGIN INSERT INTO t(rowid, c0) VALUES (new.id, new.c0); END",
            "CREATE VIRTUAL TABLE t USING fts5(c0, content=ext, content_rowid=id)",
            "INSERT INTO ext VALUES(27, 'gamma alpha')",
            "INSERT OR REPLACE INTO ext(id, c0) VALUES (27, 'eta')",
            CHECK,
            "INSERT INTO t(t, rank) VALUES('integrity-check', 0)",
            "INSERT INTO t(t, rank) VALUES('integrity-check', 1)",
            "SELECT 'done'",
        ]),
    );
    // bm25 over rows of no tokens is NULL (0 / 0), as rank too.
    same(
        "r-bm25nan",
        &v(&[
            "CREATE VIRTUAL TABLE t USING fts5(a, b, content='')",
            "INSERT INTO t(rowid, a, b) VALUES(1, 'x', ''), (2, '', 'x')",
            "INSERT INTO t(t, rowid, a, b) VALUES('delete', 1, 'x', '')",
            "INSERT INTO t(t, rowid, a, b) VALUES('delete', 2, '', 'x')",
            "INSERT INTO t(rowid, a, b) VALUES(3, '', '')",
            "SELECT rowid, bm25(t), rank FROM t WHERE t MATCH 'x'",
        ]),
    );
    // The `version` a statement holds: a 'rebuild' writes 4 under it,
    // a flush inside the statement does not move it back to 5, the
    // commit's flush (or the next statement's) does.
    for (tag, hs) in [("r-ver-commit", 1_000_000), ("r-ver-stmt", 1)] {
        let hs = format!("INSERT INTO t(t, rank) VALUES('hashsize', {hs})");
        same_checked(
            tag,
            &dumped(&[
                "CREATE VIRTUAL TABLE t USING fts5(c0)",
                "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
                "INSERT INTO t(rowid, c0) SELECT 1, group_concat('aa', ' ') FROM generate_series(1, 70)",
                "INSERT INTO t(rowid, c0) VALUES(2, 'x')",
                &hs,
                "INSERT INTO t(t) VALUES('rebuild')",
                "DELETE FROM t WHERE rowid = 2",
            ]),
        );
    }
    // A rolled-back `secure-delete` stays on in the connection.
    same(
        "r-sdsticky",
        &dumped(&[
            "CREATE VIRTUAL TABLE t USING fts5(c0)",
            "BEGIN",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "ROLLBACK",
            "INSERT INTO t VALUES('a b'), ('c d')",
            "UPDATE t SET c0 = 'e' WHERE rowid = 1",
        ]),
    );
    // detail=none: a row deleted and written again in the same pending
    // batch keeps the rows after it.
    same_checked(
        "r-none-readd",
        &dumped(&[
            "CREATE VIRTUAL TABLE t USING fts5(c0, detail=none)",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "INSERT INTO t(rowid, c0) VALUES (1, 'a b')",
            "BEGIN",
            "INSERT INTO t(rowid, c0) VALUES (5, 'a c')",
            "UPDATE t SET c0 = 'a d' WHERE rowid = 5",
            "INSERT INTO t(rowid, c0) VALUES (8, 'a e')",
            "COMMIT",
            "SELECT rowid FROM t('a')",
        ]),
    );
    // A size field of a long position list whose first byte is odd
    // (the pin tests its low bit) moves `version` to 5.
    same_checked(
        "r-oddsize",
        &dumped(&[
            "CREATE VIRTUAL TABLE t USING fts5(c0)",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "INSERT INTO t(rowid, c0) SELECT 1, group_concat('aa', ' ') FROM generate_series(1, 70)",
            "INSERT INTO t(rowid, c0) SELECT 2, group_concat('aa', ' ') FROM generate_series(1, 170)",
        ]),
    );
    // An UPDATE's new rowid NULL is the pin's "datatype mismatch".
    same(
        "r-nullrowid",
        &v(&[
            "CREATE VIRTUAL TABLE t USING fts5(a)",
            "INSERT INTO t(rowid, a) VALUES(3, 'x')",
            "UPDATE t SET rowid = NULL WHERE rowid = 3",
            "SELECT rowid, a FROM t",
        ]),
    );
    // A contentless table: an UPDATE of UNINDEXED columns alone is a
    // no-op; of an indexed one, refused.
    let e = same(
        "r-contentless-upd",
        &v(&[
            "CREATE VIRTUAL TABLE c USING fts5(a, b UNINDEXED, content='')",
            "INSERT INTO c(rowid, a) VALUES(3, 'x'), (7, 'y')",
            "UPDATE c SET b = 'z' WHERE rowid = 3",
            "SELECT last_insert_rowid(), changes()",
            "UPDATE c SET b = 'z'",
            "SELECT changes()",
            "UPDATE c SET a = 'z' WHERE rowid = 3",
            "UPDATE c SET rowid = 3 WHERE rowid = 3",
            "SELECT changes()",
        ]),
    );
    assert!(
        matches!(
            e,
            Some(Error::Schema {
                kind: SchemaKind::Fts5Contentless
            })
        ),
        "{e:?}"
    );
    let e = same(
        "r-cdel-subset",
        &v(&[
            "CREATE VIRTUAL TABLE t USING fts5(a, b, content='', contentless_delete=1)",
            "INSERT INTO t VALUES('x', 'y')",
            "UPDATE t SET a = 'z' WHERE rowid = 1",
        ]),
    );
    assert!(
        matches!(
            e,
            Some(Error::Schema {
                kind: SchemaKind::Fts5ContentlessSubset
            })
        ),
        "{e:?}"
    );
}

// ---------------- Review fixes ----------------

/// A position list of 64..127 bytes (its size varint's first byte is
/// odd, so the pin's flush looks for an entry to remove) for row `r`.
fn long_row(r: i64, n: usize) -> String {
    format!(
        "INSERT OR REPLACE INTO t(rowid, a) SELECT {r}, group_concat('x', ' ') FROM generate_series(1, {n})"
    )
}

/// secure-delete on a contentless_delete table: an entry the segment's
/// tombstone hides stays where it is (the pin's segment iterator skips
/// tombstoned rowids; hipp cut it out of the leaf).
#[test]
fn tombstoned_entries_stay_under_secure_delete() {
    for detail in ["full", "column", "none"] {
        let create = format!(
            "CREATE VIRTUAL TABLE t USING fts5(a, content='', contentless_delete=1, detail={detail})"
        );
        let base = [
            create.as_str(),
            "INSERT INTO t(rowid, a) VALUES(1, 'y')",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
        ];
        let replace = long_row(2, 70);
        let replace3 = long_row(3, 75);
        let replace4 = long_row(4, 90);
        let mut w: Vec<&str> = base.to_vec();
        w.insert(2, &replace);
        w.extend([
            replace.as_str(),
            "SELECT rowid FROM t('x')",
            replace3.as_str(),
            replace4.as_str(),
            "UPDATE t SET a = (SELECT group_concat('x', ' ') FROM generate_series(1, 66)) WHERE rowid = 3",
            "DELETE FROM t WHERE rowid = 4",
            replace4.as_str(),
            "BEGIN",
            "DELETE FROM t WHERE rowid = 2",
            replace.as_str(),
            "COMMIT",
            "SELECT rowid FROM t('x')",
            CHECK,
            "INSERT INTO t(t) VALUES('optimize')",
            "SELECT rowid FROM t('x')",
        ]);
        same_checked(&format!("rf-tomb-sd-{detail}"), &dumped(&w));
    }
}

/// The pin forgets a rolled-back `secure-delete` setting whenever it
/// reparses the schema (ALTER TABLE, VACUUM, a rollback that undid a
/// schema change, `PRAGMA writable_schema = RESET`); a CREATE or DROP
/// on its own, or an ALTER that fails, keeps it.
#[test]
fn schema_reload_forgets_rolled_back_setting() {
    let mids: &[&[&str]] = &[
        &["SELECT 1"],
        &["CREATE TABLE z2(y)"],
        &["DROP TABLE z"],
        &["ALTER TABLE z ADD COLUMN q"],
        &["ALTER TABLE z RENAME TO zz"],
        &["ALTER TABLE z RENAME COLUMN y TO yy"],
        &["ALTER TABLE z DROP COLUMN w"],
        &["ALTER TABLE z ADD COLUMN q NOT NULL"],
        &["ALTER TABLE t RENAME TO t2", "ALTER TABLE t2 RENAME TO t"],
        &["VACUUM"],
        &["BEGIN", "CREATE TABLE z3(a)", "ROLLBACK"],
        &["BEGIN", "CREATE TABLE z3(a)", "COMMIT"],
        &["BEGIN", "DROP TABLE z", "ROLLBACK"],
        &["BEGIN", "INSERT INTO z VALUES(5, 5)", "ROLLBACK"],
        &[
            "SAVEPOINT a",
            "CREATE TABLE z3(a)",
            "ROLLBACK TO a",
            "RELEASE a",
        ],
        &[
            "SAVEPOINT a",
            "INSERT INTO z VALUES(5, 5)",
            "ROLLBACK TO a",
            "RELEASE a",
        ],
        &[
            "CREATE TABLE u(x UNIQUE)",
            "BEGIN",
            "CREATE TABLE z3(a)",
            "INSERT INTO u VALUES(1)",
            "INSERT OR ROLLBACK INTO u VALUES(1)",
        ],
        &[
            "CREATE TABLE u(x UNIQUE)",
            "BEGIN",
            "INSERT INTO u VALUES(1)",
            "INSERT OR ROLLBACK INTO u VALUES(1)",
        ],
        &["PRAGMA writable_schema = RESET"],
    ];
    for (i, mid) in mids.iter().enumerate() {
        let mut w = vec![
            "CREATE VIRTUAL TABLE t USING fts5(a, b)",
            "CREATE TABLE z(y, w)",
            "INSERT INTO z VALUES(1, 1)",
            "BEGIN",
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
            "ROLLBACK",
        ];
        w.extend(mid.iter().copied());
        w.extend([
            "INSERT INTO t(rowid, a, b) SELECT value, 'w', 'c' FROM generate_series(1, 20)",
            "DELETE FROM t WHERE rowid < 5",
            "SELECT rowid FROM t('w')",
        ]);
        same(&format!("rf-reload{i}"), &dumped(&w));
    }
}

/// A contentless UPDATE compares the rowids as `sqlite3_value_int64`
/// reads them: `rowid = '1'` on row 1 does not move it (an UPDATE of
/// UNINDEXED columns alone stays the no-op); a non-integer that reads
/// as the old rowid is "datatype mismatch" only after the contentless
/// rules.
#[test]
fn contentless_update_rowid_values() {
    let tables = [
        "a, b UNINDEXED, c, content='', contentless_delete=1",
        "a, b UNINDEXED, c, content=''",
        "a, b UNINDEXED, c",
    ];
    let sets = ["b = 'z'", "a = 'q'", "a = 'q', c = 'r'"];
    let rids = [
        "'1'", "' 1'", "'+1'", "'1.0'", "1.0", "1.5", "'1e0'", "'x'", "NULL", "x'01'", "'2'",
        "2.0", "' 0x1'", "1", "2", "1e30", "0", "0.0", "''", "x''", "'-1'", "-1.0",
    ];
    // (Row 0 too: NULL, '' and x'' read as rowid 0 there, so they do
    // not move it, and the no-op is "datatype mismatch" for them;
    // review regression.)
    for old in [1, 0, -1] {
        for (ti, tbl) in tables.iter().enumerate() {
            for (si, set) in sets.iter().enumerate() {
                for (ri, rid) in rids.iter().enumerate() {
                    let create = format!("CREATE VIRTUAL TABLE u USING fts5({tbl})");
                    let ins = format!("INSERT INTO u(rowid, a, b, c) VALUES({old}, 'x', 'y', 'w')");
                    let upd = format!("UPDATE u SET {set}, rowid = {rid} WHERE rowid = {old}");
                    same(
                        &format!("rf-upd{old}-{ti}-{si}-{ri}"),
                        &v(&[
                            &create,
                            &ins,
                            &upd,
                            "SELECT changes()",
                            "SELECT rowid, a, b, c FROM u",
                        ]),
                    );
                }
            }
        }
    }
}

/// OR FAIL keeps the rows an fts5 write made before the conflict (and
/// their index entries); OR ROLLBACK ends the transaction (both acted
/// as ABORT).
#[test]
fn fts5_conflict_resolutions() {
    let body = [
        "INSERT INTO t(rowid, a) VALUES(3, 'x'), (4, 'y')",
        "BEGIN",
        "DELETE FROM t WHERE rowid = 3",
        "INSERT INTO t(rowid, a) VALUES(5, '')",
        "INSERT OR ROLLBACK INTO t(rowid, a) VALUES(6, ''), (4, 'dup')",
        "COMMIT",
        "SELECT rowid FROM t",
        "INSERT OR FAIL INTO t(rowid, a) VALUES(8, ''), (9, 'k'), (4, 'q'), (11, 'q')",
        "SELECT rowid FROM t",
        "BEGIN",
        "INSERT OR FAIL INTO t(rowid, a) VALUES(12, 'p'), (4, 'q'), (13, 'q')",
        "SAVEPOINT s",
        "INSERT OR FAIL INTO t(rowid, a) VALUES(14, 'm'), (4, 'q')",
        "ROLLBACK TO s",
        "RELEASE s",
        "INSERT INTO t(rowid, a) VALUES(15, 'z')",
        "COMMIT",
        "UPDATE OR FAIL t SET rowid = rowid + 1 WHERE rowid IN (3, 10, 11)",
        "UPDATE OR ROLLBACK t SET rowid = 4 WHERE rowid = 3",
        "DELETE FROM t WHERE rowid = 9",
        "INSERT OR ROLLBACK INTO t(rowid, a) VALUES(16, 'a'), (4, 'q')",
        "SELECT rowid FROM t",
        "SELECT rowid FROM t('q OR p OR k OR z OR a')",
        "SELECT id, hex(sz) FROM t_docsize",
    ];
    for (i, opts) in [
        "",
        ", content='', contentless_delete=1",
        ", detail=column",
        ", detail=none",
        ", prefix='1'",
    ]
    .iter()
    .enumerate()
    {
        for sd in [false, true] {
            let create = format!("CREATE VIRTUAL TABLE t USING fts5(a{opts})");
            let mut w = vec![create.as_str()];
            if sd {
                w.push("INSERT INTO t(t, rank) VALUES('secure-delete', 1)");
            }
            w.extend(body);
            same_checked(&format!("rf-conflict{i}-{sd}"), &dumped(&w));
        }
    }
}

/// Deletes of many rows in one statement (the tombstone pages and the
/// secure-delete walk read a page or so per row, not the index; the
/// walk seeks the term's page and, inside a long doclist, the rowid's):
/// the same bytes as the pin over several page sizes.
#[test]
fn bulk_deletes_alike() {
    for (opts, sd) in [
        (", content='', contentless_delete=1", false),
        (", content='', contentless_delete=1", true),
        ("", true),
        (", detail=column", true),
        (", detail=none", true),
        (", prefix='1 2'", true),
    ] {
        for pgsz in [64, 200, 4050] {
            let create = format!("CREATE VIRTUAL TABLE t USING fts5(a{opts})");
            let pg = format!("INSERT INTO t(t, rank) VALUES('pgsz', {pgsz})");
            let mut w = vec![create, pg];
            if sd {
                w.push("INSERT INTO t(t, rank) VALUES('secure-delete', 1)".into());
            }
            w.extend(v(&[
                "INSERT INTO t(rowid, a) SELECT value, 'x ' || value || ' y' || (value % 7) FROM generate_series(1, 1500)",
                "INSERT INTO t(rowid, a) SELECT value, 'x z' || (value % 3) FROM generate_series(1501, 1800)",
                "DELETE FROM t WHERE rowid % 2 = 0",
                "SELECT count(*) FROM t('x')",
                "DELETE FROM t WHERE rowid % 3 = 1",
                "SELECT count(*), sum(rowid) FROM t('y3 OR z1')",
                "UPDATE t SET a = 'x w' WHERE rowid % 5 = 0",
                "SELECT count(*) FROM t('w')",
            ]));
            same_checked(&format!("rf-bulk{}-{sd}-{pgsz}", opts.len()), &w);
        }
    }
}

// ---------------- Generated workloads ----------------

const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "eps", "zeta", "eta", "theta", "iota", "kappa", "lam", "mu",
    "nu", "xi", "omi", "pi", "rho", "sig", "tau", "ups",
];

fn doc(r: &mut Rng) -> String {
    let n = r.below(9) as usize;
    let k = *r.pick(&[5usize, 10, 20]);
    (0..n)
        .map(|_| *r.pick(&WORDS[..k]))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One generated workload on a contentless_delete or secure-delete
/// table (normal, external with triggers, or contentless with the
/// 'delete' command): random configuration (detail, prefix, pgsz,
/// UNINDEXED, merge settings), inserts (explicit, huge, 8-byte and
/// negative rowids), deletes, updates, replaces, optimize / merge /
/// rebuild / delete-all, transactions and savepoints, queries and the
/// shadow tables along the way.
fn workload(seed: u64, n: usize) -> Vec<String> {
    let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let ncol = r.range(1, 3) as usize;
    let cols: Vec<String> = (0..ncol).map(|i| format!("c{i}")).collect();
    let detail = *r.pick(&["full", "full", "column", "none"]);
    let mode = *r.pick(&["cdel", "cdel", "sec", "sec", "secext", "secnone"]);
    let mut opts = format!(", detail={detail}");
    if r.chance(30) {
        opts.push_str(", prefix='1 2'");
    }
    let un = ncol > 1 && r.chance(30);
    let defs: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if un && i == ncol - 1 {
                format!("{c} UNINDEXED")
            } else {
                c.clone()
            }
        })
        .collect();
    let cl = cols.join(", ");
    let mut out = Vec::new();
    match mode {
        "cdel" => opts.push_str(", content='', contentless_delete=1"),
        "secnone" => opts.push_str(", content=''"),
        "secext" => {
            let nc: Vec<String> = cols.iter().map(|c| format!("new.{c}")).collect();
            let oc: Vec<String> = cols.iter().map(|c| format!("old.{c}")).collect();
            let (nc, oc) = (nc.join(", "), oc.join(", "));
            out.push(format!("CREATE TABLE ext(id INTEGER PRIMARY KEY, {cl})"));
            out.push(format!("CREATE TRIGGER ai AFTER INSERT ON ext BEGIN INSERT INTO t(rowid, {cl}) VALUES (new.id, {nc}); END"));
            out.push(format!("CREATE TRIGGER ad AFTER DELETE ON ext BEGIN INSERT INTO t(t, rowid, {cl}) VALUES ('delete', old.id, {oc}); END"));
            out.push(format!("CREATE TRIGGER au AFTER UPDATE ON ext BEGIN INSERT INTO t(t, rowid, {cl}) VALUES ('delete', old.id, {oc}); INSERT INTO t(rowid, {cl}) VALUES (new.id, {nc}); END"));
            opts.push_str(", content=ext, content_rowid=id");
        }
        _ => {}
    }
    out.push(format!(
        "CREATE VIRTUAL TABLE t USING fts5({}{opts})",
        defs.join(", ")
    ));
    out.push(format!(
        "INSERT INTO t(t, rank) VALUES('pgsz', {})",
        r.pick(&[32, 40, 48, 64, 100, 128, 4050])
    ));
    // (detail=none tables get the setting from the start and keep it:
    // a delete-and-reinsert marker written without it is one the pin's
    // later secure delete mis-parses, corrupting its own page.)
    // (Tombstones and secure deletes together: review fix, a secure
    // delete cut out an entry the segment's tombstone hides.)
    if (mode != "cdel" || r.chance(50)) && (r.chance(80) || detail == "none") {
        out.push("INSERT INTO t(t, rank) VALUES('secure-delete', 1)".into());
    }
    for (k, vals) in [
        ("automerge", &[0, 2, 4, 8][..]),
        ("crisismerge", &[2, 3, 4, 16][..]),
        ("usermerge", &[2, 4][..]),
        ("deletemerge", &[0, 5, 10, 50][..]),
        ("hashsize", &[50, 200, 1_000_000][..]),
    ] {
        if r.chance(30) {
            out.push(format!(
                "INSERT INTO t(t, rank) VALUES('{k}', {})",
                r.pick(vals)
            ));
        }
    }
    let tgt = if mode == "secext" { "ext" } else { "t" };
    let rid = if mode == "secext" { "id" } else { "rowid" };
    let mut live: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
    let mut snap = live.clone();
    let mut intx = false;
    let mut nalter = 0;
    let lit = |vs: &[String]| {
        vs.iter()
            .map(|v| format!("'{v}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for _ in 0..n {
        let k = r.below(100);
        if k < 32 {
            let m = *r.pick(&[1, 1, 1, 2, 3, 5]);
            let mut rows: Vec<(i64, Vec<String>)> = Vec::new();
            for _ in 0..m {
                let i = if r.chance(30) {
                    if r.chance(66) {
                        r.range(1, 60)
                    } else {
                        let near = (1 << 33) + r.range(1, 9);
                        *r.pick(&[0, -3, (1 << 32) + 5, near, 9_000_000_000])
                    }
                } else {
                    live.keys().next_back().copied().unwrap_or(0).max(0) + r.range(1, 3)
                };
                let mut vs: Vec<String> = (0..ncol).map(|_| doc(&mut r)).collect();
                if r.chance(8) {
                    vs[0] = (0..r.range(40, 300))
                        .map(|_| *r.pick(&["aa", "bb", "aa"]))
                        .collect::<Vec<_>>()
                        .join(" ");
                }
                if !rows.iter().any(|x| x.0 == i) {
                    rows.push((i, vs));
                }
            }
            let mut verb = "INSERT";
            if (mode == "cdel" || mode == "sec") && r.chance(20) {
                verb = "INSERT OR REPLACE";
            }
            if mode == "secext" {
                verb = if r.chance(10) {
                    "INSERT OR REPLACE"
                } else {
                    "INSERT OR IGNORE"
                };
            }
            if (mode == "secnone" || mode == "cdel") && verb == "INSERT" {
                rows.retain(|x| !live.contains_key(&x.0));
                if rows.is_empty() {
                    continue;
                }
            }
            let vals: Vec<String> = rows
                .iter()
                .map(|(i, vs)| format!("({i}, {})", lit(vs)))
                .collect();
            out.push(format!(
                "{verb} INTO {tgt}({rid}, {cl}) VALUES {}",
                vals.join(", ")
            ));
            for (i, vs) in rows {
                if verb == "INSERT OR IGNORE" && live.contains_key(&i) {
                    continue;
                }
                live.insert(i, vs);
            }
        } else if k < 50 {
            if live.is_empty() {
                continue;
            }
            let keys: Vec<i64> = live.keys().copied().collect();
            if mode == "secnone" {
                let i = *r.pick(&keys);
                let vs = live.remove(&i).unwrap_or_default();
                out.push(format!(
                    "INSERT INTO t(t, rowid, {cl}) VALUES('delete', {i}, {})",
                    lit(&vs)
                ));
            } else if r.chance(60) {
                let ids: Vec<i64> = (0..r.range(1, 3)).map(|_| *r.pick(&keys)).collect();
                for i in &ids {
                    live.remove(i);
                }
                let ids: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
                out.push(format!(
                    "DELETE FROM {tgt} WHERE {rid} IN ({})",
                    ids.join(", ")
                ));
            } else {
                let m = r.range(2, 5);
                let x = r.range(0, m - 1);
                out.push(format!("DELETE FROM {tgt} WHERE {rid} % {m} = {x}"));
                live.retain(|i, _| i % m != x);
            }
        } else if k < 62 {
            if live.is_empty() || mode == "secnone" {
                continue;
            }
            let keys: Vec<i64> = live.keys().copied().collect();
            let i = *r.pick(&keys);
            let vs: Vec<String> = (0..ncol).map(|_| doc(&mut r)).collect();
            if mode == "cdel" || r.chance(50) {
                if mode == "cdel" && r.chance(15) {
                    out.push(format!(
                        "UPDATE {tgt} SET {} = 'zz' WHERE {rid} = {i}",
                        cols[ncol - 1]
                    ));
                } else {
                    let sets: Vec<String> = cols
                        .iter()
                        .zip(&vs)
                        .map(|(c, v)| format!("{c} = '{v}'"))
                        .collect();
                    out.push(format!(
                        "UPDATE {tgt} SET {} WHERE {rid} = {i}",
                        sets.join(", ")
                    ));
                    live.insert(i, vs);
                }
            } else {
                let c = r.below(ncol as u64) as usize;
                out.push(format!(
                    "UPDATE {tgt} SET {} = '{}' WHERE {rid} = {i}",
                    cols[c], vs[c]
                ));
                if let Some(row) = live.get_mut(&i) {
                    row[c] = vs[c].clone();
                }
            }
        } else if k < 67 {
            out.push(if r.chance(50) {
                "INSERT INTO t(t) VALUES('optimize')".into()
            } else {
                format!(
                    "INSERT INTO t(t, rank) VALUES('merge', {})",
                    r.pick(&[-5, 1, 4, 30, 500])
                )
            });
        } else if k < 69 {
            if mode == "sec" || mode == "secext" {
                out.push("INSERT INTO t(t) VALUES('rebuild')".into());
            } else if mode == "secnone" && r.chance(30) {
                out.push("INSERT INTO t(t) VALUES('delete-all')".into());
                live.clear();
            }
        } else if k < 71 {
            if !intx && detail != "none" && r.chance(30) {
                // A setting rolled back stays on in the connection until
                // the schema is reparsed (review fix: hipp kept it past
                // ALTER TABLE and VACUUM).
                out.push("BEGIN".into());
                out.push(format!(
                    "INSERT INTO t(t, rank) VALUES('secure-delete', {})",
                    r.pick(&[0, 1])
                ));
                out.push("ROLLBACK".into());
                out.push(REREAD.into());
                nalter += 1;
                // (hipp refuses to rename a virtual table a trigger
                // names: the external-content workloads rename none.)
                let z = if mode == "secext" {
                    *r.pick(&[0, 1, 3])
                } else {
                    *r.pick(&[0, 1, 2, 3])
                };
                match z {
                    0 => out.push("VACUUM".into()),
                    1 => {
                        out.push(format!("CREATE TABLE z{nalter}(y)"));
                        out.push(format!("ALTER TABLE z{nalter} ADD COLUMN q"));
                    }
                    2 => {
                        out.push("ALTER TABLE t RENAME TO tq".into());
                        out.push("ALTER TABLE tq RENAME TO t".into());
                    }
                    _ => out.push(format!("CREATE TABLE z{nalter}(y)")),
                }
                // (A reparse leaves the pin's connection holding cookie
                // -1 until the structure is read: the honest gap of
                // REREAD.)
                out.push(REREAD.into());
            } else if !intx && detail != "none" {
                out.push(format!(
                    "INSERT INTO t(t, rank) VALUES('secure-delete', {})",
                    r.pick(&[0, 1, 1])
                ));
            }
        } else if k < 75 && !intx {
            out.push("BEGIN".into());
            intx = true;
            snap = live.clone();
        } else if k < 79 && intx {
            let c = *r.pick(&["COMMIT", "COMMIT", "ROLLBACK"]);
            out.push(c.into());
            intx = false;
            if c == "ROLLBACK" {
                live = snap.clone();
                out.push(REREAD.into());
            }
        } else if k < 81 && intx {
            let mut sps = vec!["SAVEPOINT s1", "RELEASE s1"];
            if mode != "secnone" {
                sps.push("ROLLBACK TO s1");
            }
            let sp = r.pick(&sps).to_string();
            let back = sp.starts_with("ROLLBACK");
            out.push(sp);
            if back {
                out.push(REREAD.into());
            }
        } else {
            let w = *r.pick(&WORDS[..10]);
            let w2 = *r.pick(&WORDS[..10]);
            let qs: Vec<String> = if detail == "full" {
                vec![
                    w.into(),
                    format!("{w}*"),
                    format!("{w} OR {w2}"),
                    format!("{w} AND {w2}"),
                    "aa".into(),
                    "bb".into(),
                    "a*".into(),
                    format!("aa NOT {w}"),
                ]
            } else {
                vec![
                    w.into(),
                    format!("{w}*"),
                    format!("{w} OR {w2}"),
                    format!("{w} {w2}"),
                ]
            };
            let q = r.pick(&qs).clone();
            let mut sels = vec!["rowid", "rowid, bm25(t)", "count(*)"];
            if mode == "sec" || mode == "secext" {
                sels.push("rowid, highlight(t, 0, '[', ']')");
            }
            let sel = *r.pick(&sels);
            out.push(format!("SELECT {sel} FROM t WHERE t MATCH '{q}'"));
        }
        if r.chance(30) {
            out.extend(shadow());
        }
    }
    if intx {
        out.push("COMMIT".into());
    }
    out.extend(shadow());
    out.push("SELECT id, hex(sz) FROM t_docsize".into());
    out.push("CREATE VIRTUAL TABLE v USING fts5vocab(t, row)".into());
    out.push("SELECT * FROM v".into());
    out.push(CHECK.into());
    out
}

/// The first generated seed (`HIPP_FTS5_DELETE_FROM`, default 0), to
/// run a range of a larger corpus.
fn first_seed() -> u64 {
    std::env::var("HIPP_FTS5_DELETE_FROM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn generated_delete_workloads_match_byte_for_byte() {
    let n: u64 = std::env::var("HIPP_FTS5_DELETE_CORPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in first_seed()..n {
        let w = workload(seed, 60);
        let h = common::temp_db(&format!("v17fd-gen{seed}-h"));
        let o = common::temp_db(&format!("v17fd-gen{seed}-o"));
        same_on(&h, &o, &w);
        cross_check(&format!("seed {seed}"), &h, &o);
    }
}

/// Page-level secure deletes and tombstones: long doclists of few
/// distinct terms over small pages (doclist indexes, terms at page
/// ends, position lists over several pages), rows deleted singly, in
/// sets and in runs, updates, merges, AND / phrase / prefix queries
/// that seek through the edited pages.
fn page_workload(seed: u64) -> Vec<String> {
    let mut r = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
    let detail = *r.pick(&["full", "column", "none"]);
    let words = ["a", "b", "c", "d", "e", "f", "gg", "hh", "iii", "jjjj"];
    let nw = *r.pick(&[2usize, 3, 5, 10]);
    let cdel = r.chance(30);
    let mut opts = format!("detail={detail}");
    if r.chance(30) {
        opts.push_str(", prefix='1'");
    }
    if cdel {
        opts.push_str(", content='', contentless_delete=1");
    }
    let mut out = vec![
        format!("CREATE VIRTUAL TABLE t USING fts5(x, {opts})"),
        format!(
            "INSERT INTO t(t, rank) VALUES('pgsz', {})",
            r.pick(&[32, 33, 40, 64, 100])
        ),
    ];
    if r.chance(50) {
        out.push(format!(
            "INSERT INTO t(t, rank) VALUES('automerge', {})",
            r.pick(&[0, 2, 8])
        ));
    }
    if cdel && r.chance(50) {
        out.push(format!(
            "INSERT INTO t(t, rank) VALUES('deletemerge', {})",
            r.pick(&[1, 5, 20])
        ));
    }
    let old_markers = r.chance(30) && detail != "none";
    if !old_markers && !cdel {
        out.push("INSERT INTO t(t, rank) VALUES('secure-delete', 1)".into());
    }
    let n = *r.pick(&[30usize, 100, 300]);
    let big = r.chance(30);
    let doc = |r: &mut Rng| -> String {
        let len = if big && r.chance(20) {
            r.range(30, 200)
        } else {
            r.range(1, 6)
        };
        (0..len)
            .map(|_| *r.pick(&words[..nw]))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut rid = 0i64;
    let mut live: Vec<i64> = Vec::new();
    for _ in 0..r.range(1, 4) {
        let mut rows = Vec::new();
        for _ in 0..n / 2 {
            rid += *r.pick(&[1, 1, 2, 7, 1000]);
            rows.push(format!("({rid}, '{}')", doc(&mut r)));
            live.push(rid);
        }
        out.push(format!(
            "INSERT INTO t(rowid, x) VALUES {}",
            rows.join(", ")
        ));
    }
    if old_markers {
        for _ in 0..3 {
            if !live.is_empty() {
                let i = live.remove(r.below(live.len() as u64) as usize);
                out.push(format!("DELETE FROM t WHERE rowid = {i}"));
            }
        }
        out.push("INSERT INTO t(t, rank) VALUES('secure-delete', 1)".into());
    }
    for _ in 0..r.range(5, 25) {
        let k = r.below(100);
        if k < 50 && !live.is_empty() {
            let mut ids: Vec<i64> = (0..*r.pick(&[1, 1, 3, 10, 40]))
                .map(|_| *r.pick(&live))
                .collect();
            if r.chance(30) && live.len() > 5 {
                let a = r.below(live.len() as u64) as usize;
                let b = (a + r.range(2, 30) as usize).min(live.len());
                ids = live[a..b].to_vec();
            }
            ids.sort();
            ids.dedup();
            live.retain(|i| !ids.contains(i));
            if ids.len() == 1 {
                out.push(format!("DELETE FROM t WHERE rowid = {}", ids[0]));
            } else {
                let s: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
                out.push(format!("DELETE FROM t WHERE rowid IN ({})", s.join(", ")));
            }
        } else if k < 65 && !live.is_empty() {
            let i = *r.pick(&live);
            out.push(format!(
                "UPDATE t SET x = '{}' WHERE rowid = {i}",
                doc(&mut r)
            ));
        } else if k < 75 {
            rid += 1;
            live.push(rid);
            out.push(format!(
                "INSERT INTO t(rowid, x) VALUES ({rid}, '{}')",
                doc(&mut r)
            ));
        } else if k < 80 {
            out.push(
                r.pick(&[
                    "INSERT INTO t(t) VALUES('optimize')",
                    "INSERT INTO t(t, rank) VALUES('merge', 5)",
                    "INSERT INTO t(t, rank) VALUES('merge', -20)",
                ])
                .to_string(),
            );
        } else {
            let a = *r.pick(&words[..nw]);
            let b = *r.pick(&words[..nw]);
            let mut qs = vec![
                a.to_string(),
                format!("{a} AND {b}"),
                format!("{a} {b}"),
                format!("{a}*"),
                format!("{a} OR {b}"),
            ];
            if detail == "full" {
                qs.push(format!("\"{a} {b}\""));
            }
            let q = r.pick(&qs).clone();
            out.push(format!(
                "SELECT group_concat(rowid) FROM t WHERE t MATCH '{q}'"
            ));
            out.push(format!(
                "SELECT count(*), sum(rowid) FROM t WHERE t MATCH '{q}' AND rowid > {}",
                r.range(0, rid)
            ));
        }
        if r.chance(50) {
            out.extend(shadow());
        }
    }
    out.extend(shadow());
    out.push("CREATE VIRTUAL TABLE v USING fts5vocab(t, row)".into());
    out.push("SELECT * FROM v".into());
    out.push(CHECK.into());
    out
}

#[test]
fn generated_page_level_deletes_match_byte_for_byte() {
    let n: u64 = std::env::var("HIPP_FTS5_DELETE_CORPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in first_seed()..n {
        let w = page_workload(seed);
        let h = common::temp_db(&format!("v17fd-pg{seed}-h"));
        let o = common::temp_db(&format!("v17fd-pg{seed}-o"));
        same_on(&h, &o, &w);
        cross_check(&format!("page seed {seed}"), &h, &o);
    }
}

/// Files move between the engines: one engine writes the first half
/// of a workload, then each engine continues on its own copy; outputs
/// and shadow tables agree, both ways round.
#[test]
fn files_move_between_engines() {
    for seed in [2001u64, 2002, 2003, 2004, 2005, 2006, 2007, 2008] {
        let w = if seed % 2 == 0 {
            workload(seed, 60)
        } else {
            page_workload(seed)
        };
        let mut depth = 0i32;
        let mut safe = Vec::new();
        for (i, s) in w.iter().enumerate() {
            if s == "BEGIN" {
                depth += 1;
            } else if s == "COMMIT" || s == "ROLLBACK" {
                depth = 0;
            }
            if depth == 0 && !s.starts_with("CREATE") {
                safe.push(i + 1);
            }
        }
        let half = w.len() / 2;
        let cut = safe
            .iter()
            .copied()
            .min_by_key(|c| c.abs_diff(half))
            .unwrap_or(half);
        let (first, second) = w.split_at(cut);
        for pin_first in [true, false] {
            let base = common::temp_db(&format!("v17fd-mv{seed}-{pin_first}-base"));
            if pin_first {
                pin_run(&base, first);
            } else {
                hipp_run(&base, first);
            }
            let h = common::temp_db(&format!("v17fd-mv{seed}-{pin_first}-h"));
            let o = common::temp_db(&format!("v17fd-mv{seed}-{pin_first}-o"));
            std::fs::copy(&base, &h).expect("copy");
            std::fs::copy(&base, &o).expect("copy");
            let mut rest = second.to_vec();
            rest.extend(shadow());
            same_on(&h, &o, &rest);
            cross_check(&format!("move seed {seed}"), &h, &o);
        }
    }
}
