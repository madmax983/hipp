//! v0.16 expression operators, literals and row values,
//! differentially vs the pinned oracle (SQLite 3.54.1). The
//! semantics were probed on the pin before implementation; the
//! matrix the tests encode:
//!
//! - Precedence (ungated fix): `=`-level operators (`=`, `<>`,
//!   IS, IN, LIKE, GLOB, BETWEEN, ISNULL, NOTNULL) bind looser
//!   than `<` `<=` `>` `>=`, which bind looser than the bitwise
//!   `&` `|` `<<` `>>` (one level, left-associative), then `+ -`,
//!   `* / %`, `||`, COLLATE, and the unary `- + ~`. A postfix
//!   operator's result can be extended by tighter operators: `x
//!   ISNULL + 1` is `(x ISNULL) + 1`. hipp had `<` and `=` on one
//!   level (`3 = 2 < 3` was 1).
//! - Bitwise operators and `~` (`sql`): operands as 64-bit
//!   integers (a text's integer prefix, a real truncated and
//!   saturated), NULL in -> NULL out, negative shift counts shift
//!   the other way, counts >= 64 shift everything out (-1 for a
//!   negative value shifted right).
//! - Hex literals: 16 significant digits as two's complement
//!   (`0xffffffffffffffff` = -1; hipp refused anything past
//!   `0x7fff...` — ungated with the evaluator), more is "hex
//!   literal too big", as is `-` over `0x8000000000000000`;
//!   `'0x10'` text is never hex. Digit separators `1_000`
//!   (3.46+; hipp read `1` aliased `_000`) and a literal glued
//!   to an identifier character (`1a`) are fixed ungated with
//!   the evaluator.
//! - `x IS [NOT] TRUE / FALSE` is the pin's truth test (ungated:
//!   `2 IS TRUE` was 0); `IS [NOT] DISTINCT FROM` and postfix
//!   `NOT NULL` (`sql`).
//! - Row values (`sql`): `(a, b) op (c, d)` for `= <> < <= > >=
//!   IS [NOT]`, BETWEEN, `IN ((..), (..))` (the pin's VALUES
//!   rewrite: per-column affinity of the rightmost row), `IN
//!   (SELECT a, b ...)`, CASE, multi-column scalar subqueries,
//!   `UPDATE ... SET (a, b) = (..)` / `= (SELECT ..)`, and the
//!   pin's errors for size mismatches and row values anywhere
//!   else.
//! - Aggregate scoping (ungated fix): an aggregate inside a
//!   subquery whose arguments reference only an enclosing
//!   query's columns belongs to that query — `SELECT (SELECT
//!   max(t.a) FROM u) FROM t` is one row — and is "misuse of
//!   aggregate" where that query cannot aggregate (WHERE, an
//!   aggregate's arguments, UPDATE / DELETE); an aggregate in
//!   ORDER BY alone does not make a query an aggregate one.

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

/// The CLI's list rendering of a REAL (the core's REAL -> TEXT
/// conversion, which the pin's CLI prints; no negative zero).
fn fmt_real(f: f64) -> String {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    String::from_utf8_lossy(t.as_slice()).into_owned()
}

/// Step a statement to completion, collecting its rows in the
/// CLI's list rendering, or the error.
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
                Some(hipp::Value::Real(f)) => line.push_str(&fmt_real(f)),
                Some(hipp::Value::Text(b)) | Some(hipp::Value::Blob(b)) => {
                    line.push_str(&String::from_utf8_lossy(b));
                }
            }
        }
        rows.push(line);
    }
    Ok(rows)
}

/// One batch through the oracle CLI: (success, stdout lines).
/// Error text is not compared.
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

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp = common::temp_db(&format!("v16ex-{tag}-h"));
    let oracle = common::temp_db(&format!("v16ex-{tag}-o"));
    common::cli(&oracle, if setup.is_empty() { "SELECT 1;" } else { setup });
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

type HErr = Error<<FileStorage as hipp::Storage>::Error>;

impl Twins {
    /// One statement on both twins: equal rows (order
    /// included), or both refuse it. Returns hipp's error when
    /// both refused.
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

    /// Both engines refuse `sql`; hipp with schema error `kind`.
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

    /// Both engines refuse `sql` (any error kind).
    fn refuse_any(&self, sql: &str) {
        assert!(self.same(sql).is_some(), "expected an error for {sql}");
    }

