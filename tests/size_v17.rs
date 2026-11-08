//! v0.17 size pass, Stage 1 unit 1 (`size-sort`): every sort in the
//! engine now goes through one shared, non-generic stable merge sort
//! (`arena::sort_perm`) instead of a `slice::sort` instantiation per
//! element type and comparator. A refactor, so the claim is "nothing
//! observable changed", checked differentially against the pinned
//! oracle (SQLite 3.54.1) on the paths that sort, with inputs long
//! enough to cross the insertion-sort cutoff and several merge passes
//! and with many ties (stability shows):
//!
//! - ORDER BY (ascending, descending, NULLS FIRST / LAST, collations,
//!   with and without LIMIT / OFFSET), DISTINCT + ORDER BY;
//! - GROUP BY under collations and the window rewrite's directions;
//! - compound SELECTs (UNION / INTERSECT / EXCEPT, with ORDER BY);
//! - window PARTITION BY / ORDER BY, aggregate ORDER BY, percentile;
//! - IN lists with duplicates, UPDATE / DELETE two-pass writes,
//!   WITHOUT ROWID tables, R*Tree and fts5 rowid-ordered writes;
//! - the fts5 rank sorter, including the one comparator the audit
//!   found was not a total order: a NaN rank (bm25 under an infinite
//!   weight sum) compared "equal to everything", so its rows stayed
//!   where the sort put them. The pin reads the rank as an SQL value,
//!   where NaN is NULL: those rows sort first ascending, last
//!   descending, ties in rowid order. Regression tests below.
//!
//! Stage 1 unit 2 (`size-mono`) adds corpora at the end of the file
//! for the machinery it compiles once instead of per type: the
//! evaluator's helpers and conditions, the aggregate walks, the
//! compound ORDER BY resolver, B-tree page splits, `AVec` growth and
//! the per-execution plans.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

/// A hand-rolled xorshift PRNG (deterministic, no dependencies).
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
        self.next() % n.max(1)
    }
}

