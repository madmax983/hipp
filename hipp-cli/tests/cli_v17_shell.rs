//! hipp-cli v0.17 parity unit `shell`: the pinned shell's (SQLite
//! 3.54.1) dot-commands and output modes, compared byte for byte with
//! the live pin's shell — stdout, stderr (an SQL error's text aside:
//! hipp's are its own), the exit status and the files left behind —
//! over one-shot arguments and stdin scripts:
//!
//! - `.mode` list, csv, column, line, box, markdown, table, quote,
//!   json, tabs, insert, ascii, with `.headers`, `.separator`,
//!   `.nullvalue` and `.width` (wrapping included), over fixed and
//!   xorshift-generated corpora of values, names, widths and settings;
//! - the column names the titles show (aliases, column references,
//!   `*`, joins, subqueries, views, CTEs, PRAGMAs, RETURNING);
//! - `.help`, `.show`, `.print`, `.echo`, `.bail`, `.changes`,
//!   `.timer`, `.output`, `.once`, `.read`, `.tables`, `.indexes`,
//!   `.limit`, `.fullschema`, command prefixes, argument parsing and
//!   the error messages (usage lines, the `line N:` / `argv[N]:`
//!   caret form);
//! - `.dump` options `--data-only`, `--newlines`, `--nosys`,
//!   `--preserve-rowids` and LIKE patterns;
//! - a new database file left at 0 bytes until it is written;
//! - the `-version` line's layout.

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

/// A fresh, empty directory for one run.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v17sh-{tag}"));
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

/// What a run left in its directory: each file's name with its size,
/// and the bytes of those that are not databases (`.output` files).
fn files(dir: &Path) -> Vec<(String, u64, Vec<u8>)> {
    let mut v: Vec<(String, u64, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| {
            let e = e.expect("entry");
            let name = e.file_name().to_string_lossy().into_owned();
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            let body = if name.ends_with(".db") {
                Vec::new()
            } else {
                std::fs::read(e.path()).unwrap_or_default()
            };
            (name, len, body)
        })
        .collect();
    v.sort();
    v
}

/// How strictly stderr is compared.
#[derive(Clone, Copy, PartialEq)]
enum Err {
    /// Byte for byte (the program name read as such).
    Exact,
    /// Only whether there is any: the run holds an SQL error, whose
    /// text is hipp's own.
    Sql,
}

/// One command line on both shells, each in its own directory holding
/// `setup` files: the same exit status, stdout, stderr and files.
fn check(tag: &str, args: &[&str], input: &str, setup: &[(&str, &str)], err: Err) {
    let (hd, od) = (temp_dir(&format!("{tag}-h")), temp_dir(&format!("{tag}-o")));
    for (n, t) in setup {
        std::fs::write(hd.join(n), t).expect("setup");
        std::fs::write(od.join(n), t).expect("setup");
    }
    let h = run_in(&hipp_bin(), &hd, args, input);
    let o = run_in(&oracle(), &od, args, input);
    let who = |b: &Path, out: &[u8]| {
        String::from_utf8_lossy(out).replace(b.to_string_lossy().as_ref(), "SHELL")
    };
    let ctx = format!("{tag}: {args:?} {input:?}");
    assert_eq!(h.status.code(), o.status.code(), "{ctx}: exit status");
    assert_eq!(
        String::from_utf8_lossy(&h.stdout),
        String::from_utf8_lossy(&o.stdout),
        "{ctx}: stdout"
    );
    match err {
        Err::Exact => assert_eq!(
            who(&hipp_bin(), &h.stderr),
            who(&oracle(), &o.stderr),
            "{ctx}: stderr"
        ),
        Err::Sql => assert_eq!(
            h.stderr.is_empty(),
            o.stderr.is_empty(),
            "{ctx}: stderr {:?} / {:?}",
            String::from_utf8_lossy(&h.stderr),
            String::from_utf8_lossy(&o.stderr)
        ),
    }
    assert_eq!(files(&hd), files(&od), "{ctx}: files");
}

fn same(tag: &str, args: &[&str]) {
    check(tag, args, "", &[], Err::Exact);
}

fn same_stdin(tag: &str, args: &[&str], input: &str) {
    check(tag, args, input, &[], Err::Exact);
}

/// A deterministic xorshift generator (as `tests/fuzz_smoke.rs`).
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
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

const MODES: [&str; 12] = [
    "list", "csv", "column", "line", "box", "markdown", "table", "quote", "json", "tabs", "insert",
    "ascii",
];

/// A table of every value kind: integers, reals, text with quotes,
/// separators, a newline, a tab and non-ASCII, blobs, NULL.
const TABLE: &str = "CREATE TABLE t(a INTEGER, b TEXT, c REAL, d BLOB, e); INSERT INTO t \
    VALUES(1,'hello',2.5,x'0102',NULL),(-20,'a|b\"c''d',1e100,x'',3),\
    (NULL,'line1'||char(10)||'line2',0.0,NULL,'tab'||char(9)||'x'),\
    (123456789,'ünï',-0.5,x'41',1.0);";

