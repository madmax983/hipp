//! v0.16 aggregate completion, differentially vs the pinned
//! oracle (SQLite 3.54.1, built with SQLITE_ENABLE_PERCENTILE).
//! The semantics were probed on the pin before implementation
//! (STATUS.md, "Aggregate completion"); the matrix the tests
//! encode:
//!
//! - `string_agg(X, SEP)` and the aggregate ORDER BY (`v16`):
//!   `f(args ORDER BY ...)` sorts the stepped rows stably (ties in
//!   input order) for every aggregate but min / max (which drop
//!   it); DISTINCT keeps the first value seen, then sorts, except
//!   when the single ORDER BY term is the argument itself, where
//!   the sorter deduplicates and the last of equal keys survives.
//! - `FILTER (WHERE ...)` on every aggregate (`v16`): a row the
//!   filter rejects is not stepped (and loads no min / max bare
//!   column); FILTER / ORDER BY ownership in subqueries follows
//!   every column they name; on a non-aggregate they are refused.
//! - `median` / `percentile` / `percentile_cont` /
//!   `percentile_disc` (`v16`): the pin's P validation, constant-P
//!   check, NULL handling and REAL results.
//! - Ungated fixes: `sum()` integer overflow is an error (total /
//!   avg go REAL), REAL sums use Kahan–Babuška–Neumaier
//!   summation, a text addend counts as REAL unless it is wholly
//!   an integer, a NULL group_concat separator adds nothing,
//!   DISTINCT needs exactly one argument, and a group's
//!   aggregates finalize before HAVING decides (HAVING terms
//!   without aggregates and the LIMIT of a group-ordered query
//!   keep groups from forming, as on the pin).

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

/// The CLI's list rendering of bytes: control characters but tab
/// and newline in caret notation (a TEXT value ends at its first
/// NUL, a BLOB renders in full).
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

/// Step a prepared statement to completion, collecting its rows in
/// the CLI's list rendering, or the error.
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
    let hipp = common::temp_db(&format!("v16agg-{tag}-h"));
    let oracle = common::temp_db(&format!("v16agg-{tag}-o"));
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
    /// (hipp's error returned).
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

    /// Both refuse, hipp with `kind`.
    fn refuse(&self, sql: &str, kind: SchemaKind) {
        match self.same(sql) {
            Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "statement: {sql}"),
            other => panic!("expected {kind:?} for {sql}, got {other:?}"),
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
}

const T: &str = "CREATE TABLE t(a, b, c TEXT COLLATE NOCASE, r REAL);
INSERT INTO t VALUES (1,'x','A',1.5),(2,'y','a',2),(3,'z','B',NULL),(NULL,'w',NULL,0.1),(2,'v','b',0.2),('7','u','c','x');
CREATE TABLE u(k); INSERT INTO u VALUES (1),(2);
CREATE TABLE v(k, x, c TEXT COLLATE NOCASE);
INSERT INTO v VALUES (1, 1e308, 'a'), (2, 1e308, 'A'), (3, -1e308, 'b');";

// ---------------- string_agg / aggregate ORDER BY ----------------

