//! hipp-cli v0.17 parity unit `gaps_plan_cli`: the shell's command
//! line and `.dump`, compared with the pinned oracle's shell (SQLite
//! 3.54.1), and the unit's planner fixes as the shell shows them:
//!
//! - Options: `-help` / `--help` print the usage to stderr and exit 0,
//!   `-version` / `--version` print the version line and exit 0, an
//!   unknown option (`-foo`, `--foo`, `---x`, case-sensitive) is the
//!   pin's two-line error with exit status 1, and `--` / `-` end the
//!   options; the first other argument is the database (`:memory:`
//!   when none), every later one an SQL text run in order. None of
//!   these makes a file, and the database is opened only when input
//!   needs it (an empty or comment-only script makes none; a blank
//!   one-shot argument does, as on the pin).
//! - `.dump`: byte-identical over reals (`1`, `1e+300`, `-0.0`, `3e-05`,
//!   infinities), blobs, text with quotes and control characters
//!   (`unistr('...')`), names that need quoting (keywords, blanks,
//!   digits first), `sqlite_sequence`, `sqlite_stat1`, generated
//!   columns, WITHOUT ROWID tables, views, triggers and indexes, and
//!   the header (`PRAGMA foreign_keys=OFF;`, BEGIN / COMMIT); the dump
//!   replays in the pin.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

/// A fresh, empty directory for one case.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17pc-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// `bin` with `args` in `dir`, `input` on stdin.
fn run_in(bin: &Path, dir: &Path, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(bin)
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli spawns");
    let _ = child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes());
    child.wait_with_output().expect("cli finishes")
}

/// The file names in `dir`, sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// One command line on both shells, each in its own empty directory:
/// the same exit status, stdout, stderr (the program name read as
/// such; for an SQL error, only whether there is one: hipp's error
/// texts are its own) and files made.
fn same_line(tag: &str, args: &[&str], input: &str, sql_error: bool) {
    let (hd, od) = (temp_dir(&format!("{tag}-h")), temp_dir(&format!("{tag}-o")));
    let h = run_in(&hipp_bin(), &hd, args, input);
    let o = run_in(&oracle(), &od, args, input);
    let who = |b: &Path, out: &[u8]| {
        String::from_utf8_lossy(out).replace(b.to_string_lossy().as_ref(), "SHELL")
    };
    assert_eq!(h.status.code(), o.status.code(), "{args:?}: exit status");
    assert_eq!(
        String::from_utf8_lossy(&h.stdout),
        String::from_utf8_lossy(&o.stdout),
        "{args:?}: stdout"
    );
    if sql_error {
        assert!(
            !h.stderr.is_empty() && !o.stderr.is_empty(),
            "{args:?}: stderr"
        );
    } else {
        assert_eq!(
            who(&hipp_bin(), &h.stderr),
            who(&oracle(), &o.stderr),
            "{args:?}: stderr"
        );
    }
    assert_eq!(files(&hd), files(&od), "{args:?}: files");
}