/// Every mode under every `.headers`, separator and NULL text, over
/// rows and over no row, and `.mode`'s own description of the result.
#[test]
fn modes_render_like_the_pin() {
    let mut n = 0;
    for m in MODES {
        for h in [None, Some("on"), Some("off"), Some("always")] {
            for sep in [None, Some(".separator , X"), Some(".separator ;")] {
                for nv in [None, Some(".nullvalue NIL")] {
                    let mode = format!(".mode {m}");
                    let mut args = vec![":memory:", TABLE];
                    let hs = h.map(|h| format!(".headers {h}"));
                    if let Some(h) = &hs {
                        args.push(h);
                    }
                    args.push(&mode);
                    if let Some(s) = sep {
                        args.push(s);
                    }
                    if let Some(v) = nv {
                        args.push(v);
                    }
                    args.extend(["SELECT * FROM t", "SELECT * FROM t WHERE 0", ".mode"]);
                    n += 1;
                    same(&format!("modes{n}"), &args);
                }
            }
        }
    }
    // Reals and texts each mode writes its own way.
    for m in [
        "list", "quote", "insert", "json", "csv", "line", "tabs", "box",
    ] {
        let mode = format!(".mode {m}");
        same(
            "reals",
            &[
                ":memory:",
                &mode,
                "SELECT 0.1, 1.0/3, 1e-5, 123456789012345678.0, -0.0, 1e15, 1e16, 9e999, \
                 -9e999, 1.5e300, 100.0, 5e-324, 1.7976931348623157e308",
            ],
        );
        same(
            "texts",
            &[
                ":memory:",
                &mode,
                "SELECT '' AS e, 'a,b' AS c, 'a b' AS s, ' lead' AS l, 'a''b' AS q, \
                 'a\"b' AS dq, char(13) AS cr, char(127) AS del, 'ü' AS u, char(1) AS c1, \
                 'a\\b' AS bs, 'x'||char(0)||'y' AS nul, x'00ff80' AS bl, 'a|b' AS p, \
                 char(8)||char(12)||char(31) AS ctl, '<&>' AS h",
            ],
        );
    }
    // Every printable ASCII character in csv and tabs (which quote).
    for m in ["csv", "tabs"] {
        let mode = format!(".mode {m}");
        same(
            "ascii",
            &[
                ":memory:",
                &mode,
                ".headers on",
                "WITH RECURSIVE c(i) AS (SELECT 32 UNION ALL SELECT i+1 FROM c WHERE i<126) \
                 SELECT i, 'a'||char(i)||'b' AS \"x y\", char(i) FROM c",
            ],
        );
    }
}

/// A generated corpus: random columns (names quoted, non-ASCII,
/// empty), random rows of every value kind, random modes, titles,
/// separators, NULL texts and widths.
#[test]
fn generated_modes_match_pin() {
    const VALS: &[&str] = &[
        "NULL",
        "0",
        "1",
        "-1",
        "42",
        "123456789012",
        "9223372036854775807",
        "-9223372036854775808",
        "0.5",
        "-2.25",
        "1e100",
        "1.0",
        "0.0",
        "-0.0",
        "1e-7",
        "3.14159265358979",
        "1e15",
        "1e16",
        "9e999",
        "-9e999",
        "''",
        "'a'",
        "'hello world'",
        "'a,b'",
        "'a|b'",
        "'tab'||char(9)||'x'",
        "'l1'||char(10)||'l2'",
        "'l1'||char(10)||char(10)",
        "char(10)||'x'",
        "'x'||char(13)",
        "'q''s'",
        "'d\"q'",
        "'ünï'",
        "'日本語'",
        "'e'||char(769)",
        "'😀'",
        "' lead'",
        "'trail '",
        "'a  b c'",
        "'abcdefghijklmnopqrstuvwxyz abcdefghij'",
        "'\\'",
        "char(1)||'z'",
        "'x'||char(0)||'y'",
        "x''",
        "x'00'",
        "x'41'",
        "x'ff00'",
        "x'0a'",
        "'-'",
        "'1'",
        "'NULL'",
        "'<&>'",
        "'a;b'",
        "'aXb'",
    ];
    const NAMES: &[&str] = &[
        "a",
        "b",
        "\"c d\"",
        "x",
        "\"Q\"",
        "日",
        "longer_name",
        "n1",
        "\"a\"\"b\"",
        "\"\"",
    ];
    const SEPS: &[&str] = &[
        ",",
        ";",
        "XX",
        "' '",
        "'\\t'",
        "\"\\t\" \"\\n\"",
        ", ;",
        "\"\\x01\"",
    ];
    const NULLS: &[&str] = &["NIL", "''", "'a b'", "\"\\n\""];
    let mut rng = Rng(0x5eed_0017_5e11);
    for it in 0..240 {
        let nc = 1 + rng.below(4);
        let nr = rng.below(5);
        let cols: Vec<&str> = (0..nc).map(|_| *rng.pick(NAMES)).collect();
        let q = if nr == 0 {
            let items: Vec<String> = cols.iter().map(|c| format!("1 AS {c}")).collect();
            format!("SELECT {} WHERE 0", items.join(", "))
        } else {
            let mut cores = Vec::new();
            for i in 0..nr {
                let vals: Vec<&str> = (0..nc).map(|_| *rng.pick(VALS)).collect();
                if i == 0 {
                    let items: Vec<String> = vals
                        .iter()
                        .zip(&cols)
                        .map(|(v, c)| format!("{v} AS {c}"))
                        .collect();
                    cores.push(format!("SELECT {}", items.join(", ")));
                } else {
                    cores.push(format!("SELECT {}", vals.join(", ")));
                }
            }
            cores.join(" UNION ALL ")
        };
        let mut args: Vec<String> = vec![":memory:".into()];
        if rng.chance(60) {
            let h = *rng.pick(&["on", "off", "always", "yes", "0"]);
            args.push(format!(".headers {h}"));
        }
        let m = *rng.pick(&MODES);
        args.push(if rng.chance(20) {
            format!(".mode {m} tbl")
        } else {
            format!(".mode {m}")
        });
        if rng.chance(30) {
            args.push(format!(".separator {}", rng.pick(SEPS)));
        }
        if rng.chance(30) {
            args.push(format!(".nullvalue {}", rng.pick(NULLS)));
        }
        if rng.chance(30) {
            let ws: Vec<String> = (0..1 + rng.below(nc))
                .map(|_| rng.pick(&[0, 1, 2, 3, 5, 8, -1, -3, -6, 12]).to_string())
                .collect();
            args.push(format!(".width {}", ws.join(" ")));
        }
        args.push(q);
        args.push(".mode".into());
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        same(&format!("gen{it}"), &a);
    }
}