#[test]
fn string_agg_and_order_by() {
    let tw = twins("order", T);
    tw.all(&[
        "SELECT string_agg(b, ',') FROM t",
        "SELECT group_concat(b ORDER BY a DESC) FROM t",
        "SELECT group_concat(b, '-' ORDER BY a DESC, b) FROM t",
        "SELECT string_agg(b, '-' ORDER BY a DESC, b) FROM t",
        "SELECT string_agg(b, ',' ORDER BY b) FROM t",
        "SELECT string_agg(b, NULL), group_concat(b, NULL) FROM t",
        "SELECT group_concat(b, a), group_concat(a, b), group_concat(b, a ORDER BY a) FROM t",
        "SELECT group_concat(b ORDER BY a NULLS LAST), group_concat(b ORDER BY a DESC NULLS FIRST) FROM t",
        // An integer is a constant here, never an ordinal.
        "SELECT group_concat(b ORDER BY 1), group_concat(b ORDER BY 2), group_concat(b ORDER BY (SELECT 1)) FROM t",
        "SELECT group_concat(b ORDER BY a, c, r), group_concat(b ORDER BY c, r DESC) FROM t",
        "SELECT group_concat(b ORDER BY c COLLATE BINARY), group_concat(b ORDER BY b COLLATE NOCASE DESC) FROM t",
        "SELECT group_concat(r ORDER BY r), group_concat(x'41' ORDER BY a), typeof(group_concat(x'41')) FROM t",
        "SELECT group_concat(b ORDER BY a) FROM t GROUP BY c ORDER BY 1",
        "SELECT a, group_concat(b ORDER BY b DESC) FROM t GROUP BY a ORDER BY 1",
        "SELECT group_concat(b ORDER BY a) AS x FROM t ORDER BY x",
        "SELECT a FROM t UNION SELECT group_concat(b ORDER BY b DESC) FROM t ORDER BY 1",
        "SELECT x FROM (SELECT string_agg(b, '+' ORDER BY a DESC) x FROM t)",
        // Order-sensitive REAL sums honour it; min / max drop it.
        "SELECT sum(x ORDER BY k DESC), sum(x ORDER BY k), sum(x ORDER BY x), total(x ORDER BY k DESC), avg(x ORDER BY k DESC) FROM v",
        "SELECT max(c ORDER BY k DESC), min(c ORDER BY k DESC), max(c), min(c) FROM v",
        "SELECT max(c ORDER BY k DESC), k FROM v",
        "SELECT min(b ORDER BY a), count(c ORDER BY k DESC), count(DISTINCT c ORDER BY k) FROM v, t",
    ]);
    tw.refuse(
        "SELECT string_agg(b) FROM t",
        SchemaKind::WrongArgumentCount,
    );
    tw.refuse(
        "SELECT string_agg(b, ',', 1) FROM t",
        SchemaKind::WrongArgumentCount,
    );
    tw.refuse(
        "SELECT abs(a ORDER BY a) FROM t",
        SchemaKind::OrderByNonAggregate,
    );
    tw.refuse(
        "SELECT coalesce(a, b ORDER BY a) FROM t",
        SchemaKind::OrderByNonAggregate,
    );
    tw.refuse(
        "SELECT min(a, b ORDER BY a) FROM t",
        SchemaKind::OrderByNonAggregate,
    );
    tw.refuse(
        "SELECT group_concat(b ORDER BY count(*)) FROM t",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse(
        "SELECT group_concat(b ORDER BY nope) FROM t",
        SchemaKind::UnknownColumn,
    );
    // Syntax errors on both: ORDER BY after `*` or without arguments.
    assert!(matches!(
        tw.same("SELECT count(* ORDER BY b) FROM t"),
        Some(Error::Parse { .. })
    ));
    assert!(tw.same("SELECT group_concat(ORDER BY b) FROM t").is_some());
}

#[test]
fn distinct_with_order_by() {
    let tw = twins("dist", T);
    tw.all(&[
        // The ORDER BY is the argument: the sorter dedups, last wins.
        "SELECT group_concat(DISTINCT c ORDER BY c) FROM t",
        "SELECT group_concat(DISTINCT c ORDER BY c DESC) FROM t",
        "SELECT group_concat(DISTINCT a ORDER BY a DESC) FROM t",
        "SELECT group_concat(DISTINCT (c) ORDER BY c) FROM t",
        // Not the argument: DISTINCT first (first wins), then sort.
        "SELECT group_concat(DISTINCT a ORDER BY b) FROM t",
        "SELECT group_concat(DISTINCT c ORDER BY c COLLATE BINARY) FROM t",
        "SELECT group_concat(DISTINCT c ORDER BY c || '') FROM t",
        "SELECT group_concat(DISTINCT c ORDER BY b DESC), count(DISTINCT c ORDER BY b) FROM t",
        "SELECT group_concat(DISTINCT x COLLATE NOCASE ORDER BY x COLLATE NOCASE) FROM (SELECT 'a' x UNION ALL SELECT 'A')",
        "SELECT group_concat(DISTINCT x COLLATE NOCASE ORDER BY x) FROM (SELECT 'a' x UNION ALL SELECT 'A' UNION ALL SELECT 'B' UNION ALL SELECT 'b')",
        "SELECT group_concat(DISTINCT x ORDER BY x COLLATE NOCASE) FROM (SELECT 'a' x UNION ALL SELECT 'A' UNION ALL SELECT 'B' UNION ALL SELECT 'b')",
        "SELECT group_concat(DISTINCT x ORDER BY x DESC NULLS FIRST) FROM (SELECT 'a' x UNION ALL SELECT NULL UNION ALL SELECT 'b')",
        // 1 and 1.0 are one key: the unique form keeps the later one.
        "SELECT count(DISTINCT x ORDER BY x), sum(DISTINCT x ORDER BY x) FROM (SELECT 1 x UNION ALL SELECT 1.0 UNION ALL SELECT '1')",
        "SELECT typeof(sum(DISTINCT x ORDER BY x)), typeof(sum(DISTINCT x)) FROM (SELECT 1.0 x UNION ALL SELECT 1)",
        "SELECT median(DISTINCT x ORDER BY x DESC) FROM (SELECT 1 x UNION ALL SELECT 1 UNION ALL SELECT 4)",
    ]);
    tw.refuse(
        "SELECT group_concat(DISTINCT c, ',' ORDER BY c) FROM t",
        SchemaKind::DistinctAggregateArgs,
    );
    tw.refuse(
        "SELECT string_agg(DISTINCT b, ',') FROM t",
        SchemaKind::DistinctAggregateArgs,
    );
}

// ---------------- FILTER ----------------

#[test]
fn filter_clause() {
    let tw = twins("filter", T);
    tw.all(&[
        "SELECT count(*) FILTER (WHERE a > 1), count(a) FILTER (WHERE 0), sum(a) FILTER (WHERE NULL), total(a) FILTER (WHERE 0), avg(a) FILTER (WHERE 0), min(a) FILTER (WHERE a > 1), max(a) FILTER (WHERE a < 7), group_concat(b) FILTER (WHERE 0) FROM t",
        "SELECT count(DISTINCT a) FILTER (WHERE b > 'v'), sum(DISTINCT a) FILTER (WHERE b < 'z') FROM t",
        "SELECT count(*) FILTER (WHERE (SELECT count(*) FROM t) > 1), count(*) FILTER (WHERE a IN (SELECT a FROM t WHERE a > 2)) FROM t",
        "SELECT count(*) FILTER (WHERE '1'), count(*) FILTER (WHERE 'abc'), count(*) FILTER (WHERE 0.5), count(*) FILTER (WHERE x'01') FROM t",
        "SELECT count(*) FILTER ( WHERE a > 1 ) AS n FROM t",
        "SELECT count(*) FILTER (WHERE a > 1) FROM t GROUP BY b > 'w' ORDER BY 1",
        "SELECT b > 'w', count(*) FILTER (WHERE a > 1) FROM t GROUP BY 1 HAVING count(*) FILTER (WHERE a > 2) > 0",
        "SELECT a FROM t GROUP BY a ORDER BY count(*) FILTER (WHERE b > 'v') DESC, a",
        "SELECT count(*) FROM t HAVING count(*) FILTER (WHERE a > 100) = 0",
        "SELECT group_concat(b, ';' ORDER BY b DESC) FILTER (WHERE a % 2 = 0) FROM t",
        "SELECT group_concat(b ORDER BY a) FILTER (WHERE a > 1) FROM t",
        "SELECT sum(a) FILTER (WHERE a > 1) + count(*) FILTER (WHERE a < 3) FROM t",
        "SELECT a, sum(a) FILTER (WHERE b <> 'y') FROM t GROUP BY a HAVING sum(a) FILTER (WHERE a > 1) IS NOT NULL ORDER BY sum(a) FILTER (WHERE a > 0) DESC",
        "SELECT sum(a) FILTER (WHERE c = 'a'), count(DISTINCT c) FILTER (WHERE c <> 'c') FROM t",
        "SELECT count(*) FILTER (WHERE b = (SELECT max(b) FROM t)) FROM t",
        "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM u WHERE k = t.a)) FROM t",
        "SELECT sum((SELECT k FROM u WHERE k = t.a)) FILTER (WHERE a < 3) FROM t",
        // `count()` is `count(*)`; FILTER is no keyword elsewhere.
        "SELECT count(), count() FILTER (WHERE a > 1) FROM t",
        "SELECT filter FROM (SELECT 1 filter)",
        "SELECT count(*) filter FROM t",
    ]);
    tw.refuse(
        "SELECT abs(a) FILTER (WHERE 1) FROM t",
        SchemaKind::FilterNonAggregate,
    );
    tw.refuse(
        "SELECT min(a, b) FILTER (WHERE 1) FROM t",
        SchemaKind::FilterNonAggregate,
    );
    tw.refuse(
        "SELECT count(*) FILTER (WHERE count(*) > 1) FROM t",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse(
        "SELECT count(*) FILTER (WHERE max(a) > 1) FROM t",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse(
        "SELECT count(DISTINCT) FILTER (WHERE 1) FROM t",
        SchemaKind::DistinctAggregateArgs,
    );
    assert!(tw.same("SELECT count(*) FILTER (a > 1) FROM t").is_some());
    assert!(
        tw.same("SELECT count(*) x FILTER (WHERE 1) FROM t")
            .is_some()
    );
}

#[test]
fn filter_and_bare_columns() {
    let tw = twins("bare", T);
    tw.all(&[
        "SELECT max(a) FILTER (WHERE a < 3), b FROM t",
        "SELECT min(a) FILTER (WHERE a > 1), b FROM t",
        // No row passes: the bare columns read the first row.
        "SELECT min(a) FILTER (WHERE a > 99), b FROM t",
        "SELECT max(a) FILTER (WHERE a > 99), b FROM t",
        "SELECT min(a) FILTER (WHERE a IS NULL), b FROM t",
        "SELECT max(c) FILTER (WHERE b < 'z'), b FROM t",
        "SELECT b, min(a) FILTER (WHERE a > 99) FROM t GROUP BY a > 1",
        "SELECT b, max(a) FILTER (WHERE a > 1) FROM t GROUP BY c",
        "SELECT max(a) FILTER (WHERE a > 99), b FROM t WHERE 0",
        "SELECT count(*) FILTER (WHERE a > 2), b FROM t",
        "SELECT sum(a) FILTER (WHERE a > 2), b FROM t GROUP BY c",
        // A subquery in the filter takes the per-row path.
        "SELECT max(a) FILTER (WHERE a < (SELECT 3)), b FROM t",
        "SELECT max(a) FILTER (WHERE EXISTS (SELECT 1 FROM u WHERE k = t.a)), b FROM t",
    ]);
}

#[test]
fn clause_ownership_in_subqueries() {
    let tw = twins("owner", T);
    tw.all(&[
        // FILTER naming the inner source: the inner query's.
        "SELECT (SELECT sum(t.a) FILTER (WHERE u.k > 1) FROM u) FROM t",
        "SELECT (SELECT sum(t.a) FROM u) FROM t",
        "SELECT (SELECT group_concat(t.b ORDER BY u.k) FROM u) FROM t",
        "SELECT (SELECT sum(u.k) FILTER (WHERE t.a > 1) FROM u) FROM t",
        "SELECT (SELECT group_concat(u.k ORDER BY t.a) FROM u) FROM t",
        // Only outer columns anywhere: the outer query's (one row).
        "SELECT (SELECT count(*) FILTER (WHERE t.a > 1) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FILTER (WHERE t.a < 3) FROM u) FROM t",
        "SELECT (SELECT count(*) FILTER (WHERE t.a > u.k) FROM u) FROM t ORDER BY 1",
        "SELECT a, (SELECT group_concat(k, '-' ORDER BY k DESC) FROM u WHERE k <= t.a) FROM t ORDER BY a",
        "SELECT group_concat(b ORDER BY (SELECT k FROM u WHERE k = t.a)) FROM t",
        "SELECT group_concat(b, (SELECT '+')) FROM t",
    ]);
}

// ---------------- the percentile family ----------------

const P: &str = "CREATE TABLE t(a, b, c TEXT COLLATE NOCASE, r REAL);
INSERT INTO t VALUES (1,'x','A',1.5),(2,'y','a',2),(3,'z','B',NULL),(NULL,'w',NULL,0.1),(2,'v','b',0.2),(7,'u','c',-3);
CREATE TABLE s(a); INSERT INTO s VALUES (1),('7');";

#[test]
fn percentile_values() {
    let tw = twins("pct", P);
    tw.all(&[
        "SELECT median(a), median(r), typeof(median(a)), typeof(percentile_disc(a, 0.5)) FROM t",
        "SELECT percentile(a, '50'), percentile(a, ' 50 '), percentile(a, '50.5'), percentile(a, true) FROM t",
        "SELECT percentile(a, 33.3), percentile(a, 66.7), percentile(a, 99.99), percentile(a, 0), percentile(a, 100) FROM t",
        "SELECT percentile_cont(a, 0.333), percentile_disc(a, 0.333), percentile_disc(a, 0), percentile_disc(a, 1), percentile_disc(a, 0.5), percentile_disc(a, 0.75) FROM t",
        "SELECT percentile(a, 50) FILTER (WHERE a <> 3), median(r) FILTER (WHERE r IS NOT NULL) FROM t",
        "SELECT percentile(a, 50 + 0), percentile(a, (SELECT 50)), percentile(a, 50.0), percentile(a, 1e2), percentile(a, 0.0) FROM t",
        "SELECT b, percentile(a, 50) FROM t GROUP BY b ORDER BY b",
        // Within 0.001 of the first row's fraction is "the same".
        "SELECT percentile(a, CASE WHEN a = 1 THEN 50 ELSE 50.05 END) FROM t",
        "SELECT percentile(a, 'abc') FROM t WHERE 0",
        "SELECT percentile(a, 50), median(a), percentile_disc(a, 0.5) FROM t WHERE 0",
        "SELECT median(DISTINCT x) FROM (SELECT 1 x UNION ALL SELECT 1 UNION ALL SELECT 4)",
        "SELECT median(x ORDER BY x DESC) FROM (SELECT 1 x UNION ALL SELECT 1 UNION ALL SELECT 4)",
        "SELECT percentile_disc(x, 0.5) FROM (SELECT 1 x UNION ALL SELECT 1 UNION ALL SELECT 4 UNION ALL SELECT 5)",
        "SELECT percentile_disc(x, 0.34), percentile_disc(x, 0.33333) FROM (SELECT 1 x UNION ALL SELECT 2 UNION ALL SELECT 4 UNION ALL SELECT 5)",
        "SELECT percentile_cont(x, 0.1) FROM (SELECT 9007199254740993 x UNION ALL SELECT 9007199254740995)",
        "SELECT median(-0.0), percentile(1, -0.0), percentile(1, 1e-400)",
        "SELECT median(x), percentile(x, 99) FROM (SELECT 3 x UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 0.5)",
        "SELECT percentile(r, 12.5) FILTER (WHERE r > 0) FROM t",
        "SELECT median(a) FILTER (WHERE a > 100), percentile(a, 500) FILTER (WHERE a > 100) FROM t",
        "SELECT a, median(r) FROM t GROUP BY a HAVING median(r) > 0 ORDER BY 1",
        "SELECT median(a) FROM t HAVING median(a) > 1",
        "SELECT (SELECT median(k) FROM (SELECT 1 k UNION ALL SELECT 4))",
        "SELECT percentile(a, 50 ORDER BY b), percentile(a, 'x' ORDER BY b) FILTER (WHERE 0) FROM t",
        "SELECT median(x) FROM (SELECT 1e308 x UNION ALL SELECT 1e308)",
        "SELECT median(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 9223372036854775806)",
    ]);
}

#[test]
fn percentile_errors() {
    let tw = twins("pcterr", P);
    let frac = SchemaKind::PercentileFraction;
    for sql in [
        "SELECT percentile(a, '50x') FROM t",
        "SELECT percentile(a, x'01') FROM t",
        "SELECT percentile(a, NULL) FROM t",
        "SELECT percentile(a, CASE WHEN a IS NULL THEN NULL ELSE 50 END) FROM t",
        "SELECT percentile(a, 101) FROM t",
        "SELECT percentile(a, -1) FROM t",
        "SELECT percentile(a, 100.0000001) FROM t",
        "SELECT percentile_cont(a, 1.5) FROM t",
        "SELECT percentile_disc(a, -0.1) FROM t",
        "SELECT percentile_cont(a, 50) FROM t",
        "SELECT percentile(a, b) FROM t",
    ] {
        tw.refuse(sql, frac);
    }
    for sql in [
        "SELECT percentile(a, CASE WHEN a = 1 THEN 50 ELSE 50.11 END) FROM t",
        "SELECT percentile(a, CASE WHEN a IS NULL THEN 10 ELSE 50 END) FROM t",
        "SELECT percentile(a, a) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::PercentileFractionVaries);
    }
    for sql in [
        "SELECT median(a) FROM s",
        "SELECT median(c) FROM t",
        "SELECT median(x) FROM (SELECT NULL x UNION ALL SELECT 'a')",
        "SELECT median(x) FROM (SELECT x'01' x)",
        "SELECT percentile_disc(b, 0.5) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::PercentileNotNumeric);
    }
    for sql in [
        "SELECT median(x) FROM (SELECT 1e308 * 10 x)",
        "SELECT percentile(x, 50) FROM (SELECT 1 x UNION ALL SELECT -1e308 * 10)",
    ] {
        tw.refuse(sql, SchemaKind::PercentileInfinite);
    }
    tw.refuse("SELECT median(1, 2) FROM t", SchemaKind::WrongArgumentCount);
    tw.refuse("SELECT median() FROM t", SchemaKind::WrongArgumentCount);
    tw.refuse(
        "SELECT percentile(a) FROM t",
        SchemaKind::WrongArgumentCount,
    );
    tw.refuse(
        "SELECT percentile(DISTINCT a, 50) FROM t",
        SchemaKind::DistinctAggregateArgs,
    );
}

/// A bound P: the check runs at step (the error depends on the
/// binding) and agrees with the pin's literal form.
#[test]
fn percentile_bound_fraction() {
    let tw = twins("pctbind", P);
    for (p, lit) in [
        (25.0, "25.0"),
        (99.5, "99.5"),
        (150.0, "150.0"),
        (-0.5, "-0.5"),
    ] {
        let got = block_on(async {
            let mut db = open(&tw.hipp).await;
            let mut q = db.prepare("SELECT percentile(a, ?1) FROM t").await?;
            q.bind_f64(1, p)?;
            collect_stmt(&mut q).await
        });
        let (ok, want) = oracle_raw(&tw.oracle, &format!("SELECT percentile(a, {lit}) FROM t"));
        match got {
            Ok(rows) => {
                assert!(ok, "{lit}");
                assert_eq!(rows, want, "{lit}");
            }
            Err(e) => {
                assert!(!ok, "{lit}: {e:?}");
                assert!(matches!(
                    e,
                    Error::Schema {
                        kind: SchemaKind::PercentileFraction
                    }
                ));
            }
        }
    }
}

// ---------------- the ungated sweep ----------------

#[test]
fn sum_overflow_and_compensated_sums() {
    let tw = twins("sum", T);
    tw.all(&[
        "SELECT sum(a), total(a), avg(a), count(a), min(a), max(a) FROM t",
        "SELECT total(9223372036854775807), avg(9223372036854775807) FROM t",
        "SELECT sum(a) FROM (SELECT 9223372036854775807 a UNION ALL SELECT -1 UNION ALL SELECT 1)",
        // A REAL after the overflow absorbs it (integer, text and
        // NULL addends do not).
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT 1.0)",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT 'a')",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT x'31')",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT 1.0 UNION ALL SELECT 5)",
        "SELECT avg(x), total(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1)",
        "SELECT avg(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 9223372036854775807 UNION ALL SELECT -9223372036854775807)",
        // Kahan–Babuška–Neumaier REAL sums.
        "SELECT sum(r), total(r), avg(r) FROM t",
        "SELECT (SELECT sum(r) FROM t) + 1",
        "SELECT sum(x) FROM (SELECT 0.1 x UNION ALL SELECT 0.2 UNION ALL SELECT 0.3)",
        "SELECT sum(x), total(x), avg(x) FROM (SELECT 1e18 x UNION ALL SELECT 1 UNION ALL SELECT -1e18)",
        "SELECT sum(x) FROM (SELECT 1e16 x UNION ALL SELECT 1 UNION ALL SELECT 1 UNION ALL SELECT -1e16)",
        "SELECT sum(x) FROM (SELECT 1e308 x UNION ALL SELECT 1e308 UNION ALL SELECT -1e308)",
        "SELECT sum(x) FROM (SELECT 1e308 x UNION ALL SELECT 1e308 UNION ALL SELECT -1e308 UNION ALL SELECT -1e308)",
        "SELECT sum(x), avg(x) FROM (SELECT 1e308 x UNION ALL SELECT 1e308)",
        "SELECT sum(x) FROM (SELECT 4503599627370497 x UNION ALL SELECT 0.5)",
        "SELECT sum(x) FROM (SELECT 0.5 x UNION ALL SELECT 4503599627370497)",
        "SELECT sum(x), avg(x) FROM (SELECT 0.5 x UNION ALL SELECT 9223372036854775807 UNION ALL SELECT -9223372036854775807)",
        "SELECT sum(x), total(x) FROM (SELECT -0.0 x)",
        // Text addends: INTEGER only when wholly an integer.
        "SELECT sum(x), typeof(sum(x)) FROM (SELECT x'31' x)",
        "SELECT sum(x), typeof(sum(x)) FROM (SELECT '  12abc' x)",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT ' 5 ' x)",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT '5.0' x)",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT '5e0' x)",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT '-9223372036854775808' x)",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT '+7' x UNION ALL SELECT '00012')",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT '1_000' x UNION ALL SELECT '12.' UNION ALL SELECT '.5')",
        "SELECT typeof(sum(x)), sum(x) FROM (SELECT CAST(x'3500' AS TEXT) x)",
        "SELECT sum(x), avg(x), total(x) FROM (SELECT '1e2' x UNION ALL SELECT ' 5 ' UNION ALL SELECT '0x10' UNION ALL SELECT '9223372036854775808' UNION ALL SELECT '-0')",
        "SELECT avg(b), sum(b), total(b), avg(c) FROM t",
        "SELECT avg(x), sum(x), total(x) FROM (SELECT '1' x UNION ALL SELECT '2')",
        "SELECT total(x), sum(x), typeof(sum(x)), typeof(total(x)) FROM (SELECT 'a' x UNION ALL SELECT NULL)",
    ]);
    for sql in [
        "SELECT sum(9223372036854775807) FROM t",
        "SELECT sum(a) FROM (SELECT 9223372036854775807 a UNION ALL SELECT 1 UNION ALL SELECT -5)",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT -1)",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT '1')",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT NULL)",
        "SELECT total(x), sum(x) FROM (SELECT -9223372036854775808 x UNION ALL SELECT -1)",
        "SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 9223372036854775807 UNION ALL SELECT -9223372036854775807)",
    ] {
        tw.refuse(sql, SchemaKind::IntegerOverflow);
    }
}