    /// The oracle reads the hipp file clean and both twins hold
    /// the same rows for `dump`.
    fn finish(&self, dump: &str) {
        assert_eq!(oracle_raw(&self.hipp, "PRAGMA integrity_check;").1, ["ok"]);
        assert_eq!(oracle_raw(&self.hipp, dump), oracle_raw(&self.oracle, dump));
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

/// Compare many scalar expressions, a few per SELECT (fewer
/// oracle processes, and a failure still names its statement).
fn exprs_same(tw: &Twins, exprs: &[String]) {
    for chunk in exprs.chunks(12) {
        tw.same(&format!("SELECT {}", chunk.join(", ")));
    }
}

// ---------------- precedence (ungated) ----------------

#[test]
fn operator_precedence_matches_the_pin() {
    let tw = twins("prec", "");
    tw.all(&[
        "SELECT 3 = 2 < 3, 1 < 2 = 0, 0 = 1 > 2, 2 > 1 IS 0, 0 IS 2 > 1",
        "SELECT 1 IN (1) = 1, 2 = 1 IN (1), 1 < 2 BETWEEN 0 AND 1, 5 BETWEEN 1 AND 9 = 1",
        "SELECT 0 = 5 BETWEEN 1 AND 9, 1 BETWEEN 0 AND 2 < 3, 'a' LIKE 'a' = 1, 0 = 'a' LIKE 'b'",
        "SELECT 1 = 1 ISNULL, 1 NOTNULL = 1, NULL IS NULL = 1, 1 < 2 < 3, 3 > 2 > 1",
        "SELECT 1 <> 2 = 1, 1 = 2 <> 2 = 3, 1 ISNULL + 1, 2 = 1 ISNULL + 1",
        "SELECT 1 IN (1) + 1, 1 IN (1) * 5, 1 IN (2) = 0, 1 IN (1) < 2, 5 IN (5) IN (1)",
        "SELECT 2 BETWEEN 1 = 1 AND 3, 1 BETWEEN 0 < 1 AND 2, 1 BETWEEN 0 AND 2 < 1",
        "SELECT 1 BETWEEN 0 AND 1 = 1, 3 BETWEEN 1 AND 2 + 1, 1 BETWEEN 0 AND 2 BETWEEN 1 AND 1",
        "SELECT 'a' LIKE 'A' < 1, 'a' LIKE 'b' = 0, 1 < 'a' LIKE 'a'",
        "SELECT NOT 0 + 1, NOT 0 = 1, - 1 ISNULL, 1 + NOT 0, - NOT 0 = 1, 1 + NOT 0 AND 0",
        "SELECT NOT NOT 1, 2 BETWEEN NOT 1 AND 3, 1 NOT IN (2) + 1, 1 NOT BETWEEN 2 AND 3 + 1",
        "SELECT 'a' NOT LIKE 'b' + 1, 1 IS NOT 2 + 1, 1 ISNULL = 0, 0 = 1 ISNULL, 1 < 2 ISNULL",
    ]);
    tw.refuse_parse("SELECT 1 BETWEEN 1 OR 0 AND 3");
}

/// Random expression trees over small integers, NULL and every
/// operator level, compared value for value: any precedence or
/// associativity difference shows up as a different answer.
#[test]
fn generated_precedence_corpus() {
    let tw = twins("precgen", "");
    let mut rng = Rng(0x0e1f_0016_5eed_7a11);
    let atoms = ["0", "1", "2", "3", "NULL", "-1", "(1)", "'2'"];
    let mut ops = vec![
        " = ", " == ", " <> ", " != ", " < ", " <= ", " > ", " >= ", " IS ", " IS NOT ", " + ",
        " - ", " * ", " AND ", " OR ",
    ];
    if cfg!(feature = "sql") {
        ops.extend([
            " & ",
            " | ",
            " << ",
            " >> ",
            " IS DISTINCT FROM ",
            " IS NOT DISTINCT FROM ",
        ]);
    }
    let mut out = Vec::new();
    for _ in 0..480 {
        let mut e = rng.pick(&atoms).to_string();
        for _ in 0..(2 + rng.below(4)) {
            match rng.below(9) {
                0 => e.push_str(" ISNULL"),
                1 => e.push_str(" NOTNULL"),
                2 => e.push_str(&format!(" IN ({}, {})", rng.pick(&atoms), rng.pick(&atoms))),
                3 => e.push_str(&format!(
                    " BETWEEN {} AND {}",
                    rng.pick(&atoms),
                    rng.pick(&atoms)
                )),
                4 => e = format!("NOT {e}"),
                5 if cfg!(feature = "sql") => e = format!("~{e}"),
                5 => e = format!("-{e}"),
                6 if cfg!(feature = "sql") => e.push_str(" NOT NULL"),
                _ => {
                    let op = rng.pick(&ops);
                    let rhs = rng.pick(&atoms);
                    e = format!("{e}{op}{rhs}");
                }
            }
        }
        out.push(e);
    }
    exprs_same(&tw, &out);
}

// ---------------- IS TRUE / FALSE (ungated) ----------------

#[test]
fn truth_tests() {
    let tw = twins(
        "truth",
        "CREATE TABLE t(a); INSERT INTO t VALUES (2),(0),(NULL),('x'),(0.5),('1e1'),(x'31')",
    );
    tw.all(&[
        "SELECT 1 IS TRUE, 0 IS FALSE, NULL IS NOT TRUE, 2 IS TRUE, 0.5 IS TRUE, 'a' IS FALSE",
        "SELECT NULL IS TRUE, 1 IS NOT FALSE, NULL IS NOT FALSE, '1' IS TRUE, x'01' IS TRUE",
        "SELECT 2 IS NOT TRUE, 0 IS NOT TRUE, 1 IS TRUE + 1, 0 IS FALSE = 1, (1 IS TRUE) + 1",
        "SELECT a IS TRUE, a IS FALSE, a IS NOT TRUE, a IS NOT FALSE, a IS 1, a IS 0 FROM t",
        "SELECT count(*) FROM t WHERE a IS TRUE",
        "SELECT count(*) FROM t WHERE a IS NOT FALSE",
    ]);
}

#[cfg(feature = "sql")]
#[test]
fn is_distinct_from_and_not_null() {
    let tw = twins(
        "distinct",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (1,1),(1,2),(NULL,1),(NULL,NULL),('1',1)",
    );
    tw.all(&[
        "SELECT 1 IS DISTINCT FROM 1, NULL IS DISTINCT FROM NULL, 1 IS NOT DISTINCT FROM 1",
        "SELECT NULL IS NOT DISTINCT FROM 1, 1 IS DISTINCT FROM 2 = 1, 2 = 1 IS DISTINCT FROM 1",
        "SELECT 1 IS NOT DISTINCT FROM 1 + 0, 1 IS DISTINCT FROM TRUE, 2 IS NOT DISTINCT FROM TRUE",
        "SELECT a IS DISTINCT FROM b, a IS NOT DISTINCT FROM b FROM t",
        "SELECT 1 NOT NULL, NULL NOT NULL, 1 = 1 NOT NULL, 2 NOT NULL = 1, NOT 1 NOT NULL",
        "SELECT 1 NOT NULL + 1, 1 ISNULL NOTNULL, 1 NOT NULL NOT NULL, 'a' NOT NULL LIKE 1",
        "SELECT a NOT NULL, b ISNULL FROM t",
        "SELECT count(*) FROM t WHERE a NOT NULL AND b IS NOT DISTINCT FROM a",
    ]);
    tw.refuse_parse("SELECT 1 IS DISTINCT 2");
    tw.refuse_parse("SELECT 1 NOT DISTINCT FROM 2");
}

// ---------------- bitwise operators (v16) ----------------

#[cfg(feature = "sql")]
#[test]
fn bitwise_operators() {
    let tw = twins("bits", "");
    tw.all(&[
        "SELECT 1|2, 5&3, 1<<2, 8>>1, ~5, ~0, ~-1",
        "SELECT 1<<63, 1<<64, 1<<65, 1<<-1, 8>>-1, -1>>1, -1>>63, -1>>64, -1>>100, 1>>64",
        "SELECT -8>>1, 1<<-64, 1<<-65, -1 << -64, -1<< -100, 5 << 0, -5 >> 0",
        "SELECT 1 << 9223372036854775807, 1 << -9223372036854775808, -1 >> -9223372036854775808",
        "SELECT 1+2<<1, 1<<1+2, 2*3<<1, 1|2=3, 3=1|2, 1|2&3, 1&2|3, 6&3|8, 1|6&3",
        "SELECT 1<<2|1, 1|1<<2, 1<<2&3, 1&3<<2, 5 & 3 = 1, 1 = 5 & 1, 4 >> 1 < 3, 3 < 4 >> 1",
        "SELECT 1 + 1 << 1 + 1, 1 || 2 << 1, 1 << 1 || 2, 1 << 1 * 3, '1' || 1 | 1, 2 * 3 & 2",
        "SELECT 1 & 2 COLLATE nocase, 6 % 4 >> 1, 1 << 2 % 3, 'x' || ~1",
        "SELECT ~1+1, -~1, ~-1, ~~5, ~'5', ~'abc', ~5.7, ~-5.7, ~NULL, ~x'01', NOT ~0, ~1 IS -2",
        "SELECT '3'|4, '3.9'|4, 3.9|4, -3.9|0, 'abc'|1, '12abc'|1, x'31'|0",
        "SELECT NULL|1, 1&NULL, NULL<<1, 1<<NULL, 1e18|0, 1e19|0, -1e19|0, 9.2e18|0",
        "SELECT ' 12 '|0, '0x10'|0, '1e3'|0, '-5'|0, '+5'|0, '  -7.9e1'|0",
        "SELECT '9223372036854775808'|0, '-9223372036854775809'|0, 'inf'|0",
        "SELECT typeof(1|2), typeof(1.5|0), typeof('a'|0), typeof(NULL|0), typeof(~1.5)",
    ]);
    tw.refuse_parse("SELECT 1 < < 2");
    tw.refuse_parse("SELECT 1 >>= 2");
}

/// Bitwise operators over a pool of operands of every storage
/// class, plus the shift counts around 0 and 64.
#[cfg(feature = "sql")]
#[test]
fn generated_bitwise_corpus() {
    let tw = twins("bitgen", "");
    let mut rng = Rng(0xb175_0016_dead_beef);
    let vals = [
        "0",
        "1",
        "-1",
        "7",
        "-8",
        "255",
        "9223372036854775807",
        "-9223372036854775808",
        "4611686018427387904",
        "3.7",
        "-3.7",
        "1e300",
        "-1e300",
        "'12'",
        "' 34x'",
        "'-5.5'",
        "'abc'",
        "x'3132'",
        "NULL",
        "0x7f",
        "0xffffffffffffffff",
    ];
    let counts = [
        "0", "1", "2", "31", "32", "62", "63", "64", "65", "-1", "-63", "-64", "-65", "100", "'3'",
        "2.9", "NULL",
    ];
    let mut out = Vec::new();
    for _ in 0..360 {
        let a = rng.pick(&vals);
        let e = match rng.below(5) {
            0 => format!("{a} & {}", rng.pick(&vals)),
            1 => format!("{a} | {}", rng.pick(&vals)),
            2 => format!("{a} << {}", rng.pick(&counts)),
            3 => format!("{a} >> {}", rng.pick(&counts)),
            _ => format!("~{a}"),
        };
        out.push(e.clone());
        out.push(format!("typeof({e})"));
    }
    exprs_same(&tw, &out);
}

#[cfg(feature = "sql")]
#[test]
fn bitwise_over_columns_and_affinity() {
    let tw = twins(
        "bitcol",
        "CREATE TABLE t(i INTEGER, r REAL, x TEXT, n NUMERIC, b BLOB);
         INSERT INTO t VALUES (6, 2.5, '12', '3', x'35'), (-1, -0.5, 'a', 'x', NULL), (NULL, 1e20, ' 9 ', 4.0, 7)",
    );
    tw.all(&[
        "SELECT i & 3, r | 0, x << 1, n >> 1, b & 7, ~i, ~r, ~x, ~n, ~b FROM t",
        "SELECT i FROM t WHERE i & 2 = 2",
        "SELECT i | r = '7', (x | 0) = 12, x | 0 = '12' FROM t",
        "SELECT sum(i << 2), max(~i), group_concat(i & 1) FROM t",
        "SELECT i FROM t ORDER BY ~i",
    ]);
    tw.same("CREATE TABLE g(a, c AS (a << 2), d AS (~a) STORED)");
    tw.same("INSERT INTO g(a) VALUES (1), (-3), (NULL), ('5')");
    tw.same("UPDATE t SET i = i | 8 WHERE i NOT NULL");
    tw.finish("SELECT * FROM t; SELECT * FROM g;");
}

// ---------------- literals ----------------

#[test]
fn hex_literals() {
    let tw = twins("hex", "");
    tw.all(&[
        "SELECT 0x10, 0X1f, 0xffffffffffffffff, 0x7fffffffffffffff, 0x8000000000000000",
        "SELECT 0x00000000000000001, typeof(0xffffffffffffffff), -0x10, 0x0, -0xffffffffffffffff",
        "SELECT -0x7fffffffffffffff, 0-0x8000000000000000, +0x8000000000000000",
        "SELECT 0x0000000000000000000000001, 0x00000000000000000ffffffffffffffff",
        "SELECT 0xABCDEF, 0xabcdef12345, 0x10e5, 0xe+1, 0x1e-1, 0x10 AS a",
        "SELECT CAST('0x10' AS INTEGER), '0x10'+0, CAST('0x10' AS REAL), CAST('0x10' AS NUMERIC)",
        "SELECT 0x10 = '16', typeof('0x10'+0), CAST(0x10 AS TEXT), 0x10 || ''",
        "SELECT 1 LIMIT 0x1",
    ]);
    for bad in [
        "SELECT 0x1ffffffffffffffff",
        "SELECT -0x8000000000000000",
        "SELECT -(0x8000000000000000)",
        "SELECT - 0x8000000000000000",
        "SELECT -+0x8000000000000000",
        "SELECT - - 0x8000000000000000",
        "SELECT -(+0x8000000000000000)",
        "SELECT 0x",
        "SELECT 0x1g",
        "SELECT 0x10.5",
    ] {
        tw.refuse_parse(bad);
    }
}

#[test]
fn hex_literals_in_affinity_contexts() {
    let tw = twins(
        "hexaff",
        "CREATE TABLE t(a INTEGER, b REAL, c TEXT, d NUMERIC, e);
         CREATE TABLE dflt(k, a DEFAULT 0x10, b DEFAULT -0x10, c DEFAULT 0xffffffffffffffff)",
    );
    tw.same("INSERT INTO t VALUES ('0x10','0x10','0x10','0x10','0x10')");
    tw.same("INSERT INTO t VALUES (0x10,0x10,0x10,0x10,0x10)");
    tw.same("INSERT INTO t VALUES (0xffffffffffffffff, 0x8000000000000000, 0x7f, -0x1, 0x0)");
    tw.same("INSERT INTO dflt(k) VALUES (1)");
    tw.refuse_parse("INSERT INTO t(a) VALUES (-0x8000000000000000)");
    tw.all(&[
        "SELECT typeof(a), a, typeof(b), b, typeof(c), c, typeof(d), d, typeof(e), e FROM t",
        "SELECT * FROM dflt",
        "SELECT a FROM t WHERE a = 0x10",
        "SELECT c FROM t WHERE c = 0x10",
    ]);
    tw.finish("SELECT * FROM t; SELECT * FROM dflt;");
}

#[test]
fn digit_separators_and_glued_identifiers() {
    let tw = twins("sep", "");
    tw.all(&[
        "SELECT 1_000, 1_0.5, 1.0_5, 1e1_0, .5_1, 0x1_0, 0xff_ff",
        "SELECT 9_223_372_036_854_775_807, 9_223_372_036_854_775_808",
        "SELECT -9_223_372_036_854_775_808, typeof(-9_223_372_036_854_775_808)",
        "SELECT '1_000'+0, CAST('1_0' AS INTEGER), 1.e5, 1., .1e-2",
    ]);
    for bad in [
        "SELECT 1__0",
        "SELECT 1_",
        "SELECT 1_e5",
        "SELECT 1e_5",
        "SELECT 0x1__0",
        "SELECT 0x1_",
        "SELECT 1_a",
        "SELECT 0x_",
        "SELECT 0x_1",
        "SELECT 1a",
        "SELECT 1$",
        "SELECT 1e5x",
        "SELECT 0x10a_",
    ] {
        tw.refuse_parse(bad);
    }
}

#[test]
fn smallest_integer_literal() {
    let tw = twins("minint", "CREATE TABLE t(a)");
    tw.same("INSERT INTO t VALUES (-9223372036854775808), (-(9223372036854775808)), (- -9223372036854775808)");
    tw.all(&[
        "SELECT -9223372036854775808, typeof(-9223372036854775808), typeof(- 9223372036854775808)",
        "SELECT -(9223372036854775808), typeof(-+9223372036854775808), -09223372036854775808",
        "SELECT typeof(- -9223372036854775808), typeof(-(-(9223372036854775808)))",
        "SELECT typeof(9223372036854775808), typeof(-9223372036854775808.0)",
        "SELECT typeof(-9223372036854775808e0), typeof(-9223372036854775809)",
        "SELECT CAST(-(-9223372036854775808) AS TEXT)",
        "SELECT a, typeof(a) FROM t",
    ]);
    tw.finish("SELECT a, typeof(a) FROM t;");
}

// ---------------- row values (v16) ----------------

#[cfg(feature = "sql")]
#[test]
fn row_value_comparisons() {
    let tw = twins("rows", "");
    tw.all(&[
        "SELECT (1,2)=(1,2), (1,2)=(1,3), (1,2)<(1,3), (1,2)<(2,0), (2,0)<(1,9), (1,2)<=(1,2)",
        "SELECT (1,2)>(1,1), (1,2)>=(1,3), (1,2)<>(1,2), (1,2)!=(1,3), (1, 2) == (1, 2)",
        "SELECT (1,NULL)=(1,2), (1,NULL)=(2,2), (NULL,1)=(1,2), (1,NULL)<>(2,3), (1,NULL)<>(1,3)",
        "SELECT (NULL,1)<(2,3), (1,NULL)<(1,2), (1,NULL)<(2,0), (2,NULL)<(1,0), (NULL,1)<=(NULL,1)",
        "SELECT (1,NULL)<=(1,NULL), (1,2) IS (1,2), (1,NULL) IS (1,NULL), (1,NULL) IS NOT (1,2)",
        "SELECT (NULL,NULL) IS (NULL,NULL), (1,2) IS DISTINCT FROM (1,3), (1,2) IS NOT DISTINCT FROM (1,2)",
        "SELECT (1,2,3)=(1,2,3), (1,2,3)<(1,2,4), (1,2,3)>(1,2,4), (1,2,3)<=(1,1,9), (1,2,3)>=(1,2,3)",
        "SELECT (1,2) = (1,2) = 1, ((1,2) = (1,2)) + 1, (1,'a') > (1,'B'), ('a',1) < ('b',0)",
        "SELECT (1)=(1), ((1)), (1,(SELECT 2)) = (1,2)",
        "SELECT (2,3) BETWEEN (1,1) AND (3,3), (1,2) BETWEEN (1,2) AND (1,2), (0,5) BETWEEN (1,1) AND (3,3)",
        "SELECT (2,3) NOT BETWEEN (1,1) AND (3,3), (NULL,1) BETWEEN (0,0) AND (2,2)",
        "SELECT CASE (1,2) WHEN (1,2) THEN 'y' END, CASE (1,2) WHEN (3,4) THEN 'a' WHEN (1,2) THEN 'b' ELSE 'c' END",
        "SELECT CASE (1,NULL) WHEN (1,NULL) THEN 1 ELSE 0 END",
        "SELECT ('a' COLLATE nocase,'b') = ('A','b'), ('a','b') = ('A' COLLATE nocase,'b')",
    ]);
}

/// Random row values of two or three elements (integers, text,
/// NULL) under every comparison operator, BETWEEN and IN.
#[cfg(feature = "sql")]
#[test]
fn generated_row_value_corpus() {
    let tw = twins("rowgen", "");
    let mut rng = Rng(0x0016_0f0e_cafe_f00d);
    let atoms = ["0", "1", "2", "NULL", "'1'", "'a'", "1.0"];
    let ops = ["=", "<>", "<", "<=", ">", ">=", "IS", "IS NOT", "==", "!="];
    let row = |rng: &mut Rng, n: usize| {
        let parts: Vec<&str> = (0..n).map(|_| rng.pick(&atoms)).collect();
        format!("({})", parts.join(", "))
    };
    let mut out = Vec::new();
    for _ in 0..420 {
        let n = 2 + rng.below(2);
        let a = row(&mut rng, n);
        let e = match rng.below(4) {
            0 | 1 => format!("{a} {} {}", rng.pick(&ops), row(&mut rng, n)),
            2 => format!(
                "{a} {}BETWEEN {} AND {}",
                ["", "NOT "][rng.below(2)],
                row(&mut rng, n),
                row(&mut rng, n)
            ),
            _ => {
                let items: Vec<String> = (0..1 + rng.below(3)).map(|_| row(&mut rng, n)).collect();
                format!(
                    "{a} {}IN ({})",
                    ["", "NOT "][rng.below(2)],
                    items.join(", ")
                )
            }
        };
        out.push(e);
    }
    exprs_same(&tw, &out);
}

#[cfg(feature = "sql")]
#[test]
fn row_value_in_lists_and_subqueries() {
    let tw = twins(
        "rowin",
        "CREATE TABLE t(a INTEGER, b TEXT, c); INSERT INTO t VALUES (1,'x',1),(2,'y','2'),(3,'1',3);
         CREATE TABLE p(x, y); CREATE TABLE q(x, y);
         INSERT INTO p VALUES (1,2),(3,4),(5,NULL); INSERT INTO q VALUES (1,2),(5,6),(NULL,4)",
    );
    tw.all(&[
        "SELECT (1,2) IN ((1,2),(3,4)), (1,2) IN ((1,2),(3,4)) + 1, (1,2) IN ((1,2)), (1,2) IN ()",
        "SELECT (1,2) NOT IN ((1,2),(3,4)), (9,9) NOT IN ((1,2),(3,4)), (1,2) IN ((1,2),(NULL,1))",
        "SELECT (1,2) IN (VALUES (1,2),(3,4)), (5,6) IN (VALUES (1,2),(3,4)), (1,NULL) IN (VALUES (1,2))",
        "SELECT (1,NULL) NOT IN (VALUES (1,2)), (2,NULL) IN (VALUES (1,2)), (1,2) IN (VALUES (1,NULL))",
        "SELECT (1,2) IN (VALUES (2,NULL)), (1,2) NOT IN (VALUES (2,NULL)), (NULL,NULL) IN (VALUES (1,1))",
        "SELECT (1,2) IN (SELECT 1,2 UNION SELECT 3,4), (1,2) IN (SELECT 3,4 UNION ALL SELECT 1,2)",
        "SELECT (SELECT 1,2) IN (SELECT 1,2), (1,2) IN ((SELECT 1,2))",
        "SELECT a FROM t WHERE (a,b) = (2,'y')",
        "SELECT a FROM t WHERE (a,b) > (1,'x') ORDER BY a",
        "SELECT a FROM t WHERE (a,c) = ('2',2)",
        "SELECT a FROM t WHERE (b,a) = (1,'3')",
        "SELECT a FROM t WHERE (a,b) IN ((1,'x'),('3',1))",
        "SELECT a FROM t WHERE (a,b) IN (SELECT a,b FROM t WHERE a>1)",
        "SELECT a FROM t WHERE (c,b) IN (VALUES ('1','x'), (2,'y'))",
        // The rightmost row's element affinity decides a written list.
        "SELECT a FROM t WHERE (c,b) IN ((a,'q'),(2,'y'))",
        "SELECT a FROM t WHERE (c,b) IN ((2,'y'),(a,'q'))",
        "SELECT a FROM t WHERE (c,1) IN ((a,1),('2',1))",
        "SELECT a FROM t WHERE (c,1) IN (('2',1),(a,1))",
        "SELECT a FROM t WHERE (a, b) IN (SELECT c, a FROM t)",
        "SELECT a FROM t WHERE (c, b) IN (SELECT a, b FROM t)",
        "SELECT ('a' COLLATE nocase,'b') IN (('A','b')), ('a','b') IN (('A' COLLATE nocase,'b'))",
        "SELECT x, (x,y) IN (SELECT x,y FROM q), (x,y) NOT IN (SELECT x,y FROM q) FROM p",
        "SELECT x, (x,y) = (SELECT x,y FROM q WHERE q.x = p.x) FROM p",
        "SELECT * FROM p WHERE (x,y) IN (SELECT y-1, y FROM p)",
        "SELECT (x, y) < (SELECT 1, 3), (SELECT x, y) = (1, 2) FROM p",
    ]);
}

#[cfg(feature = "sql")]
#[test]
fn scalar_subquery_row_values() {
    let tw = twins(
        "rowsub",
        "CREATE TABLE u(a, b); INSERT INTO u VALUES (1, 2), (3, 4)",
    );
    tw.all(&[
        "SELECT (SELECT 1,2) = (1,2), (SELECT 1,2) < (1,3), (1,2) = (SELECT 1,2)",
        "SELECT (SELECT 1,2) = (SELECT 1,2), (SELECT 1, 2 WHERE 0) = (1,2)",
        "SELECT (SELECT 1,2 UNION SELECT 0,0) = (0,0)",
        "SELECT (SELECT a, b FROM u ORDER BY a DESC) = (3, 4), (SELECT a, b FROM u) > (0, 9)",
        "SELECT a FROM u WHERE (a, b) = (SELECT a, b FROM u ORDER BY a DESC)",
        "SELECT a FROM u AS o WHERE (SELECT o.a, o.b) IS (1, 2)",
    ]);
}

#[cfg(feature = "sql")]
#[test]
fn row_value_errors() {
    let tw = twins(
        "rowerr",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (1, 2)",
    );
    for sql in [
        "SELECT ((1,2),3) = ((1,2),3)",
        "SELECT (1,2)=(1,2,3)",
        "SELECT (1,2)=1",
        "SELECT 1=(1,2)",
        "SELECT (1,2)",
        "SELECT (1,2)+1",
        "SELECT -(1,2)",
        "SELECT NOT (1,2)",
        "SELECT (1,2) IS NULL",
        "SELECT (1,2) ISNULL",
        "SELECT abs((1,2))",
        "SELECT CAST((1,2) AS INT)",
        "SELECT (1,2) COLLATE nocase = (1,2)",
        "SELECT (1,2) AND 1",
        "SELECT 1 WHERE (1,2)",
        "SELECT (1,2) LIKE (1,2)",
        "SELECT (1,2) IN (1,2)",
        "SELECT 1 IN ((1,2))",
        "SELECT 1 IN (1,(1,2))",
        "SELECT 2 BETWEEN (1,1) AND 3",
        "SELECT (SELECT 1,2) = 1",
        "SELECT (1,2) IN ((1,2),3)",
        "SELECT (1,2) IN ((1,2,3))",
        "SELECT (SELECT 1,2) IN ((1,2))",
        "SELECT 1 FROM t WHERE (a,b) IN (t)",
        "INSERT INTO t VALUES((1,2),3)",
        "SELECT * FROM t ORDER BY (a,b)",
        "SELECT count(*) FROM t GROUP BY (a,b)",
        "SELECT max((1,2))",
        "SELECT 1 LIMIT (1,2)",
        "SELECT (a,b) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::RowValueMisused);
    }
    for sql in [
        "SELECT (1,2) IN (SELECT 1)",
        "SELECT 1 IN (SELECT 1,2)",
        "SELECT (SELECT 1,2)",
        "SELECT (SELECT 1,2) + 1",
        "SELECT (1,2) = (1,(SELECT 2,3))",
        "UPDATE t SET (a,b) = (1,2,3)",
        "UPDATE t SET (a,b) = (SELECT 1)",
        "UPDATE t SET (a,b) = 1",
        "UPDATE t SET (a,b) = (SELECT 1,2,3)",
        "UPDATE t SET (a) = (SELECT 1,2)",
        "UPDATE t SET (a,b) = (SELECT 1,2) + 1",
    ] {
        tw.refuse(sql, SchemaKind::ColumnCountMismatch);
    }
    tw.refuse("UPDATE t SET (a,nope) = (1,2)", SchemaKind::UnknownColumn);
    tw.finish("SELECT * FROM t;");
}

#[cfg(feature = "sql")]
#[test]
fn update_set_row_values() {
    let tw = twins(
        "rowset",
        "CREATE TABLE t(a, b, c); INSERT INTO t VALUES (1,2,3),(4,5,6);
         CREATE TABLE s(k PRIMARY KEY, v, w); INSERT INTO s VALUES (1,'a','b'),(2,'c','d');
         CREATE TABLE r(a INTEGER, b TEXT); INSERT INTO r VALUES (1, 'x')",
    );
    tw.same("UPDATE t SET (a,b) = (b,a)");
    tw.same("SELECT * FROM t");
    tw.same("UPDATE t SET (a,c) = (SELECT 10, 20) WHERE a = 2");
    tw.same("SELECT * FROM t");
    tw.same("UPDATE t SET (b,c) = (SELECT a*2, a*3 FROM t AS u WHERE u.rowid = t.rowid), a = a+1");
    tw.same("SELECT * FROM t");
    tw.same("UPDATE t SET (a) = 5 WHERE rowid = 1");
    tw.same("UPDATE t SET (a,a) = (7,8) WHERE rowid = 2");
    tw.same("UPDATE t SET (a,b) = ((SELECT 7),8), b = 9 WHERE rowid = 1");
    tw.same("UPDATE t SET (b,c) = (SELECT 70,80 UNION ALL SELECT 90,90) WHERE rowid = 2");
    tw.same("UPDATE t SET (a,b) = (SELECT 7,8 WHERE 0) WHERE rowid = 2");
    tw.same("UPDATE t SET (rowid,a) = (5,6) WHERE rowid = 1");
    tw.same("UPDATE r SET (a, b) = ('12', 34)");
    tw.same("INSERT INTO s VALUES (1,'z','q') ON CONFLICT(k) DO UPDATE SET (w, v) = (excluded.v, excluded.w)");
    tw.same(
        "INSERT INTO s VALUES (2,'z','q') ON CONFLICT(k) DO UPDATE SET (v, w) = (SELECT 'm', 'n')",
    );
    tw.same("UPDATE s SET (v, w) = (w, v) WHERE (k, v) = (1, 'q') RETURNING k, v, w");
    tw.finish("SELECT rowid, * FROM t; SELECT * FROM s; SELECT a, typeof(a), b, typeof(b) FROM r;");
}

#[cfg(feature = "sql")]
#[test]
fn row_values_in_check_and_delete() {
    let tw = twins(
        "rowchk",
        "CREATE TABLE t(a, b, CHECK ((a, b) <> (0, 0))); INSERT INTO t VALUES (1,2),(3,4),(5,6)",
    );
    tw.refuse("INSERT INTO t VALUES (0, 0)", SchemaKind::CheckViolation);
    tw.same("INSERT INTO t VALUES (0, 1)");
    tw.same("DELETE FROM t WHERE (a, b) = (3, 4)");
    tw.same("DELETE FROM t WHERE (a, b) IN ((5, 6), (9, 9))");
    tw.finish("SELECT * FROM t;");
}

// ---------------- aggregate scoping (ungated) ----------------

const AGG: &str = "CREATE TABLE t(a, z); INSERT INTO t VALUES (1,10),(2,20),(3,30);
    CREATE TABLE u(b); INSERT INTO u VALUES (5),(6); CREATE TABLE e(c)";

#[test]
fn outer_aggregate_inside_a_subquery() {
    let tw = twins("aggscope", AGG);
    tw.all(&[
        "SELECT (SELECT max(t.a) FROM u) FROM t",
        "SELECT coalesce((SELECT max(t.a) FROM e), 'none') FROM t",
        "SELECT (SELECT max(t.a) + b FROM u) FROM t",
        "SELECT (SELECT max(t.a + b) FROM u) FROM t",
        "SELECT (SELECT max(b) FROM u), (SELECT count(t.a) FROM u) FROM t",
        "SELECT a, (SELECT count(t.a) FROM u) FROM t",
        "SELECT (SELECT sum(t.a) FROM u WHERE b > 5) FROM t",
        "SELECT (SELECT sum(t.a) FROM u WHERE b > t.a) FROM t",
        "SELECT a FROM t GROUP BY a HAVING a = (SELECT max(t.a) FROM u)",
        "SELECT a, (SELECT max(t.a) FROM u) FROM t GROUP BY a",
        "SELECT coalesce((SELECT (SELECT max(t.a) FROM e) FROM u), 'none') FROM t",
        "SELECT (SELECT max(t.a) FROM u) + count(*) FROM t",
        "SELECT count(*), (SELECT max(t.a) FROM u) FROM t",
        "SELECT (SELECT count(*) FROM u) FROM t",
        "SELECT (SELECT count(*) + max(t.a) FROM u) FROM t",
        "SELECT EXISTS (SELECT max(t.a) FROM u) FROM t",
        "SELECT a IN (SELECT max(t.a) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FROM u GROUP BY b) FROM t",
        "SELECT (SELECT group_concat(t.a) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FROM u u2 WHERE u2.b = 5) FROM t WHERE a > 1",
        "SELECT (SELECT max(x.a) FROM u) FROM t AS x",
        "SELECT max(a), (SELECT max(t.z) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FROM u) FROM t GROUP BY z",
        "SELECT (SELECT max(t.a) FROM u ORDER BY 1) FROM t",
        "SELECT (SELECT b FROM u ORDER BY max(t.a)) FROM t",
        "SELECT (SELECT max(1) FROM u) FROM t",
        "SELECT (SELECT count(*) FROM u WHERE t.a > 1) FROM t",
        "SELECT count(*) FROM t ORDER BY (SELECT max(t.a) FROM u)",
        "SELECT a, (SELECT count(t.a) FROM u), count(*) FROM t",
        "SELECT (SELECT x FROM (SELECT max(t.a) AS x)) FROM t",
        "SELECT (SELECT x FROM (SELECT max(t.a) AS x FROM u)) FROM t",
        "SELECT coalesce((SELECT max(t.a) FROM u), 'none') FROM t WHERE 0",
        "SELECT (SELECT max(t.a) FROM u), (SELECT min(t.a) FROM u) FROM t",
        "SELECT coalesce((SELECT (SELECT max(u.b) FROM e) FROM u), 'none') FROM t",
        "SELECT (SELECT max(t.a) FROM u) FROM t JOIN u ON (SELECT 1)",
        "SELECT (SELECT 1 FROM u GROUP BY max(t.a)) FROM t",
        "SELECT (SELECT count(*) FROM u GROUP BY b HAVING max(t.a) > 2) FROM t",
        "SELECT (SELECT sum(b) FROM u GROUP BY b HAVING max(t.a) > 2) FROM t",
        "SELECT a, (SELECT max(t.a) FROM u) FROM t GROUP BY a HAVING (SELECT min(t.z) FROM u) > 10",
        "SELECT coalesce((SELECT count(t.a) FROM e), 'none') FROM t",
        "SELECT (SELECT count(t.a) FROM u) FROM t GROUP BY a",
        "SELECT z, (SELECT max(t.a) FROM u) FROM t",
        "SELECT z, (SELECT min(t.a) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FROM u WHERE b = t.z) FROM t",
        "SELECT (SELECT max(t.a) FROM u AS t) FROM t",
        "SELECT (SELECT avg(t.a) * count(*) FROM u) FROM t",
        "SELECT (SELECT total(t.z) FROM u) FROM t",
        "SELECT (SELECT max(DISTINCT t.a) FROM u), (SELECT count(DISTINCT t.z/10) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FROM u) AS m FROM t ORDER BY m",
        "SELECT DISTINCT (SELECT max(t.a) FROM u) FROM t",
        "SELECT (SELECT max(t.a) FROM u) FROM t UNION ALL SELECT 9",
        "SELECT * FROM (SELECT (SELECT max(t.a) FROM u) AS q FROM t)",
        "SELECT (SELECT max(u.b) + t.a FROM u) FROM t",
        "SELECT (SELECT u.b FROM u WHERE u.b > (SELECT max(t.a) FROM e)) FROM t",
        "SELECT (SELECT max(t.a) FROM u LIMIT 0) FROM t",
        "SELECT a, count(*) FROM t",
        // An enclosing query's aggregate in the WHERE of an
        // aggregate subquery is allowed (of a plain one it is not).
        "SELECT (SELECT count(*) FROM u WHERE b < min(t.a)) FROM t",
        "SELECT (SELECT count(*) FROM u WHERE b < max(t.a) GROUP BY b) FROM t",
    ]);
}

#[test]
fn outer_aggregate_misuse_is_refused() {
    let tw = twins("aggmisuse", AGG);
    for sql in [
        "SELECT a FROM t WHERE a = (SELECT max(t.a) FROM u)",
        "SELECT * FROM t WHERE (SELECT max(t.a) FROM u) > 0",
        "SELECT 1 FROM t WHERE 3 IN (SELECT max(t.a) FROM u)",
        "SELECT (SELECT 1 FROM u WHERE b = max(t.a)) FROM t",
        "SELECT (SELECT max(t.a) FROM u WHERE max(t.a) > 0) FROM t",
        "SELECT sum((SELECT max(t.a) FROM u)) FROM t",
        "SELECT sum(max(a)) FROM t",
        "SELECT 1 FROM t GROUP BY (SELECT max(t.a) FROM u)",
        // An aggregate in ORDER BY alone (regression: hipp used
        // to aggregate the query to one row).
        "SELECT a FROM t ORDER BY max(a)",
        "SELECT a FROM t ORDER BY count(*)",
    ] {
        tw.refuse(sql, SchemaKind::MisusedAggregate);
    }
    tw.refuse_any("SELECT (SELECT 1 FROM u HAVING max(t.a) > 2) FROM t");
    tw.refuse_any("UPDATE t SET a = (SELECT max(t.a) FROM u)");
    tw.refuse_any("DELETE FROM t WHERE a = (SELECT max(t.a) FROM u)");
    tw.refuse_any("SELECT (SELECT sum((SELECT max(t.a) FROM e)) FROM u) FROM t");
    tw.finish("SELECT * FROM t;");
}

#[cfg(feature = "sql")]
#[test]
fn outer_aggregate_through_an_alias() {
    let tw = twins("aggalias", AGG);
    for sql in [
        "SELECT a AS z FROM t WHERE z = (SELECT max(z) FROM u)",
        "SELECT a AS y FROM t WHERE y = (SELECT max(y) FROM u)",
        "SELECT a AS y FROM t ORDER BY (SELECT max(y) FROM u)",
    ] {
        tw.refuse(sql, SchemaKind::MisusedAggregate);
    }
    tw.refuse(
        "SELECT a AS y, (SELECT max(y) FROM u) FROM t",
        SchemaKind::UnknownColumn,
    );
}

/// A qualifier that names an inner source lacking the column
/// falls through to the enclosing scopes, as on the pin
/// (regression: hipp answered "no such column").
#[test]
fn qualified_name_falls_through_to_outer_scope() {
    let tw = twins("qualfall", AGG);
    tw.all(&[
        "SELECT (SELECT t.a FROM u AS t) FROM t",
        "SELECT (SELECT max(t.a) FROM u AS t) FROM t",
        "SELECT t.b FROM u AS t",
    ]);
    tw.refuse(
        "SELECT (SELECT t.zz FROM u AS t) FROM t",
        SchemaKind::UnknownColumn,
    );
    tw.refuse("SELECT t.zz FROM t", SchemaKind::UnknownColumn);
}

/// Random tables, with outer aggregates in subqueries under
/// grouping, filtering and correlation.
#[test]
fn generated_aggregate_scope_corpus() {
    let mut rng = Rng(0xa66_0016_beef_cafe);
    for round in 0..4 {
        let mut setup = String::from("CREATE TABLE t(a, g); CREATE TABLE u(b);");
        for _ in 0..rng.below(6) {
            let a = ["NULL", "1", "2", "3", "5"][rng.below(5)];
            let g = rng.below(3);
            setup.push_str(&format!("INSERT INTO t VALUES ({a}, {g});"));
        }
        for _ in 0..rng.below(4) {
            setup.push_str(&format!("INSERT INTO u VALUES ({});", rng.below(6)));
        }
        let tw = twins(&format!("agggen{round}"), &setup);
        let aggs = [
            "max(t.a)",
            "min(t.a)",
            "count(t.a)",
            "sum(t.a)",
            "total(t.a)",
            "count(DISTINCT t.g)",
        ];
        for _ in 0..16 {
            let ag = rng.pick(&aggs);
            let inner = match rng.below(4) {
                0 => format!("(SELECT {ag} FROM u)"),
                1 => format!("(SELECT {ag} + b FROM u ORDER BY b)"),
                2 => format!("(SELECT count(*) FROM u WHERE b < {ag})"),
                _ => format!("(SELECT {ag} FROM u WHERE b > t.g)"),
            };
            let shape = match rng.below(3) {
                0 => format!("SELECT {inner} FROM t"),
                1 => format!("SELECT g, {inner} FROM t GROUP BY g ORDER BY g"),
                _ => format!("SELECT count(*), {inner} FROM t WHERE a NOT NULL"),
            };
            tw.same(&shape);
        }
    }
}

// ---------------- review fixes ----------------

/// A written row-value IN list applies the set's comparison
/// affinity (left operand + rightmost row) to both sides, the
/// rows' values included, as the pin's VALUES rewrite stores
/// them (regression: only the left operand was converted, so
/// `(1, 1) IN ((x, y), (a, b))` with INTEGER x, y and TEXT a, b
/// was 0 on hipp, 1 on the pin).
#[cfg(feature = "sql")]
#[test]
fn row_value_in_list_affinity_applies_to_both_sides() {
    let tw = twins(
        "rowinaff",
        "CREATE TABLE t(a TEXT, b TEXT); INSERT INTO t VALUES ('z','z');
         CREATE TABLE s(x INT, y INT); INSERT INTO s VALUES (1,1)",
    );
    tw.all(&[
        "SELECT (1, 1) IN ((x, y), (a, b)), (1, 1) IN ((x, y)), (1, 1) IN ((a, b), (x, y)) FROM t, s",
        "SELECT ('1', '1') IN ((x, y), (a, b)), ('1', 1) IN ((a, b), (x, y)) FROM t, s",
        "SELECT (1, 1) IN ((a, b), (x, y), (a, b)), (x, y) IN (('1', '1'), (a, b)) FROM t, s",
        "SELECT (a, b) IN ((x, y), ('z', 'z')), (x, 1) IN (('1', 1), (a, y)) FROM t, s",
    ]);
}

/// Generated tables with every affinity, written row-value IN
/// lists of mixed column and literal rows.
#[cfg(feature = "sql")]
#[test]
fn generated_row_value_in_list_affinity_corpus() {
    let mut rng = Rng(0x0016_1a1f_5e75_0b07);
    let vals = [
        "1", "'1'", "1.0", "'1.0'", "'a'", "NULL", "x'31'", "2", "' 1'",
    ];
    let mut setup =
        String::from("CREATE TABLE c(i INTEGER, t TEXT, r REAL, n NUMERIC, b BLOB, e);");
    for _ in 0..4 {
        let row: Vec<&str> = (0..6).map(|_| rng.pick(&vals)).collect();
        setup.push_str(&format!("INSERT INTO c VALUES ({});", row.join(", ")));
    }
    let tw = twins("rowinaffgen", &setup);
    let terms = [
        "i", "t", "r", "n", "b", "e", "1", "'1'", "1.0", "'a'", "NULL", "x'31'", "+i", "t || ''",
    ];
    let mut exprs = Vec::new();
    for _ in 0..300 {
        let lhs = format!("({}, {})", rng.pick(&terms), rng.pick(&terms));
        let rows: Vec<String> = (0..1 + rng.below(3))
            .map(|_| format!("({}, {})", rng.pick(&terms), rng.pick(&terms)))
            .collect();
        let not = if rng.below(4) == 0 { "NOT " } else { "" };
        exprs.push(format!("{lhs} {not}IN ({})", rows.join(", ")));
    }
    for chunk in exprs.chunks(10) {
        tw.same(&format!("SELECT {} FROM c", chunk.join(", ")));
    }
}

/// An aggregate directly inside another's arguments is misuse
/// whichever queries own them (regression: `(SELECT sum(c +
/// max(t.a)) FROM u)` returned a value; the pin refuses it at
/// prepare).
#[test]
fn outer_aggregate_inside_an_inner_aggregate_is_misuse() {
    let tw = twins(
        "aggnest",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (1,2),(3,4);
         CREATE TABLE u(c); INSERT INTO u VALUES (9),(10)",
    );
    for sql in [
        "SELECT (SELECT sum(c + max(t.a)) FROM u) FROM t",
        "SELECT (SELECT sum(c * max(t.a)) FROM u) FROM t",
        "SELECT (SELECT total(c) + sum(c + min(t.b)) FROM u) FROM t",
        "SELECT a, (SELECT sum(c + max(t.a)) FROM u) FROM t GROUP BY a",
        "SELECT (SELECT count(max(t.a)) FROM u) FROM t",
        "SELECT (SELECT max(max(t.a)) FROM u) FROM t",
        "SELECT a FROM t WHERE EXISTS (SELECT sum(c + max(t.a)) FROM u)",
        "SELECT sum(max(a)) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::MisusedAggregate);
    }
    tw.refuse_any("SELECT (SELECT sum((SELECT max(t.a))) FROM u) FROM t");
    tw.all(&[
        "SELECT (SELECT max(c) + max(t.a) FROM u) FROM t",
        "SELECT (SELECT sum(c) FROM u) + max(a) FROM t",
    ]);
}

/// The pin never evaluates an EXISTS subquery's result list or
/// ORDER BY (UNION / UNION ALL compounds included), so an
/// enclosing query's aggregate there is no misuse in WHERE / ON
/// / DELETE / UPDATE, and the EXISTS is not an aggregate query
/// (regression: hipp refused all of these; the base commit ran
/// them).
#[test]
fn exists_result_list_aggregates_are_inert() {
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES (1,2),(3,4);
         CREATE TABLE u(c); INSERT INTO u VALUES (9),(10); CREATE TABLE e(c)";
    let tw = twins("aggexists", setup);
    tw.all(&[
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u)",
        "SELECT a FROM t WHERE NOT EXISTS (SELECT max(t.a) FROM u)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u WHERE c > 9)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u WHERE c > 100)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM e)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u GROUP BY c)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) + 1 FROM u)",
        "SELECT a FROM t WHERE EXISTS (SELECT (SELECT max(t.a)) FROM u)",
        "SELECT a FROM t WHERE EXISTS (SELECT c FROM u ORDER BY max(t.a)) AND a > 1",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u) OR a > 5",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u LIMIT 0)",
        "SELECT a FROM t WHERE EXISTS (SELECT count(*), max(t.a) FROM u WHERE c > 100)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u UNION SELECT 5)",
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM e UNION ALL SELECT 5 FROM e)",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE EXISTS (SELECT max(t.a) FROM u))",
        "SELECT sum(a) FROM t WHERE EXISTS (SELECT max(t.a) FROM u)",
        "SELECT count(*) FROM t WHERE EXISTS (SELECT max(t.a) FROM u WHERE c > 100)",
        "SELECT a FROM t ORDER BY EXISTS (SELECT max(t.a) FROM u), a",
        "SELECT a FROM t GROUP BY a HAVING EXISTS (SELECT max(t.a) FROM u WHERE c > 100)",
        "SELECT a FROM t GROUP BY a HAVING EXISTS (SELECT max(t.a) FROM u)",
        "SELECT t.a FROM t JOIN u ON EXISTS (SELECT max(t.a) FROM e)",
        "SELECT t.a FROM t JOIN u ON EXISTS (SELECT max(t.a) FROM u) ORDER BY 1",
        "SELECT t.a FROM t LEFT JOIN u ON EXISTS (SELECT max(t.a) FROM e)",
        // In the result list it still makes t an aggregate query.
        "SELECT EXISTS (SELECT max(t.a) FROM u) FROM t",
        "SELECT a, EXISTS (SELECT max(t.a) FROM u) FROM t",
        "SELECT EXISTS (SELECT max(t.a) FROM u WHERE c > 100) FROM t",
        "SELECT a, EXISTS (SELECT max(t.a) FROM u INTERSECT SELECT 3) FROM t",
    ]);
    // Under INTERSECT / EXCEPT the values count: misuse.
    for sql in [
        "SELECT a FROM t WHERE EXISTS (SELECT max(t.a) FROM u EXCEPT SELECT 1)",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 INTERSECT SELECT max(t.a) FROM u)",
        "SELECT a FROM t WHERE EXISTS (SELECT c FROM u WHERE c > max(t.a))",
        "SELECT a FROM t WHERE EXISTS (SELECT c FROM u GROUP BY c HAVING max(t.a) > 0)",
    ] {
        tw.refuse_any(sql);
    }
    tw.same("UPDATE t SET b = 7 WHERE EXISTS (SELECT max(t.a) FROM u)");
    tw.same("UPDATE t SET b = EXISTS (SELECT max(t.a) FROM e) WHERE a = 1");
    tw.same("DELETE FROM t WHERE EXISTS (SELECT max(t.a) FROM e)");
    tw.finish("SELECT * FROM t ORDER BY a;");
    tw.same("DELETE FROM t WHERE EXISTS (SELECT max(t.a) FROM u)");
    tw.finish("SELECT count(*) FROM t;");
}