/// `.width` wrapping (generated): blanks, word characters, other
/// ASCII, tabs, two-byte and wide characters, in every columnar mode
/// and at fixed widths from 1 (which the pin widens to 2) up, left
/// and right aligned.
#[test]
fn generated_width_wrapping_matches_pin() {
    const AL: &[&str] = &[
        "a", "b", "c", "1", "_", " ", " ", ",", ".", "-", "é", "日", "\t", "ü", "'",
    ];
    let mut rng = Rng(0x0077_1d7e_0017);
    for it in 0..160 {
        let len = 1 + rng.below(20);
        let v: String = (0..len).map(|_| *rng.pick(AL)).collect();
        let lit = format!("'{}'", v.replace('\'', "''"));
        let w = *rng.pick(&[1, 2, 3, 4, 5, 6, 7, 8, 10, -1, -2, -3, -5, -8]);
        let m = *rng.pick(&["box", "column", "table", "markdown"]);
        let mode = format!(".mode {m}");
        let width = format!(".width {w} 2");
        let q = format!("SELECT {lit} AS a, 'zz' AS b UNION ALL SELECT 'q', {lit}");
        same(&format!("wrap{it}"), &[":memory:", &mode, &width, &q]);
    }
    // The shapes the wrap rules were read from.
    for (w, v) in [
        (3, "abcdefg hij"),
        (3, "abcd efgh"),
        (3, "   abc"),
        (3, "1.0e+100"),
        (2, "ünï"),
        (2, "üna"),
        (3, "aüüüü"),
        (3, "aaüüü"),
        (-2, "a  b c"),
        (-2, "tab\tx"),
        (-10, "aaaa bbbbbbbbbbbb"),
        (-10, "aaaaa bbbbbbbbbbbb"),
        (-10, "aaaa  bbbbbbbbbbbb"),
        (-10, "aaaaaaaa  ,bbbb"),
        (12, "abcdefghijklmnopqrstuvwxyz abcdefghij"),
        (5, "  c1abc1"),
        (5, "cb 日日 aaaaab"),
    ] {
        let width = format!(".width {w}");
        let q = format!("SELECT '{v}' AS a UNION ALL SELECT 'x'");
        same("wrapshape", &[":memory:", ".mode table", &width, &q]);
    }
}