#[test]
fn group_concat_separators() {
    let tw = twins("sep", "");
    tw.all(&[
        // Each value's own separator goes before it; a NULL one
        // adds nothing (hipp returned NULL for the whole result).
        "SELECT group_concat(x, s) FROM (SELECT 'a' x, '1' s UNION ALL SELECT 'b', NULL UNION ALL SELECT 'c', '3')",
        "SELECT group_concat(x, s) FROM (SELECT 'a' x, NULL s UNION ALL SELECT 'b', '2' UNION ALL SELECT 'c', '3')",
        "SELECT group_concat(x, s) FROM (SELECT NULL x, 'Q' s UNION ALL SELECT 'b', '2' UNION ALL SELECT 'c', '3')",
        "SELECT group_concat(x, s ORDER BY x DESC) FROM (SELECT 'a' x, '1' s UNION ALL SELECT 'b', '2' UNION ALL SELECT 'c', '3')",
        "SELECT group_concat(x, '') FROM (SELECT 3 x UNION ALL SELECT 1)",
        "SELECT group_concat(x) FROM (SELECT '' x UNION ALL SELECT '' UNION ALL SELECT 'a')",
        "SELECT group_concat(x, ',') FROM (SELECT NULL x UNION ALL SELECT '' UNION ALL SELECT 'a')",
        "SELECT typeof(group_concat('')), quote(group_concat('')), quote(group_concat(x'')), quote(group_concat(x'00ff'))",
        "SELECT group_concat(1.5), group_concat(1e300), group_concat(-0.0)",
        "SELECT hex(group_concat(x)) FROM (SELECT CAST(x'610062' AS TEXT) x UNION ALL SELECT 'q')",
        "SELECT hex(group_concat(x, CAST(x'2c002c' AS TEXT))) FROM (SELECT 'a' x UNION ALL SELECT 'q')",
    ]);
    let tw = twins("sep2", T);
    tw.refuse(
        "SELECT group_concat(DISTINCT b, ',') FROM t",
        SchemaKind::DistinctAggregateArgs,
    );
    tw.refuse(
        "SELECT count(DISTINCT a, b) FROM t",
        SchemaKind::WrongArgumentCount,
    );
    // The scalar min / max forms ignore DISTINCT, as on the pin.
    tw.same("SELECT min(DISTINCT a, b), max(DISTINCT a) FROM t");
}

