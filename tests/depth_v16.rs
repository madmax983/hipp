//! v0.16 parser and evaluator depth limits, differentially vs the
//! pinned oracle (SQLite 3.54.1), and proof that no statement
//! overflows the stack.
//!
//! Probed on the pin before implementation (STATUS.md has the
//! notes): the expression-tree limit counts the pin's
//! `Expr.nHeight` (1000; a leaf is 1, parentheses add nothing, `+`
//! over a `+` node re-uses it, `x ISNULL` over a literal folds to
//! one), and name resolution adds the heights of the expressions a
//! subquery sits in (43 nested scalar subqueries read, 44 do not);
//! the parser stack holds 2500 LALR symbols (2493 nested
//! parentheses in a result column, 831 nested calls, 498 nested
//! CASEs, 414 nested FROM subqueries); a compound takes 500 SELECTs,
//! a call 1000 arguments, a table / index / result set / SET list
//! 2000 columns, an ORDER BY / GROUP BY 2000 terms, a join 64
//! tables.
//!
//! Every boundary runs on a thread of a fixed, modest stack: a
//! stack overflow aborts the test binary, so a passing run is the
//! proof. The tests at the pin's own limits (what the CLI uses) get
//! the stack those need; the others run the limits `Config` derives
//! for a small stack (`Config::stack_budget`) and the defaults.

mod common;

use hipp::storage::{MemStorage, block_on};
use hipp::{Config, Connection, Error, Step, Value};

/// What one statement did: its rows (`|`-separated columns, one
/// line each), or the class of its error.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Out {
    Rows(String),
    Err(String),
}

/// The class of a pin error message (each verified on the pin).
fn pin_class(msg: &str) -> String {
    const KINDS: [(&str, &str); 12] = [
        ("Expression tree is too large", "ExprTooDeep"),
        ("Recursion limit", "RecursionLimit"),
        ("too many terms in compound SELECT", "TooManyCompoundTerms"),
        ("too many arguments on function", "TooManyArguments"),
        ("too many columns", "TooManyColumns"),
        ("too many terms in", "TooManyTerms"),
        ("at most 64 tables in a join", "TooManyTables"),
        ("variable number must be", "VariableNumber"),
        ("LIKE or GLOB pattern too complex", "PatternTooComplex"),
        ("term out of range", "TermOutOfRange"),
        ("no such column", "UnknownColumn"),
        ("row value misused", "RowValueMisused"),
    ];
    KINDS
        .iter()
        .find(|(m, _)| msg.contains(m))
        .map_or_else(|| format!("other: {msg}"), |(_, k)| (*k).to_string())
}

/// The class of a hipp error, named as `pin_class` names the pin's.
fn hipp_class<E: core::fmt::Debug>(e: &Error<E>) -> String {
    match e {
        Error::Parse { kind, .. } => format!("{kind:?}"),
        Error::Schema { kind } => format!("{kind:?}"),
        other => format!("other: {other:?}"),
    }
}