/// The titles name each column as the pin does.
#[test]
fn column_names_match_pin() {
    let schema = "CREATE TABLE t(Id INTEGER PRIMARY KEY, Ab, \"c d\", g AS (Ab+1)); \
        CREATE TABLE u(x, Ab); CREATE TABLE w(q PRIMARY KEY, r) WITHOUT ROWID; \
        CREATE VIEW v AS SELECT Ab AS vv, 1+1 FROM t; CREATE TEMP TABLE tt(Z, y); \
        ATTACH ':memory:' AS aux; CREATE TABLE aux.at(K, l); \
        INSERT INTO t(Id, Ab, \"c d\") VALUES(1, 2, 3); INSERT INTO u VALUES(5, 2);";
    let queries = [
        "SELECT ab, AB, t.ab, main.t.AB FROM t",
        "SELECT rowid, _rowid_, oid, t.ROWID FROM t",
        "SELECT rowid, oid FROM u",
        "SELECT * FROM t",
        "SELECT *, * FROM t",
        "SELECT * FROM t, u",
        "SELECT * FROM t JOIN u USING (Ab)",
        "SELECT * FROM t NATURAL JOIN u",
        "SELECT * FROM t LEFT JOIN u USING (ab)",
        "SELECT * FROM u RIGHT JOIN t USING (Ab)",
        "SELECT u.*, t.* FROM t, u",
        "SELECT * FROM (SELECT 1 AS a, 2 AS a)",
        "SELECT * FROM (SELECT x, x, x, x FROM u)",
        "SELECT * FROM (SELECT ab FROM t) AS s",
        "SELECT s.ab FROM (SELECT AB FROM t) AS s",
        "SELECT * FROM (SELECT ab COLLATE nocase FROM t)",
        "SELECT * FROM (SELECT * FROM t, u)",
        "SELECT 1+1, 1 +  1 , 'x', -5, x'ab', NULL, ab||'' FROM t",
        "SELECT ab COLLATE nocase, (ab), +ab FROM t",
        "SELECT * FROM v",
        "SELECT vv, \"1+1\" FROM v",
        "WITH c(p,q) AS (SELECT 1,2) SELECT * FROM c",
        "WITH c AS (SELECT 1 AS p, 2) SELECT * FROM c",
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<3) SELECT * FROM c",
        "SELECT count(*), max(ab) FROM t",
        "SELECT * FROM generate_series(1,3)",
        "SELECT * FROM json_each('[1]')",
        "SELECT * FROM pragma_table_info('t')",
        "SELECT \"c d\", [c d], `c d` FROM t",
        "SELECT ab AS \"X Y\", ab \"Z\", ab AS 'Q' FROM t",
        "SELECT 1 UNION SELECT 2 AS b",
        "VALUES (1,2)",
        "SELECT * FROM (VALUES (1,2))",
        "SELECT (SELECT 1), EXISTS (SELECT 1) FROM t",
        "SELECT x FROM t AS z, u WHERE z.id=1",
        "SELECT * FROM t AS a JOIN t AS b USING (id)",
        "SELECT * FROM tt",
        "SELECT * FROM aux.at",
        "SELECT k FROM at",
        "SELECT *, g FROM t",
        "SELECT count(*) OVER (), row_number() OVER w FROM t WINDOW w AS (ORDER BY ab)",
        "INSERT INTO u VALUES(1,2) RETURNING *",
        "INSERT INTO u VALUES(1,2) RETURNING x+1, Ab, rowid",
        "UPDATE t SET Ab=3 RETURNING *, AB",
        "DELETE FROM u RETURNING x AS y, u.ab",
        "PRAGMA table_info(t)",
        "PRAGMA USER_VERSION",
        "PRAGMA index_list(t)",
        "PRAGMA database_list",
        "PRAGMA table_xinfo(t)",
        "PRAGMA integrity_check",
    ];
    for q in queries {
        for m in [".headers always", ".mode json", ".mode line"] {
            check("names", &[":memory:", schema, m, q], "", &[], Err::Exact);
        }
    }
}

/// Dot-commands and their errors, each in one-shot mode, in a script
/// followed by a statement, and in a script against a database file
/// (which the pin opens only for the commands that read it).
#[test]
fn dot_commands_match_pin() {
    let lines = [
        ".help mode",
        ".help .mode",
        ".help m",
        ".help h",
        ".help he",
        ".help output",
        ".help once",
        ".help o",
        ".help dump",
        ".help nosuch",
        ".help e?ho",
        ".help sch",
        ".help ta",
        ".help pa",
        ".help rowid",
        ".help exit",
        ".help NULL",
        ".show",
        ".show x",
        ".mode",
        ".mode bogus",
        ".mode csv --foo",
        ".mode list extra",
        ".mode list extra more",
        ".mode insert a b",
        ".mode insert foo",
        ".mode --list",
        ".mode Csv",
        ".mode --titles",
        ".mode --titles bogus",
        ".mode --titles on",
        ".mode csv --null N --colsep ; --rowsep X",
        ".mode column --widths 1,2,-3",
        ".mode column --widths 1,x,-3",
        ".mode list -titles on",
        ".mode list --TITLES on",
        ".mode \"bogus\"",
        ".mode   bogus",
        ".mode bogus;",
        ".headers",
        ".headers on off",
        ".headers maybe",
        ".headers ON",
        ".headers 1",
        ".headers 0x10",
        ".headers -1",
        ".headers true",
        ".headers always",
        ".headers yes",
        ".headers 4294967296",
        ".separator",
        ".separator a b c",
        ".separator , X",
        ".separator \"\\t\" \"\\n\"",
        ".separator \\t",
        ".separator , ;",
        ".separator ;",
        ".nullvalue",
        ".nullvalue a b",
        ".nullvalue NIL",
        ".nullvalue 'a b'",
        ".width",
        ".width x",
        ".width 5 -3 0",
        ".width 1KB",
        ".print",
        ".print a  b \"c d\" 'e f'",
        ".print hi;",
        ".print hi;;",
        ".print hi ; ",
        ".print \"a;\"",
        ".print a;b",
        ".print \"\\x41\\101\\q\\x4g\\7z\\\"e\"",
        ".print \"a\\\\\"b",
        ".print 'a\\' b",
        ".print ''''",
        ".print \"ab\"cd ef",
        ".print \"\"",
        ".print\ttab",
        ".pri x",
        ".echo",
        ".echo maybe",
        ".echo on off",
        ".bail",
        ".bail maybe",
        ".changes",
        ".changes maybe",
        ".timer",
        ".timer bogus",
        ".output a b",
        ".once a b",
        ".once",
        ".output",
        ".read",
        ".read nosuch.sql",
        ".open --bogus",
        ".bogus",
        ".MODE",
        ".h on",
        ".he on",
        ".tim on",
        ".time on",
        ".tables a b",
        ".exit 300",
        ".exit -1",
        ".exit 0x10",
        ".exit abc",
        ".exit 2 3",
        ".quit 3",
        ".exit",
        ".quit",
        ".ex 4",
        ".q",
        ".limit",
        ".limit length",
        ".limit len",
        ".limit bogus",
        ".limit l",
        ".limit a b c",
        ".limits",
        ".lim",
        ".fullschema",
        ".fullschema x",
        ".indexes",
        ".indexes a b",
        ".indexes -a",
        ".indexes --bogus",
        ".dump --bogus",
        ".dump",
        ".dump --nosys",
        ".dump -data-only",
        ".schema",
        ".tables",
        ".parameter list",
        ".parameter bogus",
    ];
    for (k, l) in lines.iter().enumerate() {
        same(&format!("dot{k}a"), &[":memory:", l, "SELECT 'after'"]);
        same_stdin(
            &format!("dot{k}b"),
            &[":memory:"],
            &format!("{l}\nSELECT 'after';\n"),
        );
        same_stdin(&format!("dot{k}c"), &["x.db"], &format!("{l}\n"));
    }
}