const MIX: &str = "CREATE TABLE t(a, b, c TEXT COLLATE NOCASE, r REAL, d TEXT COLLATE RTRIM);
INSERT INTO t VALUES (1,'x','A',1.5,'p '),(2,'y','a',2,'p'),(3,'z','B',NULL,'q'),(NULL,'w',NULL,0.1,NULL),(2,'v','b',0.2,'q  '),('7','u','c',-3,x'70'),(x'01','t','C',5,'P'),(2.0,'s','é',2.0,'é');";

#[test]
fn mixed_classes_and_collations() {
    let tw = twins("mix", MIX);
    tw.all(&[
        "SELECT min(a), max(a), min(b), max(b), min(c), max(c), min(d), max(d), min(r), max(r) FROM t",
        "SELECT count(DISTINCT a), count(DISTINCT c), count(DISTINCT d), count(DISTINCT r) FROM t",
        "SELECT group_concat(DISTINCT d), group_concat(DISTINCT c) FROM t",
        "SELECT c, count(*), min(b), max(b) FROM t GROUP BY c ORDER BY c",
        "SELECT d, count(*), group_concat(b) FROM t GROUP BY d ORDER BY d",
        "SELECT max(a), min(a), b FROM t",
        "SELECT min(c), b FROM t",
        "SELECT max(c), b FROM t",
        "SELECT max(d), b, min(d) FROM t",
        "SELECT max(r), count(*), b FROM t",
        "SELECT sum(a), total(a), avg(a), typeof(avg(a)), typeof(sum(r)) FROM t",
        "SELECT min(x), max(x), typeof(min(x)), typeof(max(x)) FROM (SELECT 1 x UNION ALL SELECT 1.0)",
        "SELECT min(x), max(x), typeof(min(x)), typeof(max(x)) FROM (SELECT 1.0 x UNION ALL SELECT 1)",
        "SELECT max(x COLLATE NOCASE), min(x COLLATE NOCASE) FROM (SELECT 'a' x UNION ALL SELECT 'A' UNION ALL SELECT 'b' UNION ALL SELECT 'B')",
        "SELECT count(DISTINCT x), count(DISTINCT x COLLATE NOCASE), group_concat(DISTINCT x COLLATE NOCASE) FROM (SELECT 'a' x UNION ALL SELECT 'A')",
        "SELECT count(DISTINCT x), sum(DISTINCT x), avg(DISTINCT x) FROM (SELECT 1 x UNION ALL SELECT 1.0 UNION ALL SELECT '1')",
        "SELECT count(DISTINCT c COLLATE BINARY), max(c COLLATE BINARY), min(b COLLATE NOCASE) FROM t",
    ]);
}

