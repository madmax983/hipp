//! v0.16 window functions (feature `window`), differentially vs
//! the pinned oracle (SQLite 3.54.1). The semantics were probed on
//! the pin (its answers and its `EXPLAIN` programs) before the
//! implementation (STATUS.md, "Window functions"); the matrix the
//! tests encode:
//!
//! - `f(...) [FILTER (WHERE ...)] OVER (...)` / `OVER name`, the
//!   WINDOW clause and its inheritance rules (a definition takes
//!   an earlier one's PARTITION BY / ORDER BY; restating them, or
//!   inheriting from a window with a frame, is refused; names
//!   compare by their raw text, quotes included).
//! - ROWS / RANGE / GROUPS frames with every bound and EXCLUDE
//!   clause, the implicit RANGE frame, RANGE offsets over one
//!   ORDER BY term (mixed storage classes, DESC, NULLS LAST), and
//!   the pin's frame-offset checks.
//! - The built-in functions over their fixed frames, every
//!   aggregate as a window function, with the pin's step /
//!   inverse order (sums turn REAL once a REAL passed through the
//!   frame; group_concat's inverse removes by length).
//! - Window classes nest in order of appearance: the rows come out
//!   in the first class's sort, every class sorting its input
//!   stably.
//! - The pin's refusals: window calls outside a result column or
//!   ORDER BY term (WHERE, GROUP BY, HAVING, aliases there, write
//!   statements, CHECK, indexes, aggregate or window arguments),
//!   DISTINCT, an aggregate ORDER BY, FILTER on a built-in, OVER on
//!   a scalar function, and the runtime argument errors.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type HErr = Error<<FileStorage as hipp::Storage>::Error>;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> String {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    String::from_utf8_lossy(t.as_slice()).into_owned()
}

/// The CLI's list rendering of bytes (control characters but tab
/// and newline in caret notation; a TEXT value ends at its first
/// NUL).
fn fmt_bytes(b: &[u8], text: bool) -> String {
    let end = if text {
        b.iter().position(|c| *c == 0).unwrap_or(b.len())
    } else {
        b.len()
    };
    let mut out = Vec::new();
    for &c in &b[..end] {
        if c < 0x20 && c != b'\t' && c != b'\n' {
            out.push(b'^');
            out.push(c + 0x40);
        } else {
            out.push(c);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn collect_stmt<S: hipp::Storage>(
    q: &mut hipp::Statement<'_, S>,
) -> Result<Vec<String>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut rows = Vec::new();
    while q.step().await? == Step::Row {
        let mut line = String::new();
        for i in 0..q.column_count() {
            if i > 0 {
                line.push('|');
            }
            match q.column_value(i) {
                Some(hipp::Value::Null) | None => {}
                Some(hipp::Value::Integer(v)) => line.push_str(&v.to_string()),
                Some(hipp::Value::Real(f)) => line.push_str(&fmt_real(f)),
                Some(hipp::Value::Text(b)) => line.push_str(&fmt_bytes(b, true)),
                Some(hipp::Value::Blob(b)) => line.push_str(&fmt_bytes(b, false)),
            }
        }
        rows.push(line);
    }
    Ok(rows)
}

async fn try_collect<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
) -> Result<Vec<String>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare(sql).await?;
    collect_stmt(&mut q).await
}

/// One batch through the oracle CLI: (success, stdout lines).
fn oracle_raw(path: &std::path::Path, sql: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .arg(sql)
        .output()
        .expect("oracle runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

struct Twins {
    hipp: std::path::PathBuf,
    oracle: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp = common::temp_db(&format!("v16win-{tag}-h"));
    let oracle = common::temp_db(&format!("v16win-{tag}-o"));
    common::cli(&oracle, if setup.is_empty() { "SELECT 1;" } else { setup });
    block_on(async {
        let mut db = open(&hipp).await;
        for stmt in setup.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                try_collect(&mut db, stmt).await.expect("setup");
            }
        }
    });
    Twins { hipp, oracle }
}

impl Twins {
    /// One statement on both twins: equal rows, or both refuse it
    /// (hipp's error returned). (When the pin fails after printing
    /// some rows — an argument error in a later partition — only
    /// the refusal is compared: hipp returns the error alone.)
    fn same(&self, sql: &str) -> Option<HErr> {
        let got = block_on(async {
            let mut db = open(&self.hipp).await;
            try_collect(&mut db, sql).await
        });
        let (ok, want) = oracle_raw(&self.oracle, sql);
        match got {
            Ok(rows) => {
                assert!(ok, "oracle refused what hipp ran: {sql}\nhipp: {rows:?}");
                assert_eq!(rows, want, "statement: {sql}");
                None
            }
            Err(e) => {
                assert!(
                    !ok,
                    "hipp refused what the oracle ran: {sql}\nhipp: {e:?}\noracle: {want:?}"
                );
                Some(e)
            }
        }
    }

    fn all(&self, sqls: &[&str]) {
        for sql in sqls {
            self.same(sql);
        }
    }

    /// The same rows in any order (a join's row order is the
    /// planner's choice).
    fn same_set(&self, sql: &str) {
        let got = block_on(async {
            let mut db = open(&self.hipp).await;
            try_collect(&mut db, sql).await
        })
        .unwrap_or_else(|e| panic!("hipp refused {sql}: {e:?}"));
        let (ok, want) = oracle_raw(&self.oracle, sql);
        assert!(ok, "oracle refused {sql}");
        let (mut g, mut w) = (got, want);
        g.sort();
        w.sort();
        assert_eq!(g, w, "statement: {sql}");
    }

    /// Both refuse, hipp with `kind`.
    fn refuse(&self, sql: &str, kind: SchemaKind) {
        match self.same(sql) {
            Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "statement: {sql}"),
            other => panic!("expected {kind:?} for {sql}, got {other:?}"),
        }
    }

    /// Both refuse, hipp with a parse error.
    fn refuse_parse(&self, sql: &str) {
        match self.same(sql) {
            Some(Error::Parse { .. }) => {}
            other => panic!("expected a parse error for {sql}, got {other:?}"),
        }
    }
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
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
}