/// Aggregates in a RETURNING list's subqueries, and over an
/// upsert's `excluded` row, belong to the subquery: the pin's
/// owner search never stops at those rows (regression: hipp
/// refused them; the base commit ran them).
#[cfg(feature = "sql")]
#[test]
fn returning_and_excluded_never_own_an_aggregate() {
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES (1,10);
         CREATE TABLE u(c); INSERT INTO u VALUES (9),(10); CREATE TABLE e(c);
         CREATE UNIQUE INDEX ta ON t(a)";
    let tw = twins("aggret", setup);
    tw.all(&[
        "INSERT INTO t VALUES (9,9) RETURNING (SELECT max(t.a))",
        "INSERT INTO t VALUES (8,8) RETURNING (SELECT max(t.a) FROM u)",
        "INSERT INTO t VALUES (7,7) RETURNING (SELECT max(t.a) FROM e)",
        "INSERT INTO t VALUES (6,6) RETURNING (SELECT max(t.a) + count(*) FROM u)",
        "INSERT INTO t VALUES (5,5) RETURNING (SELECT (SELECT max(t.a) + max(c)) FROM u)",
        "INSERT INTO t VALUES (4,4) RETURNING (SELECT max(c) FROM u WHERE c > (SELECT max(t.a)))",
        "INSERT INTO t VALUES (3,3) RETURNING EXISTS (SELECT max(t.a) FROM e)",
        "UPDATE t SET b = 0 WHERE a < 3 RETURNING (SELECT max(t.a))",
        "DELETE FROM t WHERE a = 9 RETURNING (SELECT max(t.a) FROM u)",
        "INSERT INTO t VALUES (1,5) ON CONFLICT(a) DO UPDATE SET b = (SELECT max(excluded.b)) RETURNING b",
        "INSERT INTO t VALUES (1,6) ON CONFLICT(a) DO UPDATE SET b = (SELECT max(excluded.b) + count(*) FROM u) RETURNING b",
        "INSERT INTO t VALUES (1,7) ON CONFLICT(a) DO UPDATE SET b = 3 WHERE (SELECT max(excluded.a) FROM u) = 1 RETURNING b",
    ]);
    for sql in [
        "INSERT INTO t VALUES (20,20) RETURNING max(t.a)",
        "INSERT INTO t VALUES (21,21) RETURNING (SELECT count(*) FROM u WHERE c > max(t.a))",
        "INSERT INTO t VALUES (1,5) ON CONFLICT(a) DO UPDATE SET b = (SELECT max(t.b) + count(*) FROM u)",
    ] {
        tw.refuse_any(sql);
    }
    tw.finish("SELECT * FROM t ORDER BY a;");
}

