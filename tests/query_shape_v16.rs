//! v0.16 joins, ordering and query-shape completion,
//! differentially vs the pinned oracle (SQLite 3.54.1). The
//! semantics were probed on the pin before implementation; the
//! matrix the tests encode:
//!
//! - RIGHT / FULL [OUTER] JOIN (ungated: the keywords used to
//!   be eaten as a table alias, turning the join into an inner
//!   join). The FROM clause is a left-deep tree; the main
//!   nested loop runs in FROM order (a LEFT / FULL source
//!   NULL-pads an unmatched left combination in place), then
//!   each RIGHT / FULL source — earlier sources first — emits
//!   its never-matched rows, in its own row order, NULL-padded
//!   on the left and joined onward through the later sources.
//!   That is the pin's output order for unordered joins over
//!   index-less tables, so those are compared row-for-row.
//! - NATURAL and USING (ungated: NATURAL was an alias too).
//!   `SELECT *` shows each join column once, at the left-most
//!   source's position; an unqualified reference to it takes
//!   the left-most side after INNER / LEFT, the right side
//!   after RIGHT, and `coalesce(...)` of both after FULL — and
//!   star expansion of a source left of a RIGHT / FULL join
//!   reads that joined value even under `q.*`.
//! - ON clauses that name a later source: an error on an outer
//!   join (or anywhere once a RIGHT / FULL join exists), and
//!   hoisted into WHERE on an inner join (ungated: hipp used to
//!   evaluate them against a NULL slot and drop every row).
//! - ORDER BY ... NULLS FIRST / LAST (ungated: parsed and
//!   ignored before) at every ORDER BY site.
//! - Term rules (ungated): ORDER BY / GROUP BY ordinals out of
//!   range are errors, HAVING has no ordinals, a real column
//!   shadows an alias in GROUP BY / HAVING (not in ORDER BY),
//!   aggregates are refused in GROUP BY, HAVING needs an
//!   aggregate query, `NULL IN (<empty>)` is 0.
//! - Under `v16`: LIMIT / OFFSET as expressions (constant or
//!   uncorrelated subquery, the pin's `MustBeInt` conversion,
//!   negative LIMIT = no limit), result-column aliases inside
//!   WHERE / ON / GROUP BY / HAVING / ORDER BY expressions and
//!   their subqueries, `x IN tablename`, string-literal aliases.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

async fn exec<S: hipp::Storage>(db: &mut Connection<S>, sql: &str)
where
    S::Error: core::fmt::Debug,
{
    let mut st = db.prepare(sql).await.expect("prepare");
    while st.step().await.expect("step") == Step::Row {}
}

/// Step a statement to completion, collecting its rows in the
/// CLI's list rendering (integers, text, NULL as empty; the
/// corpora below avoid reals — the CLI suite covers those), or
/// the error.
async fn try_collect<S: hipp::Storage>(
    db: &mut Connection<S>,
    sql: &str,
) -> Result<Vec<String>, Error<S::Error>>
where
    S::Error: core::fmt::Debug,
{
    let mut q = db.prepare(sql).await?;
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
                Some(hipp::Value::Real(f)) => line.push_str(&format!("{f:?}")),
                Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                    line.push_str(&String::from_utf8_lossy(b));
                }
            }
        }
        rows.push(line);
    }
    Ok(rows)
}

/// Run one batch through the oracle CLI: (success, stdout
/// lines). Only success and stdout are compared — the local
/// stand-in oracle's error text is not the pin's.
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

/// Twin databases: the setup batch is run by hipp into one
/// file and by the oracle into the other.
struct Twins {
    hipp: std::path::PathBuf,
    oracle: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp = common::temp_db(&format!("v16qs-{tag}-h"));
    let oracle = common::temp_db(&format!("v16qs-{tag}-o"));
    common::cli(&oracle, setup);
    block_on(async {
        let mut db = open(&hipp).await;
        for stmt in setup.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                exec(&mut db, stmt).await;
            }
        }
    });
    Twins { hipp, oracle }
}

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

impl Twins {
    /// One statement on both twins: equal rows (order
    /// included), or both refuse it. Returns hipp's error when
    /// both refused.
    fn same(&self, sql: &str) -> Option<Error<<FileStorage as hipp::Storage>::Error>> {
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

    /// hipp's rows for `sql` (it must run).
    fn hipp_rows(&self, sql: &str) -> Vec<String> {
        block_on(async {
            let mut db = open(&self.hipp).await;
            try_collect(&mut db, sql).await
        })
        .unwrap_or_else(|e| panic!("hipp refused {sql}: {e:?}"))
    }

    /// A statement whose answer changed between the 3.53 stand-in
    /// and the 3.54.1 pin: against the live oracle when it is the
    /// pin, else against `golden` — the rows CI's real 3.54.1 pin
    /// printed (recorded from the CI run, not derived).
    fn pin(&self, sql: &str, golden: &[&str]) {
        if common::oracle_is_pinned() {
            self.same(sql);
        } else {
            assert_eq!(self.hipp_rows(sql), golden, "vs the CI pin golden: {sql}");
        }
    }

    /// Like `pin`, for a neighbour of a CI-evidenced change that CI
    /// has not shown yet: against the live oracle when it is the
    /// pin, else against `expected` — rows DERIVED from the
    /// evidenced rule (STATUS.md, v0.16 "CI vs the real pin"), not
    /// recorded; CI's live run is what confirms them.
    fn pin_inferred(&self, sql: &str, expected: &[&str]) {
        if common::oracle_is_pinned() {
            self.same(sql);
        } else {
            assert_eq!(
                self.hipp_rows(sql),
                expected,
                "vs the inferred pin rows: {sql}"
            );
        }
    }

    /// Both engines refuse `sql`; hipp with `kind`.
    fn refuse(&self, sql: &str, kind: SchemaKind) {
        match self.same(sql) {
            Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "statement: {sql}"),
            other => panic!("expected {kind:?} for {sql}, got {other:?}"),
        }
    }

    /// Both engines refuse `sql` at parse time.
    fn refuse_parse(&self, sql: &str) {
        match self.same(sql) {
            Some(Error::Parse { .. }) => {}
            other => panic!("expected a parse error for {sql}, got {other:?}"),
        }
    }

    /// The oracle reads the hipp file clean and both twins hold
    /// the same rows for `dump`.
    fn finish(&self, dump: &str) {
        assert_eq!(oracle_raw(&self.hipp, "PRAGMA integrity_check;").1, ["ok"]);
        assert_eq!(oracle_raw(&self.hipp, dump), oracle_raw(&self.oracle, dump));
    }
}

const JOIN3: &str = "CREATE TABLE a(x); CREATE TABLE b(y); CREATE TABLE c(z);
    INSERT INTO a VALUES(1),(2),(3);
    INSERT INTO b VALUES(2),(3),(4),(5);
    INSERT INTO c VALUES(3),(5),(6),(4)";

const PQ: &str = "CREATE TABLE a(x, p); CREATE TABLE b(y, q);
    INSERT INTO a VALUES(3,'a3'),(1,'a1'),(2,'a2'),(2,'a2b'),(9,'a9'),(NULL,'an');
    INSERT INTO b VALUES(2,'b2'),(5,'b5'),(1,'b1'),(2,'b2b'),(0,'b0'),(NULL,'bn')";

const USING: &str = "CREATE TABLE a(id, x, k); CREATE TABLE b(k, id, y); CREATE TABLE c(id, z);
    INSERT INTO a VALUES(1,'a1',10),(2,'a2',20),(3,'a3',30),(NULL,'an',40);
    INSERT INTO b VALUES(10,1,'b1'),(20,2,'b2'),(99,4,'b4'),(40,NULL,'bn');
    INSERT INTO c VALUES(1,'c1'),(4,'c4'),(5,'c5')";

// ---------------- (a) RIGHT / FULL JOIN ----------------