const T: &str = "CREATE TABLE t(a, b, c, n TEXT COLLATE NOCASE);
INSERT INTO t VALUES (1,'x',5,'A'),(2,'y',3,'a'),(1,'z',4,'B'),(2,'w',1,NULL),(1,'q',2,'b'),(0,'p',6,'c');
CREATE TABLE d(k, v);
INSERT INTO d VALUES (1,1),(2,2),(NULL,3),(2.5,4),('a',5),('b',6),(x'00',7),(NULL,8),(4,9),(6,10);
CREATE TABLE r(i INTEGER PRIMARY KEY, x);
INSERT INTO r(x) VALUES (0.1),(0.2),(0.3),(1e16),(1),(-1e16),(2.5),(3),(4),(9223372036854775807),(1),(-5),(4)";

// ---------------- built-in functions ----------------

#[test]
fn builtin_functions() {
    let tw = twins("builtin", T);
    tw.all(&[
        "SELECT b, a, row_number() OVER (ORDER BY a) FROM t",
        "SELECT b, a, row_number() OVER (PARTITION BY a) FROM t",
        "SELECT b, a, row_number() OVER () FROM t",
        "SELECT a, rank() OVER w, dense_rank() OVER w, percent_rank() OVER w, cume_dist() OVER w FROM t WINDOW w AS (ORDER BY a)",
        "SELECT a, rank() OVER (), dense_rank() OVER (), percent_rank() OVER (), cume_dist() OVER () FROM t",
        "SELECT a, c, ntile(4) OVER (ORDER BY c), ntile(1) OVER (), ntile(10) OVER (ORDER BY c DESC) FROM t",
        "SELECT a, c, ntile(2.5) OVER (ORDER BY c), ntile(a) OVER (ORDER BY c), ntile('3x') OVER (ORDER BY c) FROM t",
        "SELECT c, lag(c) OVER (ORDER BY c), lead(c) OVER (ORDER BY c), lag(c, 2, -1) OVER (ORDER BY c), lead(b, 1, 'none') OVER (PARTITION BY a ORDER BY c) FROM t",
        "SELECT c, lag(c, -1) OVER (ORDER BY c), lag(c, 'x') OVER (ORDER BY c), lag(c, 1.5) OVER (ORDER BY c), lag(c, 1.0) OVER (ORDER BY c), lag(c, NULL, 0) OVER (ORDER BY c) FROM t",
        // Only the buffered rows: a lag() reaching ahead finds the
        // next row, never the one after.
        "SELECT v, lag(v, -2) OVER (ORDER BY v), lag(v, -1) OVER (ORDER BY v), lead(v, 2) OVER (ORDER BY v), lead(v, -1) OVER (ORDER BY v) FROM d",
        "SELECT c, lag(c, c) OVER (ORDER BY c), lead(c, 9223372036854775807) OVER (ORDER BY c), lag(c, -9223372036854775808) OVER (ORDER BY c) FROM t",
        "SELECT c, lag(c, 1, c * 10) OVER (ORDER BY c) FROM t",
        "SELECT c, first_value(b) OVER w, last_value(b) OVER w, nth_value(b, 2) OVER w FROM t WINDOW w AS (ORDER BY c ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)",
        "SELECT c, nth_value(c, 2.0) OVER (ORDER BY c), nth_value(c, '2') OVER (ORDER BY c), nth_value(c, 3) OVER (ORDER BY c ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t",
        "SELECT a, b, last_value(b) OVER (PARTITION BY a ORDER BY c RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t",
        "SELECT a, c, first_value(c) OVER (ORDER BY a NULLS LAST), first_value(n) OVER (ORDER BY n) FROM t",
        // A built-in's frame is fixed: a written one is ignored.
        "SELECT a, rank() OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), dense_rank() OVER (ORDER BY a RANGE 1 PRECEDING), lag(c) OVER (ORDER BY c ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM t",
        "SELECT row_number() OVER (PARTITION BY 1 ORDER BY 1), rank() OVER (ORDER BY -c), row_number() OVER (ORDER BY n DESC) FROM t",
        "SELECT typeof(percent_rank() OVER ()), typeof(cume_dist() OVER ()), typeof(row_number() OVER ()) FROM t",
    ]);
}

// ---------------- aggregates over frames ----------------

#[test]
fn aggregates_over_frames() {
    let tw = twins("frames", T);
    tw.all(&[
        "SELECT c, sum(c) OVER (ORDER BY c), sum(c) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t",
        "SELECT a, c, sum(c) OVER (PARTITION BY a ORDER BY c), count(*) OVER (PARTITION BY a), count() OVER (), count(n) OVER (ORDER BY c) FROM t",
        "SELECT c, total(c) OVER w, avg(c) OVER w, min(b) OVER w, max(b) OVER w, group_concat(b) OVER w, string_agg(b, '-') OVER w FROM t WINDOW w AS (ORDER BY c ROWS 2 PRECEDING)",
        "SELECT c, sum(c) OVER (ORDER BY c ROWS CURRENT ROW), sum(c) OVER (ORDER BY c ROWS BETWEEN 2 FOLLOWING AND 3 FOLLOWING), sum(c) OVER (ORDER BY c ROWS BETWEEN 3 PRECEDING AND 1 PRECEDING) FROM t",
        "SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING), total(c) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND 2 PRECEDING), count(*) OVER (ORDER BY c ROWS BETWEEN 3 FOLLOWING AND 0 FOLLOWING) FROM t",
        "SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING), group_concat(c) OVER (ORDER BY c ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t",
        "SELECT a, c, sum(c) OVER (ORDER BY a RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING), sum(c) OVER (ORDER BY a GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT a, c, sum(c) OVER (ORDER BY a GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP), sum(c) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t",
        "SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN 1 FOLLOWING AND 3 FOLLOWING EXCLUDE CURRENT ROW), sum(c) OVER (ORDER BY a RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE NO OTHERS) FROM t",
        "SELECT c, first_value(c) OVER (ORDER BY c ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW), nth_value(c, 1) OVER (ORDER BY c ROWS BETWEEN 1 FOLLOWING AND 3 FOLLOWING EXCLUDE CURRENT ROW), last_value(c) OVER (ORDER BY a GROUPS CURRENT ROW EXCLUDE GROUP) FROM t",
        "SELECT c, sum(c) OVER (GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW), sum(c) OVER (RANGE CURRENT ROW), sum(c) OVER (ORDER BY c ROWS UNBOUNDED PRECEDING EXCLUDE CURRENT ROW) FROM t",
        "SELECT a, c, sum(c) FILTER (WHERE c > 2) OVER (ORDER BY a), count(*) FILTER (WHERE a = 1) OVER (ORDER BY c ROWS 1 PRECEDING), group_concat(b) FILTER (WHERE c % 2) OVER () FROM t",
        "SELECT a, median(c) OVER (PARTITION BY a), percentile(c, 25) OVER (ORDER BY c ROWS 2 PRECEDING), percentile_cont(c, 0.5) OVER (), percentile_disc(c, 0.5) OVER (ORDER BY c) FROM t",
        "SELECT n, max(n) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), min(n) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), max(n) OVER (ORDER BY c), min(n) OVER (ORDER BY c) FROM t",
        "SELECT k, min(k) OVER (ORDER BY v ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), max(k) OVER (ORDER BY v ROWS 2 PRECEDING), max(k) OVER (ORDER BY v) FROM d",
        "SELECT typeof(sum(c) OVER ()), typeof(total(c) OVER ()), typeof(avg(c) OVER ()), typeof(count(c) OVER ()), typeof(group_concat(c) OVER ()) FROM t",
    ]);
}