/// A result-column alias naming an aggregate, used inside a
/// HAVING / ORDER BY subquery, is the alias's own query's
/// aggregate, folded per group (regression: refused; earlier
/// still, re-aggregated over the subquery's rows).
#[cfg(feature = "sql")]
#[test]
fn aggregate_alias_in_having_and_order_by_subqueries() {
    let tw = twins(
        "aggalias2",
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (1,10),(5,50),(3,30),(5,7)",
    );
    tw.all(&[
        "SELECT sum(a) AS s FROM t HAVING (SELECT s) > 0",
        "SELECT b, sum(a) AS s FROM t GROUP BY a HAVING (SELECT s) > 3 ORDER BY 1",
        "SELECT a, sum(b) AS s FROM t GROUP BY a HAVING (SELECT s + 1 FROM t WHERE b > 20) > 10",
        "SELECT a, sum(b) AS s FROM t GROUP BY a HAVING EXISTS (SELECT 1 FROM t AS x WHERE x.b = s)",
        "SELECT a, sum(b) AS s FROM t GROUP BY a ORDER BY (SELECT s) DESC",
        "SELECT a, sum(b) AS s FROM t GROUP BY a HAVING (SELECT sum(x.b) FROM t x WHERE x.b < s) > 5",
        "SELECT a, count(*) AS s FROM t GROUP BY a HAVING (SELECT (SELECT s)) > 1",
        "SELECT a, count(*) AS s FROM t GROUP BY a HAVING (SELECT s FROM t x) > 1",
        "SELECT a, max(b) AS s, b FROM t GROUP BY a HAVING (SELECT s) > 10",
        "SELECT a, max(b) AS s, b FROM t GROUP BY a ORDER BY (SELECT s)",
        "SELECT a, sum(b) + a AS s FROM t GROUP BY a HAVING (SELECT s) > 11",
        "SELECT a, sum(b) AS s FROM t GROUP BY a HAVING a IN (SELECT s - 9 FROM t)",
        "SELECT a, sum(b) AS s FROM t GROUP BY a HAVING (SELECT s WHERE s > 20) IS NOT NULL",
        "SELECT a, total(b) AS s FROM t GROUP BY a HAVING (SELECT s) BETWEEN 10 AND 40",
        "SELECT a, group_concat(b) AS g FROM t GROUP BY a HAVING (SELECT length(g)) > 2",
        "SELECT count(*) FROM (SELECT a, sum(b) AS s FROM t GROUP BY a HAVING (SELECT s) > 20)",
        "SELECT a AS s FROM t WHERE (SELECT s) > 2",
    ]);
    for sql in [
        "SELECT sum(a) AS s FROM t WHERE (SELECT s) > 0",
        "SELECT sum(a) AS s FROM t GROUP BY (SELECT s)",
        "SELECT a, sum(b) AS s FROM t GROUP BY a HAVING (SELECT max(s))",
    ] {
        tw.refuse_any(sql);
    }
}