fn render(v: Option<Value<'_>>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Integer(i)) => i.to_string(),
        Some(Value::Real(f)) => format!("{f:?}"),
        Some(Value::Text(b)) | Some(Value::Blob(b)) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Run `pre` (each statement must succeed) then `sql` on a fresh
/// in-memory database under `cfg`.
fn hipp(cfg: Config, pre: &str, sql: &str) -> Out {
    block_on(async move {
        let mut db = Connection::open(MemStorage::new(), cfg)
            .await
            .expect("open");
        for s in pre.split(';').filter(|s| !s.trim().is_empty()) {
            let mut st = db.prepare(s).await.expect("pre prepares");
            while st.step().await.expect("pre steps") == Step::Row {}
        }
        let mut st = match db.prepare(sql).await {
            Ok(st) => st,
            Err(e) => return Out::Err(hipp_class(&e)),
        };
        let mut rows = Vec::new();
        loop {
            match st.step().await {
                Ok(Step::Row) => {
                    let n = st.column_count();
                    let line: Vec<String> = (0..n).map(|i| render(st.column_value(i))).collect();
                    rows.push(line.join("|"));
                }
                Ok(Step::Done) => break,
                Err(e) => return Out::Err(hipp_class(&e)),
            }
        }
        Out::Rows(rows.join("\n"))
    })
}

/// The pin's answer to `pre` then `sql`.
fn pin(pre: &str, sql: &str) -> Out {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(":memory:")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    let script = format!("{pre};\n{sql};\n");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("oracle finishes");
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        return Out::Err(pin_class(&err));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Out::Rows(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

/// Run `f` on a thread of `bytes` of stack. An overflow aborts the
/// whole test binary: returning at all is the "no crash" proof.
fn on_stack<T: Send + 'static>(bytes: usize, f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(bytes)
        .spawn(f)
        .expect("thread spawns")
        .join()
        .expect("no panic")
}

/// The pin's limits, with a statement arena for statements of
/// thousands of tokens (the default 64 KiB holds about 2500).
fn pin_cfg() -> Config {
    Config::default()
        .depth_limits(
            hipp::sql::parser::PIN_EXPR_DEPTH,
            hipp::sql::parser::PIN_PARSER_DEPTH,
        )
        .statement_arena(16 * 1024, 64 * 1024 * 1024)
}

/// Enough stack for the pin's limits in a debug build (about 13
/// MiB is the worst measured; only touched pages are committed).
const PIN_STACK: usize = 256 * 1024 * 1024;

const T: &str = "CREATE TABLE t(a, b, c); INSERT INTO t VALUES (1, 2, 3)";

fn rep(s: &str, n: usize) -> String {
    s.repeat(n)
}

/// One nesting shape: a statement of depth `n`, and the pin's
/// largest accepted `n` (probed; each test also checks it live).
struct Shape {
    name: &'static str,
    f: fn(usize) -> String,
    pin_max: usize,
}

fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            name: "parentheses",
            f: |n| format!("SELECT {}a{} FROM t", rep("(", n), rep(")", n)),
            pin_max: 2493,
        },
        Shape {
            name: "unary plus (re-used node)",
            f: |n| format!("SELECT {}1", rep("+", n)),
            pin_max: 2494,
        },
        Shape {
            name: "unary minus",
            f: |n| format!("SELECT {}a FROM t", rep("- ", n)),
            pin_max: 999,
        },
        Shape {
            name: "signed literal",
            f: |n| format!("SELECT {}1", rep("- ", n)),
            pin_max: 999,
        },
        Shape {
            name: "bitwise not",
            f: |n| format!("SELECT {}a FROM t", rep("~", n)),
            pin_max: 999,
        },
        Shape {
            name: "NOT",
            f: |n| format!("SELECT {}a FROM t", rep("NOT ", n)),
            pin_max: 999,
        },
        Shape {
            name: "left-deep +",
            f: |n| format!("SELECT a{} FROM t", rep("+a", n)),
            pin_max: 999,
        },
        Shape {
            name: "left-deep AND",
            f: |n| format!("SELECT 1{} FROM t", rep(" AND a", n)),
            pin_max: 999,
        },
        Shape {
            name: "right-deep + in parentheses",
            f: |n| format!("SELECT {}a{} FROM t", rep("a+(", n), rep(")", n)),
            pin_max: 831,
        },
        Shape {
            name: "nested CASE",
            f: |n| {
                format!(
                    "SELECT {}a{} FROM t",
                    rep("CASE WHEN a THEN ", n),
                    rep(" END", n)
                )
            },
            pin_max: 498,
        },
        Shape {
            name: "nested calls",
            f: |n| format!("SELECT {}a{} FROM t", rep("abs(", n), rep(")", n)),
            pin_max: 831,
        },
        Shape {
            name: "nested BETWEEN",
            f: |n| {
                format!(
                    "SELECT {}a{} FROM t",
                    rep("a BETWEEN 0 AND (", n),
                    rep(")", n)
                )
            },
            pin_max: 498,
        },
        Shape {
            name: "COLLATE chain",
            f: |n| format!("SELECT 'a'{}", rep(" COLLATE nocase", n)),
            pin_max: 999,
        },
        Shape {
            name: "nested CAST",
            f: |n| format!("SELECT {}a{} FROM t", rep("CAST(", n), rep(" AS INT)", n)),
            pin_max: 999,
        },
        Shape {
            name: "ISNULL chain over a column",
            f: |n| format!("SELECT a{} FROM t", rep(" ISNULL", n)),
            pin_max: 999,
        },
        Shape {
            name: "scalar subqueries (resolution sum)",
            f: |n| format!("SELECT {}1{}", rep("(SELECT ", n), rep(")", n)),
            pin_max: 43,
        },
        Shape {
            name: "EXISTS",
            f: |n| format!("SELECT {}1{}", rep("EXISTS (SELECT ", n), rep(")", n)),
            pin_max: 43,
        },
        Shape {
            name: "IN subqueries",
            f: |n| format!("SELECT 1 IN {}(1){}", rep("(SELECT 1 IN ", n), rep(")", n)),
            pin_max: 41,
        },
        Shape {
            name: "FROM subqueries",
            f: |n| {
                format!(
                    "SELECT * FROM {}(SELECT 1){}",
                    rep("(SELECT * FROM ", n),
                    rep(")", n)
                )
            },
            pin_max: 414,
        },
        Shape {
            name: "parenthesised FROM subquery",
            f: |n| format!("SELECT * FROM {}SELECT 1{}", rep("(", n), rep(")", n)),
            pin_max: 1243,
        },
        Shape {
            name: "WHERE chain",
            f: |n| format!("SELECT 1 FROM t WHERE a{}", rep("+a", n)),
            pin_max: 999,
        },
        Shape {
            name: "UPDATE SET",
            f: |n| format!("UPDATE t SET a = {}a{}", rep("(", n), rep(")", n)),
            pin_max: 2489,
        },
        Shape {
            name: "INSERT VALUES",
            f: |n| format!("INSERT INTO t VALUES (1, 2, {}1)", rep("+", n)),
            pin_max: 2489,
        },
        Shape {
            name: "INSERT VALUES signs",
            f: |n| format!("INSERT INTO t VALUES (1, 2, {}1)", rep("- ", n)),
            pin_max: 999,
        },
        Shape {
            name: "DELETE WHERE",
            f: |n| format!("DELETE FROM t WHERE {}a{}", rep("(", n), rep(")", n)),
            pin_max: 2491,
        },
        Shape {
            name: "ORDER BY",
            f: |n| format!("SELECT a FROM t ORDER BY a{}", rep("+a", n)),
            pin_max: 999,
        },
        Shape {
            name: "CTE body",
            f: |n| format!("WITH q AS (SELECT {}1) SELECT * FROM q", rep("+", n)),
            pin_max: 2489,
        },
        Shape {
            name: "compound member",
            f: |n| format!("SELECT 1 UNION SELECT {}1", rep("+", n)),
            pin_max: 2492,
        },
        Shape {
            name: "UNION result column (+1 for the sorter)",
            f: |n| format!("SELECT 1 UNION SELECT {}'x'", rep("NOT ", n)),
            pin_max: 998,
        },
        Shape {
            name: "UNION ALL result column",
            f: |n| format!("SELECT 1 UNION ALL SELECT {}'x'", rep("NOT ", n)),
            pin_max: 999,
        },
        Shape {
            name: "ON clause",
            f: |n| format!("SELECT 1 FROM t JOIN t AS u ON {}1", rep("+", n)),
            pin_max: 2489,
        },
        Shape {
            name: "function arguments",
            f: |n| format!("SELECT coalesce(1, {}1)", rep("+", n)),
            pin_max: 2489,
        },
        Shape {
            name: "FILTER",
            f: |n| format!("SELECT count(a) FILTER (WHERE {}1) FROM t", rep("+", n)),
            pin_max: 2486,
        },
        Shape {
            name: "LIMIT",
            f: |n| format!("SELECT a FROM t LIMIT {}1", rep("+", n)),
            pin_max: 2489,
        },
        Shape {
            name: "row value element",
            f: |n| format!("SELECT (1, {}1) = (1, 1)", rep("+", n)),
            pin_max: 2491,
        },
        Shape {
            name: "IN list item",
            f: |n| format!("SELECT 1 IN (1, {}1)", rep("+", n)),
            pin_max: 2489,
        },
    ]
}