// ---------------- RANGE offsets ----------------

#[test]
fn range_offsets() {
    let tw = twins("range", T);
    tw.all(&[
        "SELECT k, sum(v) OVER (ORDER BY k RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k DESC RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k NULLS LAST RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k DESC NULLS FIRST RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k RANGE BETWEEN 2 FOLLOWING AND 3 FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k RANGE BETWEEN 3 PRECEDING AND 1 PRECEDING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k RANGE BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), sum(v) OVER (ORDER BY k RANGE BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k RANGE BETWEEN 0.5 PRECEDING AND 0.5 FOLLOWING), sum(v) OVER (ORDER BY k RANGE BETWEEN '1' PRECEDING AND ' 1 ' FOLLOWING) FROM d",
        "SELECT k, sum(v) OVER (ORDER BY k DESC RANGE BETWEEN 2 FOLLOWING AND 1 FOLLOWING), sum(v) OVER (ORDER BY k RANGE BETWEEN 1 PRECEDING AND 3 PRECEDING) FROM d",
        "SELECT v, sum(v) OVER (ORDER BY v RANGE BETWEEN 9223372036854775807 PRECEDING AND 9223372036854775807 FOLLOWING), sum(v) OVER (ORDER BY v RANGE BETWEEN 1e308 PRECEDING AND 1e308 FOLLOWING) FROM d",
        "SELECT c, sum(c) OVER (ORDER BY c RANGE BETWEEN -0.0 PRECEDING AND 0 FOLLOWING) FROM t",
        "SELECT n, count(*) OVER (ORDER BY n RANGE BETWEEN 1 PRECEDING AND CURRENT ROW), count(*) OVER (ORDER BY n COLLATE BINARY) FROM t",
    ]);
}

// ---------------- named windows ----------------