/// A hex literal too big for 64 bits is the pin's error only
/// where a statement evaluates it: CREATE TABLE accepts one in
/// a DEFAULT or a CHECK, the file stays readable, and the INSERT
/// that needs the default / check fails (regression: the schema
/// parse refused it, so hipp could read nothing in such a file).
#[test]
fn too_big_hex_literal_waits_for_evaluation() {
    let path = common::temp_db("v16ex-bighex-pin");
    common::cli(
        &path,
        "CREATE TABLE t1(x); INSERT INTO t1 VALUES (1);
         CREATE TABLE t2(a DEFAULT 0x10000000000000000, b);
         CREATE TABLE t3(a CHECK (a <> 0x10000000000000000));
         CREATE TABLE t4(a DEFAULT -0x8000000000000000, b);",
    );
    block_on(async {
        let mut db = open(&path).await;
        assert_eq!(
            try_collect(&mut db, "SELECT * FROM t1").await.unwrap(),
            ["1"]
        );
        assert!(
            try_collect(&mut db, "SELECT * FROM t2")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            try_collect(&mut db, "INSERT INTO t2(b) VALUES (1)").await,
            Err(Error::Parse { .. })
        ));
        assert!(matches!(
            try_collect(&mut db, "INSERT INTO t3 VALUES (1)").await,
            Err(Error::Parse { .. })
        ));
        exec(&mut db, "INSERT INTO t2 VALUES (1, 2)").await;
        exec(&mut db, "INSERT INTO t4 VALUES (3, 4)").await;
    });
    // (The pin's integrity_check codes t3's CHECK, so it reports
    // the literal too; the rows read back.)
    assert_eq!(oracle_raw(&path, "SELECT * FROM t2, t4;").1, ["1|2|3|4"]);

    let tw = twins("bighex", "");
    tw.all(&[
        "CREATE TABLE d1(a DEFAULT 0x10000000000000000, b)",
        "CREATE TABLE d2(a DEFAULT (-0x8000000000000000), b)",
        "CREATE TABLE d3(a DEFAULT (0x10000000000000000 + 1), b)",
        "CREATE TABLE c1(a CHECK (a <> 0x10000000000000000))",
        "INSERT INTO d1 VALUES (1, 2)",
        "INSERT INTO d2 VALUES (1, 2)",
        "SELECT * FROM d1, d2, c1",
    ]);
    for sql in [
        "INSERT INTO d1(b) VALUES (1)",
        "INSERT INTO d2(b) VALUES (1)",
        "INSERT INTO d3(b) VALUES (1)",
        "INSERT INTO c1 VALUES (1)",
        "CREATE TABLE g1(a, b AS (0x10000000000000000))",
        "SELECT 0x10000000000000000 FROM d1 WHERE 0",
        "SELECT CASE WHEN 0 THEN 0x10000000000000000 END",
        "INSERT INTO d1 VALUES (0x10000000000000000, 1)",
        "UPDATE d1 SET a = -0x8000000000000000",
    ] {
        tw.refuse_any(sql);
    }
    // (The pin's integrity_check codes CHECKs, c1's literal too.)
    tw.same("DROP TABLE c1");
    tw.finish("SELECT * FROM d1, d2;");
}