#[test]
fn right_and_full_join_rows_in_pin_order() {
    let tw = twins("rf", PQ);
    tw.all(&[
        "SELECT * FROM a RIGHT JOIN b ON x=y",
        "SELECT * FROM a RIGHT OUTER JOIN b ON x=y",
        "SELECT * FROM a FULL JOIN b ON x=y",
        "SELECT * FROM a FULL OUTER JOIN b ON x=y",
        "SELECT * FROM a RIGHT JOIN b ON x<y",
        "SELECT * FROM a FULL JOIN b ON 0",
        "SELECT * FROM a RIGHT JOIN b ON 1",
        "SELECT * FROM a RIGHT JOIN b ON x=y WHERE p IS NULL",
        "SELECT * FROM a FULL JOIN b ON x=y WHERE p IS NULL OR q IS NULL",
        "SELECT * FROM a right outer join b on x=y where q > 'b1'",
        // The pin's keyword algebra: LEFT + RIGHT is FULL,
        // OUTER only qualifies an outer join.
        "SELECT * FROM a LEFT RIGHT JOIN b ON x=y",
        "SELECT * FROM a RIGHT LEFT OUTER JOIN b ON x=y",
        "SELECT * FROM a OUTER LEFT JOIN b ON x=y",
        "SELECT * FROM a INNER CROSS JOIN b ON x=y",
        // Rowids of a NULL-padded side are NULL.
        "SELECT a.rowid, b.rowid, x, y FROM a FULL JOIN b ON x=y",
        // Aggregates, DISTINCT, ORDER BY and LIMIT over them.
        "SELECT y, count(x), count(*) FROM a RIGHT JOIN b ON x=y GROUP BY y",
        "SELECT DISTINCT x IS NULL, y IS NULL FROM a FULL JOIN b ON x=y",
        "SELECT x, y FROM a FULL JOIN b ON x=y ORDER BY y NULLS LAST, x",
        "SELECT x, y FROM a FULL JOIN b ON x=y LIMIT 3 OFFSET 4",
        "SELECT count(*) FROM a FULL JOIN b ON x=y",
    ]);
}

#[test]
fn right_and_full_join_chains() {
    let tw = twins("chain", JOIN3);
    for from in [
        "a RIGHT JOIN b ON x=y RIGHT JOIN c ON y=z",
        "a RIGHT JOIN b ON x=y RIGHT JOIN c ON x=z",
        "a FULL JOIN b ON x=y FULL JOIN c ON y=z",
        "a FULL JOIN b ON x=y FULL JOIN c ON coalesce(x,y)=z",
        "a RIGHT JOIN b ON x=y JOIN c ON y=z",
        "a RIGHT JOIN b ON x=y LEFT JOIN c ON y=z",
        "a JOIN b ON x<y RIGHT JOIN c ON y=z",
        "a LEFT JOIN b ON x=y RIGHT JOIN c ON y=z",
        "a, b RIGHT JOIN c ON y=z AND x=1",
        "a CROSS JOIN b FULL JOIN c ON x+y=z",
        "a RIGHT JOIN b ON 1 RIGHT JOIN c ON 0",
    ] {
        tw.same(&format!("SELECT x, y, z FROM {from}"));
    }
}

#[test]
fn outer_joins_inside_subqueries_and_derived_tables() {
    let tw = twins(
        "rfsub",
        "CREATE TABLE a(x); CREATE TABLE b(y);
         INSERT INTO a VALUES(1),(2),(2); INSERT INTO b VALUES(2),(3),(NULL)",
    );
    tw.all(&[
        // Correlated: the tail rows carry the outer row.
        "SELECT y, (SELECT count(*) FROM a RIGHT JOIN b AS bb ON a.x = bb.y WHERE bb.y >= b.y) FROM b",
        "SELECT x FROM a WHERE EXISTS (SELECT 1 FROM b RIGHT JOIN a AS aa ON aa.x = b.y WHERE aa.x = a.x AND b.y IS NULL)",
        // Derived tables on either side.
        "SELECT * FROM (SELECT x, y FROM a RIGHT JOIN b ON x=y) s RIGHT JOIN a ON s.x = a.x",
        "SELECT * FROM a FULL JOIN (SELECT y FROM b WHERE y > 2) s ON s.y = a.x",
        "SELECT x FROM a RIGHT JOIN b ON x=y UNION ALL SELECT y FROM a LEFT JOIN b ON x=y",
        "SELECT (SELECT group_concat(coalesce(x, '-')) FROM a FULL JOIN b ON x=y)",
    ]);
    // INSERT ... SELECT over a FULL join: same rows land.
    let tw = twins(
        "rfins",
        "CREATE TABLE a(x); CREATE TABLE b(y); CREATE TABLE r(u, v);
         INSERT INTO a VALUES(1),(2); INSERT INTO b VALUES(2),(3)",
    );
    block_on(async {
        let mut db = open(&tw.hipp).await;
        exec(
            &mut db,
            "INSERT INTO r SELECT x, y FROM a FULL JOIN b ON x=y",
        )
        .await;
    });
    assert!(
        oracle_raw(
            &tw.oracle,
            "INSERT INTO r SELECT x, y FROM a FULL JOIN b ON x=y"
        )
        .0
    );
    tw.finish("SELECT rowid, * FROM r ORDER BY rowid");
}

#[test]
fn update_from_outer_and_natural_joins() {
    let setup = "CREATE TABLE t(id, v); INSERT INTO t VALUES(1,0),(2,0),(3,0);
        CREATE TABLE a(x); CREATE TABLE b(y); CREATE TABLE c(id, w);
        INSERT INTO a VALUES(1),(2); INSERT INTO b VALUES(2),(3);
        INSERT INTO c VALUES(2,'w2'),(3,'w3')";
    for (tag, sql) in [
        (
            "r",
            "UPDATE t SET v = coalesce(x,-1)*10 + coalesce(y,-1) FROM a RIGHT JOIN b ON x=y WHERE t.id = y",
        ),
        (
            "f",
            "UPDATE t SET v = coalesce(x,-1)*10 + coalesce(y,-1) FROM a FULL JOIN b ON x=y WHERE t.id = coalesce(x,y)",
        ),
        (
            "n",
            "UPDATE t SET v = w FROM c NATURAL JOIN c AS c2 WHERE t.id = c.id",
        ),
        (
            "u",
            "UPDATE t SET v = w FROM c JOIN c AS c2 USING (id, w) WHERE t.id = c2.id",
        ),
    ] {
        let tw = twins(&format!("upd-{tag}"), setup);
        assert!(tw.same(sql).is_none());
        tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
    }
    let tw = twins("upd-err", setup);
    // The FROM clause is its own join: its ON / USING never
    // see the target table.
    tw.refuse(
        "UPDATE t SET v = 1 FROM a JOIN b ON t.id = y",
        SchemaKind::UnknownColumn,
    );
    tw.refuse(
        "UPDATE t SET v = 7 FROM a JOIN b USING (x)",
        SchemaKind::UsingColumnMissing,
    );
    tw.refuse(
        "UPDATE t SET v = w FROM c WHERE t.id = id",
        SchemaKind::AmbiguousColumn,
    );
}

// ---------------- (b) NATURAL / USING ----------------