/// `-help` and `-version` behave as the pin's: the channel, the exit
/// status, no file; the usage's first line and option layout and the
/// version's first word are the pin's.
#[test]
fn help_and_version_make_no_file() {
    for args in [
        &["--help"][..],
        &["-help"],
        &["-help", "x.db", "SELECT 1"],
        &["x.db", "SELECT 1", "-help"],
    ] {
        let (hd, od) = (temp_dir("help-h"), temp_dir("help-o"));
        let h = run_in(&hipp_bin(), &hd, args, "");
        let o = run_in(&oracle(), &od, args, "");
        assert_eq!(h.status.code(), Some(0), "{args:?}");
        assert_eq!(h.status.code(), o.status.code(), "{args:?}");
        assert!(h.stdout.is_empty() && o.stdout.is_empty(), "{args:?}");
        let (he, oe) = (
            String::from_utf8_lossy(&h.stderr),
            String::from_utf8_lossy(&o.stderr),
        );
        let first = |s: &str, b: &Path| {
            s.lines()
                .next()
                .unwrap_or("")
                .replace(b.to_string_lossy().as_ref(), "SHELL")
        };
        assert_eq!(first(&he, &hipp_bin()), first(&oe, &oracle()), "{args:?}");
        // The pin's lines for the options hipp takes, verbatim.
        for opt in ["   --  ", "   -help  ", "   -version  "] {
            let pick = |s: &str| s.lines().find(|l| l.starts_with(opt)).map(str::to_string);
            assert_eq!(pick(&he), pick(&oe), "{opt}");
            assert!(pick(&he).is_some(), "{opt}");
        }
        assert!(files(&hd).is_empty() && files(&od).is_empty(), "{args:?}");
    }
    for args in [
        &["-version"][..],
        &["--version"],
        &["x.db", "-version"],
        &["-version", "-foo"],
        &["x.db", "SELECT 1", "--version"],
    ] {
        let (hd, od) = (temp_dir("ver-h"), temp_dir("ver-o"));
        let h = run_in(&hipp_bin(), &hd, args, "");
        let o = run_in(&oracle(), &od, args, "");
        assert_eq!(h.status.code(), Some(0), "{args:?}");
        assert_eq!(h.status.code(), o.status.code(), "{args:?}");
        let word = |o: &Output| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(word(&h), word(&o), "{args:?}");
        assert_eq!(word(&h), "3.54.1");
        assert!(h.stderr.is_empty() && o.stderr.is_empty(), "{args:?}");
        assert!(files(&hd).is_empty() && files(&od).is_empty(), "{args:?}");
    }
}

/// Unknown options are the pin's error, byte for byte (the program's
/// name aside); `--` and `-` end the options; later arguments are SQL
/// texts run in order, stopping at the first error.
#[test]
fn options_and_arguments_as_the_pin_reads_them() {
    let cases: &[(&str, &[&str], &str)] = &[
        ("foo", &["-foo"], ""),
        ("foo2", &["--foo"], ""),
        ("foo3", &["---version"], ""),
        ("foo4", &["-HELP"], ""),
        ("foo5", &["-Version"], ""),
        ("foo6", &["x.db", "-foo"], ""),
        ("foo7", &["-foo", "-version"], ""),
        ("foo8", &["x.db", "SELECT 1", "-foo"], ""),
        ("dash", &["-- c"], ""),
        ("ddx", &["--", "-x.db", "SELECT 1"], ""),
        ("dx", &["-", "-v.db", "SELECT 2"], ""),
        ("dlone", &["-"], "SELECT 9;\n"),
        ("ddlone", &["--"], "SELECT 9;\n"),
        ("none", &[], "SELECT 9;\n"),
        ("multi", &["x.db", "SELECT 1", "SELECT 2; SELECT 3"], ""),
        (
            "multierr",
            &["x.db", "SELECT 1", "SELECT nosuch", "SELECT 2"],
            "",
        ),
        ("aftersql", &["x.db", "SELECT 7", "--", "SELECT 8"], ""),
        ("aftersqlerr", &["x.db", "SELECT 7", "--", "-foo"], ""),
        ("blankarg", &["x.db", "   "], ""),
        ("emptyarg", &["x.db", ""], ""),
        ("semis", &["x.db", ";"], ""),
        (
            "write",
            &[
                "x.db",
                "CREATE TABLE t(a); INSERT INTO t VALUES (1)",
                "SELECT * FROM t",
            ],
            "",
        ),
    ];
    for (tag, args, input) in cases {
        let sql_error = matches!(*tag, "multierr" | "aftersqlerr");
        same_line(tag, args, input, sql_error);
    }
}