/// A bare alias is never a reserved keyword, and `AS` needs a
/// name (regression: `SELECT 1 NOT` and `SELECT 1 AS` returned
/// 1; the pin gives a syntax error).
#[test]
fn reserved_words_are_no_bare_alias() {
    let tw = twins("alias-kw", "CREATE TABLE t(a); INSERT INTO t VALUES (1)");
    for sql in [
        "SELECT 1 NOT",
        "SELECT a NOT FROM t",
        "SELECT 1 NOT, 2",
        "SELECT a NOT, a FROM t",
        "SELECT 1 AS",
        "SELECT a AS, 2 FROM t",
        "SELECT a AS FROM t",
        "SELECT 1 AND",
        "SELECT 1 NULL",
        "SELECT a SELECT FROM t",
        "SELECT x.a FROM t x NOT",
        "SELECT x.a FROM t AS NOT",
    ] {
        tw.refuse_parse(sql);
    }
    // Words the pin's grammar lets fall back to an identifier
    // stay aliases.
    let words = [
        "ADD",
        "ALL",
        "ALTER",
        "AND",
        "AS",
        "AUTOINCREMENT",
        "BETWEEN",
        "CASE",
        "CHECK",
        "COLLATE",
        "COMMIT",
        "CONSTRAINT",
        "CREATE",
        "DEFAULT",
        "DEFERRABLE",
        "DELETE",
        "DISTINCT",
        "DROP",
        "ELSE",
        "ESCAPE",
        "EXISTS",
        "FOREIGN",
        "IN",
        "INDEX",
        "INSERT",
        "INTO",
        "IS",
        "ISNULL",
        "NOT",
        "NOTHING",
        "NOTNULL",
        "NULL",
        "OR",
        "PRIMARY",
        "REFERENCES",
        "SELECT",
        "SET",
        "TABLE",
        "THEN",
        "TO",
        "TRANSACTION",
        "UNIQUE",
        "UPDATE",
        "VALUES",
        "WHEN",
        "CROSS",
        "FULL",
        "INNER",
        "LEFT",
        "NATURAL",
        "OUTER",
        "RIGHT",
        "INDEXED",
        "KEY",
        "ROW",
        "ROWS",
        "NO",
        "DO",
        "END",
        "BY",
        "TEMP",
        "VIEW",
        "FIRST",
        "LAST",
        "REPLACE",
        "IF",
    ];
    for w in words {
        for sql in [
            format!("SELECT 1 {w}"),
            format!("SELECT a {w} FROM t"),
            format!("SELECT a {w}, 2 FROM t"),
            format!("SELECT x.a FROM t x {w}"),
        ] {
            tw.same(&sql);
        }
    }
}