#[test]
fn natural_and_using_rows_and_star_expansion() {
    let tw = twins("nat", USING);
    tw.all(&[
        "SELECT * FROM a NATURAL JOIN b",
        "SELECT * FROM a JOIN b USING (id)",
        "SELECT * FROM a JOIN b USING (k, id)",
        "SELECT * FROM a JOIN b USING (ID)",
        "SELECT * FROM a JOIN b USING (id, id)",
        "SELECT * FROM a, b USING (id)",
        "SELECT * FROM a INNER JOIN b USING (id)",
        "SELECT * FROM a CROSS JOIN b USING (id)",
        "SELECT * FROM a LEFT JOIN b USING (id)",
        "SELECT * FROM a RIGHT JOIN b USING (id)",
        "SELECT * FROM a FULL JOIN b USING (id)",
        "SELECT * FROM a NATURAL LEFT JOIN b",
        "SELECT * FROM a NATURAL LEFT OUTER JOIN b",
        "SELECT * FROM a NATURAL RIGHT JOIN b",
        "SELECT * FROM a NATURAL FULL JOIN b",
        "SELECT * FROM a NATURAL FULL OUTER JOIN b",
        "SELECT * FROM a NATURAL INNER JOIN b",
        "SELECT * FROM a NATURAL CROSS JOIN c",
        "SELECT * FROM a LEFT OUTER NATURAL JOIN b",
        // Unqualified USING column: left-most / right / coalesce.
        "SELECT id, a.id, b.id FROM a JOIN b USING (id)",
        "SELECT id, a.id, b.id FROM a LEFT JOIN b USING (id)",
        "SELECT id, a.id, b.id FROM a RIGHT JOIN b USING (id)",
        "SELECT id, a.id, b.id FROM a FULL JOIN b USING (id)",
        // The right side's q.* is plain (3.53 and 3.54 agree).
        "SELECT b.* FROM a FULL JOIN b USING (id)",
        // Multi-way chains.
        "SELECT * FROM a JOIN b USING (id) JOIN c USING (id)",
        "SELECT * FROM a NATURAL JOIN c NATURAL JOIN b",
        "SELECT * FROM a FULL JOIN b USING (id) FULL JOIN c USING (id)",
        "SELECT * FROM a LEFT JOIN b USING (id) RIGHT JOIN c USING (id)",
        "SELECT * FROM a RIGHT JOIN b USING (id) LEFT JOIN c USING (id)",
        "SELECT * FROM a JOIN b USING (id) FULL JOIN c USING (id)",
        "SELECT * FROM a FULL JOIN b USING (id) RIGHT JOIN c USING (id)",
        "SELECT * FROM a RIGHT JOIN b USING (id) FULL JOIN c USING (id)",
        "SELECT id, a.id, b.id, c.id FROM a RIGHT JOIN b USING (id) FULL JOIN c USING (id)",
        "SELECT * FROM a JOIN b ON 1 JOIN c USING (id)",
        // Derived tables, self joins, no common columns.
        "SELECT * FROM a NATURAL JOIN (SELECT 1 AS id, 'q' AS w) s",
        "SELECT s.id, s.x FROM (SELECT * FROM a JOIN b USING (id)) s",
        "SELECT * FROM c NATURAL JOIN c AS c2",
        "SELECT * FROM a NATURAL JOIN (SELECT 7 AS nothing_shared) s",
        // In other clauses and subqueries.
        "SELECT id, count(*) FROM a NATURAL FULL JOIN b GROUP BY id ORDER BY id",
        "SELECT * FROM a FULL JOIN b USING (id) WHERE id > 1",
        "SELECT * FROM a RIGHT JOIN b USING (id) ORDER BY id DESC",
        "SELECT * FROM a NATURAL JOIN b WHERE id IN (SELECT id FROM a NATURAL RIGHT JOIN b)",
        "SELECT (SELECT count(*) FROM a JOIN b USING (id)), (SELECT group_concat(id) FROM a FULL JOIN b USING (id))",
        "SELECT count(*), count(a.id), count(b.id), count(id) FROM a FULL JOIN b USING (id)",
    ]);
    // q.* reads q's own columns, also left of a RIGHT / FULL join
    // (3.54.1; the 3.53 stand-in read the joined value). Recorded
    // from CI's pin run:
    tw.pin(
        "SELECT a.* FROM a FULL JOIN b USING (id)",
        &["1|a1|10", "2|a2|20", "3|a3|30", "|an|40", "||", "||"],
    );
    // Derived from that rule (CI's live run confirms):
    tw.pin_inferred(
        "SELECT a.* FROM a RIGHT JOIN b USING (id)",
        &["1|a1|10", "2|a2|20", "||", "||"],
    );
    tw.pin_inferred(
        "SELECT a.*, b.* FROM a NATURAL RIGHT JOIN b",
        &[
            "1|a1|10|10|1|b1",
            "2|a2|20|20|2|b2",
            "|||99|4|b4",
            "|||40||bn",
        ],
    );
    tw.pin_inferred(
        "SELECT a.*, '|', b.* FROM a NATURAL FULL JOIN b",
        &[
            "1|a1|10|||10|1|b1",
            "2|a2|20|||20|2|b2",
            "3|a3|30|||||",
            "|an|40|||||",
            "|||||99|4|b4",
            "|||||40||bn",
        ],
    );
}

#[test]
fn using_compares_with_column_affinity() {
    let tw = twins(
        "using-aff",
        "CREATE TABLE p(id INTEGER PRIMARY KEY, v); CREATE TABLE q(id INTEGER, w);
         CREATE TABLE r(id TEXT, u);
         INSERT INTO p VALUES(1,'p1'),(2,'p2'); INSERT INTO q VALUES(2,'q2'),(3,'q3');
         INSERT INTO r VALUES('1','r1'),('02','r2'),('3','r3')",
    );
    tw.all(&[
        // The rowid-alias column joins (and shows) as the rowid.
        "SELECT * FROM p NATURAL FULL JOIN q",
        "SELECT * FROM p JOIN r USING (id)",
        "SELECT * FROM q NATURAL JOIN r",
        "SELECT * FROM r NATURAL RIGHT JOIN q",
    ]);
}

#[test]
fn join_constraint_errors_agree() {
    let tw = twins("join-err", USING);
    tw.refuse(
        "SELECT * FROM a JOIN b USING (zz)",
        SchemaKind::UsingColumnMissing,
    );
    tw.refuse(
        "SELECT * FROM a JOIN b USING (x)",
        SchemaKind::UsingColumnMissing,
    );
    tw.refuse(
        "SELECT id FROM a JOIN b USING (k)",
        SchemaKind::AmbiguousColumn,
    );
    tw.refuse(
        "SELECT id FROM a JOIN b ON 1 JOIN c USING (id)",
        SchemaKind::AmbiguousColumn,
    );
    // With a RIGHT join anywhere, two non-USING left columns
    // make the USING reference ambiguous.
    tw.refuse(
        "SELECT * FROM a JOIN b ON 1 RIGHT JOIN c USING (id)",
        SchemaKind::AmbiguousColumn,
    );
    tw.refuse_parse("SELECT * FROM a NATURAL JOIN b USING (id)");
    tw.refuse_parse("SELECT * FROM a NATURAL JOIN b ON 1");
    tw.refuse_parse("SELECT * FROM a USING (id)");
    tw.refuse_parse("SELECT * FROM a JOIN b USING ()");
    tw.refuse_parse("SELECT * FROM a OUTER JOIN b ON 1");
    tw.refuse_parse("SELECT * FROM a NATURAL OUTER JOIN b");
    tw.refuse_parse("SELECT * FROM a LEFT CROSS JOIN b");
    tw.refuse_parse("SELECT * FROM a INNER OUTER JOIN b ON 1");
    tw.refuse_parse("SELECT * FROM a NATURAL LEFT OUTER FULL JOIN b");
    // Join keywords are never a bare table alias.
    tw.refuse_parse("SELECT * FROM a natural");
    tw.refuse_parse("SELECT * FROM a right");
}

#[test]
fn on_clause_forward_references() {
    let tw = twins("on-fwd", JOIN3);
    // Regression: an inner join's ON naming a later source is
    // hoisted into WHERE (hipp used to drop every row).
    tw.all(&[
        "SELECT x, y, z FROM a JOIN b ON x=z JOIN c ON y=z",
        "SELECT x, y, z FROM a JOIN b ON x=z LEFT JOIN c ON z=x",
        "SELECT x, y, z FROM a JOIN b ON y=z LEFT JOIN c ON z=x",
        "SELECT x, y, z FROM a, b ON x = c.z, c",
    ]);
    for sql in [
        "SELECT x, y, z FROM a LEFT JOIN b ON x=z JOIN c ON y=z",
        "SELECT x, y, z FROM a JOIN b ON x=z RIGHT JOIN c ON 1",
        "SELECT x, y, z FROM a RIGHT JOIN b ON x=z JOIN c",
        "SELECT x, y, z FROM a LEFT JOIN b ON (SELECT z) = 2 JOIN c",
    ] {
        tw.refuse(sql, SchemaKind::OnReferencesLaterSource);
    }
}

#[test]
fn subquery_in_a_non_last_sources_on_clause() {
    // Regression: a subquery inside the ON of a source that is
    // not the last one read misaligned slots (its own sources
    // are numbered after the whole FROM clause, but the row
    // held only the sources so far) and dropped or garbled rows.
    let tw = twins("on-sub", JOIN3);
    tw.all(&[
        "SELECT x, y FROM a LEFT JOIN b ON (SELECT count(*) FROM c WHERE c.z = b.y) > 0 AND x < y JOIN c AS c2 ON c2.z = 6",
        "SELECT x, y FROM a JOIN b ON y = (SELECT min(z) FROM c WHERE z > x) JOIN c AS c2 ON c2.z = 6",
        "SELECT x, y, c2.z FROM a FULL JOIN b ON y = (SELECT min(z) FROM c WHERE z > x) RIGHT JOIN c AS c2 ON c2.z = y + 1",
        "SELECT (SELECT group_concat(y) FROM b JOIN c ON z = (SELECT max(x) FROM a WHERE x < y) JOIN a ON 1 WHERE a.x = 1) FROM a",
    ]);
    let setup = "CREATE TABLE a(x); CREATE TABLE b(y); CREATE TABLE c(z); CREATE TABLE t(id, v);
        INSERT INTO a VALUES(1),(2),(3); INSERT INTO b VALUES(2),(3),(4),(5);
        INSERT INTO c VALUES(3),(5),(6),(4); INSERT INTO t VALUES(1,0),(2,0),(3,0)";
    for (tag, sql) in [
        (
            "u1",
            "UPDATE t SET v = y FROM a JOIN b ON y = (SELECT min(z) FROM c WHERE z > x) JOIN c AS c2 ON c2.z = 6 WHERE t.id = x",
        ),
        (
            "u2",
            "UPDATE t SET v = (SELECT count(*) FROM c WHERE z > y) FROM a RIGHT JOIN b ON x = y WHERE t.id = y - 1",
        ),
    ] {
        let tw = twins(&format!("on-sub-{tag}"), setup);
        assert!(tw.same(sql).is_none());
        tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
    }
}