/// The database is opened only when the input needs it: an empty
/// script, blank lines and comments make no file; a statement, a
/// `;` or a command does.
#[test]
fn the_database_opens_lazily() {
    for (i, input) in [
        "",
        "\n\n   \n",
        "-- a comment\n",
        "-- a\n/* b */\n",
        "/* b\n c */\n",
        "/* x */ ;;\n",
        "SELECT 1;\n",
        "SELECT 1 -- a\n;\n",
        ".tables\n",
        "CREATE TABLE t(a);\n",
    ]
    .iter()
    .enumerate()
    {
        let (hd, od) = (
            temp_dir(&format!("lazy{i}-h")),
            temp_dir(&format!("lazy{i}-o")),
        );
        let h = run_in(&hipp_bin(), &hd, &["x.db"], input);
        let o = run_in(&oracle(), &od, &["x.db"], input);
        assert_eq!(h.stdout, o.stdout, "{input:?}");
        assert_eq!(h.status.code(), o.status.code(), "{input:?}");
        assert_eq!(files(&hd), files(&od), "{input:?}");
    }
    // A first `.open` replaces the named database before anything
    // reads it.
    let (hd, od) = (temp_dir("lazyopen-h"), temp_dir("lazyopen-o"));
    let input = ".open other.db\nCREATE TABLE t(a);\nINSERT INTO t VALUES (5);\n";
    let h = run_in(&hipp_bin(), &hd, &["first.db"], input);
    let o = run_in(&oracle(), &od, &["first.db"], input);
    assert_eq!(h.status.code(), o.status.code());
    assert_eq!(files(&hd), files(&od));
    let q = |d: &Path| {
        let out = Command::new(oracle())
            .arg(d.join("other.db"))
            .arg("SELECT * FROM t")
            .output()
            .expect("oracle");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(q(&hd), "5\n");
    assert_eq!(q(&hd), q(&od));
}

// ------------------------------------------------------------- .dump

/// `.dump` of the file at `db` by both shells.
fn dumps(db: &Path) -> (String, String) {
    let one = |b: &Path| {
        let out = Command::new(b).arg(db).arg(".dump").output().expect("dump");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8 dump")
    };
    (one(&hipp_bin()), one(&oracle()))
}

/// `script` through `bin` (stdin) into a fresh file.
fn build(bin: &Path, tag: &str, script: &str) -> PathBuf {
    let d = temp_dir(tag);
    let db = d.join("t.db");
    let out = run_in(bin, &d, &[db.to_str().expect("path")], script);
    assert!(
        out.status.success(),
        "{tag}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

const DUMP: &str = "CREATE TABLE r(a REAL, b, c TEXT, d BLOB, e INTEGER);
INSERT INTO r VALUES(1.0, 1.0, '1.0', x'00ff', 1);
INSERT INTO r VALUES(1e300, -0.0, 'it''s', x'', -1);
INSERT INTO r VALUES(0.1, 123456789.123456789, 'a
b', NULL, 9223372036854775807);
INSERT INTO r VALUES(-1.5e-300, 1e15, 'tab\tx', zeroblob(3), -9223372036854775808);
INSERT INTO r VALUES(1e16, 1.7976931348623157e308, '', 'text', 2.5);
INSERT INTO r VALUES(100.0, 3.0e-5, 'cr' || char(13) || 'x', x'41', 0);
INSERT INTO r VALUES(1e308*10, -1e308*10, 'back\\slash' || char(10), 'nul' || char(0) || 'cut', 4.9e-324);
INSERT INTO r VALUES(0.30000000000000004, 1e22, char(1) || char(31) || char(127), 'é\\u0041', -2.0);
CREATE TABLE s(id INTEGER PRIMARY KEY AUTOINCREMENT, v);
INSERT INTO s(v) VALUES('x'),('y');
CREATE INDEX ri ON r(a);
CREATE UNIQUE INDEX rc ON r(c, e);
CREATE VIEW rv AS SELECT a, c FROM r;
CREATE VIEW rv2 AS SELECT '--' AS x;
CREATE TRIGGER rt AFTER INSERT ON r BEGIN INSERT INTO s(v) VALUES(new.c); END;
CREATE TABLE \"we ird\"(\"x y\", \"z\"\"q\");
INSERT INTO \"we ird\" VALUES(1, 2);
CREATE TABLE w(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO w VALUES('k', 1);
CREATE TABLE e(x);
CREATE TABLE \"order\"(x); INSERT INTO \"order\" VALUES(1);
CREATE TABLE within(x); INSERT INTO within VALUES(1);
CREATE TABLE \"true\"(x); INSERT INTO \"true\" VALUES(1);
CREATE TABLE \"1a\"(x); INSERT INTO \"1a\" VALUES(1);
CREATE TABLE [sel ect](\"we\"\"ird\" INTEGER); INSERT INTO [sel ect] VALUES(2);
CREATE TABLE \"sq'\"(v); INSERT INTO \"sq'\" VALUES('q''');
CREATE TABLE g(a, b AS (a+1)); INSERT INTO g(a) VALUES(5);
CREATE TABLE c2(a DEFAULT '--');
CREATE TABLE st(a, b); INSERT INTO st VALUES(1, 2), (1, 3); CREATE INDEX sta ON st(a);
ANALYZE;
";

/// The corpus written by the pin and by hipp: both shells dump each
/// file byte for byte alike, and hipp's dump replays in the pin to
/// the same dump.
#[test]
fn dump_matches_the_pin_byte_for_byte() {
    for (tag, bin) in [("pin", oracle()), ("hipp", hipp_bin())] {
        let db = build(&bin, &format!("dump-{tag}"), DUMP);
        let (h, o) = dumps(&db);
        assert_eq!(h, o, "file written by {tag}");
        assert!(h.starts_with("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n"));
        assert!(h.ends_with("PRAGMA writable_schema=OFF;\nCOMMIT;\n"));
        let replay = build(&oracle(), &format!("dump-{tag}-replay"), &h);
        // (Not `h` itself: `%.16g` is lossy on the pin too, so
        // 1.7976931348623157e308 replays as an infinity.)
        let (h2, o2) = dumps(&replay);
        assert_eq!(h2, o2, "replayed {tag}");
    }
}

/// A small xorshift PRNG (tests/fuzz_smoke.rs style).
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
    fn n(&mut self, k: u64) -> u64 {
        self.next() % k
    }
    fn pick<'a>(&mut self, l: &[&'a str]) -> &'a str {
        l[self.n(l.len() as u64) as usize]
    }
}

/// Generated tables of every affinity holding generated values: the
/// two shells' dumps of each pin-written file are identical.
#[test]
fn generated_dumps_match_the_pin() {
    let reals = [
        "1.0",
        "0.5",
        "-0.0",
        "1e300",
        "1e-300",
        "123456789.123456789",
        "1.7976931348623157e308",
        "3.0e-5",
        "1e16",
        "1e15",
        "2.5",
        "100.0",
        "0.1",
        "4.9e-324",
        "1e308*10",
        "-1e308*10",
        "1e22",
        "9007199254740993.0",
        "0.30000000000000004",
        "-123.456e-7",
    ];
    let texts = [
        "'a'",
        "'it''s'",
        "char(10)",
        "char(13)",
        "char(9)",
        "char(0)",
        "char(1)",
        "char(127)",
        "'é'",
        "'\\'",
        "'\\n'",
        "'--'",
        "'\"'",
        "char(31)",
        "char(32)",
    ];
    let names = [
        "t",
        "\"we ird\"",
        "\"order\"",
        "_x",
        "\"1a\"",
        "[a b]",
        "\"q\"\"t\"",
        "abc",
    ];
    let affs = ["", "REAL", "TEXT", "INTEGER", "NUMERIC", "BLOB"];
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    for case in 0..24 {
        let mut script = String::new();
        let mut used: Vec<&str> = Vec::new();
        while used.len() < 3 {
            let n = r.pick(&names);
            if !used.contains(&n) {
                used.push(n);
            }
        }
        for n in used {
            script.push_str(&format!("CREATE TABLE {n}(a {}, b);\n", r.pick(&affs)));
            for _ in 0..r.n(5) {
                let val = |r: &mut Rng| -> String {
                    match r.n(10) {
                        0..=1 => [
                            "0",
                            "1",
                            "-1",
                            "9223372036854775807",
                            "-9223372036854775808",
                            "12345",
                        ][r.n(6) as usize]
                            .to_string(),
                        2..=4 => r.pick(&reals).to_string(),
                        5..=7 => {
                            let k = r.n(5);
                            if k == 0 {
                                "''".to_string()
                            } else {
                                (0..k)
                                    .map(|_| r.pick(&texts))
                                    .collect::<Vec<_>>()
                                    .join("||")
                            }
                        }
                        8 => r
                            .pick(&["x''", "x'00'", "x'ff00ab'", "zeroblob(2)"])
                            .to_string(),
                        _ => "NULL".to_string(),
                    }
                };
                let (a, b) = (val(&mut r), val(&mut r));
                script.push_str(&format!("INSERT INTO {n} VALUES({a},{b});\n"));
            }
        }
        let db = build(&oracle(), &format!("gen{case}"), &script);
        let (h, o) = dumps(&db);
        assert_eq!(h, o, "script:\n{script}");
    }
}

// ------------------------------------------- planner fixes, shown

/// The unit's planner fixes through the shell: INDEXED BY another
/// table's index fails (exit status and output as the pin's), `OR 1`
/// keeps the FROM order, a mixed-affinity compound filters per arm,
/// and a NOCASE merge key keeps the pin's row.
#[test]
fn planner_fixes_through_the_shell() {
    let setup = "CREATE TABLE t(a, b); CREATE TABLE u(c, d); CREATE INDEX nx ON u(c); \
        INSERT INTO t VALUES (1, 2), (3, 4); INSERT INTO u VALUES (5, 6); \
        CREATE TABLE a(a INTEGER PRIMARY KEY, x); CREATE TABLE b(a, y); \
        INSERT INTO a VALUES (1, 1), (2, 2), (3, 3); INSERT INTO b VALUES (1, 10), (2, 20), (-1, 30); \
        CREATE TABLE r(r REAL); INSERT INTO r VALUES (1), (2.5); \
        CREATE TABLE n(x TEXT, k INTEGER PRIMARY KEY); CREATE INDEX nxx ON n(x); \
        INSERT INTO n(x) VALUES ('b'), ('A'), ('a'), ('B'), ('c'), ('C'), ('a');";
    for (i, q) in [
        "SELECT a FROM t INDEXED BY nx;",
        "SELECT a FROM t INDEXED BY nosuch;",
        "SELECT * FROM a JOIN b ON b.a > 0 OR 1;",
        "EXPLAIN QUERY PLAN SELECT * FROM a JOIN b ON b.a > 0 OR 1;",
        "SELECT * FROM (SELECT r AS x FROM r UNION ALL SELECT 1) WHERE x = '1';",
        "SELECT x COLLATE NOCASE AS c FROM n UNION SELECT x FROM n WHERE k > 2;",
        "EXPLAIN QUERY PLAN SELECT x COLLATE NOCASE AS c FROM n WHERE k < 3 UNION SELECT x FROM n WHERE k > 2;",
    ]
    .iter()
    .enumerate()
    {
        let (hd, od) = (temp_dir(&format!("pf{i}-h")), temp_dir(&format!("pf{i}-o")));
        let input = format!("{setup}\n{q}\n");
        let h = run_in(&hipp_bin(), &hd, &["x.db"], &input);
        let o = run_in(&oracle(), &od, &["x.db"], &input);
        assert_eq!(
            String::from_utf8_lossy(&h.stdout),
            String::from_utf8_lossy(&o.stdout),
            "{q}"
        );
        assert_eq!(h.status.code(), o.status.code(), "{q}");
        assert_eq!(h.stderr.is_empty(), o.stderr.is_empty(), "{q}");
    }
}