/// `.help PATTERN` prints the pin's text for every command hipp has;
/// `.help` alone lists those commands' first lines, in the pin's order
/// (the pin lists its others too).
#[test]
fn help_text_is_the_pins() {
    let cmds = [
        "bail",
        "changes",
        "databases",
        "dump",
        "echo",
        "exit",
        "fullschema",
        "headers",
        "help",
        "indexes",
        "limit",
        "mode",
        "nullvalue",
        "once",
        "open",
        "output",
        "parameter",
        "print",
        "quit",
        "read",
        "schema",
        "tables",
        "timer",
    ];
    for c in cmds {
        let arg = format!(".help {c}");
        same("help", &[":memory:", &arg]);
    }
    let h = run_in(
        &hipp_bin(),
        &temp_dir("helpall-h"),
        &[":memory:", ".help"],
        "",
    );
    let o = run_in(
        &oracle(),
        &temp_dir("helpall-o"),
        &[":memory:", ".help"],
        "",
    );
    let (h, o) = (
        String::from_utf8_lossy(&h.stdout).into_owned(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    );
    let pin: Vec<&str> = o.lines().collect();
    let ours: Vec<&str> = h.lines().collect();
    assert_eq!(ours.len(), cmds.len());
    let mut at = 0;
    for l in &ours {
        let i = pin[at..]
            .iter()
            .position(|p| p == l)
            .unwrap_or_else(|| panic!("{l:?} not the pin's, in its order"));
        at += i + 1;
    }
}

/// `.dump`'s options and patterns, byte for byte.
#[test]
fn dump_options_match_pin() {
    let s1 = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); \
        INSERT INTO t VALUES(5,'x'||char(10)||'y'),(7,NULL); CREATE TABLE u(x, y); \
        INSERT INTO u VALUES(1,2),(3,'a'||char(13)||'b'); DELETE FROM u WHERE x=1; \
        INSERT INTO u VALUES(9,9); CREATE INDEX ui ON u(y); CREATE VIEW v AS SELECT * FROM u; \
        CREATE TABLE w(k INTEGER PRIMARY KEY AUTOINCREMENT, z); INSERT INTO w(z) VALUES(1); \
        CREATE TABLE r2(q PRIMARY KEY) WITHOUT ROWID; INSERT INTO r2 VALUES(1); ANALYZE u; \
        CREATE TRIGGER tr AFTER INSERT ON u BEGIN SELECT 1; END; \
        CREATE TABLE g(a, b AS (a*2), c); INSERT INTO g(a,c) VALUES(1,'p'); \
        CREATE TABLE rr(rowid, x); INSERT INTO rr VALUES('r',1); \
        CREATE TABLE rr2(rowid, oid, x); INSERT INTO rr2 VALUES(1,2,3); \
        CREATE TABLE \"we ird\"(\"col 1\", [select]); INSERT INTO \"we ird\" VALUES(1,2); \
        CREATE TABLE rr3(rowid,oid,_rowid_); INSERT INTO rr3 VALUES(1,2,3)";
    let s2 = "CREATE VIRTUAL TABLE f USING fts5(x); INSERT INTO f VALUES('hello'); \
        CREATE TABLE p(a); INSERT INTO p VALUES(1)";
    let opts = [
        "",
        "--data-only",
        "--newlines",
        "--nosys",
        "--preserve-rowids",
        "--data-only --nosys",
        "--preserve-rowids --newlines",
        "--data-only --preserve-rowids",
        "-data-only",
        "--nosys --preserve-rowids",
        "u",
        "u%",
        "%",
        "t u",
        "--data-only u",
        "w",
        "sqlite_%",
        "--nosys sqlite%",
        "ui",
        "v",
        "tr",
        "--preserve-rowids r2",
        "nosuch",
        "--preserve-rowids rr%",
        "f",
        "f%",
        "--data-only f",
        "--preserve-rowids f",
        "p",
        "--bogus",
        "--data-only --bogus",
        "u;",
        "'we ird'",
        "--nosys --data-only w",
    ];
    for s in [s1, s2] {
        for o in opts {
            let d = format!(".dump {o}");
            same("dump", &[":memory:", s, &d]);
            same_stdin("dumpin", &[":memory:"], &format!("{s};\n{d}\n"));
        }
    }
}