#[test]
fn rowid_and_real_columns_named_rowid() {
    let tw = twins(
        "rowid",
        "CREATE TABLE a(x, rowid); CREATE TABLE b(y);
         INSERT INTO a VALUES(1, 100),(2, 200); INSERT INTO b VALUES(2),(3)",
    );
    // Regression: a real column named rowid wins over another
    // source's rowid (hipp called it ambiguous).
    tw.same("SELECT rowid FROM a, b");
    tw.same("SELECT rowid FROM b, a");
    tw.refuse("SELECT rowid FROM b, b AS c", SchemaKind::AmbiguousColumn);
}

// ---------------- (c) NULLS FIRST / LAST ----------------

#[test]
fn nulls_first_last_at_every_order_by_site() {
    let tw = twins(
        "nulls",
        "CREATE TABLE t(a, b); CREATE TABLE u(x);
         INSERT INTO t VALUES(2,'x'),(NULL,'y'),(1,'z'),(NULL,'w'),(3,NULL)",
    );
    tw.all(&[
        "SELECT a FROM t ORDER BY a NULLS LAST",
        "SELECT a FROM t ORDER BY a ASC NULLS LAST",
        "SELECT a FROM t ORDER BY a NULLS FIRST",
        "SELECT a FROM t ORDER BY a DESC NULLS FIRST",
        "SELECT a FROM t ORDER BY a DESC NULLS LAST",
        "SELECT a FROM t ORDER BY a DESC",
        "SELECT a, b FROM t ORDER BY a ASC NULLS FIRST, b DESC NULLS FIRST",
        "SELECT a, b FROM t ORDER BY b NULLS LAST, a DESC NULLS LAST",
        "SELECT a FROM t ORDER BY -a NULLS LAST",
        "SELECT a FROM t ORDER BY a NULLS LAST LIMIT 2 OFFSET 2",
        "SELECT DISTINCT a FROM t ORDER BY a DESC NULLS FIRST",
        "SELECT b, max(a) m FROM t GROUP BY b ORDER BY m NULLS LAST, b",
        // Compound ORDER BY.
        "SELECT a FROM t UNION ALL SELECT NULL ORDER BY 1 NULLS LAST",
        "SELECT a FROM t UNION SELECT 5 ORDER BY a DESC NULLS FIRST",
        "SELECT a FROM t INTERSECT SELECT a FROM t ORDER BY 1 DESC NULLS FIRST",
        // Subqueries and derived tables.
        "SELECT (SELECT a FROM t ORDER BY a NULLS LAST LIMIT 1), (SELECT a FROM t ORDER BY a DESC NULLS FIRST LIMIT 1)",
        "SELECT * FROM (SELECT a FROM t ORDER BY a NULLS LAST LIMIT 3)",
        "SELECT (SELECT group_concat(coalesce(a, 'n')) FROM (SELECT a FROM t ORDER BY a DESC NULLS FIRST))",
    ]);
    // INSERT ... SELECT keeps the ordered window.
    block_on(async {
        let mut db = open(&tw.hipp).await;
        exec(
            &mut db,
            "INSERT INTO u SELECT a FROM t ORDER BY a NULLS LAST LIMIT 2",
        )
        .await;
    });
    assert!(
        oracle_raw(
            &tw.oracle,
            "INSERT INTO u SELECT a FROM t ORDER BY a NULLS LAST LIMIT 2"
        )
        .0
    );
    tw.finish("SELECT rowid, x FROM u ORDER BY rowid");
    // NULLS needs FIRST or LAST; GROUP BY and indexes take none.
    tw.refuse_parse("SELECT a FROM t ORDER BY a NULLS");
    tw.refuse_parse("SELECT a FROM t GROUP BY a NULLS FIRST");
    tw.refuse_parse("CREATE INDEX i ON t(a NULLS FIRST)");
}

// ---------------- ordinal / alias / HAVING term rules ----------------

#[test]
fn group_having_order_term_rules() {
    let tw = twins(
        "terms",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,10),(2,10),(3,20),(0,30)",
    );
    tw.all(&[
        // Regression: a real column shadows an alias in GROUP
        // BY / HAVING (hipp grouped by the alias).
        "SELECT a+1 AS b, count(*) FROM t GROUP BY b",
        "SELECT b AS a FROM t GROUP BY a ORDER BY a",
        "SELECT a AS b, sum(a) FROM t GROUP BY b HAVING b > 10",
        // ... but ORDER BY takes the alias first.
        "SELECT a AS b FROM t ORDER BY b DESC",
        "SELECT -a AS b FROM t ORDER BY (b)",
        "SELECT a AS b, b AS a FROM t WHERE a > 1 ORDER BY a",
        // Regression: HAVING has no ordinals (hipp read item 1).
        "SELECT a, count(*) FROM t GROUP BY a HAVING 1",
        "SELECT a, sum(b) FROM t GROUP BY a HAVING a = 2 OR 2",
        // Ordinals in range, and constants that are not ordinals.
        "SELECT a, b FROM t ORDER BY +2, 1",
        "SELECT a, count(*) FROM t GROUP BY 2147483648",
        "SELECT a FROM t ORDER BY 2147483648",
        "SELECT a FROM t ORDER BY 1.0",
        "SELECT a % 2 AS par, count(*) FROM t GROUP BY par ORDER BY par",
    ]);
    // Regression: out-of-range ordinals are errors (hipp sorted
    // by the constant).
    for sql in [
        "SELECT a, b FROM t ORDER BY 3",
        "SELECT a, b FROM t ORDER BY 0",
        "SELECT a, b FROM t ORDER BY -1",
        "SELECT a, count(*) FROM t GROUP BY 3",
        "SELECT a FROM (SELECT a FROM t ORDER BY 2)",
    ] {
        tw.refuse(sql, SchemaKind::TermOutOfRange);
    }
    tw.refuse(
        "SELECT count(*) FROM t GROUP BY 1",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse(
        "SELECT count(*) AS c FROM t GROUP BY c",
        SchemaKind::MisusedAggregate,
    );
    // Regression: HAVING needs an aggregate query.
    for sql in [
        "SELECT a FROM t HAVING 1",
        "SELECT a FROM t HAVING count(*) > 0",
        "SELECT (SELECT a FROM t HAVING 1)",
    ] {
        tw.refuse(sql, SchemaKind::HavingWithoutAggregate);
    }
    tw.same("SELECT count(*) FROM t HAVING 1");
}

#[test]
fn empty_in_set_is_false_even_for_null() {
    let tw = twins(
        "in-empty",
        "CREATE TABLE e(a); CREATE TABLE s(v); INSERT INTO s VALUES(2),(NULL)",
    );
    // Regression: `NULL IN (<empty>)` is 0 and NOT IN is 1.
    tw.all(&[
        "SELECT NULL IN (SELECT a FROM e), NULL NOT IN (SELECT a FROM e)",
        "SELECT NULL IN (), NULL NOT IN ()",
        "SELECT v, v IN (SELECT a FROM e), v NOT IN (SELECT a FROM e) FROM s",
        "SELECT v FROM s WHERE v NOT IN (SELECT a FROM e)",
        "SELECT NULL IN (1), NULL IN (NULL), 1 IN (NULL, 1)",
    ]);
}

// ---------------- generated join corpus ----------------

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
}