/// The largest `n` in `0..=hi` for which `ok(n)` (monotone).
fn boundary(hi: usize, ok: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, hi);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if ok(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// The limit errors this unit adds: both engines must agree on
/// which one (other errors are compared by presence: hipp's syntax
/// error classes are not the pin's messages).
const LIMIT_CLASSES: [&str; 9] = [
    "ExprTooDeep",
    "RecursionLimit",
    "TooManyCompoundTerms",
    "TooManyArguments",
    "TooManyColumns",
    "TooManyTerms",
    "TooManyTables",
    "PatternTooComplex",
    "VariableNumber",
];

/// `got` (hipp) agrees with `want` (the pin).
fn agrees(got: &Out, want: &Out) -> bool {
    match (got, want) {
        (Out::Rows(a), Out::Rows(b)) => a == b,
        (Out::Err(a), Out::Err(b)) => {
            a == b || !(LIMIT_CLASSES.contains(&a.as_str()) || LIMIT_CLASSES.contains(&b.as_str()))
        }
        _ => false,
    }
}

fn accepted(o: &Out) -> bool {
    matches!(o, Out::Rows(_))
}

/// At the pin's limits hipp agrees with the pin on both sides of
/// every boundary — the answer below, the error class above — and
/// the probed boundaries still hold on the live pin.
#[test]
fn pin_limits_match_the_oracle_at_every_boundary() {
    for s in shapes() {
        for n in [s.pin_max, s.pin_max + 1] {
            let sql = (s.f)(n);
            let want = pin(T, &sql);
            assert_eq!(
                accepted(&want),
                n == s.pin_max,
                "{}: the pin's boundary moved ({n})",
                s.name
            );
            let got = on_stack(PIN_STACK, {
                let sql = sql.clone();
                move || hipp(pin_cfg(), T, &sql)
            });
            assert_eq!(got, want, "{} at depth {n}", s.name);
        }
    }
}

/// Any limits refuse cleanly: on a thread of the stack they were
/// derived for (`Config::stack_budget`), each shape is accepted up
/// to hipp's boundary — with the pin's answer — and refused with a
/// depth error past it, never an overflow.
fn budgeted_run(stack: usize, cfg: fn() -> Config) {
    for s in shapes() {
        let run = |n: usize| {
            let sql = (s.f)(n);
            on_stack(stack, move || hipp(cfg(), T, &sql))
        };
        let b = boundary(s.pin_max + 1, |n| accepted(&run(n)));
        assert!(b <= s.pin_max, "{}: hipp accepts past the pin", s.name);
        let at = run(b);
        assert_eq!(at, pin(T, &(s.f)(b)), "{} at hipp's boundary {b}", s.name);
        match run(b + 1) {
            Out::Err(k) => assert!(
                k == "ExprTooDeep" || k == "RecursionLimit",
                "{} past hipp's boundary: {k}",
                s.name
            ),
            Out::Rows(r) => panic!("{} accepted past its boundary: {r}", s.name),
        }
        // Far past every limit, too (one statement 50 times deeper).
        let deep = (s.f)(s.pin_max * 50 + 100);
        assert!(
            matches!(on_stack(stack, move || hipp(cfg(), T, &deep)), Out::Err(_)),
            "{}: a far deeper statement",
            s.name
        );
    }
}

/// The small thread: 512 KiB in a debug build (hipp's own call path
/// needs about 336 KiB there before any nesting), 256 KiB in a
/// release build.
const SMALL_STACK: usize = if cfg!(debug_assertions) {
    512 * 1024
} else {
    256 * 1024
};

#[test]
fn budgeted_limits_on_a_small_stack_refuse_cleanly() {
    budgeted_run(SMALL_STACK, || {
        Config::default()
            .stack_budget(SMALL_STACK)
            .statement_arena(16 * 1024, 64 * 1024 * 1024)
    });
}

/// `Config::default()`'s limits (the pin's 1000-deep expressions, a
/// 500-symbol parser stack) on an 8 MiB thread, a debug build's
/// worst case being about 5.3 MiB.
#[test]
fn default_limits_on_an_8_mib_thread_refuse_cleanly() {
    budgeted_run(8 * 1024 * 1024, || {
        Config::default().statement_arena(16 * 1024, 64 * 1024 * 1024)
    });
}

/// The limits `stack_budget` derives grow with the stack and stop
/// at the pin's.
#[test]
fn stack_budget_is_monotone_and_capped() {
    let mut last = (0, 0);
    for kib in [64usize, 256, 512, 1024, 4096, 16384, 1 << 20] {
        let c = Config::default().stack_budget(kib * 1024);
        let now = (c.max_expr_depth, c.max_parser_depth);
        assert!(now.0 >= last.0 && now.1 >= last.1, "{kib} KiB");
        assert!(now.0 <= 1000 && now.1 <= 2500);
        last = now;
    }
    assert_eq!(last, (1000, 2500));
}

/// The count limits, each on both sides of its boundary.
#[test]
fn count_limits_match_the_oracle() {
    let cols = |n: usize| {
        (0..n)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let ones = |n: usize| vec!["1"; n].join(",");
    let wide = format!("CREATE TABLE w({})", cols(1000));
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, String, Box<dyn Fn(usize) -> String>, usize)> = vec![
        (
            "compound SELECT",
            String::new(),
            Box::new(|n| format!("SELECT 1{}", " UNION ALL SELECT 1".repeat(n - 1))),
            500,
        ),
        (
            "compound UNION",
            String::new(),
            Box::new(|n| format!("SELECT 1{}", " UNION SELECT 1".repeat(n - 1))),
            500,
        ),
        (
            "call arguments",
            String::new(),
            Box::new(move |n| format!("SELECT max({})", ones(n))),
            1000,
        ),
        (
            "char() arguments",
            String::new(),
            Box::new(|n| format!("SELECT length(char({}))", vec!["65"; n].join(","))),
            1000,
        ),
        (
            "result columns",
            String::new(),
            Box::new(move |n| format!("SELECT {}", ones(n))),
            2000,
        ),
        (
            "VALUES width",
            String::new(),
            Box::new(move |n| format!("SELECT count(*) FROM (VALUES ({}))", ones(n))),
            2000,
        ),
        (
            "table columns",
            String::new(),
            Box::new(move |n| format!("CREATE TABLE z({})", cols(n))),
            2000,
        ),
        (
            "index columns",
            T.to_string(),
            Box::new(|n| format!("CREATE INDEX i ON t({})", vec!["a"; n].join(","))),
            2000,
        ),
        (
            "SET list",
            T.to_string(),
            Box::new(|n| format!("UPDATE t SET {}", vec!["a = 1"; n].join(","))),
            2000,
        ),
        (
            "ORDER BY terms",
            T.to_string(),
            Box::new(|n| format!("SELECT a FROM t ORDER BY {}", vec!["a"; n].join(","))),
            2000,
        ),
        (
            "GROUP BY terms",
            T.to_string(),
            Box::new(|n| format!("SELECT a FROM t GROUP BY {}", vec!["a"; n].join(","))),
            2000,
        ),
        (
            "joined tables",
            T.to_string(),
            Box::new(|n| format!("SELECT count(*) FROM {}", vec!["t"; n].join(","))),
            64,
        ),
        (
            "UPDATE ... FROM tables",
            T.to_string(),
            Box::new(|n| {
                let src: Vec<String> = (1..n).map(|i| format!("t AS x{i}")).collect();
                format!("UPDATE t SET a = 1 FROM {}", src.join(" JOIN "))
            }),
            64,
        ),
        (
            "result set after *",
            wide.clone(),
            Box::new(|n| {
                let items: Vec<String> = (0..n - 1000).map(|i| format!("1 AS x{i}")).collect();
                format!(
                    "SELECT count(*) FROM (SELECT * FROM w, (SELECT {}))",
                    items.join(",")
                )
            }),
            2000,
        ),
        (
            "ADD COLUMN",
            String::new(),
            Box::new(move |n| {
                format!(
                    "CREATE TABLE z({}); ALTER TABLE z ADD COLUMN y",
                    cols(n - 1)
                )
            }),
            2000,
        ),
        (
            "bound parameter number",
            String::new(),
            Box::new(|n| format!("SELECT ?{n}")),
            32766,
        ),
        (
            "LIKE pattern length",
            String::new(),
            Box::new(|n| format!("SELECT 'a' LIKE '{}'", "a".repeat(n))),
            50000,
        ),
        (
            "GLOB pattern length",
            String::new(),
            Box::new(|n| format!("SELECT 'a' GLOB '{}'", "a".repeat(n))),
            50000,
        ),
    ];
    for (name, pre, f, max) in cases.iter() {
        for n in [*max, max + 1] {
            let sql = f(n);
            // A statement pair (ADD COLUMN): the setup runs first.
            let (setup, last) = match sql.rsplit_once("; ") {
                Some((a, b)) => (format!("{pre};{a}"), b.to_string()),
                None => (pre.clone(), sql.clone()),
            };
            let want = pin(&setup, &last);
            assert_eq!(
                accepted(&want),
                n == *max,
                "{name}: the pin's boundary moved"
            );
            let got = on_stack(PIN_STACK, move || hipp(pin_cfg(), &setup, &last));
            assert_eq!(got, want, "{name} at {n}");
        }
    }
}

/// A deterministic PRNG (xorshift64*), as in tests/fuzz_smoke.rs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// A generated corpus: a random wrapper (or two, alternating)
/// nested `n` deep around a random operand in a random statement
/// context. For each case the pin's boundary is searched live and
/// hipp must agree on it — accepted (with the same answer) at it,
/// refused (with the same error class) past it. `HIPP_DEPTH_CORPUS`
/// sets the number of cases (default 24).
#[test]
fn generated_nesting_corpus_matches_the_oracle() {
    const W: [(&str, &str); 28] = [
        ("(", ")"),
        ("- ", ""),
        ("NOT ", ""),
        ("~", ""),
        ("+", ""),
        ("a+(", ")"),
        ("abs(", ")"),
        ("coalesce(a,", ")"),
        ("CASE WHEN a THEN ", " END"),
        ("CASE a WHEN ", " THEN 1 END"),
        ("CASE WHEN 1 THEN 1 ELSE ", " END"),
        ("CAST(", " AS INT)"),
        ("a BETWEEN 0 AND (", ")"),
        ("a IN (", ")"),
        ("a IN (1,", ")"),
        ("(SELECT ", ")"),
        ("EXISTS (SELECT ", ")"),
        ("a IN (SELECT ", ")"),
        ("(a, ", ") = (1,2)"),
        ("a LIKE (", ")"),
        ("a IS NOT (", ")"),
        ("(SELECT 1 FROM t WHERE ", ")"),
        ("(SELECT 1 FROM t GROUP BY ", ")"),
        ("(SELECT 1 FROM t ORDER BY a, ", ")"),
        ("a ISNULL OR (", ")"),
        ("(", ") ISNULL"),
        ("a || (", ")"),
        ("t.a * (", ")"),
    ];
    const X: [&str; 14] = [
        "1",
        "a",
        "t.a",
        "-1",
        "+a",
        "'x'",
        "NULL",
        "a+b*c",
        "abs(a)",
        "a ISNULL",
        "1 ISNULL",
        "a IN (1)",
        "CASE a WHEN 1 THEN 2 END",
        "(SELECT 1)",
    ];
    const C: [&str; 12] = [
        "SELECT {} FROM t",
        "SELECT 1 FROM t WHERE {}",
        "SELECT a FROM t ORDER BY {}",
        "SELECT a FROM t GROUP BY a HAVING {}",
        "UPDATE t SET a = {}",
        "DELETE FROM t WHERE {}",
        "INSERT INTO t VALUES (1, 2, {})",
        "INSERT INTO t SELECT 1, 2, {}",
        "SELECT 1 FROM t JOIN t AS u ON {}",
        "WITH q AS (SELECT {}) SELECT * FROM q",
        "SELECT 1 UNION ALL SELECT {}",
        "SELECT a FROM t LIMIT {}",
    ];
    let n_cases = std::env::var("HIPP_DEPTH_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24usize);
    let mut rng = Rng(0x6869_7070_d3e7_0016);
    for case in 0..n_cases {
        let c = *rng.pick(&C);
        let w = *rng.pick(&W);
        let w2 = if rng.next().is_multiple_of(3) {
            Some(*rng.pick(&W))
        } else {
            None
        };
        let x = *rng.pick(&X);
        let make = move |n: usize| {
            let mut inner = x.to_string();
            for i in 0..n {
                let (a, b) = match w2 {
                    Some(v) if i % 2 == 1 => v,
                    _ => w,
                };
                inner = format!("{a}{inner}{b}");
            }
            c.replace("{}", &inner)
        };
        let b = boundary(3000, |n| accepted(&pin(T, &make(n))));
        if b == 0 && !accepted(&pin(T, &make(0))) {
            // The operand is an error in this context on the pin:
            // hipp must refuse it too.
            let sql = make(0);
            let got = on_stack(PIN_STACK, move || hipp(pin_cfg(), T, &sql));
            assert!(matches!(got, Out::Err(_)), "case {case}: {}", make(0));
            continue;
        }
        for n in [b, b + 1] {
            let sql = make(n);
            let want = pin(T, &sql);
            let got = on_stack(PIN_STACK, move || hipp(pin_cfg(), T, &sql));
            assert!(
                agrees(&got, &want),
                "case {case} at {n}: {} -> {got:?} / pin {want:?}",
                make(1)
            );
        }
    }
}

/// Regressions found while probing this unit, each against the pin.
#[test]
fn regressions_match_the_oracle() {
    let cases: Vec<(String, String)> = vec![
        // `x ISNULL` / NOTNULL / IS [NOT] NULL / NOT NULL over a
        // literal is the pin's integer literal: an ORDER BY / GROUP
        // BY ordinal (hipp sorted and grouped by a constant).
        (T.into(), "SELECT a FROM t ORDER BY 2 NOTNULL".into()),
        (T.into(), "SELECT a FROM t ORDER BY 1 ISNULL".into()),
        (
            T.into(),
            "SELECT a, count(*) FROM t GROUP BY 5 NOTNULL".into(),
        ),
        (T.into(), "SELECT a FROM t ORDER BY 'x' NOT NULL".into()),
        (T.into(), "SELECT a FROM t ORDER BY - 3 IS NOT NULL".into()),
        (
            T.into(),
            "SELECT a FROM t UNION SELECT 9 ORDER BY 3 NOTNULL".into(),
        ),
        (
            T.into(),
            "SELECT 1 ISNULL, 'x' NOTNULL, - 'x' IS NULL, a IS NULL, NULL IS NULL FROM t".into(),
        ),
        // CREATE TABLE items nesting past 32 parentheses (they were
        // refused).
        (
            String::new(),
            format!(
                "CREATE TABLE z(a CHECK ({}a > 0{}), b DEFAULT ({}1{}))",
                rep("(", 40),
                rep(")", 40),
                rep("(", 40),
                rep(")", 40)
            ),
        ),
        // A view the pin creates although reading it is too deep:
        // its body is resolved only when read.
        (
            T.into(),
            format!(
                "CREATE VIEW v AS SELECT {}1{}",
                rep("(SELECT ", 60),
                rep(")", 60)
            ),
        ),
        (
            format!(
                "{T}; CREATE VIEW v AS SELECT {}1{}",
                rep("(SELECT ", 60),
                rep(")", 60)
            ),
            "SELECT * FROM v".into(),
        ),
        // The empty IN list and a one-constant IN list (the pin's
        // `x = +item`) count as the pin counts them.
        (
            T.into(),
            format!("SELECT {}(a IN ()) FROM t", rep("- ", 999)),
        ),
        (
            T.into(),
            format!("SELECT {}(a IN (1)) FROM t", rep("- ", 997)),
        ),
        (
            T.into(),
            format!("SELECT {}(a IN (1)) FROM t", rep("- ", 998)),
        ),
        (
            T.into(),
            format!("SELECT {}(a IN (b)) FROM t", rep("- ", 998)),
        ),
        // ON terms are ANDed into the WHERE the pin resolves.
        (
            T.into(),
            format!(
                "SELECT 1 FROM t JOIN t AS u ON {}1 WHERE {}1",
                rep("- ", 600),
                rep("- ", 399)
            ),
        ),
        (
            T.into(),
            format!(
                "SELECT 1 FROM t JOIN t AS u ON {}1 WHERE {}1",
                rep("- ", 998),
                rep("- ", 1)
            ),
        ),
        // A LIMIT and its OFFSET are one node.
        (
            T.into(),
            format!("SELECT (SELECT 1 LIMIT 1 OFFSET {}1)", rep("- ", 993)),
        ),
        (
            T.into(),
            format!("SELECT (SELECT 1 LIMIT 1 OFFSET {}1)", rep("- ", 994)),
        ),
        // A compound's ORDER BY sends every member through the
        // sorter.
        (
            T.into(),
            format!(
                "SELECT {}'x' UNION ALL SELECT 1 ORDER BY 1",
                rep("NOT ", 999)
            ),
        ),
        (
            T.into(),
            format!(
                "SELECT {}'x' UNION ALL SELECT 1 ORDER BY 1",
                rep("NOT ", 998)
            ),
        ),
    ];
    for (pre, sql) in cases.iter() {
        let want = pin(pre, sql);
        let (p, s) = (pre.clone(), sql.clone());
        let got = on_stack(PIN_STACK, move || hipp(pin_cfg(), &p, &s));
        assert_eq!(got, want, "{}", &sql[..sql.len().min(120)]);
    }
}

/// Statements far past every limit are refused cleanly under the
/// default configuration on a 2 MiB thread in a release build (an
/// 8 MiB one in a debug build) — the shapes that used to overflow
/// the stack: 100 000 parentheses, signs, NOTs, calls, CASEs and
/// subqueries, and a 100 000-term chain.
#[test]
fn hostile_statements_are_refused_on_a_default_thread() {
    let stack = if cfg!(debug_assertions) {
        8 * 1024 * 1024
    } else {
        2 * 1024 * 1024
    };
    let n = 100_000;
    let hostile = [
        format!("SELECT {}1{}", rep("(", n), rep(")", n)),
        format!("SELECT {}1", rep("- ", n)),
        format!("SELECT {}1", rep("+", n)),
        format!("SELECT {}1", rep("NOT ", n)),
        format!("SELECT {}1{}", rep("abs(", n), rep(")", n)),
        format!("SELECT {}1{}", rep("CASE WHEN 1 THEN ", n), rep(" END", n)),
        format!("SELECT {}1{}", rep("(SELECT ", n), rep(")", n)),
        format!(
            "SELECT * FROM {}t{}",
            rep("(SELECT * FROM ", n),
            rep(")", n)
        ),
        format!("SELECT 1{}", rep("+1", n)),
        format!("INSERT INTO t VALUES (1, 2, {}1)", rep("- ", n)),
        format!("SELECT 1 FROM t WHERE a = {}1", rep("- ", n)),
    ];
    for sql in hostile {
        let shown = sql[..60].to_string();
        let got = on_stack(stack, move || {
            hipp(
                Config::default().statement_arena(16 * 1024, 256 * 1024 * 1024),
                T,
                &sql,
            )
        });
        assert!(matches!(got, Out::Err(_)), "{shown}: {got:?}");
    }
}

// ---------------- v0.16 review fixes ----------------

/// A script: a chain of `k` views whose bodies read the previous
/// one through `body(m, prev)`, then the last one read.
fn view_chain(k: usize, m: usize, body: fn(usize, &str) -> String) -> String {
    let mut sql = format!("{};\n", T.replace("; ", ";\n"));
    let mut prev = String::from("t");
    for i in 0..k {
        sql.push_str(&format!("CREATE VIEW v{i} AS {};\n", body(m, &prev)));
        prev = format!("v{i}");
    }
    sql.push_str(&format!("SELECT * FROM {prev};\n"));
    sql
}

/// `view_chain` with CTEs.
fn cte_chain(k: usize, m: usize, body: fn(usize, &str) -> String) -> String {
    let mut parts = Vec::new();
    let mut prev = String::from("t");
    for i in 0..k {
        parts.push(format!("c{i} AS ({})", body(m, &prev)));
        prev = format!("c{i}");
    }
    format!(
        "{};\nWITH {} SELECT * FROM {prev};\n",
        T.replace("; ", ";\n"),
        parts.join(", ")
    )
}

/// View and CTE bodies are resolved where they are read, inside the
/// reader's expressions, so their depths add up along a chain (the
/// review's finding: hipp checked each body on its own, accepted
/// chains the pin refuses and overflowed the stack on long ones).
/// Both sides of the pin's boundary, searched live, for chains of
/// 1 to 5 views or CTEs reading the previous one in a scalar
/// subquery, an IN subquery, an ON term, a LIMIT and EXISTS — and a
/// view body's lone leaves, which add nothing there (the pin keeps a
/// view's tree reduced).
#[test]
fn view_and_cte_chains_add_their_depths() {
    type Body = fn(usize, &str) -> String;
    let bodies: [(&str, Body); 6] = [
        ("scalar", |m, p| {
            format!("SELECT {}(SELECT a FROM {p}) AS a", rep("- ", m))
        }),
        ("IN", |m, p| {
            format!(
                "SELECT a FROM t WHERE {}a IN (SELECT a FROM {p})",
                rep("NOT ", m)
            )
        }),
        ("ON", |m, p| {
            format!(
                "SELECT x.a AS a FROM t AS x JOIN t AS y ON {}(SELECT a FROM {p})",
                rep("NOT ", m)
            )
        }),
        ("LIMIT", |m, p| {
            format!("SELECT a FROM t LIMIT {}(SELECT a FROM {p})", rep("- ", m))
        }),
        ("EXISTS", |m, p| {
            format!(
                "SELECT a FROM t WHERE {}EXISTS (SELECT a FROM {p})",
                rep("NOT ", m)
            )
        }),
        ("leaf", |m, p| {
            format!("SELECT {}(SELECT (SELECT 1) FROM {p}) AS a", rep("- ", m))
        }),
    ];
    type Chain = fn(usize, usize, Body) -> String;
    let chains: [(&str, Chain); 2] = [("views", view_chain), ("CTEs", cte_chain)];
    for (cn, chain) in chains {
        for (bn, body) in bodies {
            for k in [1, 2, 3, 5] {
                let b = boundary(1000, |m| pin_script(&chain(k, m, body)).1 == 0);
                assert!(b > 0, "{cn} / {bn} / {k}");
                for m in [b, b + 1] {
                    let sql = chain(k, m, body);
                    assert_eq!(
                        hipp_script(&sql),
                        pin_script(&sql),
                        "{cn} of {k} / {bn} at {m}"
                    );
                }
            }
        }
    }
    // A view read by a CTE read in a deep expression.
    for m in [600, 700, 800] {
        let pre = format!(
            "{T}; CREATE VIEW v AS SELECT {}1{} AS x",
            rep("(SELECT ", 20),
            rep(")", 20)
        );
        let sql = format!(
            "WITH c AS (SELECT (SELECT x FROM v) AS y) SELECT {}(SELECT y FROM c)",
            rep("- ", m)
        );
        let want = pin(&pre, &sql);
        let got = on_stack(PIN_STACK, move || hipp(pin_cfg(), &pre, &sql));
        assert_eq!(got, want, "view in a CTE at {m}");
    }
}

/// The review's crash shapes, refused cleanly under the default
/// configuration on a 2 MiB thread in a release build (8 MiB in a
/// debug one): 4 chained views and 170 chained CTEs, each reading
/// the previous one in a scalar subquery under 990 `+ 1` terms.
#[test]
fn view_and_cte_chains_are_refused_on_a_default_thread() {
    let stack = if cfg!(debug_assertions) {
        8 * 1024 * 1024
    } else {
        2 * 1024 * 1024
    };
    let body = |p: &str| format!("SELECT (SELECT a FROM {p}){} AS a", rep("+1", 990));
    let mut pre = String::from(T);
    let mut prev = String::from("t");
    for i in 0..4 {
        pre.push_str(&format!("; CREATE VIEW v{i} AS {}", body(&prev)));
        prev = format!("v{i}");
    }
    let mut parts = Vec::new();
    let mut cprev = String::from("t");
    for i in 0..170 {
        parts.push(format!("c{i}(a) AS ({})", body(&cprev)));
        cprev = format!("c{i}");
    }
    let cases = [
        (pre.clone(), format!("SELECT * FROM {prev}")),
        (pre, "SELECT * FROM v1".to_string()),
        (
            T.to_string(),
            format!("WITH {} SELECT * FROM {cprev}", parts.join(",")),
        ),
    ];
    for (pre, sql) in cases {
        let shown = sql[..sql.len().min(40)].to_string();
        let got = on_stack(stack, move || {
            hipp(
                Config::default().statement_arena(16 * 1024, 256 * 1024 * 1024),
                &pre,
                &sql,
            )
        });
        assert_eq!(got, Out::Err("ExprTooDeep".into()), "{shown}");
    }
}

/// A script run statement by statement: the rows of each, and how
/// many failed (the pin's CLI goes on after an error, as this does).
fn hipp_script(sql: &str) -> (String, usize) {
    let sql = sql.to_string();
    on_stack(PIN_STACK, move || {
        block_on(async move {
            let mut db = Connection::open(MemStorage::new(), pin_cfg())
                .await
                .expect("open");
            let (mut out, mut errors) = (String::new(), 0);
            for s in sql.split(";\n").filter(|s| !s.trim().is_empty()) {
                let mut st = match db.prepare(s).await {
                    Ok(st) => st,
                    Err(_) => {
                        errors += 1;
                        continue;
                    }
                };
                loop {
                    match st.step().await {
                        Ok(Step::Row) => {
                            let line: Vec<String> = (0..st.column_count())
                                .map(|i| render(st.column_value(i)))
                                .collect();
                            out.push_str(&line.join("|"));
                            out.push('\n');
                        }
                        Ok(Step::Done) => break,
                        Err(_) => {
                            errors += 1;
                            break;
                        }
                    }
                }
            }
            (out, errors)
        })
    })
}

fn pin_script(sql: &str) -> (String, usize) {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(":memory:")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(sql.as_bytes())
        .expect("write");
    let out = child.wait_with_output().expect("oracle finishes");
    let errors = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.starts_with("Error") || l.starts_with("Parse error"))
        .count();
    (String::from_utf8_lossy(&out.stdout).into_owned(), errors)
}

/// Both engines print the same rows and fail the same statements.
fn scripts_agree(sql: &str) {
    let want = pin_script(sql);
    let got = hipp_script(sql);
    assert_eq!(got, want, "{}", &sql[..sql.len().min(160)]);
}

/// ORDER BY / GROUP BY ordinals as the pin's `sqlite3ExprIsInteger`
/// sees them (review fix): through COLLATE, unary signs and
/// parentheses over a 32-bit integer (or a folded null test) — so
/// `-(-2)` is the ordinal 2 and `-(2)` out of range — but never
/// TRUE / FALSE.
#[test]
fn ordinal_terms_match_the_oracle() {
    let terms = [
        "-(-2)",
        "+(2)",
        "-(2)",
        "-(-2 NOTNULL)",
        "(2)",
        "-(-(2))",
        "+(+(+2))",
        "(-(-1))",
        "- (+ (- -2))",
        "-(1 ISNULL)",
        "-('a' NOTNULL)",
        "+(-(-1))",
        "-(-2) COLLATE nocase",
        "-(2) COLLATE nocase",
        "-1 COLLATE nocase",
        "TRUE",
        "FALSE",
        "-TRUE",
        "+TRUE",
        "(TRUE)",
        "TRUE NOTNULL",
        "4294967297",
        "-(-4294967297)",
        "2147483648",
        "-0",
        "2.0",
        "1 IN ()",
    ];
    let mut sql =
        String::from("CREATE TABLE t(a, b);\nINSERT INTO t VALUES (3, 'x'), (1, 'z'), (2, 'y');\n");
    for t in terms {
        sql.push_str(&format!(
            "SELECT 'term {t}';\nSELECT a, b FROM t ORDER BY {t};\nSELECT a, b FROM t GROUP BY {t};\n\
             SELECT a, b FROM t UNION ALL SELECT 9, 9 ORDER BY {t};\n"
        ));
    }
    scripts_agree(&sql);
}

/// The width limits the pin checks only where it reads a select as
/// one (review fixes): RETURNING's result set (refused before any
/// row changes), and none for an INSERT's column list and one-row
/// VALUES, an upsert's SET, an unused CTE or a CREATE VIEW body —
/// which is refused when read.
#[test]
fn deferred_width_limits_match_the_oracle() {
    let a = |n: usize| vec!["a"; n].join(",");
    let one = |n: usize| vec!["1"; n].join(",");
    let cols = (1..=2000)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "CREATE TABLE u(a UNIQUE);\n\
         INSERT INTO u VALUES (1) RETURNING {a2001};\nSELECT count(*) FROM u;\n\
         INSERT INTO u VALUES (1) RETURNING {a2000};\nSELECT count(*) FROM u;\n\
         UPDATE u SET a = 2 RETURNING {a2001};\nSELECT a FROM u;\n\
         DELETE FROM u RETURNING {a2001};\nSELECT count(*) FROM u;\n\
         INSERT INTO u VALUES (5) RETURNING (SELECT {o2001});\nSELECT count(*) FROM u;\n\
         CREATE TABLE w({cols});\n\
         INSERT INTO w(c1) VALUES (1) RETURNING *, 1;\nSELECT count(*) FROM w;\n\
         INSERT INTO w(c1) VALUES (1) RETURNING *;\nSELECT count(*) FROM w;\n\
         DELETE FROM u;\n\
         INSERT INTO u({a2001}) VALUES ({o2001});\nSELECT * FROM u;\n\
         INSERT INTO u({a2001}) VALUES ({o2001}), ({o2001});\nSELECT count(*) FROM u;\n\
         INSERT INTO u VALUES (1) ON CONFLICT(a) DO UPDATE SET {set};\nSELECT a FROM u;\n\
         UPDATE u SET {set};\nSELECT a FROM u;\n\
         WITH c AS (SELECT {o2001}) SELECT 'unused';\n\
         WITH c AS (SELECT {o2001}) SELECT 2 FROM c;\n\
         WITH c AS (VALUES ({o2001})) SELECT 'unused values';\n\
         WITH c AS (SELECT 1 FROM u ORDER BY {a2001}) SELECT 'unused order';\n\
         WITH c AS (SELECT 1 FROM u GROUP BY {a2001}) SELECT 'unused group';\n\
         WITH c AS (SELECT 1 FROM u GROUP BY {a2001}) SELECT * FROM c;\n\
         WITH c AS (SELECT (SELECT {o2001})) SELECT 'unused nested';\n\
         WITH c AS (SELECT (SELECT {o2001})) SELECT * FROM c;\n\
         CREATE VIEW v1 AS SELECT {o2001};\nSELECT 'v1';\nSELECT * FROM v1;\n\
         CREATE VIEW v2 AS SELECT 1 FROM u ORDER BY {a2001};\nSELECT 'v2';\nSELECT * FROM v2;\n\
         CREATE VIEW v3(x{xs}) AS SELECT 1;\nSELECT 'v3';\n\
         VALUES ({o2001});\nSELECT 'end';\n",
        a2001 = a(2001),
        a2000 = a(2000),
        o2001 = one(2001),
        set = vec!["a = a + 1"; 2001].join(", "),
        xs = (1..2001).map(|i| format!(",x{i}")).collect::<String>(),
    );
    scripts_agree(&sql);
}