#[test]
fn named_windows() {
    let tw = twins("named", T);
    tw.all(&[
        "SELECT c, sum(c) OVER w FROM t WINDOW w AS (ORDER BY c)",
        "SELECT c, sum(c) OVER (w) FROM t WINDOW w AS (ORDER BY c)",
        "SELECT c, sum(c) OVER (w ROWS 1 PRECEDING) FROM t WINDOW w AS (ORDER BY c)",
        "SELECT c, sum(c) OVER (w ORDER BY c) FROM t WINDOW w AS (PARTITION BY a)",
        // OVER name copies the whole window, frame included.
        "SELECT c, sum(c) OVER w FROM t WINDOW w AS (ORDER BY c ROWS 1 PRECEDING)",
        // A later definition shadows an earlier one of the same name.
        "SELECT c, sum(c) OVER w FROM t WINDOW w AS (ORDER BY c), w AS (ORDER BY a)",
        "SELECT b, sum(a) OVER w FROM t WINDOW w AS (PARTITION BY a), w AS (w ORDER BY c)",
        "SELECT c, sum(c) OVER w2 FROM t WINDOW w1 AS (ORDER BY c), w2 AS (w1 ROWS 1 PRECEDING)",
        "SELECT b, sum(a) OVER z FROM t WINDOW w AS (PARTITION BY a), y AS (w), z AS (y ORDER BY c)",
        // The first definition's base is never looked up (the pin).
        "SELECT c, sum(c) OVER w1 FROM t WINDOW w1 AS (w2), w2 AS (ORDER BY c)",
        "SELECT c FROM t WINDOW w AS (nosuch)",
        // Unused windows are not resolved.
        "SELECT c FROM t WINDOW w AS (ORDER BY nosuch)",
        // Names compare by their raw text, case-insensitively.
        "SELECT sum(c) OVER W FROM t WINDOW w AS (ORDER BY c)",
        "SELECT sum(c) OVER \"w\" FROM t WINDOW \"w\" AS (ORDER BY c)",
        "SELECT sum(c) OVER 'w' FROM t WINDOW 'w' AS (ORDER BY c)",
        "SELECT sum(c) OVER `w` FROM t WINDOW `w` AS (ORDER BY c)",
        "SELECT sum(c) OVER x FROM t WINDOW \"w\" AS (ORDER BY c), x AS (\"w\")",
        // WINDOW, OVER, FILTER stay names where the pin reads them so.
        "SELECT a, c, sum(c) OVER (ORDER BY c) AS window FROM t",
        "SELECT a, sum(c) OVER (ORDER BY c) over FROM t",
        "SELECT a, sum(c) OVER (ORDER BY c) window FROM t",
        "SELECT a, sum(a) OVER (PARTITION BY a ORDER BY c) filter FROM t",
        "SELECT window, over, filter, range, rows, groups, exclude, ties, others, unbounded, preceding, following FROM (SELECT 1 window, 2 over, 3 filter, 4 range, 5 rows, 6 groups, 7 exclude, 8 ties, 9 others, 10 unbounded, 11 preceding, 12 following)",
    ]);
    tw.refuse(
        "SELECT c, sum(c) OVER (w ORDER BY c) FROM t WINDOW w AS (ORDER BY a)",
        SchemaKind::WindowOverride,
    );
    tw.refuse(
        "SELECT c, sum(c) OVER (w PARTITION BY a) FROM t WINDOW w AS (ORDER BY a)",
        SchemaKind::WindowOverride,
    );
    tw.refuse(
        "SELECT c, sum(c) OVER (w) FROM t WINDOW w AS (ORDER BY c ROWS 1 PRECEDING)",
        SchemaKind::WindowOverride,
    );
    tw.refuse(
        "SELECT sum(c) OVER x FROM t WINDOW w AS (ORDER BY c ROWS 1 PRECEDING), x AS (w)",
        SchemaKind::WindowOverride,
    );
    tw.refuse(
        "SELECT b, sum(a) OVER w FROM t WINDOW w AS (ORDER BY c), y AS (w), z AS (y ORDER BY a)",
        SchemaKind::WindowOverride,
    );
    tw.refuse(
        "SELECT c, sum(c) OVER x FROM t WINDOW w AS (ORDER BY c)",
        SchemaKind::NoSuchWindow,
    );
    tw.refuse(
        "SELECT sum(c) OVER (nosuch) FROM t",
        SchemaKind::NoSuchWindow,
    );
    tw.refuse(
        "SELECT c FROM t WINDOW w AS (ORDER BY a), v AS (nosuch)",
        SchemaKind::NoSuchWindow,
    );
    tw.refuse(
        "SELECT sum(c) OVER [w] FROM t WINDOW w AS (ORDER BY c)",
        SchemaKind::NoSuchWindow,
    );
    tw.refuse(
        "SELECT sum(c) OVER w FROM t WINDOW \"w\" AS (ORDER BY c)",
        SchemaKind::NoSuchWindow,
    );
    // A subquery sees only its own WINDOW clause.
    tw.refuse(
        "SELECT (SELECT sum(c) OVER w FROM t LIMIT 1) FROM t WINDOW w AS (ORDER BY c)",
        SchemaKind::NoSuchWindow,
    );
    tw.refuse(
        "SELECT c, sum(c) OVER w FROM t WINDOW w AS (ORDER BY nosuch)",
        SchemaKind::UnknownColumn,
    );
}

// ---------------- the step / inverse order ----------------

/// Regressions for the pin's incremental frame engine: a REAL that
/// has passed through a sliding frame leaves the sum REAL, an
/// INTEGER overflow inside a window persists, and REAL sums round
/// as the pin's step / inverse sequence makes them (the frame
/// engine steps rows ahead of a FOLLOWING frame and inverts them).
#[test]
fn step_inverse_order() {
    let tw = twins("steps", T);
    tw.all(&[
        "SELECT x, sum(x) OVER (ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (SELECT 1 x UNION ALL SELECT 2.5 UNION ALL SELECT 3 UNION ALL SELECT 4)",
        "SELECT x, sum(x) OVER (ROWS BETWEEN 2 FOLLOWING AND 3 FOLLOWING) FROM (SELECT 1.5 x UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6)",
        "SELECT x, total(x) OVER (ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING), sum(x) OVER (ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING), avg(x) OVER (ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING), count(x) OVER (ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING) FROM (SELECT 1 x UNION ALL SELECT 2.5 UNION ALL SELECT 3 UNION ALL SELECT 4)",
        "SELECT x, total(x) OVER (ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING) FROM (SELECT 0.1 x UNION ALL SELECT 0.2 UNION ALL SELECT 0.3)",
        "SELECT i, x, sum(x) OVER (ORDER BY i ROWS 2 PRECEDING), sum(x) OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), avg(x) OVER (ORDER BY i ROWS BETWEEN CURRENT ROW AND 2 FOLLOWING) FROM r WHERE i < 9",
        "SELECT i, x, total(x) OVER (ORDER BY i ROWS BETWEEN 3 PRECEDING AND 1 PRECEDING), total(x) OVER (ORDER BY i ROWS BETWEEN 1 FOLLOWING AND 4 FOLLOWING) FROM r WHERE i < 9",
        "SELECT i, x, sum(x) OVER (ORDER BY x RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING), sum(x) OVER (ORDER BY x GROUPS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM r WHERE i < 9",
        // An overflow inside a sliding window: the pin's sumInverse.
        "SELECT i, x, total(x) OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM r WHERE i >= 9",
    ]);
    // sum() over an overflowing frame errors on both.
    tw.refuse(
        "SELECT i, sum(x) OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM r WHERE i >= 9",
        SchemaKind::IntegerOverflow,
    );
}

#[test]
fn group_concat_inverse() {
    let tw = twins(
        "gcinv",
        "CREATE TABLE g(i INTEGER PRIMARY KEY, s, sep);
         INSERT INTO g(s, sep) VALUES ('a', ','), ('bb', '::'), ('', ''), (NULL, '-'), ('ccc', NULL), ('', ','), ('d', '--'), (1.5, '+'), (x'41', '')",
    );
    tw.all(&[
        "SELECT i, group_concat(s) OVER (ORDER BY i ROWS 1 PRECEDING), group_concat(s, sep) OVER (ORDER BY i ROWS 2 PRECEDING) FROM g",
        "SELECT i, quote(group_concat(s, sep) OVER (ORDER BY i ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING)), quote(string_agg(s, sep) OVER (ORDER BY i ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING)) FROM g",
        "SELECT i, quote(group_concat(s, '') OVER (ORDER BY i ROWS BETWEEN 1 PRECEDING AND 1 PRECEDING)), quote(group_concat(s) OVER (ORDER BY i ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING)) FROM g",
        "SELECT i, group_concat(s, sep) OVER (ORDER BY s GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE TIES) FROM g",
    ]);
}