#[test]
fn empty_input_and_query_shapes() {
    let tw = twins("shape", MIX);
    tw.all(&[
        "SELECT count(*), count(a), sum(a), total(a), avg(a), min(a), max(a), group_concat(a), string_agg(a, ';'), median(r), percentile(r, 5) FROM t WHERE 0",
        "SELECT count(*), sum(a) FROM t WHERE 0 GROUP BY a",
        "SELECT max(b), b FROM t WHERE 0",
        "SELECT count(*), b FROM t WHERE 0",
        "SELECT count(*) FILTER (WHERE 1), group_concat(b ORDER BY a) FROM t WHERE 0",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1 ORDER BY sum(r)",
        "SELECT x FROM (SELECT a x FROM t UNION SELECT 5) GROUP BY x HAVING sum(x) > 2 ORDER BY count(*), x",
        "SELECT max(a) FROM t UNION SELECT min(a) FROM t ORDER BY 1",
        "SELECT count(*) FROM (SELECT 1 UNION ALL SELECT 2) HAVING count(*) > 1",
        "SELECT a FROM t UNION ALL SELECT count(*) FROM t ORDER BY 1",
        "SELECT x, count(*) FROM (SELECT a x FROM t UNION ALL SELECT b FROM t) GROUP BY x HAVING count(*) > 1 ORDER BY 2 DESC, 1",
        "SELECT (SELECT count(*) FROM t u WHERE u.a < t.a) c, a FROM t ORDER BY c, b",
        "SELECT a, (SELECT max(b) FROM t u WHERE u.a = t.a HAVING count(*) > 1) FROM t ORDER BY b",
        "SELECT count(*) FROM t GROUP BY a ORDER BY count(*) DESC, min(b) LIMIT 2",
        "SELECT group_concat(b) FROM t GROUP BY c HAVING c IS NULL",
        "SELECT (SELECT group_concat(b, '' ORDER BY b) FROM t WHERE c = 'a') || (SELECT count(*) FILTER (WHERE r > 1) FROM t)",
        "SELECT b FROM t WHERE r > (SELECT median(r) FROM t) ORDER BY b",
        "SELECT count(*) FROM t WHERE a IN (SELECT max(a) FILTER (WHERE typeof(a) = 'integer') FROM t)",
    ]);
}