/// A subquery used as an expression (scalar value or IN set)
/// takes its comparison affinity from the RIGHTMOST compound
/// core, a one-row VALUES from its row, a longer one from the
/// pin's co-routine / UNION ALL rule, and a nested scalar
/// subquery from its own result (regression: hipp read the
/// leftmost core and gave scalar subqueries and VALUES rows no
/// affinity, so `'1' IN (SELECT 1 UNION SELECT i FROM c)` over
/// INTEGER i was 0, the pin 1).
#[cfg(feature = "sql")]
#[test]
fn subquery_affinity_comes_from_the_rightmost_core() {
    let tw = twins(
        "subaff",
        "CREATE TABLE c(i INTEGER, t TEXT); INSERT INTO c VALUES (1, '1')",
    );
    tw.all(&[
        "SELECT '1' IN (SELECT i FROM c UNION SELECT 1), '1' IN (SELECT 1 UNION SELECT i FROM c)",
        "SELECT '1' IN (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT i FROM c)",
        "SELECT '1' IN (SELECT i FROM c UNION ALL SELECT 2 UNION ALL SELECT 1)",
        "SELECT 1 IN (SELECT t FROM c UNION SELECT 2), 1 IN (SELECT 2 UNION SELECT t FROM c)",
        "SELECT '1' IN (VALUES (1), ((SELECT i FROM c))), '1' IN (VALUES ((SELECT i FROM c)), (1))",
        "SELECT '1' IN (VALUES (CAST(1 AS INTEGER))), '1' IN (VALUES (CAST(1 AS INTEGER)), (2))",
        "SELECT '1' IN (VALUES (2), (CAST(1 AS INTEGER))), 1 IN (VALUES (1), ('1'))",
        "SELECT '1' IN (VALUES (i)), '1' IN (VALUES (i), (2)), '1' IN (VALUES (2), (i)) FROM c",
        "SELECT (SELECT i FROM c UNION SELECT 1) = '1', (SELECT 1 UNION SELECT i FROM c) = '1'",
        "SELECT (SELECT (SELECT i FROM c)) = '1', '1' IN (SELECT (SELECT i FROM c))",
        "SELECT x = '1' FROM (SELECT (SELECT i FROM c) AS x)",
        "SELECT (SELECT x FROM (SELECT (SELECT i FROM c) AS x)) = '1'",
        "SELECT x = '1' FROM (SELECT (SELECT i FROM c) AS x UNION SELECT 2)",
        "SELECT '1' IN (SELECT 1 INTERSECT SELECT i FROM c), '1' IN (SELECT i FROM c INTERSECT SELECT 1)",
        "SELECT '1' = (SELECT 2 EXCEPT SELECT i FROM c), (SELECT 1 UNION SELECT i FROM c) IN ('1')",
    ]);
    // Generated: every affinity and literal kind in compound
    // and VALUES sets.
    let tw = twins(
        "subaffgen",
        "CREATE TABLE c(i INTEGER, t TEXT, r REAL, n NUMERIC, b BLOB, e);
         INSERT INTO c VALUES (1,'1',1,'1',1,'1'),('1',1,'1.0',1.0,'1',1),('a',2,'x',NULL,x'31','a')",
    );
    let terms = [
        "i", "t", "r", "n", "b", "e", "1", "'1'", "1.0", "'a'", "NULL", "+i",
    ];
    let mut rng = Rng(0x0016_5ab0_aff1_7e57);
    for _ in 0..120 {
        let l = rng.pick(&terms);
        let op = [" UNION ", " UNION ALL "][rng.below(2)];
        let cores: Vec<String> = (0..1 + rng.below(3))
            .map(|_| format!("SELECT {} FROM c", rng.pick(&terms)))
            .collect();
        let (v1, v2) = (rng.pick(&terms), rng.pick(&terms));
        tw.same(&format!(
            "SELECT {l} IN ({}), {l} IN (VALUES ({v1}), ({v2})), (SELECT {v1} UNION SELECT {v2}) = {l} FROM c",
            cores.join(op)
        ));
    }
}