// ---------------- output order ----------------

#[test]
fn output_order() {
    let tw = twins("order", T);
    tw.all(&[
        // Three classes: the first decides the output order, the
        // later ones nest inside it, each sorting stably.
        "SELECT b, a, c, rank() OVER (ORDER BY b), row_number() OVER (ORDER BY a DESC), row_number() OVER (ORDER BY c) FROM t",
        "SELECT b, a, c, rank() OVER (ORDER BY b), row_number() OVER (ORDER BY c), row_number() OVER (ORDER BY a DESC) FROM t",
        "SELECT b, a, c, rank() OVER (ORDER BY b) FROM t ORDER BY row_number() OVER (ORDER BY a DESC)",
        "SELECT b, a, row_number() OVER (ORDER BY a DESC), rank() OVER (ORDER BY b) FROM t",
        "SELECT a, row_number() OVER (ORDER BY a DESC) FROM t ORDER BY a",
        "SELECT a, sum(c) OVER (PARTITION BY a ORDER BY c) FROM t ORDER BY 2 DESC LIMIT 3",
        "SELECT a, row_number() OVER (PARTITION BY a) AS rn FROM t ORDER BY rn, a",
        "SELECT a, c, sum(c) OVER (ORDER BY a, c DESC) FROM t",
        "SELECT DISTINCT a, count(*) OVER () FROM t",
        "SELECT DISTINCT a, count(*) OVER (PARTITION BY a) FROM t",
        "SELECT a, row_number() OVER () FROM t LIMIT 2 OFFSET 1",
        "SELECT *, row_number() OVER () FROM t",
        "SELECT t.*, row_number() OVER (ORDER BY n) FROM t",
        "SELECT a, b FROM t ORDER BY row_number() OVER (ORDER BY c DESC) LIMIT 2",
        "SELECT a, b, c FROM t ORDER BY rank() OVER (PARTITION BY a ORDER BY c), a",
        "SELECT c, sum(c) OVER w AS s FROM t WINDOW w AS (ORDER BY c) ORDER BY s DESC",
        "SELECT a, c, sum(c) OVER (PARTITION BY a COLLATE NOCASE), sum(c) OVER (PARTITION BY n) FROM t",
    ]);
    tw.same_set(
        "SELECT t.a, u.b, row_number() OVER (ORDER BY t.c, u.c) FROM t JOIN t u ON t.a = u.a",
    );
    tw.same_set("SELECT x.a, count(*) OVER () FROM t x LEFT JOIN t y ON x.c = y.c + 10");
}

// ---------------- windows over groups ----------------

#[test]
fn windows_over_groups() {
    let tw = twins("groups", T);
    tw.all(&[
        "SELECT count(*) OVER () FROM t GROUP BY a",
        "SELECT a, sum(sum(c)) OVER (ORDER BY a) FROM t GROUP BY a",
        "SELECT a, count(*), sum(count(*)) OVER (ORDER BY a), rank() OVER (ORDER BY count(*) DESC) FROM t GROUP BY a",
        "SELECT a, max(c), first_value(max(c)) OVER (ORDER BY a DESC) FROM t GROUP BY a ORDER BY a",
        "SELECT a, count(*) OVER () FROM t GROUP BY a HAVING a > 0",
        "SELECT a, count(*) OVER () FROM t GROUP BY a LIMIT 1",
        "SELECT sum(a), row_number() OVER (), count(*) OVER () FROM t",
        "SELECT count(*), count(*) OVER () FROM t WHERE 0",
        "SELECT sum(a) OVER (), a FROM t WHERE 0",
        "SELECT count(*) OVER (), sum(c) FROM t HAVING sum(c) > 0",
        "SELECT max(c) OVER (), min(c) FROM t",
        "SELECT a, sum(c) OVER () FROM t GROUP BY a ORDER BY count(*)",
        "SELECT a, b, max(c) OVER (PARTITION BY a) FROM t WHERE c > 1 ORDER BY 1, 2",
        "SELECT a, b FROM t ORDER BY sum(c) OVER (PARTITION BY a), b",
    ]);
    tw.refuse(
        "SELECT sum(c) OVER () FROM t HAVING count(*) > 0",
        SchemaKind::HavingWithoutAggregate,
    );
}

// ---------------- subqueries, views, CTEs, writes ----------------