/// Which groups finalize their aggregates (an erroring one fails
/// the statement): the pin finalizes every aggregate of a group
/// before its HAVING, never a group its WHERE-able HAVING terms
/// drop, and none past a group-ordered LIMIT.
#[test]
fn aggregate_errors_and_group_finalization() {
    let tw = twins(
        "final",
        "CREATE TABLE g(k, v); INSERT INTO g VALUES (1, 5), (2, 9223372036854775807), (2, 1), (3, 'x');",
    );
    tw.all(&[
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1",
        "SELECT k FROM g GROUP BY k HAVING k = 1 AND sum(v) > 0",
        "SELECT k, sum(v) FROM g GROUP BY k LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY k DESC LIMIT 1",
        "SELECT k, count(*) FROM g GROUP BY k HAVING count(*) < 2 ORDER BY k LIMIT 1",
        "SELECT k, count(*) FROM g GROUP BY k ORDER BY count(*) LIMIT 1",
        "SELECT k, sum(v) FROM g GROUP BY k LIMIT 0",
        "SELECT (SELECT sum(v) FROM g GROUP BY k)",
        "SELECT (SELECT sum(v) FROM g GROUP BY k ORDER BY k DESC)",
        "SELECT EXISTS (SELECT sum(v) FROM g GROUP BY k)",
        "SELECT * FROM (SELECT k, sum(v) s FROM g GROUP BY k LIMIT 1)",
        "SELECT (SELECT sum(v) FROM g)",
        "SELECT sum(v) FROM g UNION ALL SELECT 1",
        "SELECT count(*) FROM (SELECT sum(v) s FROM g)",
    ]);
    for sql in [
        "SELECT k FROM g GROUP BY k HAVING count(*) > 5 AND sum(v) > 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING count(*) < 2",
        "SELECT k FROM g GROUP BY k HAVING count(*) < 2 ORDER BY sum(v)",
        "SELECT k, max(v) FROM g GROUP BY k HAVING sum(v) IS NOT NULL",
        "SELECT k, sum(v) FROM g GROUP BY k LIMIT 1 OFFSET 1",
        "SELECT k, sum(v) FROM g GROUP BY k ORDER BY sum(v) LIMIT 1",
        "SELECT 1 IN (SELECT sum(v) FROM g GROUP BY k)",
        "SELECT k, (SELECT sum(v) FROM g WHERE k = 2) FROM g LIMIT 1",
    ] {
        tw.refuse(sql, SchemaKind::IntegerOverflow);
    }
    for sql in [
        "SELECT k, median(v) FROM g GROUP BY k HAVING k < 3",
        "SELECT k FROM g GROUP BY k HAVING k < 3 OR median(v) > 0",
        "SELECT k FROM g GROUP BY k ORDER BY median(v) LIMIT 1",
    ] {
        tw.same(sql);
    }
}

// ---------------- review fixes ----------------

const OI: &str = "CREATE TABLE o(x); CREATE TABLE i(y);
INSERT INTO o VALUES (1),(2),(3); INSERT INTO i VALUES (10),(20);";

/// Regression: min / max drop their ORDER BY only when the pin
/// generates code; ownership still reads the keys, so an ORDER BY
/// naming the inner source makes the inner query the owner (hipp
/// gave the outer query the aggregate: one row, a misuse error, or
/// EXISTS flipping).
#[test]
fn minmax_order_by_still_decides_the_owner() {
    let tw = twins("mmown", OI);
    tw.all(&[
        "SELECT (SELECT max(o.x ORDER BY i.y) FROM i) FROM o",
        "SELECT x FROM o WHERE x = (SELECT max(o.x ORDER BY i.y) FROM i)",
        "SELECT EXISTS (SELECT max(o.x ORDER BY i.y) FROM (SELECT 1 y) i WHERE 0) FROM (SELECT 1 x) o",
        "SELECT x, (SELECT min(o.x ORDER BY i.y) + count(*) FROM i) FROM o",
        "SELECT (SELECT min(o.x ORDER BY i.y DESC, o.x) FROM i) FROM o ORDER BY 1",
        "SELECT (SELECT max(o.x ORDER BY i.y) FILTER (WHERE i.y > 10) FROM i) FROM o",
        // Keys naming only the outer query: the outer's (one row).
        "SELECT (SELECT max(1 ORDER BY o.x) FROM i) FROM o",
        "SELECT (SELECT min(o.x ORDER BY o.x) FROM i) FROM o",
        "SELECT (SELECT max(i.y ORDER BY o.x) FROM i) FROM o",
        // Still resolved: an unknown column errors.
        "SELECT max(x ORDER BY nosuch) FROM o",
        "SELECT max(x ORDER BY (SELECT count(*))) FROM o",
    ]);
}

/// Regression: an aggregate inside min / max's ORDER BY is the
/// pin's "misuse of aggregate function" (hipp ran it).
#[test]
fn minmax_order_by_nested_aggregate_is_misuse() {
    let tw = twins("mmmis", OI);
    for sql in [
        "SELECT max(x ORDER BY count(*)) FROM (SELECT 1 x)",
        "SELECT min(x ORDER BY x, sum(x)) FROM o",
        "SELECT (SELECT max(i.y ORDER BY sum(o.x)) FROM (SELECT 1 y UNION ALL SELECT 3) i) FROM (SELECT 1 x UNION ALL SELECT 2 UNION ALL SELECT 3) o",
        "SELECT (SELECT max(i.y ORDER BY sum(o.x)) FROM i) FROM o",
        "SELECT group_concat(x ORDER BY count(*)) FROM o",
    ] {
        tw.refuse(sql, SchemaKind::MisusedAggregate);
    }
}

/// Regression: `f(ORDER BY ...)` with no arguments parses on the
/// pin, which drops the clause unresolved (`count(ORDER BY x)` is
/// `count(*)`); hipp refused it as a syntax error.
#[test]
fn order_by_without_arguments() {
    let tw = twins("noargs", OI);
    tw.all(&[
        "SELECT count(ORDER BY x) FROM (SELECT 1 x)",
        "SELECT count(ORDER BY x) FILTER (WHERE x > 1) FROM (SELECT 1 x UNION ALL SELECT 2)",
        "SELECT count(ORDER BY nosuch) FROM o",
        "SELECT count(ORDER BY count(*)) FROM o",
        "SELECT count(ORDER BY x DESC NULLS LAST, 2) FROM o",
        "SELECT count(ORDER BY x) FROM o WHERE 0",
        "SELECT x, (SELECT count(ORDER BY o.x) FROM i) FROM o",
        "SELECT typeof(changes(ORDER BY x)) FROM o",
        // Syntax and argument errors stand.
        "SELECT count(* ORDER BY x) FROM o",
        "SELECT count(ORDER BY) FROM o",
        "SELECT sum(ORDER BY x) FROM o",
        "SELECT max(ORDER BY x) FROM o",
        "SELECT group_concat(ORDER BY x) FROM o",
        "SELECT abs(ORDER BY x) FROM o",
        "SELECT random(ORDER BY x) FILTER (WHERE 1) FROM o",
    ]);
    tw.refuse(
        "SELECT count(DISTINCT ORDER BY x) FROM o",
        SchemaKind::DistinctAggregateArgs,
    );
}