/// The row-value forms of the rightmost-core rule.
#[cfg(feature = "sql")]
#[test]
fn row_value_subquery_affinity_comes_from_the_rightmost_core() {
    let tw = twins(
        "rowsubaff",
        "CREATE TABLE c(i INTEGER, t TEXT); INSERT INTO c VALUES (1, '1')",
    );
    tw.all(&[
        "SELECT ('1',1) IN (VALUES (i,1),(i,1)), ('1',1) IN (VALUES (i,1)), ('1',1) IN ((i,1),(i,1)) FROM c",
        "SELECT ('1',1) IN (SELECT 1,1 UNION SELECT i,1 FROM c), ('1',1) IN (SELECT i,1 FROM c UNION SELECT 1,1) FROM c",
        "SELECT (SELECT 1, 1 UNION SELECT i, t FROM c) = ('1', 1), (t, i) IN (SELECT 1, '1' UNION SELECT t, i FROM c) FROM c",
    ]);
}

/// `x IN ((SELECT ...), ...)` is a list whose first item is a
/// scalar subquery (regression: a syntax error on hipp).
#[cfg(feature = "sql")]
#[test]
fn in_list_led_by_a_scalar_subquery() {
    let tw = twins("inlistsub", "");
    tw.all(&[
        "SELECT 1 IN ((SELECT 1), 2), 1 IN ((SELECT 2) + -1), 1 IN ((SELECT 1)), 1 IN (((SELECT 1)))",
        "SELECT 1 IN (((SELECT 1)), 3), 1 IN ((SELECT 1 UNION SELECT 2)), 2 IN ((VALUES (2)), 5)",
        "SELECT 3 NOT IN ((SELECT 1), (SELECT 2)), NULL IN ((SELECT 1), 1)",
    ]);
    for sql in [
        "SELECT 1 IN ((SELECT 1) UNION SELECT 2)",
        "SELECT 1 IN ((SELECT 1)",
        "SELECT 1 IN ((SELECT 1),)",
    ] {
        tw.refuse_any(sql);
    }
}

/// Comparison affinity over every affinity and literal kind,
/// scalar subqueries included: the pin applies the chosen
/// affinity to both operands (`adjust_cmp`).
#[test]
fn generated_comparison_affinity_corpus() {
    let tw = twins(
        "cmpaffgen",
        "CREATE TABLE c(i INTEGER, t TEXT, r REAL, n NUMERIC, b BLOB, e);
         INSERT INTO c VALUES (1,'1',1,'1',1,'1'),('1',1,'1.0',1.0,'1',1),('a',2,'x',NULL,x'31','a'),
           (2.5,'2.5',2.5,'2.5','2.5',2.5)",
    );
    let terms = [
        "i",
        "t",
        "r",
        "n",
        "b",
        "e",
        "1",
        "'1'",
        "1.0",
        "'1.0'",
        "'a'",
        "NULL",
        "+i",
        "t || ''",
        "CAST(t AS INTEGER)",
        "CAST(i AS TEXT)",
        "(SELECT t FROM c LIMIT 1)",
        "(SELECT i FROM c LIMIT 1)",
        "(SELECT n FROM c WHERE n > 1)",
        "(SELECT (SELECT t FROM c LIMIT 1))",
        "'2.5'",
        "2.5",
        "x'31'",
    ];
    let ops = ["=", "<", ">=", "<>", "IS"];
    let mut rng = Rng(0x0016_c0a1_5ce0_0001);
    for _ in 0..100 {
        let mut xs = Vec::new();
        for _ in 0..6 {
            let (a, b) = (rng.pick(&terms), rng.pick(&terms));
            if rng.below(6) == 0 {
                xs.push(format!("{a} IN ({b}, {})", rng.pick(&terms)));
            } else {
                xs.push(format!("{a} {} {b}", rng.pick(&ops)));
            }
        }
        tw.same(&format!("SELECT {} FROM c", xs.join(", ")));
    }
}

// ---------------- AND / OR constant operands (ungated) ----------------

/// The pin simplifies `x AND 0` / `x OR 1` (skipping `x`) only when
/// the constant is a lone integer token of a 32-bit value (decimal
/// or hex, any parentheses) or TRUE / FALSE: `-1`, `+0`, `-0`,
/// `1_0`, `2147483648`, `0x80000000` and `1+0` leave `x` evaluated
/// (v0.16 review fix: hipp skipped it for any integer literal its
/// parser folded, so `-1 OR abs(-9223372036854775808)` was 1).
#[test]
fn and_or_constant_operands() {
    let tw = twins("andor", "");
    let boom = "abs(-9223372036854775808)";
    let consts = [
        "0",
        "1",
        "7",
        "(0)",
        "((1))",
        "0x0",
        "0x7fffffff",
        "0x80000000",
        "2147483647",
        "2147483648",
        "00000000000002",
        "+0",
        "-1",
        "-0",
        "1_0",
        "0.0",
        "1+0",
        "true",
        "false",
        "(0 AND 1)",
        "(1 OR 0)",
        "CAST(0 AS INT)",
        "'0'",
    ];
    for c in consts {
        for sql in [
            format!("SELECT {c} AND {boom}"),
            format!("SELECT {boom} AND {c}"),
            format!("SELECT {c} OR {boom}"),
            format!("SELECT {boom} OR {c}"),
        ] {
            tw.same(&sql);
        }
    }
    // Generated nests of constants, columns-free.
    let mut rng = Rng(0xa2d0_0016_0b5e_eed1);
    for _ in 0..60 {
        let mut e = rng.pick(&consts).to_string();
        for _ in 0..1 + rng.below(3) {
            let op = rng.pick(&["AND", "OR"]);
            let other = if rng.below(2) == 0 {
                boom.to_string()
            } else {
                rng.pick(&consts).to_string()
            };
            e = if rng.below(2) == 0 {
                format!("({e}) {op} {other}")
            } else {
                format!("{other} {op} ({e})")
            };
        }
        tw.same(&format!("SELECT {e}"));
    }
    tw.all(&["SELECT -1 OR 0, +0 AND 1, 1_0 OR 0, typeof(+5 AND 1), 1 AND 7, 7 OR 0"]);
}

/// `RETURNING t.*` is the pin's error (v0.16 review fix: hipp
/// expanded it).
#[test]
fn returning_table_star_is_refused() {
    let tw = twins("retstar", "CREATE TABLE t(a)");
    tw.refuse(
        "INSERT INTO t VALUES (1) RETURNING t.*",
        SchemaKind::ReturningTableStar,
    );
    tw.refuse(
        "UPDATE t SET a = 2 RETURNING x.*",
        SchemaKind::ReturningTableStar,
    );
    tw.all(&["INSERT INTO t VALUES (3) RETURNING *", "SELECT * FROM t"]);
    tw.finish("SELECT * FROM t;");
}