#[test]
fn windows_elsewhere() {
    let tw = twins("elsewhere", T);
    tw.all(&[
        "SELECT a, row_number() OVER (ORDER BY c) FROM t UNION ALL SELECT 9, 9",
        "SELECT a, row_number() OVER (ORDER BY c) FROM t UNION SELECT 9, 9 ORDER BY 2",
        "SELECT (SELECT row_number() OVER () FROM t LIMIT 1)",
        "SELECT a, (SELECT count(*) OVER () FROM t u WHERE u.a = t.a LIMIT 1) FROM t",
        "SELECT a, (SELECT count(*) OVER (PARTITION BY t.a) FROM t u LIMIT 1) FROM t",
        "SELECT * FROM (SELECT a, row_number() OVER (ORDER BY c) rn FROM t) WHERE rn > 3",
        "SELECT * FROM (SELECT a, count(*) OVER () n FROM t) LIMIT 1",
        "WITH w AS (SELECT a, sum(c) OVER (PARTITION BY a) s FROM t) SELECT * FROM w ORDER BY a",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n, sum(n) OVER (ORDER BY n ROWS 1 PRECEDING) FROM r",
        "WITH q AS (SELECT a, c FROM t) SELECT a, c, lag(c) OVER (PARTITION BY a ORDER BY c) FROM q",
        "SELECT a, b, row_number() OVER (ORDER BY (SELECT count(*) FROM t u WHERE u.a <= t.a)) FROM t",
        "SELECT a, b, sum((SELECT max(c) FROM t u WHERE u.a = t.a)) OVER (ORDER BY b) FROM t",
        "SELECT a, b, sum(c) OVER (PARTITION BY (SELECT 1)) FROM t",
        "SELECT a, b, (SELECT group_concat(b) OVER (ORDER BY b) FROM t u WHERE u.a = t.a ORDER BY 1 DESC LIMIT 1) FROM t",
        "SELECT k, (SELECT sum(v) OVER () FROM d u WHERE u.k = d.k) FROM d",
        "SELECT a, b, sum(c) OVER () + 1, -row_number() OVER (ORDER BY c), abs(rank() OVER (ORDER BY a) - 3) FROM t",
        "SELECT CASE WHEN row_number() OVER (ORDER BY c) > 3 THEN 'hi' ELSE 'lo' END, row_number() OVER (ORDER BY c) = c, b || (row_number() OVER (ORDER BY c)), row_number() OVER (ORDER BY c) IN (1, 2) FROM t",
    ]);
    tw.all(&[
        // Each VALUES row is a one-row SELECT core on the pin: its
        // window calls run over that row.
        "VALUES (row_number() OVER (), sum(5) OVER (), lag(3) OVER (), count(*) OVER ()), (2, 2, 2, 2)",
        "SELECT * FROM (VALUES (row_number() OVER ()))",
        "SELECT 1 UNION ALL VALUES (rank() OVER ())",
        "CREATE VIEW v AS SELECT a, b, rank() OVER (ORDER BY c) r FROM t",
        "SELECT * FROM v WHERE r < 4 ORDER BY r",
        "CREATE VIEW v2 AS SELECT a, sum(c) OVER w s FROM t WINDOW w AS (PARTITION BY a ORDER BY c)",
        "SELECT * FROM v2",
        "SELECT * FROM v2 WHERE s > 3",
        "CREATE TABLE u2 AS SELECT a, row_number() OVER (ORDER BY c) rn, sum(c) OVER (ORDER BY c) s FROM t",
        "SELECT * FROM u2",
        "SELECT sql FROM sqlite_schema WHERE name = 'u2'",
        "CREATE TABLE u3(x, y)",
        "INSERT INTO u3 SELECT a, rank() OVER (ORDER BY a) FROM t",
        "SELECT * FROM u3 ORDER BY rowid",
        "UPDATE u3 SET y = (SELECT count(*) OVER () FROM t WHERE t.c < u3.y LIMIT 1)",
        "SELECT * FROM u3 ORDER BY rowid",
        "DELETE FROM u3 WHERE rowid IN (SELECT r FROM (SELECT rowid r, row_number() OVER (ORDER BY x DESC) rn FROM u3) WHERE rn <= 2)",
        "SELECT * FROM u3 ORDER BY rowid",
    ]);
}

// ---------------- refusals ----------------