/// Regression: a NaN REAL (`inf + -inf`) is NULL on the pin, for
/// sum / total / avg and for the arithmetic operators; hipp
/// answered a REAL (0.0) and wrote it to the file.
#[test]
fn nan_results_are_null() {
    let tw = twins("nan", OI);
    tw.all(&[
        "SELECT typeof(sum(x)), typeof(total(x)), typeof(avg(x)), sum(x) IS NULL FROM (SELECT 9e999 x UNION ALL SELECT -9e999)",
        "SELECT sum(x) FROM (SELECT 1e308 x UNION ALL SELECT 1e308 UNION ALL SELECT -9e999)",
        "SELECT count(s) FROM (SELECT total(x) s FROM (SELECT 9e999 x UNION ALL SELECT -9e999))",
        "SELECT typeof(avg(x)) FROM (SELECT 9e999 x UNION ALL SELECT -9e999 UNION ALL SELECT 1)",
        "SELECT avg(x), total(x), sum(x) FROM (SELECT 9e999 x UNION ALL SELECT 9e999)",
        "SELECT typeof(sum(x)) FROM (SELECT 9e999 x UNION ALL SELECT 1)",
        "SELECT group_concat(s), max(s), min(s) FROM (SELECT sum(x) s FROM (SELECT 9e999 x UNION ALL SELECT -9e999))",
        "SELECT median(x), percentile(x, 50), percentile_cont(x, 0.25) FROM (SELECT -1.7e308 x UNION ALL SELECT 1.7e308)",
        "SELECT typeof(9e999 + -9e999), typeof(9e999 - 9e999), typeof(0 * 9e999), typeof(9e999 / 9e999), typeof(9e999 * -9e999)",
        "SELECT 9e999 - 9e999 IS NULL, -9e999 / -9e999, 0.0 * -9e999",
        "SELECT x * 9e999 FROM o",
        "SELECT sum(x * 9e999 - 9e999) FROM o",
    ]);
    // Through the file: the stored value is a NULL, not a NaN.
    tw.all(&[
        "CREATE TABLE n AS SELECT total(x) s, 9e999 - 9e999 d FROM (SELECT 9e999 x UNION ALL SELECT -9e999)",
        "INSERT INTO n SELECT sum(x), x - x FROM (SELECT 9e999 x UNION ALL SELECT -9e999)",
        "UPDATE n SET d = 9e999 + -9e999 WHERE rowid = 1",
        "SELECT typeof(s), typeof(d), s IS NULL, d IS NULL FROM n",
        "SELECT count(s), count(d) FROM n",
    ]);
}

/// Regression: an outer LIMIT that stops reading a derived table
/// keeps its later groups from being finalized on the pin, so a
/// sum() overflow in such a group does not fail the statement
/// (the ungated overflow fix made hipp refuse it).
#[test]
fn outer_limit_stops_derived_groups() {
    let tw = twins(
        "dlim",
        "CREATE TABLE g(k, v); INSERT INTO g VALUES (1,1),(2,9223372036854775807),(2,1),(3,5);",
    );
    tw.all(&[
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 1",
        "SELECT * FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 1",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 0",
        "SELECT * FROM (SELECT * FROM (SELECT k x, sum(v) s FROM g GROUP BY k)) LIMIT 1",
        "SELECT * FROM (SELECT * FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 5) LIMIT 1",
        "SELECT * FROM (SELECT * FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 2) LIMIT 1",
        "SELECT (SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 1)",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k ORDER BY k) LIMIT 1",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k ORDER BY k DESC) LIMIT 1",
        "SELECT x, (SELECT 1) FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 1",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k HAVING sum(v) > 0) LIMIT 1",
        "SELECT EXISTS (SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k))",
    ]);
    for sql in [
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 2",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 1 OFFSET 1",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k) ORDER BY x LIMIT 1",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k ORDER BY s) LIMIT 1",
        "SELECT count(*) FROM (SELECT k x, sum(v) s FROM g GROUP BY k) LIMIT 1",
        "SELECT x FROM (SELECT k x, sum(v) s FROM g GROUP BY k)",
    ] {
        tw.refuse(sql, SchemaKind::IntegerOverflow);
    }
}