/// Random small tables (duplicate keys, NULLs, empty tables)
/// joined every way, each query ordered by every output column
/// under random NULL placement (so the comparison is exact
/// whatever loop order the pin's planner picks for the inner
/// joins), plus unordered two-table RIGHT / FULL joins, whose
/// order over index-less tables is deterministic on the pin.
#[test]
fn generated_join_corpus() {
    let mut rng = Rng(0x5eed_0016_0a1b_2c3d);
    for round in 0..6 {
        let mut setup =
            String::from("CREATE TABLE p(k, v); CREATE TABLE q(k, w); CREATE TABLE r(w, z);");
        for (t, n) in [("p", 0..7u64), ("q", 0..6), ("r", 0..5)] {
            let rows = rng.below(n.end);
            for i in 0..rows {
                let k = match rng.below(5) {
                    0 => "NULL".to_string(),
                    x => (x % 3).to_string(),
                };
                setup.push_str(&format!("INSERT INTO {t} VALUES({k}, '{t}{i}');"));
            }
        }
        let tw = twins(&format!("gen{round}"), &setup);
        let kinds = [
            "JOIN",
            "LEFT JOIN",
            "RIGHT JOIN",
            "FULL JOIN",
            "CROSS JOIN",
            "NATURAL JOIN",
            "NATURAL LEFT JOIN",
            "NATURAL RIGHT JOIN",
            "NATURAL FULL JOIN",
        ];
        let conds = ["ON p.k = q.k", "ON p.k < q.k", "ON 1", "USING (k)"];
        for _ in 0..24 {
            let k1 = kinds[rng.below(kinds.len() as u64) as usize];
            let c1 = if k1.starts_with("NATURAL") {
                ""
            } else if k1 == "CROSS JOIN" {
                "ON p.k IS NOT q.k"
            } else {
                conds[rng.below(conds.len() as u64) as usize]
            };
            let k2 = kinds[rng.below(kinds.len() as u64) as usize];
            let c2 = if k2.starts_with("NATURAL") {
                ""
            } else {
                ["ON q.w = r.w", "ON r.z > q.w", "USING (w)", "ON 0"][rng.below(4) as usize]
            };
            let nulls = |rng: &mut Rng| ["", " NULLS FIRST", " NULLS LAST"][rng.below(3) as usize];
            let dir = |rng: &mut Rng| ["", " DESC"][rng.below(2) as usize];
            let order: Vec<String> = (1..=6)
                .map(|i| format!("{i}{}{}", dir(&mut rng), nulls(&mut rng)))
                .collect();
            tw.same(&format!(
                "SELECT p.k, p.v, q.k, q.w, r.w, r.z FROM p {k1} q {c1} {k2} r {c2} ORDER BY {}",
                order.join(", ")
            ));
            tw.same(&format!(
                "SELECT p.k, p.v, q.k, q.w FROM p {k1} q {c1} ORDER BY 1, 2, 3, 4"
            ));
        }
        for k in ["RIGHT JOIN", "FULL JOIN", "NATURAL FULL JOIN"] {
            let c = if k.starts_with("NATURAL") {
                ""
            } else {
                "ON p.k = q.k"
            };
            tw.same(&format!("SELECT * FROM p {k} q {c}"));
        }
    }
}

// ---------------- v16: LIMIT / OFFSET expressions ----------------

#[cfg(feature = "v16")]
#[test]
fn limit_offset_expressions() {
    let tw = twins(
        "limit",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3),(4),(5)",
    );
    for l in [
        "1+1",
        "'2'",
        "' 2 '",
        "'2.0'",
        "'2e0'",
        "2.0",
        "-0.0",
        "-1",
        "-5",
        "'-1'",
        "0",
        "0 OFFSET 1",
        "0x2",
        "+2",
        "true",
        "abs(-2)",
        "CAST(2 AS TEXT)",
        "CASE WHEN 1 THEN 2 END",
        "(SELECT 2)",
        "(VALUES(2))",
        "(SELECT a FROM t)",
        "(SELECT max(a) FROM t)",
        "(SELECT count(*) FROM t) - 3",
        "1 OFFSET 1+1",
        "2 OFFSET (SELECT 3)",
        "2 OFFSET '1'",
        "2 OFFSET -1",
        "-1 OFFSET 2",
        "2, 1",
        "1+1, 2",
        "1, -1",
        "9223372036854775807",
        "9223372036854775807 OFFSET 2",
        "9223372036854775807 OFFSET 9223372036854775807",
        "2 OFFSET 9223372036854775807",
        "4294967296",
        "1 OFFSET 4294967296",
        "1e1",
    ] {
        tw.same(&format!("SELECT a FROM t LIMIT {l}"));
    }
    // A UNION with a LIMIT of exactly 1 and no ORDER BY / OFFSET
    // takes its first row in core order on the pin (3.54.0's
    // "UNION queries with a LIMIT of exactly 1"); the scalar
    // subquery's implicit LIMIT 1 counts. Recorded from CI's pin
    // run (the 3.53 stand-in gives 1 row):
    tw.pin(
        "SELECT a FROM t LIMIT (SELECT 2 UNION SELECT 1)",
        &["1", "2"],
    );
    // Derived from that rule (CI's live run confirms): a written
    // LIMIT 1, a three-core chain, an empty first core, EXISTS
    // (unchanged value), and the shapes that keep the sorted
    // first row (ORDER BY, OFFSET, a mixed chain, LIMIT 2).
    tw.pin_inferred("SELECT 2 UNION SELECT 1 LIMIT 1", &["2"]);
    tw.pin_inferred("SELECT 3 UNION SELECT 2 UNION SELECT 1 LIMIT 1", &["3"]);
    tw.pin_inferred(
        "SELECT (SELECT a FROM t WHERE a > 9 UNION SELECT 7 UNION SELECT 5)",
        &["7"],
    );
    tw.pin_inferred("SELECT EXISTS (SELECT 2 UNION SELECT 1)", &["1"]);
    tw.all(&[
        "SELECT 2 UNION SELECT 1 ORDER BY 1 LIMIT 1",
        "SELECT 3 UNION SELECT 2 UNION SELECT 1 LIMIT 1 OFFSET 1",
        "SELECT 2 UNION SELECT 1 LIMIT 2",
        "SELECT a FROM t UNION SELECT 0 LIMIT 3",
    ]);
    // Not losslessly an integer: the pin's datatype mismatch,
    // raised even when no row is produced.
    for l in [
        "2.5",
        "NULL",
        "'abc'",
        "'3abc'",
        "'0x10'",
        "x'01'",
        "1e19",
        "2.0000000001",
        "(SELECT NULL)",
        "(SELECT 'x')",
        "(SELECT a FROM t WHERE 0)",
        "2 OFFSET NULL",
        "2 OFFSET 1.5",
        "2 OFFSET 'x'",
    ] {
        tw.refuse(
            &format!("SELECT a FROM t LIMIT {l}"),
            SchemaKind::DatatypeMismatch,
        );
    }
    tw.refuse(
        "SELECT a FROM t WHERE 0 LIMIT 'x'",
        SchemaKind::DatatypeMismatch,
    );
    // LIMIT sees no columns at all, not even an outer query's.
    tw.refuse("SELECT a FROM t LIMIT a", SchemaKind::UnknownColumn);
    tw.refuse(
        "SELECT a, (SELECT 1 FROM t AS u LIMIT t.a) FROM t",
        SchemaKind::UnknownColumn,
    );
    tw.refuse(
        "SELECT a FROM t LIMIT count(*)",
        SchemaKind::MisusedAggregate,
    );
    tw.refuse_parse("SELECT a FROM t LIMIT 1 OFFSET 2, 3");
    // Every LIMIT site.
    tw.all(&[
        "SELECT a FROM t UNION ALL SELECT 9 LIMIT 1+1 OFFSET (SELECT 4)",
        "SELECT a FROM t ORDER BY a DESC LIMIT (SELECT count(*) FROM t)-3",
        "SELECT * FROM (SELECT a FROM t LIMIT 1+1) s",
        "SELECT (SELECT a FROM t ORDER BY a LIMIT 1 OFFSET 1+2)",
        "SELECT (SELECT 1 LIMIT 'x') FROM t WHERE 0",
        "SELECT sum(a) FROM t LIMIT (SELECT 0)",
        "SELECT EXISTS (SELECT a FROM t LIMIT 0)",
        "SELECT a FROM t WHERE a IN (SELECT a FROM t ORDER BY a DESC LIMIT '2')",
    ]);
}

#[cfg(feature = "v16")]
#[test]
fn limit_expression_into_insert_select() {
    let setup = "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3),(4),(5); CREATE TABLE u(x)";
    let tw = twins("limit-ins", setup);
    let sql = "INSERT INTO u SELECT a FROM t LIMIT (SELECT 2) OFFSET 1";
    block_on(async {
        let mut db = open(&tw.hipp).await;
        exec(&mut db, sql).await;
    });
    assert!(oracle_raw(&tw.oracle, sql).0);
    tw.finish("SELECT rowid, x FROM u ORDER BY rowid");
}

// ---------------- v16: result-column aliases ----------------