#[test]
fn refusals() {
    let tw = twins("refuse", T);
    for sql in [
        "SELECT sum(row_number() OVER ()) FROM t",
        "SELECT row_number() OVER (ORDER BY row_number() OVER ()) FROM t",
        "SELECT sum(a) OVER (ORDER BY sum(b) OVER ()) FROM t",
        "SELECT sum(a) OVER (PARTITION BY rank() OVER ()) FROM t",
        "SELECT count(*) FILTER (WHERE row_number() OVER () > 1) OVER () FROM t",
        "SELECT a FROM t WHERE row_number() OVER () > 1",
        "SELECT a FROM t GROUP BY row_number() OVER ()",
        "SELECT a FROM t GROUP BY a HAVING row_number() OVER () > 1",
        "SELECT * FROM t JOIN t u ON row_number() OVER () = 1",
        "SELECT a, rank() OVER (ORDER BY c) AS r FROM t WHERE r > 1",
        "SELECT a, rank() OVER (ORDER BY c) AS r FROM t GROUP BY r",
        "SELECT a, rank() OVER (ORDER BY c) AS r FROM t GROUP BY a HAVING r > 1",
        "SELECT a, rank() OVER (ORDER BY c) AS r FROM t GROUP BY 2",
        "SELECT row_number() FROM t",
        "SELECT lag(a) FROM t",
        "UPDATE t SET a = row_number() OVER ()",
        "DELETE FROM t WHERE a = rank() OVER ()",
        "INSERT INTO t VALUES (row_number() OVER (), 1, 1, 1)",
        "SELECT 1 LIMIT row_number() OVER ()",
        "CREATE TABLE z(a CHECK (a > row_number() OVER ()))",
    ] {
        tw.refuse(sql, SchemaKind::MisusedWindow);
    }
    // hipp has no expression indexes: refused either way.
    tw.same("CREATE INDEX zi ON t(row_number() OVER ())")
        .expect("refused");
    tw.refuse(
        "SELECT abs(a) OVER () FROM t",
        SchemaKind::NotWindowFunction,
    );
    tw.refuse(
        "SELECT min(a, c) OVER () FROM t",
        SchemaKind::NotWindowFunction,
    );
    tw.refuse("SELECT foo(a) OVER () FROM t", SchemaKind::UnknownFunction);
    tw.refuse(
        "SELECT sum(DISTINCT a) OVER () FROM t",
        SchemaKind::WindowDistinct,
    );
    tw.refuse(
        "SELECT count(DISTINCT a) FILTER (WHERE a > 1) OVER () FROM t",
        SchemaKind::WindowDistinct,
    );
    tw.refuse(
        "SELECT group_concat(b ORDER BY c) FILTER (WHERE a > 0) OVER () FROM t",
        SchemaKind::WindowClauseMisused,
    );
    tw.refuse(
        "SELECT row_number() FILTER (WHERE c > 2) OVER (ORDER BY a) FROM t",
        SchemaKind::WindowClauseMisused,
    );
    for sql in [
        "SELECT row_number(1) OVER () FROM t",
        "SELECT rank(a) OVER () FROM t",
        "SELECT ntile() OVER () FROM t",
        "SELECT lag() OVER (ORDER BY c) FROM t",
        "SELECT lag(a, 1, 2, 3) OVER (ORDER BY c) FROM t",
        "SELECT nth_value(a) OVER () FROM t",
        "SELECT string_agg(a) OVER () FROM t",
    ] {
        tw.refuse(sql, SchemaKind::WrongArgumentCount);
    }
    for sql in [
        "SELECT c, sum(c) OVER (ORDER BY c, a RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT c, sum(c) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT c, sum(c) OVER (RANGE BETWEEN CURRENT ROW AND 1 FOLLOWING) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::RangeOffsetOrder);
    }
    for sql in [
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN a PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN 1.5 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN NULL PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN (SELECT 1) PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN abs(1) PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c GROUPS BETWEEN x'01' PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c RANGE BETWEEN 'x' PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c RANGE BETWEEN -1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c RANGE BETWEEN x'01' PRECEDING AND CURRENT ROW) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::FrameStartOffset);
    }
    tw.refuse(
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN CURRENT ROW AND -1 FOLLOWING) FROM t",
        SchemaKind::FrameEndOffset,
    );
    tw.refuse(
        "SELECT sum(c) OVER (ORDER BY c RANGE BETWEEN CURRENT ROW AND NULL FOLLOWING) FROM t",
        SchemaKind::FrameEndOffset,
    );
    for sql in [
        "SELECT nth_value(a, 0) OVER (ORDER BY c) FROM t",
        "SELECT nth_value(a, 2.5) OVER (ORDER BY c) FROM t",
        "SELECT nth_value(a, NULL) OVER (ORDER BY c) FROM t",
        "SELECT nth_value(a, -1) OVER (ORDER BY c) FROM t",
        "SELECT nth_value(a, 0) OVER (ORDER BY c ROWS UNBOUNDED PRECEDING EXCLUDE TIES) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::NthValueArgument);
    }
    for sql in [
        "SELECT ntile(0) OVER () FROM t",
        "SELECT ntile(-1) OVER () FROM t",
        "SELECT ntile('a') OVER () FROM t",
        "SELECT ntile(NULL) OVER () FROM t",
    ] {
        tw.refuse(sql, SchemaKind::NtileArgument);
    }
    // Over no rows the runtime checks never run.
    tw.all(&[
        "SELECT sum(c) OVER (ROWS -1 PRECEDING) FROM t WHERE 0",
        "SELECT ntile(0) OVER (), nth_value(c, 0) OVER () FROM t WHERE 0",
    ]);
    tw.refuse(
        "SELECT count(*), count(*) OVER (ROWS -1 PRECEDING) FROM t WHERE 0",
        SchemaKind::FrameStartOffset,
    );
    for sql in [
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS 1 FOLLOWING) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS UNBOUNDED FOLLOWING) FROM t",
        "SELECT count(*) OVER () FILTER (WHERE a > 1) FROM t",
        "SELECT sum(a) OVER (ORDER BY c) OVER () FROM t",
        "SELECT sum(c) OVER (ORDER BY c EXCLUDE CURRENT ROW) FROM t",
        "SELECT a OVER () FROM t",
        "SELECT sum(c) OVER w FROM t WINDOW w AS (ORDER BY c) WINDOW x AS (ORDER BY c)",
        "SELECT sum(c) OVER (PARTITION BY) FROM t",
        "SELECT sum(c) OVER (ORDER BY c ROWS 1) FROM t",
    ] {
        tw.refuse_parse(sql);
    }
    // A compound's ORDER BY never names a window call.
    tw.same("SELECT a FROM t UNION SELECT c FROM t ORDER BY row_number() OVER ()")
        .expect("refused");
}

// ---------------- bound parameters ----------------

#[test]
fn bound_parameters() {
    let tw = twins("params", T);
    let sql = "SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN ?1 PRECEDING AND CURRENT ROW), nth_value(c, ?2) OVER (ORDER BY c), lag(c, ?2, ?3) OVER (ORDER BY c) FROM t";
    let inline = "SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), nth_value(c, 2) OVER (ORDER BY c), lag(c, 2, 'z') OVER (ORDER BY c) FROM t";
    let (ok, want) = oracle_raw(&tw.oracle, inline);
    assert!(ok);
    let got = block_on(async {
        let mut db = open(&tw.hipp).await;
        let mut q = db.prepare(sql).await.expect("prepare");
        q.bind_i64(1, 1).expect("bind");
        q.bind_i64(2, 2).expect("bind");
        q.bind_text(3, "z").expect("bind");
        let first = collect_stmt(&mut q).await.expect("run");
        // Re-execution with another offset.
        q.reset();
        q.bind_i64(1, 0).expect("bind");
        let second = collect_stmt(&mut q).await.expect("rerun");
        (first, second)
    });
    assert_eq!(got.0, want);
    let (ok, want0) = oracle_raw(
        &tw.oracle,
        "SELECT c, sum(c) OVER (ORDER BY c ROWS BETWEEN 0 PRECEDING AND CURRENT ROW), nth_value(c, 2) OVER (ORDER BY c), lag(c, 2, 'z') OVER (ORDER BY c) FROM t",
    );
    assert!(ok);
    assert_eq!(got.1, want0);
    // An unbound offset is NULL: the pin's offset error.
    let e = block_on(async {
        let mut db = open(&tw.hipp).await;
        let mut q = db
            .prepare("SELECT sum(c) OVER (ROWS ? PRECEDING) FROM t")
            .await
            .expect("prepare");
        collect_stmt(&mut q).await
    });
    assert!(matches!(
        e,
        Err(Error::Schema {
            kind: SchemaKind::FrameStartOffset
        })
    ));
}

// ---------------- the generated corpus ----------------