/// Regression: a HAVING term moves into WHERE on the pin (so the
/// groups it rejects never form) only with a GROUP BY, when it is
/// not the literal 0 / FALSE (an AND with such an operand folds to
/// 0), and when it is constant or built from BINARY GROUP BY terms
/// and deterministic functions. hipp moved every term without
/// aggregates or subqueries, skipping errors the pin raises.
#[test]
fn having_terms_moved_to_where() {
    let tw = twins(
        "hmove",
        "CREATE TABLE g(k, v); INSERT INTO g VALUES (1,1),(2,9223372036854775807),(2,1),(3,5);
CREATE TABLE h(k TEXT COLLATE NOCASE, v); INSERT INTO h VALUES ('a',1),('b',9223372036854775807),('b',1);
CREATE TABLE t2(k TEXT, v); INSERT INTO t2 VALUES ('a',1),('b',9223372036854775807),('b',1);",
    );
    // Moved: the overflowing group never forms.
    tw.all(&[
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING abs(k) = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k + 0 = 1",
        "SELECT k + 1, sum(v) FROM g GROUP BY k + 1 HAVING k + 1 = 2",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 OR 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k IN (1)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k BETWEEN 1 AND 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING +k = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING coalesce(k, 0) = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k COLLATE NOCASE = 1",
        "SELECT k, sum(v) FROM g AS x GROUP BY x.k HAVING k = 1",
        "SELECT k, sum(v) FROM g GROUP BY 1 HAVING k = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING CASE WHEN k = 1 THEN 1 END",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING typeof(k) = 'integer' AND k < 2",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING printf('%d', k) = '1'",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING likely(k = 1)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING iif(k = 1, 1, 0)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING sqlite_version() = 'x'",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING NULL",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING 0.0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING '0'",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING 1 = 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING TRUE AND k = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND v = 1",
        "SELECT k, sum(v) FROM g GROUP BY k, v HAVING v = 1",
        "SELECT k, sum(v) FROM t2 GROUP BY k HAVING k = 'a'",
        "SELECT k, sum(v) FROM t2 GROUP BY k COLLATE BINARY HAVING k = 'a'",
        "SELECT k, sum(v) FROM h GROUP BY k COLLATE BINARY HAVING k COLLATE BINARY = 'a'",
        // An AND over 0 folds to 0 only without function calls.
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND random() = 5 AND 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING abs(k) = 1 AND 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND (0 AND abs(k))",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND sum(v) AND 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND (0 OR 0)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND CAST(0 AS INT)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND k LIKE 1 AND 0",
    ]);
    // Not moved: every group's sum() is computed and overflows.
    for sql in [
        "SELECT k, sum(v) FROM g GROUP BY k HAVING random() = 5",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING changes() = 5",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING last_insert_rowid() = 99",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING (0)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING 00",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING 0x0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING false",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING (k = 1 AND 0) AND k = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING 0 AND k = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND (SELECT 1) AND 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND 0x0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k = 1 AND (k = 2 AND 0)",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING k COLLATE NOCASE = 1 AND 0",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING v = 1",
        "SELECT k, sum(v) FROM g GROUP BY k HAVING (SELECT 0)",
        "SELECT sum(v) FROM g HAVING 0",
        "SELECT sum(v) FROM g HAVING random() = 5",
        "SELECT k, sum(v) FROM h GROUP BY k HAVING k = 'a'",
        "SELECT k, sum(v) FROM h GROUP BY k HAVING k COLLATE BINARY = 'a'",
        "SELECT k, sum(v) FROM h GROUP BY k HAVING upper(k) = 'A'",
        "SELECT k, sum(v) FROM g GROUP BY k COLLATE NOCASE HAVING k = 1",
        "SELECT k, sum(v) FROM t2 GROUP BY k COLLATE NOCASE HAVING k COLLATE NOCASE = 'a'",
        "SELECT k, sum(v) FROM g GROUP BY k COLLATE RTRIM HAVING k COLLATE RTRIM = 1",
        "SELECT k, sum(v) FROM (SELECT * FROM g) GROUP BY k HAVING random() = 5",
        "SELECT (SELECT sum(v) FROM g GROUP BY k HAVING random() = 5)",
    ] {
        tw.refuse(sql, SchemaKind::IntegerOverflow);
    }
}

// ---------------- generated corpus ----------------

/// Random aggregate calls — every kind, with and without DISTINCT,
/// ORDER BY and FILTER — over a table of mixed storage classes and
/// collations, grouped and not; hipp and the pin must agree on
/// every result, or both refuse. `HIPP_AGG_CORPUS` scales the
/// statement count (default 240) and `HIPP_AGG_SEED` varies it.
#[test]
fn generated_aggregate_corpus() {
    let n: usize = std::env::var("HIPP_AGG_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    let seed: u64 = std::env::var("HIPP_AGG_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x0a99_2026_1102_5eed);
    let mut rng = Rng(seed | 1);
    let pool = [
        "NULL",
        "0",
        "1",
        "2",
        "-3",
        "7",
        "1.5",
        "-0.25",
        "2.0",
        "1e300",
        "0.1",
        "'5'",
        "' 7 '",
        "'1.5'",
        "'abc'",
        "'B'",
        "'b'",
        "'a '",
        "x'31'",
        "x'00'",
        "9223372036854775807",
        "4503599627370497",
    ];
    let mut setup = String::from("CREATE TABLE t(g, x, y, s TEXT COLLATE NOCASE, n);\n");
    for _ in 0..40 {
        setup.push_str(&format!(
            "INSERT INTO t VALUES ({}, {}, {}, {}, {});\n",
            rng.below(4),
            rng.pick(&pool),
            rng.pick(&pool),
            rng.pick(&["'a'", "'A'", "'b'", "'B '", "'c'", "NULL", "'é'"]),
            rng.pick(&["1", "2", "3.5", "-1", "NULL", "10", "0.25", "7"]),
        ));
    }
    let tw = twins("corpus", &setup);
    let cols = ["x", "y", "s", "n", "g"];
    let keys = [
        "x",
        "y DESC",
        "s",
        "s DESC NULLS FIRST",
        "n NULLS LAST",
        "x COLLATE NOCASE",
        "g, y",
        "y, x DESC",
        "1",
        "-n",
    ];
    let preds = [
        "x > 1",
        "y IS NOT NULL",
        "s = 'a'",
        "n < 5",
        "g <> 2",
        "typeof(x) = 'integer'",
        "0",
        "s",
        "x",
    ];
    let mut checked = 0;
    for _ in 0..n {
        let mut items = Vec::new();
        for _ in 0..3 {
            let col = rng.pick(&cols);
            let kind = rng.below(12);
            let distinct = rng.below(4) == 0;
            let d = if distinct { "DISTINCT " } else { "" };
            let mut call = match kind {
                0 => format!("count({d}{col}"),
                1 => format!("sum({d}{col}"),
                2 => format!("total({d}{col}"),
                3 => format!("avg({d}{col}"),
                4 => format!("min({d}{col}"),
                5 => format!("max({d}{col}"),
                6 if !distinct => {
                    format!("group_concat({col}, {}", rng.pick(&["'-'", "s", "NULL"]))
                }
                7 if !distinct => format!("string_agg({col}, ';'"),
                8 => format!("median({d}n"),
                9 if !distinct => {
                    format!("percentile(n, {}", rng.pick(&["50", "12.5", "100", "'75'"]))
                }
                10 if !distinct => format!("percentile_disc(n, {}", rng.pick(&["0.5", "0", "0.9"])),
                11 if !distinct => format!("percentile_cont(n, {}", rng.pick(&["0.5", "1", "0.3"])),
                _ => format!("group_concat({d}{col}"),
            };
            if rng.below(2) == 0 {
                let k = if distinct && rng.below(2) == 0 {
                    // The argument itself (the unique form) or a
                    // differently-collated copy of it.
                    let arg = call.rsplit(['(', ' ']).next().unwrap_or("x").to_string();
                    if rng.below(2) == 0 {
                        arg
                    } else {
                        format!("{arg} DESC")
                    }
                } else {
                    rng.pick(&keys).to_string()
                };
                call.push_str(&format!(" ORDER BY {k}"));
            }
            call.push(')');
            if rng.below(2) == 0 {
                call.push_str(&format!(" FILTER (WHERE {})", rng.pick(&preds)));
            }
            items.push(call);
        }
        let sql = if rng.below(2) == 0 {
            format!("SELECT {} FROM t", items.join(", "))
        } else {
            format!(
                "SELECT g, {} FROM t GROUP BY g ORDER BY g",
                items.join(", ")
            )
        };
        if tw.same(&sql).is_none() {
            checked += 1;
        }
    }
    // Most of the corpus runs (not just agrees on refusing).
    assert!(checked > n / 2, "only {checked} of {n} statements ran");
}