/// A new database file stays at 0 bytes until something is written to
/// it, as the pin leaves it (v0.17 fix: hipp wrote its 4,096-byte
/// header page when it opened the file).
#[test]
fn fresh_database_file_stays_empty_until_written() {
    for sql in [
        ";",
        "SELECT 1",
        "CREATE TABLE t(a)",
        ".tables",
        "PRAGMA user_version",
        "BEGIN; COMMIT;",
        "PRAGMA page_size=1024",
        "PRAGMA journal_mode=wal",
        "CREATE TABLE t(a); DROP TABLE t;",
        "PRAGMA user_version=0",
        "PRAGMA user_version=7",
        ".show",
        ".schema",
        ".dump",
        ".indexes",
        ".limit",
        ".fullschema",
        ".parameter list",
        ".mode",
        ".print x",
        "PRAGMA page_size=1024; CREATE TABLE t(a)",
        "VACUUM",
        "SELECT * FROM sqlite_schema",
        "ATTACH 'b.db' AS b",
        "ATTACH 'b.db' AS b; CREATE TABLE b.t(x)",
    ] {
        check("fresh", &["n.db", sql], "", &[], Err::Sql);
        check("freshin", &["n.db"], &format!("{sql}\n"), &[], Err::Sql);
    }
    // An empty file the pin made is a database to hipp, and stays empty.
    check(
        "empty",
        &["e.db", "SELECT count(*) FROM sqlite_schema"],
        "",
        &[("e.db", "")],
        Err::Exact,
    );
    check(
        "empty2",
        &[
            "e.db",
            "CREATE TABLE t(a)",
            "INSERT INTO t VALUES(1)",
            "SELECT * FROM t",
        ],
        "",
        &[("e.db", "")],
        Err::Exact,
    );
}