/// A generated corpus of random window queries over random tables
/// (mixed storage classes, NULLs, REALs that round, big integers),
/// against the live pin: every result, or both refuse.
/// `HIPP_WIN_CORPUS` scales the statement count (default 240) and
/// `HIPP_WIN_SEED` varies it.
#[test]
fn generated_window_corpus() {
    let n: usize = std::env::var("HIPP_WIN_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    let seed: u64 = std::env::var("HIPP_WIN_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5eed_0016);
    let mut rng = Rng(seed | 1);
    const X: &[&str] = &[
        "NULL",
        "0",
        "1",
        "2",
        "-3",
        "7",
        "-20",
        "0.1",
        "0.2",
        "0.3",
        "1e16",
        "-1e16",
        "2.5",
        "-3.75",
        "1e308",
        "123456789.123",
        "0.7",
        "1e-5",
        "1.1",
        "2.2",
        "-7.7",
        "9223372036854775807",
        "-9223372036854775808",
        "4611686018427387904",
        "4503599627370497",
        "'7'",
        "'2.5'",
        "' 3 '",
        "'x1'",
        "'abc'",
    ];
    const K: &[&str] = &[
        "NULL", "0", "1", "2", "3", "4", "5", "6", "1.5", "2.5", "0.5", "'a'", "'b'", "'B'",
        "x'01'",
    ];
    const S: &[&str] = &["'a'", "'bb'", "''", "NULL", "'ccc'", "'d'"];
    const SEP: &[&str] = &["','", "'::'", "''", "NULL", "'-'"];
    let mut setup = String::new();
    for ti in 0..6 {
        setup.push_str(&format!(
            "CREATE TABLE w{ti}(id INTEGER PRIMARY KEY, p, k, x, s, sep);"
        ));
        let rows = rng.below(30);
        if rows > 0 {
            let mut vals = Vec::new();
            for i in 0..rows {
                vals.push(format!(
                    "({},{},{},{},{},{})",
                    i + 1,
                    rng.pick(&["NULL", "1", "2", "3"]),
                    rng.pick(K),
                    rng.pick(X),
                    rng.pick(S),
                    rng.pick(SEP)
                ));
            }
            setup.push_str(&format!("INSERT INTO w{ti} VALUES {};", vals.join(",")));
        }
    }
    let tw = twins("corpus", &setup);
    const F: &[&str] = &[
        "sum(x)",
        "total(x)",
        "avg(x)",
        "count(x)",
        "count(*)",
        "min(x)",
        "max(x)",
        "group_concat(s)",
        "group_concat(s, sep)",
        "string_agg(x, '-')",
        "first_value(x)",
        "last_value(x)",
        "nth_value(x, 2)",
        "row_number()",
        "rank()",
        "dense_rank()",
        "percent_rank()",
        "cume_dist()",
        "ntile(3)",
        "lag(x)",
        "lead(x, 2, 'd')",
        "sum(x) FILTER (WHERE k > 2)",
        "count(*) FILTER (WHERE p = 1)",
        "group_concat(s) FILTER (WHERE x IS NOT NULL)",
        "min(s)",
        "max(k)",
        "sum(k)",
        "total(id * 0.1)",
        "avg(id / 3.0)",
    ];
    let bound = |rng: &mut Rng, start: bool, unit: &str, offsets: bool| -> String {
        let c = if start {
            rng.pick(&["UNBOUNDED PRECEDING", "P", "CURRENT ROW", "F"])
        } else {
            rng.pick(&["UNBOUNDED FOLLOWING", "P", "CURRENT ROW", "F"])
        };
        if c != "P" && c != "F" {
            return c.to_string();
        }
        if !offsets {
            return "CURRENT ROW".to_string();
        }
        let off = if unit == "RANGE" {
            rng.pick(&["0", "1", "2", "0.5", "1.5", "3"])
        } else {
            rng.pick(&["0", "1", "2", "3"])
        };
        format!("{off} {}", if c == "P" { "PRECEDING" } else { "FOLLOWING" })
    };
    let kind = |b: &str| -> usize {
        match b {
            "UNBOUNDED PRECEDING" => 0,
            "CURRENT ROW" => 2,
            "UNBOUNDED FOLLOWING" => 4,
            b if b.ends_with("PRECEDING") => 1,
            _ => 3,
        }
    };
    for _ in 0..n {
        let ti = rng.below(6);
        let nf = 1 + rng.below(3);
        let mut calls = Vec::new();
        for _ in 0..nf {
            let mut parts = Vec::new();
            if rng.chance(40) {
                parts.push("PARTITION BY p".to_string());
            }
            let norder = [0, 1, 1, 1, 2][rng.below(5)];
            if norder > 0 {
                let mut cols = vec!["k", "x", "s", "id"];
                let mut terms = Vec::new();
                for _ in 0..norder {
                    let c = cols.remove(rng.below(cols.len()));
                    let dir = rng.pick(&["", " DESC", " ASC NULLS LAST", " DESC NULLS FIRST"]);
                    terms.push(format!("{c}{dir}"));
                }
                parts.push(format!("ORDER BY {}", terms.join(", ")));
            }
            if rng.chance(75) {
                let unit = rng.pick(&["ROWS", "RANGE", "GROUPS"]);
                let offsets = unit != "RANGE" || norder == 1;
                let (mut s, mut e) = ("UNBOUNDED PRECEDING".to_string(), "CURRENT ROW".to_string());
                for _ in 0..20 {
                    let (a, b) = (
                        bound(&mut rng, true, unit, offsets),
                        bound(&mut rng, false, unit, offsets),
                    );
                    let (ka, kb) = (kind(&a), kind(&b));
                    if (ka == 2 && kb == 1) || (ka == 3 && (kb == 1 || kb == 2)) {
                        continue;
                    }
                    (s, e) = (a, b);
                    break;
                }
                let mut f = format!("{unit} BETWEEN {s} AND {e}");
                if rng.chance(30) {
                    f.push_str(" EXCLUDE ");
                    f.push_str(rng.pick(&["NO OTHERS", "CURRENT ROW", "GROUP", "TIES"]));
                }
                parts.push(f);
            }
            calls.push(format!("{} OVER ({})", rng.pick(F), parts.join(" ")));
        }
        let tail = if rng.chance(50) { " ORDER BY id" } else { "" };
        let sql = format!("SELECT id, {} FROM w{ti}{tail}", calls.join(", "));
        tw.same(&sql);
    }
}