#[cfg(feature = "v16")]
#[test]
fn aliases_in_where_on_group_having_order() {
    let tw = twins(
        "alias",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,10),(2,10),(3,20),(0,30)",
    );
    tw.all(&[
        "SELECT a*2 AS d FROM t WHERE d>2",
        "SELECT a+b AS s FROM t WHERE s>15 ORDER BY s",
        "SELECT a AS q, b FROM t WHERE q BETWEEN 1 AND 2 ORDER BY q*-1",
        "SELECT a AS b FROM t ORDER BY b+0 DESC",
        "SELECT b AS bb, sum(a) FROM t GROUP BY bb+0",
        "SELECT b, sum(a) AS x FROM t GROUP BY b HAVING x > 1",
        "SELECT a+1 AS q FROM t GROUP BY q HAVING q > 2 ORDER BY q",
        "SELECT a AS q FROM t WHERE q > 0 GROUP BY q HAVING count(q) = 1 ORDER BY -q",
        "SELECT a AS c FROM t GROUP BY a HAVING c > 1",
        "SELECT count(*) AS c FROM t ORDER BY c + 1",
        "SELECT a AS x, count(*) AS c FROM t GROUP BY x HAVING c > 0",
        "SELECT max(a) AS m FROM t HAVING m > 2",
        // A real column still shadows the alias inside an
        // expression; rowid beats an alias named rowid.
        "SELECT a+1 AS a FROM t WHERE a > 1",
        "SELECT a AS rowid FROM t WHERE rowid = 2",
        "SELECT a AS rowid, rowid FROM t ORDER BY rowid DESC",
        // Duplicate aliases: the first wins.
        "SELECT a AS z, b AS z FROM t WHERE z > 15",
        // Subqueries see the enclosing query's aliases.
        "SELECT a AS x FROM t WHERE x IN (SELECT x)",
        "SELECT a AS s FROM t WHERE (SELECT s) > 1",
        "SELECT a AS r FROM t ORDER BY (SELECT r) DESC",
        "SELECT b AS g, count(*) FROM t GROUP BY (SELECT g)",
        "SELECT a AS z FROM t WHERE EXISTS (SELECT 1 FROM t AS u WHERE u.a = z + 1)",
        "SELECT a AS z FROM t WHERE z IN (SELECT a AS z FROM t WHERE z > 1)",
        "SELECT (SELECT max(a) FROM t) AS m, a FROM t WHERE a = m",
        "SELECT (SELECT count(*) FROM t u WHERE u.a < t.a) AS r FROM t WHERE r > 1",
        "SELECT * FROM (SELECT a AS z FROM t WHERE z > 1) s WHERE s.z < 3",
        "SELECT a AS x FROM t WHERE x > 1 UNION SELECT 9 ORDER BY x",
        "SELECT a AS q FROM t WHERE q > 0 UNION SELECT b AS q FROM t WHERE q > 10 ORDER BY q",
        // String-literal aliases (AS 'q', bare 'q').
        "SELECT a AS 'q' FROM t WHERE q > 1",
        "SELECT a 'q' FROM t WHERE q > 1",
        "SELECT x.a FROM t AS 'x' JOIN t 'y' ON x.a = y.a",
    ]);
    // An alias substituted into ON can be ambiguous there.
    tw.refuse(
        "SELECT a AS k FROM t JOIN t AS u ON k = u.a",
        SchemaKind::AmbiguousColumn,
    );
    tw.same("SELECT t.a AS k FROM t JOIN t AS u ON k = u.b / 10");
    // Never visible to the result list itself, nor qualified.
    for sql in [
        "SELECT (SELECT s) AS z, a+1 AS s FROM t",
        "SELECT a AS s, (SELECT s+100) FROM t",
        "SELECT a AS s, s+1 FROM t",
        "SELECT t.a AS q FROM t ORDER BY t.q",
        "SELECT a AS n FROM t LIMIT n",
        "SELECT a AS q FROM t UNION SELECT b FROM t WHERE q > 0",
    ] {
        tw.refuse(sql, SchemaKind::UnknownColumn);
    }
    // An aliased aggregate in WHERE is still a misuse.
    assert!(tw.same("SELECT count(*) AS c FROM t WHERE c > 0").is_some());
    // Aliases flow through write-side subqueries too.
    let tw = twins(
        "alias-w",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,10),(2,10),(3,20),(0,30)",
    );
    for sql in [
        "INSERT INTO t SELECT a+100 AS z, b FROM t WHERE z > 101",
        "UPDATE t SET b = 1 WHERE a IN (SELECT a AS z FROM t WHERE z > 1)",
        "DELETE FROM t WHERE a IN (SELECT a - 100 AS z FROM t WHERE z > 1)",
    ] {
        assert!(tw.same(sql).is_none());
    }
    tw.finish("SELECT rowid, * FROM t ORDER BY rowid");
}

// ---------------- v16: x IN tablename ----------------

#[cfg(feature = "v16")]
#[test]
fn in_table_operand() {
    let tw = twins(
        "in-table",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(NULL);
         CREATE TABLE s(v); INSERT INTO s VALUES(2),(3),(NULL);
         CREATE TABLE e(a); CREATE TABLE u(a, b);
         CREATE TABLE tt(x TEXT); INSERT INTO tt VALUES('2');
         CREATE TABLE ti(x INTEGER PRIMARY KEY); INSERT INTO ti VALUES(2)",
    );
    tw.all(&[
        "SELECT v, v IN t, v NOT IN t FROM s",
        "SELECT v IN e, v NOT IN e FROM s",
        "SELECT v IN main.t FROM s",
        "SELECT 1 IN t, 5 IN t, NULL IN t",
        "SELECT v FROM s WHERE v IN t",
        "SELECT a FROM t WHERE a IN t AND a+1 IN t",
        // Comparison affinity as for IN (SELECT ...): a BLOB
        // column against a TEXT column converts nothing.
        "SELECT v IN tt FROM s",
        "SELECT v IN ti FROM s",
    ]);
    tw.refuse("SELECT v IN u FROM s", SchemaKind::ColumnCountMismatch);
    tw.refuse("SELECT v IN nosuch FROM s", SchemaKind::UnknownTable);
    // Parenthesised, it is an expression list naming a column.
    tw.refuse("SELECT v IN (t) FROM s", SchemaKind::UnknownColumn);
    let tw = twins(
        "in-table-w",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2);
         CREATE TABLE s(v); INSERT INTO s VALUES(1),(2),(3),(4)",
    );
    for sql in [
        "UPDATE s SET v = v * 10 WHERE v NOT IN t",
        "DELETE FROM s WHERE v IN t",
    ] {
        assert!(tw.same(sql).is_none());
    }
    tw.finish("SELECT rowid, v FROM s ORDER BY rowid");
}

// ---------------- review regressions ----------------

/// hipp's own verdict on one statement (no oracle): for shapes
/// the pin runs but hipp refuses, so a silent wrong answer
/// cannot hide behind `same`.
fn hipp_only(
    tw: &Twins,
    sql: &str,
) -> Result<Vec<String>, Error<<FileStorage as hipp::Storage>::Error>> {
    block_on(async {
        let mut db = open(&tw.hipp).await;
        try_collect(&mut db, sql).await
    })
}

/// An alias whose item holds a subquery, referenced from a
/// subquery with FROM sources of its own: the alias's
/// subquery reads the right columns (review regression: its
/// slots were not rebased onto the deeper row — no rows, or
/// the nested source's columns).
#[cfg(feature = "v16")]
#[test]
fn alias_with_subquery_used_in_a_nested_scope() {
    let tw = twins(
        "alias-nest",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);
         CREATE TABLE u(b, d); INSERT INTO u VALUES(1,'p'),(2,'q'),(3,'r');
         CREATE TABLE w(k); INSERT INTO w VALUES('p'),('r')",
    );
    tw.all(&[
        "SELECT a, (SELECT d FROM u WHERE u.b=t.a) AS r FROM t
         WHERE (SELECT count(*) FROM w WHERE w.k = r) = 1",
        "SELECT a, (SELECT d FROM u WHERE u.b=t.a) AS r FROM t
         WHERE EXISTS (SELECT 1 FROM w WHERE w.k = r)",
        "SELECT a, (SELECT max(d) FROM u) AS m FROM t
         WHERE EXISTS (SELECT 1 FROM w WHERE w.k = m)",
        // Two scopes deep, and next to the nested source's own
        // correlated reference.
        "SELECT a, (SELECT d FROM u WHERE u.b=t.a) AS r FROM t
         WHERE EXISTS (SELECT 1 FROM w WHERE w.k = r
                       AND EXISTS (SELECT 1 FROM u WHERE u.d = r AND u.b = a))",
        "SELECT a, (SELECT d FROM u WHERE u.b=t.a) AS r FROM t
         ORDER BY (SELECT count(*) FROM w, u WHERE w.k = r AND u.d = w.k) DESC, a",
        "SELECT a, (SELECT d FROM u WHERE u.b=t.a) AS r FROM t
         WHERE a IN (SELECT b FROM u WHERE d = r AND EXISTS (SELECT 1 FROM w WHERE k = r))",
        "SELECT a, (SELECT d FROM u WHERE u.b = t.a) AS r FROM t
         WHERE r IN (SELECT k FROM w JOIN u ON u.d = w.k)",
        // The alias's subquery reads a rowid and has a derived
        // table of its own.
        "SELECT a, (SELECT u.rowid FROM u WHERE u.b = t.a) AS r FROM t
         WHERE EXISTS (SELECT 1 FROM u WHERE u.b = r + 1)",
        "SELECT a, (SELECT max(z) FROM (SELECT b AS z FROM u WHERE b < t.a + 1)) AS r FROM t
         WHERE (SELECT d FROM u WHERE u.b = r) > 'p'",
        // Through GROUP BY / HAVING and an ON clause.
        "SELECT (SELECT d FROM u WHERE u.b = t.a) AS r, count(*) FROM t
         GROUP BY r HAVING EXISTS (SELECT 1 FROM w WHERE w.k = r)",
        "SELECT t.a, (SELECT d FROM u WHERE u.b = t.a) AS r FROM t JOIN u AS v
         ON v.b = (SELECT u.b FROM u, w WHERE u.d = r AND w.k = u.d)",
    ]);
}