/// The compound limit (review fix): 500 SELECTs, unless the last
/// member is a one-row VALUES (the pin's `SF_Values` exemption) — a
/// multi-row VALUES there counts.
#[test]
fn compound_values_tail_matches_the_oracle() {
    let chain =
        |members: Vec<&str>| format!("SELECT count(*) FROM ({});\n", members.join(" UNION ALL "));
    let (s, v, m) = ("SELECT 1", "VALUES(1)", "VALUES(1),(2)");
    let mut sql = String::new();
    for members in [
        vec![s; 501],
        [vec![s; 500], vec![v]].concat(),
        [vec![s; 600], vec![v]].concat(),
        [vec![s; 600], vec![v, v]].concat(),
        [vec![v], vec![s; 500]].concat(),
        [vec![s], vec![v; 500]].concat(),
        vec![v; 501],
        vec![m; 501],
        [vec![s; 499], vec![m]].concat(),
        [vec![s; 500], vec![m]].concat(),
        [vec![s; 300], vec![v], vec![s; 300]].concat(),
    ] {
        sql.push_str(&chain(members));
    }
    sql.push_str(&format!(
        "{} UNION VALUES(1);\nSELECT 'end';\n",
        vec![s; 600].join(" UNION ")
    ));
    scripts_agree(&sql);
}

/// A DEFAULT of ALTER TABLE ADD COLUMN under a run of signs (review
/// fix): held to the pin's limits (two symbols up on its stack), not
/// stored past them.
#[test]
fn add_column_default_sign_runs_match_the_oracle() {
    for r in ["+ ", "- ", "+ - "] {
        let f = |n: usize| {
            format!(
                "CREATE TABLE u(y);\nINSERT INTO u VALUES (5);\n\
                 ALTER TABLE u ADD COLUMN x DEFAULT ({}1);\nSELECT * FROM u;\n",
                r.repeat(n)
            )
        };
        let b = boundary(3000, |n| pin_script(&f(n)).1 == 0);
        assert!(b > 0, "{r}");
        scripts_agree(&f(b));
        scripts_agree(&f(b + 1));
    }
}