/// `.tables` and `.indexes` print the pin's column grid, sorted, with
/// its pattern rules (a table name LIKE the pattern; an index name, or
/// an automatic index's table, LIKE it with `%` appended).
#[test]
fn tables_and_indexes_grids_match_pin() {
    let mk = |names: &[String]| {
        names
            .iter()
            .map(|n| format!("CREATE TABLE {n}(x);"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let sets: Vec<Vec<String>> = vec![
        ["a", "bb", "ccc", "dddd", "eeeee", "ffffff"]
            .map(String::from)
            .to_vec(),
        (1..=25).map(|i| format!("t{i}")).collect(),
        (1..=40).map(|i| format!("a{i:02}")).collect(),
        (1..=11).map(|i| format!("a{i:02}")).collect(),
        (1..=20).map(|i| format!("a{i:02}")).collect(),
        vec![
            "a".repeat(20),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
            "f".into(),
            "g".into(),
            "h".into(),
            "i".into(),
            "j".into(),
            "k".into(),
            "l".into(),
            "m".into(),
            "n".into(),
            "o".into(),
            "p".into(),
        ],
        vec!["a".repeat(90), "b".into(), "c".into()],
        vec!["x".repeat(80), "y".into()],
        vec!["x".repeat(81)],
        vec![
            "Abc".into(),
            "zed".into(),
            "b".into(),
            "\"we ird\"".into(),
            "日本".into(),
        ],
    ];
    for names in &sets {
        let s = mk(names);
        same("tables", &[":memory:", &s, ".tables"]);
        same("tables", &[":memory:", &s, ".tables a%"]);
    }
    let s = "CREATE TABLE zed(x); CREATE TABLE Abc(x); CREATE TEMP TABLE tt(x); \
        CREATE TEMP VIEW tv AS SELECT 1; ATTACH ':memory:' AS aux; CREATE TABLE aux.at(x); \
        CREATE TABLE b(x); CREATE TABLE \"we ird\"(x); CREATE TABLE aux.zed(y); \
        CREATE VIEW vv AS SELECT 1";
    for a in [
        ".tables",
        ".tables aux.%",
        ".tables a%",
        ".tables temp.t%",
        ".tables a b",
        ".tables aux",
        ".tables t%",
        ".tables Z%",
        ".tables zed",
        ".tables %e%",
        ".tables _ed",
    ] {
        same("tablespat", &[":memory:", s, a]);
    }
    let s = "CREATE TABLE a(x, y UNIQUE); CREATE INDEX a1 ON a(x); CREATE INDEX a2 ON a(x); \
        CREATE INDEX a3 ON a(x+1); CREATE TABLE b(y PRIMARY KEY, z); CREATE INDEX b1 ON b(z); \
        CREATE INDEX zz ON b(z); CREATE TABLE sq(w UNIQUE); CREATE INDEX bq ON sq(w)";
    for a in [
        ".indexes",
        ".indexes a",
        ".indexes a%",
        ".indexes --expr",
        ".indexes --sys",
        ".indexes -a",
        ".indexes --all b",
        ".indexes b",
        ".indexes zz",
        ".indexes %i",
        ".indexes -a s",
        ".indexes -a sq",
        ".indexes --sys b",
        ".indexes A",
    ] {
        same("indexes", &[":memory:", s, a]);
    }
}

/// `.fullschema` and `.limit`.
#[test]
fn fullschema_and_limit_match_pin() {
    for s in [
        "CREATE TABLE a(x, y UNIQUE); CREATE INDEX a1 ON a(x); CREATE TABLE b(y PRIMARY KEY, z); \
         CREATE TEMP TABLE tt(q UNIQUE); CREATE INDEX temp.ti ON tt(q); CREATE INDEX b1 ON b(z)",
        "CREATE TABLE t(a); CREATE INDEX i ON t(a); INSERT INTO t VALUES(1),(2); ANALYZE; \
         CREATE VIEW v AS SELECT 1 -- c\n;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT); CREATE TEMP TABLE x(y); \
         CREATE TABLE u(b)",
    ] {
        same("fullschema", &[":memory:", s, ".fullschema"]);
    }
    for l in [
        ".limit",
        ".limit length",
        ".limit LENGTH",
        ".limit sql",
        ".limit t",
        ".limit trigger_d",
        ".limit worker",
        ".limit x",
    ] {
        same("limit", &[":memory:", l]);
    }
}

/// `.echo`, `.bail` and `.changes` (an SQL error's own text aside).
#[test]
fn echo_bail_changes_match_pin() {
    let sql = |tag: &str, args: &[&str], input: &str| check(tag, args, input, &[], Err::Sql);
    sql(
        "echo1",
        &[":memory:"],
        ".echo on\nSELECT 1;\n  SELECT 2; SELECT 3;\n.print hi\n-- comment\nSELECT\n 4;\n.echo off\nSELECT 5;\n",
    );
    sql(
        "echo2",
        &[":memory:"],
        ".echo on\n.bogus\nSELECT x;\n.echo off\n",
    );
    sql(
        "echo3",
        &[":memory:"],
        ".echo on\n.headers on\n.mode csv\nSELECT 1 AS a;\n#x\n\nSELECT 2;\n",
    );
    sql(
        "echo4",
        &[":memory:"],
        ".echo on\n  -- only\nSELECT 1;\n;\n  \n/* a\nb */\nSELECT 1; -- c\nSELECT 2;\n",
    );
    sql(
        "echo5",
        &[
            ":memory:",
            ".echo on",
            "SELECT 1; SELECT 2",
            ".print x",
            "SELECT 3",
        ],
        "",
    );
    sql(
        "changes1",
        &[":memory:"],
        ".changes on\nCREATE TABLE t(a);\nINSERT INTO t VALUES(1),(2);\nSELECT * FROM t;\n\
         UPDATE t SET a=3; DELETE FROM t WHERE 0;\n.print x\nSELECT 1; -- c\nSELECT 2;\n  -- only\n;\nSELECT 3\n",
    );
    sql(
        "changes2",
        &[
            ":memory:",
            ".changes on",
            "CREATE TABLE t(a)",
            "INSERT INTO t VALUES(1),(2)",
            "SELECT * FROM t",
            "UPDATE t SET a=3",
            "SELECT x",
        ],
        "",
    );
    sql(
        "bail1",
        &[":memory:"],
        ".bail on\nSELECT 1;\nSELECT x;\nSELECT 2;\n",
    );
    sql("bail2", &[":memory:"], ".bail on\n.bogus\nSELECT 2;\n");
    sql("bail3", &[":memory:"], ".bail on\n.headers\nSELECT 2;\n");
    sql(
        "bail4",
        &[":memory:"],
        ".bail on\n.headers maybe\nSELECT 2;\n",
    );
    sql(
        "bail5",
        &[":memory:", ".bail on", "SELECT x", "SELECT 2"],
        "",
    );
}

/// `.read`, `.output` and `.once`: the output files' bytes too.
#[test]
fn read_output_once_match_pin() {
    let files: &[(&str, &str)] = &[
        (
            "r1.sql",
            "SELECT 1;\n.print inread\nSELECT x;\nSELECT 2;\n.mode csv\n",
        ),
        ("r2.sql", "SELECT 9"),
        ("r3.sql", ".read r3.sql\n"),
        ("r4.sql", ".exit 4\n"),
        ("r5.sql", ".quit\n"),
        ("r6.sql", ".print a\n.mode bogus\n"),
        ("r7.sql", ".echo on\nSELECT 5;\n"),
    ];
    let c = |tag: &str, args: &[&str], input: &str, e: Err| check(tag, args, input, files, e);
    c(
        "read1",
        &[":memory:"],
        ".read r1.sql\nSELECT 3, 4;\n.read r2.sql\n.print after\n",
        Err::Sql,
    );
    c(
        "read2",
        &[":memory:", ".read r1.sql", "SELECT 3"],
        "",
        Err::Sql,
    );
    c(
        "read3",
        &[":memory:"],
        ".bail on\n.read r1.sql\nSELECT 3;\n",
        Err::Sql,
    );
    c(
        "read4",
        &[":memory:"],
        ".read r4.sql\nSELECT 3;\n",
        Err::Exact,
    );
    c(
        "read5",
        &[":memory:"],
        ".read r5.sql\nSELECT 3;\n",
        Err::Exact,
    );
    c(
        "read6",
        &[":memory:"],
        ".echo on\n.read r2.sql\n",
        Err::Exact,
    );
    c(
        "read7",
        &[":memory:"],
        ".read r3.sql\nSELECT 1;\n",
        Err::Exact,
    );
    c("read8", &[":memory:"], "\n.read r6.sql\n", Err::Exact);
    c(
        "read9",
        &[":memory:", ".print x", ".read r6.sql"],
        "",
        Err::Exact,
    );
    c(
        "read10",
        &[":memory:"],
        ".read r7.sql\nSELECT 6;\n",
        Err::Exact,
    );
    c(
        "read11",
        &[":memory:"],
        ".read nosuch.sql\nSELECT 1;\n",
        Err::Exact,
    );
    c("read12", &["x.db", ".read r2.sql"], "", Err::Exact);
    c("read13", &["x.db"], ".read r5.sql\n", Err::Exact);
    c(
        "read14",
        &[":memory:"],
        ".read '|echo SELECT 42;'\n",
        Err::Exact,
    );
    c(
        "out1",
        &[":memory:"],
        ".output f1\nSELECT 1;\n.print p\n.show\n.output\nSELECT 2;\n.once f2\nSELECT 3; SELECT 4;\n\
         SELECT 5;\n.once f3\n.print q\n.print r\n.output f4\n.output f4\nSELECT 6;\n.output f5 \n\
         .print x\n.output stdout\n.print y\n.output \"f 6\"\n.print z\n.output\n.once\n.print w\n\
         .output nodir/x\n.print v\n.once nodir/y\n.print u\n",
        Err::Exact,
    );
    c(
        "out2",
        &[
            ":memory:",
            ".once o1",
            "SELECT 1",
            "SELECT 2",
            ".output o2",
            ".print a",
            ".mode csv",
            "SELECT 3",
            ".output",
            ".print b",
        ],
        "",
        Err::Exact,
    );
    c(
        "out3",
        &[":memory:"],
        ".output o1\n.changes on\nCREATE TABLE t(a);\n.echo on\nSELECT 1;\n.output\n",
        Err::Exact,
    );
    c(
        "out4",
        &[":memory:"],
        ".once o1\n.output o2\n.print a\n",
        Err::Exact,
    );
    c(
        "out5",
        &[":memory:"],
        ".output o1\n.once o2\n.print a\n.print b\n",
        Err::Exact,
    );
    c(
        "out6",
        &[":memory:"],
        ".once o1\nSELECT x;\nSELECT 2;\n",
        Err::Sql,
    );
    c(
        "out7",
        &[":memory:"],
        ".output off\nSELECT 1;\n.show\n.output\nSELECT 2;\n",
        Err::Exact,
    );
    c(
        "out8",
        &[":memory:"],
        ".output |cat\nSELECT 1;\n.output\nSELECT 2;\n",
        Err::Exact,
    );
    c(
        "out9",
        &[":memory:"],
        ".once '|tr a-z A-Z'\nSELECT 'abc';\nSELECT 'def';\n",
        Err::Exact,
    );
}

/// `.timer`: a `Run Time:` line after each SQL chunk (its numbers are
/// the clock's), the same lines as the pin's, on the same output.
#[test]
fn timer_lines_follow_the_pins() {
    let script = ".timer on\nSELECT 1;\nSELECT 2; SELECT 3;\n.print x\n.timer once\nSELECT 4;\nSELECT 5;\n.timer off\nSELECT 6;\n";
    let shape = |out: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(out)
            .lines()
            .map(|l| {
                if let Some(rest) = l.strip_prefix("Run Time: real ") {
                    let parts: Vec<&str> = rest.split(' ').collect();
                    assert_eq!(parts.len(), 5, "{l:?}");
                    assert_eq!((parts[1], parts[3]), ("user", "sys"), "{l:?}");
                    for n in [parts[0], parts[2], parts[4]] {
                        let (i, f) = n.split_once('.').expect("decimal");
                        assert!(i.bytes().all(|c| c.is_ascii_digit()), "{l:?}");
                        assert_eq!(f.len(), 6, "{l:?}");
                    }
                    "Run Time".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect()
    };
    let h = run_in(&hipp_bin(), &temp_dir("timer-h"), &[":memory:"], script);
    let o = run_in(&oracle(), &temp_dir("timer-o"), &[":memory:"], script);
    assert_eq!(shape(&h.stdout), shape(&o.stdout));
    assert_eq!(h.status.code(), o.status.code());
    let args = [
        ":memory:",
        ".timer on",
        "SELECT 1",
        ".print x",
        "SELECT 2; SELECT 3",
    ];
    let h = run_in(&hipp_bin(), &temp_dir("timer2-h"), &args, "");
    let o = run_in(&oracle(), &temp_dir("timer2-o"), &args, "");
    assert_eq!(shape(&h.stdout), shape(&o.stdout));
}

/// `-version` keeps the pin's layout — `VERSION DATE TIME SOURCE-ID
/// (NN-bit)` — with hipp's name in the source id's place.
#[test]
fn version_line_has_the_pins_layout() {
    let h = run_in(&hipp_bin(), &temp_dir("ver-h"), &["-version"], "");
    let o = run_in(&oracle(), &temp_dir("ver-o"), &["-version"], "");
    let (h, o) = (
        String::from_utf8_lossy(&h.stdout).into_owned(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    );
    let hw: Vec<&str> = h.split_whitespace().collect();
    let ow: Vec<&str> = o.split_whitespace().collect();
    assert_eq!(hw.len(), ow.len(), "{h:?} / {o:?}");
    assert_eq!(hw[0], ow[0]);
    assert_eq!(hw[1..3], ow[1..3]);
    assert_eq!(hw[4], ow[4]);
    assert_eq!(hw[3], concat!("hipp-", env!("CARGO_PKG_VERSION")));
    assert!(h.ends_with("-bit)\n"));
}