/// The same, generated: alias definitions with correlated and
/// uncorrelated subqueries, referenced from nested subqueries
/// of various shapes, vs the oracle.
#[cfg(feature = "v16")]
#[test]
fn generated_alias_nesting_corpus() {
    let tw = twins(
        "alias-gen",
        "CREATE TABLE t(a, c); INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'x'),(4,NULL);
         CREATE TABLE u(b, d); INSERT INTO u VALUES(1,'p'),(2,'q'),(3,'r'),(4,'p');
         CREATE TABLE w(k, n); INSERT INTO w VALUES('p',1),('r',2),('q',NULL),('p',3)",
    );
    let defs = [
        "(SELECT d FROM u WHERE u.b = t.a)",
        "(SELECT max(d) FROM u)",
        "(SELECT count(*) FROM u WHERE u.b <= t.a)",
        "(SELECT d FROM u WHERE u.b = (SELECT max(b) FROM u WHERE b < t.a))",
        "a + (SELECT min(b) FROM u)",
    ];
    let uses = [
        "EXISTS (SELECT 1 FROM w WHERE w.k = z)",
        "(SELECT count(*) FROM w WHERE w.k = z OR w.n = z) > 0",
        "z IN (SELECT k FROM w)",
        "z IN (SELECT n FROM w JOIN u ON u.b = w.n)",
        "(SELECT max(n) FROM w, u WHERE u.d = w.k AND (u.b = z OR u.d = z)) IS NOT NULL",
        "EXISTS (SELECT 1 FROM (SELECT k FROM w) s WHERE s.k = z)",
    ];
    let mut rng = Rng(0x0a11_a5ed_0016_7e57);
    for _ in 0..30 {
        let d = defs[rng.below(defs.len() as u64) as usize];
        let u = uses[rng.below(uses.len() as u64) as usize];
        tw.same(&format!("SELECT a, {d} AS z FROM t WHERE {u} ORDER BY a"));
        tw.same(&format!("SELECT a, {d} AS z FROM t ORDER BY ({u}) DESC, a"));
    }
}

/// An aliased aggregate referenced inside a subquery belongs
/// to the outer query on the pin; hipp re-aggregated the
/// subquery's rows (silent wrong answers), then refused. Since
/// the v0.16 aggregate-scoping review fix the owner folds it per
/// group and substitutes the value, as on the pin.
#[cfg(feature = "v16")]
#[test]
fn aggregate_alias_inside_a_subquery_matches_the_pin() {
    let tw = twins(
        "alias-agg",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1),(1),(2),(3),(3),(3);
         CREATE TABLE u(b); INSERT INTO u VALUES(1),(2)",
    );
    tw.all(&[
        "SELECT a, count(*) AS c FROM t GROUP BY a HAVING (SELECT c) > 1",
        "SELECT a, count(*) AS c FROM t GROUP BY a HAVING (SELECT c FROM u LIMIT 1) > 1",
        "SELECT a, count(*) AS c FROM t GROUP BY a ORDER BY (SELECT c) DESC, a",
        "SELECT a, count(*) AS c FROM t GROUP BY a HAVING EXISTS (SELECT 1 FROM u WHERE u.b = c)",
        "SELECT a, count(*) + 1 AS c FROM t GROUP BY a HAVING 3 IN (SELECT c + b FROM u)",
    ]);
    // In WHERE both refuse; at the alias's own level it works.
    assert!(
        tw.same("SELECT count(*) AS c FROM t WHERE (SELECT c) > 0")
            .is_some()
    );
    tw.all(&[
        "SELECT a, count(*) AS c FROM t GROUP BY a HAVING c > 1",
        "SELECT a, count(*) AS c FROM t GROUP BY a ORDER BY c DESC, a",
        "SELECT a, count(*) + 1 AS c FROM t GROUP BY a HAVING c IN (SELECT b + 2 FROM u)",
    ]);
}

/// Compound ORDER BY names resolve per core like the pin's
/// `resolveCompoundOrderBy`: an AS name, else the term
/// resolved in the core's FROM scope and matched to the item
/// that is the same reference (review regression: a USING
/// column of a RIGHT / FULL join sorted by the wrong column).
#[test]
fn compound_order_by_resolves_terms_per_core() {
    let tw = twins(
        "comp-ord",
        "CREATE TABLE a(id, x); CREATE TABLE b(id, y);
         INSERT INTO a VALUES(1,'a1'),(5,'a5'); INSERT INTO b VALUES(5,'b5'),(3,'b3'),(0,'b0')",
    );
    tw.all(&[
        "SELECT a.id, b.id FROM a RIGHT JOIN b USING (id) UNION ALL SELECT 8, 9 ORDER BY id",
        "SELECT a.id, b.id, id FROM a FULL JOIN b USING (id) UNION ALL SELECT 8, 9, 7 ORDER BY id",
        "SELECT id FROM a RIGHT JOIN b USING (id) UNION SELECT 9 ORDER BY b.id",
        "SELECT id FROM a LEFT JOIN b USING (id) UNION SELECT 9 ORDER BY a.id",
        "SELECT a.id FROM a LEFT JOIN b USING (id) UNION SELECT 9 ORDER BY id",
        "SELECT b.id, a.id FROM a LEFT JOIN b USING (id) UNION SELECT 9, 9 ORDER BY id",
        "SELECT * FROM (SELECT a.id, b.id, id FROM a FULL JOIN b USING (id)
                       UNION ALL SELECT 8, 9, 7 ORDER BY id DESC)",
        "SELECT x AS q FROM a UNION SELECT y FROM b ORDER BY q",
        "SELECT y FROM b UNION SELECT x AS q FROM a ORDER BY q",
        "SELECT y FROM b UNION SELECT x AS y FROM a ORDER BY y",
        "SELECT x AS id, id AS x FROM a UNION SELECT 7, 8 ORDER BY x",
        "SELECT a.rowid, x FROM a UNION SELECT 1, 2 ORDER BY rowid",
        "SELECT * FROM a UNION SELECT * FROM b ORDER BY b.y",
        "VALUES (1), (2) UNION SELECT 3 ORDER BY column1",
    ]);
    for sql in [
        "SELECT id FROM a RIGHT JOIN b USING (id) UNION SELECT 9 ORDER BY a.id",
        "SELECT id FROM a LEFT JOIN b USING (id) UNION SELECT 9 ORDER BY b.id",
        "SELECT b.id FROM a LEFT JOIN b USING (id) UNION SELECT 9 ORDER BY id",
        "SELECT x, y FROM a FULL JOIN b USING (id) UNION SELECT 9, 9 ORDER BY id",
        "SELECT id + 0 FROM a FULL JOIN b USING (id) UNION SELECT 9 ORDER BY id",
        // Ambiguous in the core's FROM: no match (older bug).
        "SELECT a.id FROM a, b UNION SELECT 9 ORDER BY id",
        // An expression item's text is not a name.
        "SELECT x || '' FROM a UNION SELECT 3 ORDER BY \"x || ''\"",
    ] {
        tw.refuse(sql, SchemaKind::UnknownColumn);
    }
}

/// A derived table's duplicate column names become `id`,
/// `id:1`, ... (review regression: every duplicate stayed `id`,
/// so NATURAL / USING `SELECT *` dropped them all).
#[test]
fn derived_table_duplicate_names_are_renamed() {
    let tw = twins(
        "dup-derived",
        "CREATE TABLE a(id); CREATE TABLE b(id); CREATE TABLE c(id, z);
         INSERT INTO a VALUES(1); INSERT INTO b VALUES(7); INSERT INTO c VALUES(1,'z'),(7,'y')",
    );
    tw.all(&[
        "SELECT * FROM c JOIN (SELECT a.id, b.id FROM a, b) s USING (id)",
        "SELECT * FROM c NATURAL JOIN (SELECT 1 AS id, 7 AS id) s",
        "SELECT * FROM c NATURAL FULL JOIN (SELECT 7 AS id, 1 AS ID) s",
        "SELECT \"id:1\" FROM (SELECT 1 AS id, 7 AS id)",
        "SELECT s.id, s.\"ID:2\" FROM (SELECT 1 AS id, 7 AS id, 8 AS ID) s",
        "SELECT * FROM (SELECT 1 AS id, 7 AS id, 8 AS ID, 9 AS \"id:1\")",
        "SELECT * FROM (SELECT 1 AS \"id:1\", 2 AS id, 3 AS id)",
        "SELECT * FROM (SELECT * FROM a, b) WHERE \"id:1\" = 7",
    ]);
}