/// One row as the pin's shell prints it in list mode.
fn render<S: hipp::Storage>(q: &hipp::Statement<'_, S>, line: &mut String) {
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

/// Split a script on `;` outside quotes.
fn split(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    for c in script.chars() {
        match q {
            Some(x) if c == x => q = None,
            Some(_) => {}
            None if c == '\'' || c == '"' || c == '[' => q = Some(if c == '[' { ']' } else { c }),
            None if c == ';' => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            None => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A script on one hipp connection: every row, stopping at the
/// first error.
fn hipp_script(
    path: &std::path::Path,
    script: &str,
) -> (bool, Vec<String>, Option<Error<FsError>>) {
    block_on(async {
        let mut db = Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
            .await
            .expect("open");
        let mut lines = Vec::new();
        for stmt in split(script) {
            let mut q = match db.prepare(&stmt).await {
                Ok(q) => q,
                Err(e) => return (false, lines, Some(e)),
            };
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
                    Err(e) => return (false, lines, Some(e)),
                }
            }
        }
        (true, lines, None)
    })
}

/// The same script on the pin's shell (`-bail`).
fn oracle_script(path: &std::path::Path, script: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
        .arg("-bail")
        .arg(path)
        .arg(script)
        .output()
        .expect("oracle runs");
    (
        out.status.success() && out.stderr.is_empty(),
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// A setup script then a list of queries, on fresh twin files: the
/// same rows and the same success for each query.
fn same(tag: &str, setup: &str, queries: &[&str]) {
    let h = common::temp_db(&format!("v17sz-{tag}-h"));
    let o = common::temp_db(&format!("v17sz-{tag}-o"));
    let (hok, _, he) = hipp_script(&h, setup);
    let (ook, _) = oracle_script(&o, setup);
    assert!(hok && ook, "setup failed: hipp {he:?}");
    for q in queries {
        let (hok, hl, he) = hipp_script(&h, q);
        let (ook, ol) = oracle_script(&o, q);
        assert_eq!(hl, ol, "query: {q}\nhipp error: {he:?}");
        assert_eq!(hok, ook, "success differs: {q}\nhipp error: {he:?}");
    }
}

/// The literal 10^308 written out (the rank option accepts no
/// exponent): a weight whose sum over two hits overflows to +Inf,
/// making bm25 Inf/Inf = NaN for those rows.
fn big_weight() -> String {
    let mut s = String::from("1");
    s.push_str(&"0".repeat(308));
    s.push_str(".0");
    s
}

const NAN_DOCS: &[&str] = &[
    "a b", "a a b", "a c", "a a a", "b a", "a a", "a d d d", "a", "a a c", "x a", "a a a a", "c a",
    "a b c", "a a d", "d a", "b b a", "a a b b", "a x", "y a a", "a z", "a a z", "q a", "a q q",
    "a a q", "w a",
];

fn nan_setup() -> String {
    let mut s = String::from("CREATE VIRTUAL TABLE ft USING fts5(x);");
    s.push_str(&format!(
        "INSERT INTO ft(ft, rank) VALUES('rank', 'bm25({})');",
        big_weight()
    ));
    for d in NAN_DOCS {
        s.push_str(&format!("INSERT INTO ft VALUES('{d}');"));
    }
    s
}

#[test]
fn fts5_nan_rank_sorts_as_null_like_the_pin() {
    // Regression (v0.17 size pass): rows whose rank is NaN came back
    // in rowid order among the -Inf rows (1,2,3,...); the pin sorts
    // them first ascending (2,4,6,9,...) and last descending.
    same(
        "nanrank",
        &nan_setup(),
        &[
            "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rank",
            "SELECT rowid, rank FROM ft WHERE ft MATCH 'a' ORDER BY rank DESC",
            "SELECT rowid FROM ft WHERE ft MATCH 'a' ORDER BY rank LIMIT 5",
            "SELECT rowid FROM ft WHERE ft MATCH 'a' ORDER BY rank DESC LIMIT 3 OFFSET 9",
            "SELECT group_concat(rowid) FROM (SELECT rowid FROM ft WHERE ft MATCH 'a' ORDER BY rank)",
            "SELECT rowid, rank IS NULL FROM ft WHERE ft MATCH 'a OR c' ORDER BY rank",
            "SELECT rowid, bm25(ft) FROM ft('a') ORDER BY rank",
        ],
    );
}

#[test]
fn fts5_finite_rank_order_unchanged() {
    let mut r = Rng(0xF75);
    let words = ["alpha", "beta", "gamma", "delta", "eps"];
    let mut s = String::from("CREATE VIRTUAL TABLE ft USING fts5(x, y);");
    for _ in 0..120 {
        let mut doc = String::new();
        for _ in 0..1 + r.below(6) {
            doc.push_str(words[r.below(words.len() as u64) as usize]);
            doc.push(' ');
        }
        let other = words[r.below(5) as usize];
        s.push_str(&format!(
            "INSERT INTO ft VALUES('{}', '{other}');",
            doc.trim()
        ));
    }
    same(
        "finrank",
        &s,
        &[
            "SELECT rowid, rank FROM ft WHERE ft MATCH 'alpha' ORDER BY rank",
            "SELECT rowid FROM ft WHERE ft MATCH 'beta OR gamma' ORDER BY rank DESC",
            "SELECT rowid FROM ft WHERE ft MATCH 'delta' ORDER BY rank LIMIT 7",
            "SELECT rowid, round(bm25(ft, 2.0, 0.5), 6) FROM ft WHERE ft MATCH 'eps' ORDER BY 2, 1",
        ],
    );
}

/// A table of `n` rows with low-cardinality keys (many ties), NULLs,
/// mixed-case text and reals, in a scrambled insertion order.
fn table_setup(seed: u64, n: usize) -> String {
    let mut r = Rng(seed);
    let mut s = String::from(
        "CREATE TABLE t(a INTEGER, b TEXT, c REAL, d);\
         CREATE TABLE u(a INTEGER, b TEXT);\
         CREATE TABLE w(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;",
    );
    let texts = ["x", "X", "y", "Y ", "y", "z", "", "ab", "AB", "a b"];
    for i in 0..n {
        let a = match r.below(8) {
            0 => "NULL".to_string(),
            k => (k as i64 - 3).to_string(),
        };
        let b = match r.below(12) {
            0 => "NULL".to_string(),
            k => format!("'{}'", texts[(k as usize) % texts.len()]),
        };
        let c = match r.below(6) {
            0 => "NULL".to_string(),
            k => format!("{}.5", k as i64 - 2),
        };
        let d = match r.below(5) {
            0 => "NULL".to_string(),
            1 => format!("'{}'", r.below(4)),
            2 => format!("{}", r.below(4)),
            3 => format!("{}.0", r.below(4)),
            _ => "x'41'".to_string(),
        };
        s.push_str(&format!("INSERT INTO t VALUES({a}, {b}, {c}, {d});"));
        if i % 2 == 0 {
            s.push_str(&format!("INSERT INTO u VALUES({}, {b});", r.below(6)));
        }
        if i % 3 == 0 {
            s.push_str(&format!(
                "INSERT OR IGNORE INTO w VALUES('{}{}', {});",
                texts[r.below(texts.len() as u64) as usize],
                r.below(40),
                r.below(9)
            ));
        }
    }
    s
}

#[test]
fn order_by_with_ties_matches_the_pin() {
    for (k, n) in [(1u64, 9usize), (2, 13), (3, 25), (4, 300)] {
        same(
            &format!("ob{k}"),
            &table_setup(k * 7919, n),
            &[
                "SELECT rowid, a FROM t ORDER BY a",
                "SELECT rowid, a FROM t ORDER BY a DESC",
                "SELECT rowid, a, b FROM t ORDER BY a NULLS LAST, b DESC NULLS FIRST",
                "SELECT rowid, b FROM t ORDER BY b COLLATE NOCASE",
                "SELECT rowid, b FROM t ORDER BY b COLLATE RTRIM DESC",
                "SELECT rowid, c, d FROM t ORDER BY d, c DESC",
                "SELECT rowid FROM t ORDER BY a LIMIT 5 OFFSET 3",
                "SELECT rowid FROM t ORDER BY b DESC, a LIMIT 4",
                "SELECT DISTINCT a, b FROM t ORDER BY b, a",
                "SELECT a FROM t WHERE a IN (3, 1, 3, -2, 1, NULL, 0) ORDER BY rowid",
                "SELECT t.rowid, u.rowid FROM t JOIN u ON t.a = u.a ORDER BY t.b, u.b LIMIT 40",
                "SELECT rowid FROM (SELECT rowid, * FROM t ORDER BY c) ORDER BY a",
            ],
        );
    }
}

#[test]
fn group_by_and_aggregate_order_match_the_pin() {
    for (k, n) in [(5u64, 14usize), (6, 200)] {
        same(
            &format!("gb{k}"),
            &table_setup(k * 104_729, n),
            &[
                "SELECT a, count(*) FROM t GROUP BY a",
                "SELECT b, count(*), sum(a) FROM t GROUP BY b COLLATE NOCASE",
                "SELECT a, b, count(*) FROM t GROUP BY a, b",
                "SELECT a, count(*) FROM t GROUP BY a ORDER BY a DESC",
                "SELECT d, count(*) FROM t GROUP BY d",
                "SELECT a, group_concat(rowid, ',' ORDER BY b, c DESC) FROM t GROUP BY a",
                "SELECT group_concat(DISTINCT b ORDER BY b COLLATE NOCASE) FROM t",
                "SELECT percentile(c, 30), median(c), percentile_disc(c, 0.7) FROM t",
                "SELECT a, percentile_cont(c, 0.5) FROM t GROUP BY a",
            ],
        );
    }
}

#[test]
fn window_and_compound_order_match_the_pin() {
    for (k, n) in [(8u64, 17usize), (9, 160)] {
        same(
            &format!("wc{k}"),
            &table_setup(k * 15_485_863, n),
            &[
                "SELECT rowid, row_number() OVER (PARTITION BY a ORDER BY b) FROM t",
                "SELECT rowid, rank() OVER (ORDER BY b COLLATE NOCASE DESC) FROM t",
                "SELECT a, sum(c) OVER (PARTITION BY b ORDER BY a ROWS 2 PRECEDING) FROM t",
                "SELECT a, b FROM t UNION SELECT a, b FROM u",
                "SELECT a, b FROM t UNION ALL SELECT a, b FROM u ORDER BY b, a",
                "SELECT a FROM t INTERSECT SELECT a FROM u",
                "SELECT b FROM t EXCEPT SELECT b FROM u ORDER BY 1 DESC",
                "SELECT a, b FROM t UNION SELECT a, b FROM u ORDER BY 2 COLLATE NOCASE, 1 DESC",
            ],
        );
    }
}

#[test]
fn sorted_writes_match_the_pin() {
    let setup = table_setup(77, 150);
    same(
        "wr",
        &setup,
        &[
            "UPDATE t SET a = a + 1 WHERE b > 'x'; SELECT rowid, a FROM t",
            "DELETE FROM t WHERE rowid % 3 = 0 AND a IS NOT NULL; SELECT count(*), sum(rowid) FROM t",
            "UPDATE w SET v = v * 2 WHERE v > 3; SELECT * FROM w",
            "DELETE FROM w WHERE v % 2 = 1; SELECT * FROM w",
            "SELECT * FROM w ORDER BY v, k DESC",
            "CREATE INDEX ti ON t(b, a); SELECT rowid FROM t WHERE b IN ('y', 'x', 'y', 'X') ORDER BY rowid",
        ],
    );
}

#[test]
fn rtree_and_fts5_rowid_ordered_writes_match_the_pin() {
    let mut r = Rng(2024);
    let mut s = String::from(
        "CREATE VIRTUAL TABLE rt USING rtree(id, x0, x1);\
         CREATE VIRTUAL TABLE ft USING fts5(x);",
    );
    for i in 1..=60 {
        let lo = r.below(50);
        s.push_str(&format!(
            "INSERT INTO rt VALUES({}, {lo}, {});",
            61 - i,
            lo + r.below(9)
        ));
        s.push_str(&format!(
            "INSERT INTO ft(rowid, x) VALUES({}, 'w{} w{}');",
            61 - i,
            r.below(4),
            r.below(4)
        ));
    }
    same(
        "vt",
        &s,
        &[
            "DELETE FROM rt WHERE x0 < 20; SELECT id FROM rt",
            "DELETE FROM ft WHERE ft MATCH 'w1'; SELECT rowid FROM ft",
            "SELECT rowid FROM ft WHERE ft MATCH 'w2' ORDER BY rank",
        ],
    );
}

// ---------- Stage 1 unit 2 (`size-mono`): monomorphic glue ----------
//
// Each sub-edit compiles a piece of shared machinery once instead of
// per type; the claim is again "nothing observable changed". The
// corpora below drive exactly the paths whose instantiations were
// merged, against the pin.

/// A small table pair with ties, NULLs and an overflowing column.
const MONO_SETUP: &str = "CREATE TABLE t(a, b);\
     INSERT INTO t VALUES(1, 'x'), (2, 'y'), (3, 'x'), (NULL, 'z');\
     CREATE TABLE u(c);\
     INSERT INTO u VALUES(1), (2);\
     CREATE TABLE big(a, b);\
     INSERT INTO big VALUES(9223372036854775807, 'x'), (9223372036854775807, 'x'), (1, 'y');";

/// (a) A compound's ORDER BY names a result column through the
/// resolver (`CompKeys::hit`, now on the resolver's own instantiation).
#[test]
fn compound_order_by_names_match_the_pin() {
    same(
        "mono-comp",
        MONO_SETUP,
        &[
            "SELECT a, b FROM t UNION SELECT c, 'q' FROM u ORDER BY b, a",
            "SELECT a AS k FROM t UNION ALL SELECT c FROM u ORDER BY k DESC",
            "SELECT t.a FROM t UNION SELECT c FROM u ORDER BY t.a",
            "SELECT t.a, t.b FROM t UNION SELECT c, c FROM u ORDER BY t.b DESC, 1",
            "SELECT a FROM t UNION SELECT c FROM u ORDER BY nosuch",
            "SELECT t.a FROM t, u UNION SELECT c FROM u ORDER BY t.a",
        ],
    );
}

/// (a) Aggregates of an outer query inside subqueries: substituted
/// per group (`subst_owned_aggs`, whose walk now stops on a unit
/// signal and keeps the fold's error beside it — an overflowing
/// `sum` must still fail the statement), re-levelled through an
/// alias (`alias_from_outer`), and marked inert in an EXISTS.
#[test]
fn outer_aggregates_in_subqueries_match_the_pin() {
    same(
        "mono-agg",
        MONO_SETUP,
        &[
            "SELECT b, (SELECT count(*) FROM u WHERE c <= max(t.a)) FROM t GROUP BY b ORDER BY b",
            "SELECT b, sum(a) s FROM t GROUP BY b HAVING (SELECT s) > 1",
            "SELECT b FROM t GROUP BY b HAVING EXISTS (SELECT max(t.a) FROM u)",
            "SELECT b, (SELECT sum(a) + c FROM u WHERE c = 1) FROM t GROUP BY b",
            "SELECT (SELECT count(*) FROM t AS t2 WHERE t2.a = max(t.a)) FROM t",
            "SELECT b AS k, count(*) FROM t GROUP BY k ORDER BY (SELECT k)",
            "SELECT b, (SELECT sum(a) FROM (SELECT 1)) FROM big GROUP BY b",
            "SELECT b, (SELECT sum(a) FROM (SELECT 1)) FROM big WHERE b = 'y' GROUP BY b",
            "SELECT b FROM big GROUP BY b HAVING (SELECT sum(a)) > 0",
            "SELECT b, (SELECT max(a) + 1 FROM (SELECT 1)) FROM big GROUP BY b",
        ],
    );
}

/// (a) The evaluator's own conditions (CASE, `iif`, json arguments,
/// FILTER, AND / OR / NOT short cuts) and the window functions' frame
/// offsets, which now run the one `Error<()>` body directly.
#[test]
fn evaluator_conditions_and_frame_offsets_match_the_pin() {
    same(
        "mono-cond",
        MONO_SETUP,
        &[
            "SELECT iif(a > 1, 'big', 'small'), CASE WHEN a THEN b END, \
             json_object('k', CASE WHEN a > 1 THEN 1 ELSE 0 END) FROM t",
            "SELECT a FROM t WHERE (a > 1 AND b = 'x') OR NOT (a < 3) ORDER BY a",
            "SELECT count(*) FILTER (WHERE a > 1 AND b <> 'z') FROM t",
            "SELECT b, max(a) FROM t GROUP BY b HAVING max(a) > 1 AND count(*) > 1",
            "SELECT CASE WHEN a + 1 > 2 AND NOT b = 'x' THEN 'p' WHEN a IS NULL THEN 'n' END FROM t",
            "SELECT a FROM big WHERE a + a > 0",
            "SELECT CASE WHEN a + a > 0 THEN 1 END FROM big",
            "SELECT a, sum(a) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t",
            "SELECT a, sum(a) OVER (ORDER BY a ROWS BETWEEN 1 + 1 PRECEDING AND CURRENT ROW) FROM t",
            "SELECT a, sum(a) OVER (ORDER BY a ROWS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM t",
            "SELECT a, sum(a) OVER (ORDER BY a ROWS BETWEEN 'x' PRECEDING AND CURRENT ROW) FROM t",
        ],
    );
}

/// (d) Plans are shared, not deep-copied, per execution: a query,
/// PRAGMA or INSTEAD OF statement run, reset (part-way or at its end)
/// and run again gives the same rows each time — the pin's rows.
#[test]
fn re_executed_statements_keep_their_plans() {
    let setup = format!(
        "{MONO_SETUP}\
         CREATE VIEW v AS SELECT a, b FROM t;\
         CREATE TABLE log(x);"
    );
    // (The script splitter would cut the trigger body at its `;`.)
    let trigger =
        "CREATE TRIGGER vi INSTEAD OF INSERT ON v BEGIN INSERT INTO log VALUES(new.a); END";
    let queries = [
        "SELECT a, b FROM t ORDER BY b, a",
        "SELECT b, count(*) FROM t GROUP BY b",
        "SELECT a FROM t UNION SELECT c FROM u ORDER BY 1",
        "SELECT a, (SELECT count(*) FROM u WHERE c <= t.a) FROM t",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE c = t.a)",
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n FROM r",
        "SELECT value FROM generate_series(1, 5) WHERE value % 2 = 1",
        "PRAGMA table_info(t)",
        "INSERT INTO v VALUES(7, 'w')",
        "SELECT x FROM log",
    ];
    let h = common::temp_db("v17sz-reexec-h");
    let o = common::temp_db("v17sz-reexec-o");
    let (hok, _, he) = hipp_script(&h, &setup);
    assert!(hok, "setup: {he:?}");
    assert!(oracle_script(&o, &setup).0);
    assert!(oracle_script(&o, trigger).0);
    block_on(async {
        let mut db = Connection::open(FileStorage::open(&h).expect("fs open"), Config::default())
            .await
            .expect("open");
        let mut t = db.prepare(trigger).await.expect("prepare trigger");
        assert_eq!(t.step().await.expect("trigger"), Step::Done);
        drop(t);
        for sql in queries {
            let mut q = db.prepare(sql).await.expect("prepare");
            let mut runs: Vec<Vec<String>> = Vec::new();
            for round in 0..3 {
                let mut rows = Vec::new();
                while q.step().await.expect("step") == Step::Row {
                    let mut l = String::new();
                    render(&q, &mut l);
                    rows.push(l);
                    // The second round stops after one row: a reset
                    // part-way must not lose the plan.
                    if round == 1 {
                        break;
                    }
                }
                q.reset();
                runs.push(rows);
            }
            let mut ol = oracle_script(&o, sql).1;
            if sql.starts_with("INSERT") {
                // Three executions insert three times on hipp.
                for _ in 0..2 {
                    ol = oracle_script(&o, sql).1;
                }
            }
            assert_eq!(runs[0], ol, "first run: {sql}");
            assert_eq!(runs[2], runs[0], "third run: {sql}");
            assert_eq!(runs[1].first(), runs[0].first(), "part-way run: {sql}");
        }
    });
}