/// Scalar and EXISTS subqueries take the pin's `LIMIT (X<>0)`
/// rewrite (review regression: hipp raised a datatype
/// mismatch for a non-integer X there).
#[cfg(feature = "v16")]
#[test]
fn scalar_and_exists_subquery_limit_rewrite() {
    let tw = twins(
        "sub-limit",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(5,2),(1,4),(3,NULL)",
    );
    tw.all(&[
        "SELECT (SELECT a FROM t ORDER BY a LIMIT 2.5)",
        "SELECT (SELECT a FROM t ORDER BY a LIMIT 2.5 OFFSET 1)",
        "SELECT (SELECT a FROM t ORDER BY a LIMIT -1 OFFSET 2)",
        "SELECT (SELECT a FROM t ORDER BY a LIMIT 9223372036854775808)",
        "SELECT (SELECT a FROM t ORDER BY a LIMIT '1e0')",
        "SELECT (SELECT a FROM t UNION SELECT 9 ORDER BY 1 LIMIT 1.5 OFFSET 1)",
        "SELECT EXISTS (SELECT a FROM t LIMIT 'x')",
        "SELECT EXISTS (SELECT a FROM t LIMIT '0')",
        "SELECT EXISTS (SELECT a FROM t LIMIT ' 0 ')",
        "SELECT EXISTS (SELECT a FROM t LIMIT x'00')",
        "SELECT EXISTS (SELECT a FROM t LIMIT 0.0)",
        "SELECT a FROM t WHERE a = (SELECT a FROM t ORDER BY a DESC LIMIT 0.5)",
    ]);
    // A NULL limit, a non-integer OFFSET, and IN subqueries /
    // derived tables keep MustBeInt on both engines.
    for sql in [
        "SELECT (SELECT a FROM t LIMIT NULL)",
        "SELECT (SELECT a FROM t LIMIT 1 OFFSET 2.5)",
        "SELECT a FROM t WHERE a IN (SELECT a FROM t LIMIT 2.5)",
        "SELECT * FROM (SELECT a FROM t LIMIT 2.5)",
    ] {
        tw.refuse(sql, SchemaKind::DatatypeMismatch);
    }
    let tw = twins(
        "sub-limit-w",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES(5,2),(1,4),(3,NULL)",
    );
    assert!(
        tw.same("UPDATE t SET b = (SELECT count(*) FROM t LIMIT 1.5)")
            .is_none()
    );
    tw.finish("SELECT rowid, a, b FROM t ORDER BY rowid");
}

/// The INSERT ... SELECT upsert parse quirk only covers an
/// `ON` that could still start the last source's join
/// constraint (review regression: USING / ON-terminated joins
/// and GROUP BY / HAVING were refused; the pin accepts them).
#[test]
fn upsert_after_terminated_from_clauses() {
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, v); INSERT INTO t VALUES(2,'old');
         CREATE TABLE a(id, x); CREATE TABLE b(id, y);
         INSERT INTO a VALUES(1,'a1'),(2,'a2'); INSERT INTO b VALUES(2,'b2'),(3,'b3')";
    let tw = twins("upsert-from", setup);
    for sql in [
        "INSERT INTO t SELECT id, y FROM a JOIN b USING (id) ON CONFLICT(id) DO UPDATE SET v = excluded.v",
        "INSERT INTO t SELECT id, max(x) FROM a GROUP BY id ON CONFLICT(id) DO UPDATE SET v = excluded.v",
        "INSERT INTO t SELECT id, x FROM a GROUP BY id HAVING 1 ON CONFLICT(id) DO UPDATE SET v = excluded.v || 'h'",
        "INSERT INTO t SELECT a.id, y FROM a LEFT JOIN b ON a.id = b.id ON CONFLICT(id) DO UPDATE SET v = 'L' || excluded.v",
        "INSERT INTO t SELECT a.id, y FROM a CROSS JOIN b USING (id) ON CONFLICT(id) DO UPDATE SET v = 'c'",
    ] {
        assert!(tw.same(sql).is_none(), "{sql}");
    }
    // Still the quirk (both refuse): a bare table, a comma join
    // and an unconstrained JOIN before ON CONFLICT.
    for sql in [
        "INSERT INTO t SELECT a.id, x FROM a ON CONFLICT(id) DO UPDATE SET v = 'c'",
        "INSERT INTO t SELECT a.id, y FROM a, b ON CONFLICT(id) DO UPDATE SET v = 'c'",
        "INSERT INTO t SELECT a.id, x FROM a JOIN b ON CONFLICT(id) DO UPDATE SET v = 'c'",
    ] {
        assert!(tw.same(sql).is_some(), "{sql}");
    }
    tw.finish("SELECT id, v FROM t ORDER BY id");
}

/// UPDATE ... FROM refuses a FROM item that repeats the target
/// (same table, same alias or both bare), as the pin does
/// (review finding: hipp ran it as a self-join).
#[test]
fn update_from_repeating_the_target_is_refused() {
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES(1,2);
         CREATE TABLE u(a); INSERT INTO u VALUES(1); CREATE TABLE x(a)";
    let tw = twins("upd-self", setup);
    for sql in [
        "UPDATE t SET b = 5 FROM t",
        "UPDATE t SET b = 5 FROM T",
        "UPDATE t SET b = 5 FROM main.t",
        "UPDATE t SET b = 5 FROM u, t",
        "UPDATE t SET b = 5 FROM u NATURAL JOIN t",
        "UPDATE t AS x SET b = 5 FROM t AS X",
    ] {
        tw.refuse(sql, SchemaKind::UpdateTargetInFrom);
    }
    for sql in [
        "UPDATE t SET b = 6 FROM t AS t2 WHERE t2.a = t.a",
        "UPDATE t AS q SET b = 7 FROM t",
        "UPDATE t SET b = 8 FROM t AS t",
        "UPDATE t SET b = 9 FROM (SELECT 1) AS t",
        "UPDATE t SET b = 10 FROM u AS t",
        "UPDATE t AS u SET b = 11 FROM u",
        "UPDATE t AS x SET b = 12 FROM x",
    ] {
        assert!(tw.same(sql).is_none(), "{sql}");
    }
    tw.finish("SELECT rowid, a, b FROM t ORDER BY rowid");
}

/// A schema qualifier other than `main` is refused (review
/// finding: hipp dropped it and used the main table, e.g. for
/// the new `x IN temp.s`, where the pin has no such table).
#[test]
fn schema_qualifier_other_than_main_is_refused() {
    let tw = twins("schema-q", "CREATE TABLE s(a); INSERT INTO s VALUES(1)");
    tw.all(&["SELECT * FROM main.s", "SELECT * FROM MAIN.s"]);
    // With `attach` an unknown schema is the pin's "no such table:
    // foo.s"; without it hipp refuses the qualifier at parse.
    if cfg!(feature = "attach") {
        tw.refuse("SELECT * FROM foo.s", SchemaKind::UnknownTable);
    } else {
        tw.refuse_parse("SELECT * FROM foo.s");
    }
    // `temp.` names the TEMP database: hipp has one with `v16`
    // (empty here, so these fail as on the pin), and refuses the
    // qualifier without it.
    for sql in [
        "SELECT * FROM temp.s",
        "INSERT INTO temp.s VALUES(2)",
        "DELETE FROM temp.s",
    ] {
        if cfg!(feature = "v16") {
            assert!(tw.same(sql).is_some(), "{sql}");
        } else {
            tw.refuse_parse(sql);
        }
    }
    #[cfg(feature = "v16")]
    {
        tw.same("SELECT 1 IN main.s");
        assert!(tw.same("SELECT 1 IN temp.s").is_some());
    }
    // Without `v16` the pin would make a temporary table outside
    // the file; with it, hipp does too (never in the file).
    let r = hipp_only(&tw, "CREATE TABLE temp.q(a)");
    if cfg!(feature = "v16") {
        assert!(r.is_ok(), "{r:?}");
    } else {
        assert!(matches!(r, Err(Error::Parse { .. })), "{r:?}");
    }
    tw.finish("SELECT a FROM s");
}
