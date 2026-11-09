//! v0.17 review fixes to `PRAGMA integrity_check`'s row checks,
//! differentially vs the pinned oracle (SQLite 3.54.1, live). Each
//! case writes a file with the pin, then edits the stored schema
//! text in place (same length, so the file stays well formed) so the
//! table's rows no longer agree with it, and compares both engines'
//! integrity / quick check of copies of the file.
//!
//! - An index's findings name that index: the plans and the names
//!   are both in the pin's `pIndex` order (newest first). hipp
//!   reversed the plans twice, so "row N missing from index X"
//!   named another index of the table (a WITHOUT ROWID table's
//!   broken index came out as its `sqlite_autoindex`).
//! - A partial index has no entry count: the pin reports only each
//!   admitted row's missing entry, never "wrong # of entries".
//! - A non-STRICT column's stored type against its affinity (the
//!   pin's checks 4 and 5): "NUMERIC value in t.c" for a number in
//!   a TEXT column, "TEXT value in t.c" for text a NUMERIC
//!   conversion takes in an INTEGER / REAL / NUMERIC column. hipp
//!   checks a table with a typed column row by row now.
//! - Not modelled (STATUS.md, v0.17 honest gap): the pin's column
//!   checks read the record header's types (`OP_IsType`) and load a
//!   value only to look closer (a NOT NULL column's NULL or REAL, a
//!   numeric column's TEXT). After a finding the cursor's header
//!   cache is stale, and the next load re-parses the header only up
//!   to its own column, so a later column of that row reads as its
//!   default's type and its type finding is skipped. hipp reports
//!   it. The generated corpus edits one column per table, so a row
//!   holds at most one column finding.
//! - NOT NULL, STRICT and CHECK findings reached the same way
//!   (the stored CREATE TABLE text edited), live.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, FileStorage, Step};

/// Run `script` with hipp the way the CLI does: statements in
/// order, every row printed, stopping at the first error.
fn hipp_lines(path: &std::path::Path, script: &str) -> (bool, Vec<String>) {
    block_on(async {
        let mut db = Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
            .await
            .expect("open");
        let mut lines = Vec::new();
        for stmt in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let Ok(mut q) = db.prepare(stmt).await else {
                return (false, lines);
            };
            loop {
                match q.step().await {
                    Ok(Step::Row) => match q.column_value(0) {
                        Some(hipp::Value::Text(b)) => {
                            for part in String::from_utf8_lossy(b).split('\n') {
                                lines.push(part.to_string());
                            }
                        }
                        v => lines.push(format!("{v:?}")),
                    },
                    Ok(Step::Done) => break,
                    Err(_) => return (false, lines),
                }
            }
        }
        (true, lines)
    })
}

fn oracle_lines(path: &std::path::Path, script: &str) -> (bool, Vec<String>) {
    let out = std::process::Command::new(common::oracle())
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

/// The pin writes `setup`; each `(from, to)` edit replaces `from`
/// (which must occur) with `to` padded with spaces to its length.
fn patched(tag: &str, setup: &str, edits: &[(&str, &str)]) -> Vec<u8> {
    let p = common::temp_db(&format!("v17ic-{tag}"));
    let (ok, _) = oracle_lines(&p, setup);
    assert!(ok, "{tag}: {setup}");
    let mut d = std::fs::read(&p).expect("read");
    for (from, to) in edits {
        let (f, t) = (from.as_bytes(), to.as_bytes());
        assert!(t.len() <= f.len(), "{tag}: {to:?} longer than {from:?}");
        let mut t = t.to_vec();
        t.resize(f.len(), b' ');
        let k = d
            .windows(f.len())
            .position(|w| w == f)
            .unwrap_or_else(|| panic!("{tag}: {from:?} not in the file"));
        d[k..k + f.len()].copy_from_slice(&t);
    }
    d
}

const CHECKS: [&str; 4] = [
    "PRAGMA integrity_check",
    "PRAGMA quick_check",
    "PRAGMA integrity_check(2)",
    "PRAGMA integrity_check(t)",
];

/// Both engines' checks of copies of the edited file agree; the
/// full check's lines are returned.
fn check_both(tag: &str, file: &[u8], ctx: &str) -> Vec<String> {
    let mut full = Vec::new();
    for (k, q) in CHECKS.iter().enumerate() {
        let h = common::temp_db(&format!("v17ic-{tag}-{k}-h"));
        let o = common::temp_db(&format!("v17ic-{tag}-{k}-o"));
        std::fs::write(&h, file).expect("write");
        std::fs::write(&o, file).expect("write");
        let (hok, hl) = hipp_lines(&h, q);
        let (ook, ol) = oracle_lines(&o, q);
        assert_eq!(hl, ol, "{tag}: {q} ({ctx})");
        assert_eq!(hok, ook, "{tag}: {q} success differs ({ctx})");
        if k == 0 {
            full = hl;
        }
    }
    full
}

fn case(tag: &str, setup: &str, edits: &[(&str, &str)]) -> Vec<String> {
    check_both(
        tag,
        &patched(tag, setup, edits),
        &format!("{setup} {edits:?}"),
    )
}

#[test]
fn findings_name_the_broken_index() {
    // The reviewer's case: i1's stored key moves to a column whose
    // values its entries do not hold.
    let l = case(
        "two",
        "CREATE TABLE t(a, b, c); CREATE INDEX i1 ON t(a); CREATE INDEX i2 ON t(c); \
         INSERT INTO t VALUES (1, 2, 3), (3, 4, 5);",
        &[("i1 ON t(a)", "i1 ON t(b)")],
    );
    assert_eq!(
        l,
        ["row 1 missing from index i1", "row 2 missing from index i1"]
    );
    // The newer one broken, then two of three (the pin's newest-first
    // order across indexes, per row).
    case(
        "newer",
        "CREATE TABLE t(a, b, c); CREATE INDEX i1 ON t(a); CREATE INDEX i2 ON t(c); \
         INSERT INTO t VALUES (1, 2, 3), (3, 4, 5);",
        &[("i2 ON t(c)", "i2 ON t(b)")],
    );
    case(
        "three",
        "CREATE TABLE t(a, b, c); CREATE INDEX i1 ON t(a); CREATE INDEX i2 ON t(c); \
         CREATE INDEX i3 ON t(a, c); INSERT INTO t VALUES (1, 2, 3), (3, 4, 5);",
        &[("i1 ON t(a)", "i1 ON t(b)"), ("i2 ON t(c)", "i2 ON t(a)")],
    );
    // A UNIQUE constraint's autoindex beside a named index.
    case(
        "auto",
        "CREATE TABLE t(a UNIQUE, b, c); CREATE INDEX i ON t(b); \
         INSERT INTO t VALUES (1, 2, 3), (3, 4, 5);",
        &[("i ON t(b)", "i ON t(c)")],
    );
    // A WITHOUT ROWID table: the broken secondary index, not the
    // primary key's tree.
    let l = case(
        "wr",
        "CREATE TABLE t(a PRIMARY KEY, b, c) WITHOUT ROWID; CREATE INDEX i ON t(b); \
         INSERT INTO t VALUES (1, 2, 3), (3, 4, 5);",
        &[("i ON t(b)", "i ON t(c)")],
    );
    assert_eq!(
        l,
        ["row 1 missing from index i", "row 2 missing from index i"]
    );
    case(
        "wr2",
        "CREATE TABLE t(a PRIMARY KEY, b, c) WITHOUT ROWID; CREATE INDEX i ON t(b); \
         CREATE INDEX j ON t(c); INSERT INTO t VALUES (1, 2, 3), (3, 4, 5);",
        &[("i ON t(b)", "i ON t(a)")],
    );
}

#[test]
fn partial_index_has_no_entry_count() {
    let s = "CREATE TABLE t(a, b); CREATE INDEX i ON t(a) WHERE b>00; \
             INSERT INTO t VALUES (1, 2), (3, -4), (5, 6);";
    // More rows admitted than entries: the missing one only.
    let l = case("more", s, &[("WHERE b>00", "WHERE b<99")]);
    assert_eq!(l, ["row 2 missing from index i"]);
    case("neg", s, &[("WHERE b>00", "WHERE b>-9")]);
    case("less", s, &[("WHERE b>00", "WHERE b<00")]);
    // Fewer admitted than entries: nothing to report.
    let l = case("none", s, &[("WHERE b>00", "WHERE b>09")]);
    assert_eq!(l, ["ok"]);
    case("one", s, &[("WHERE b>00", "WHERE b=02")]);
    // Two partial indexes trading predicates.
    case(
        "swap",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(a) WHERE b>00; \
         CREATE INDEX j ON t(a) WHERE b>-9; INSERT INTO t VALUES (1, 2), (3, -4), (5, 6);",
        &[
            ("i ON t(a) WHERE b>00", "i ON t(a) WHERE b>-9"),
            ("j ON t(a) WHERE b>-9", "j ON t(a) WHERE b>00"),
        ],
    );
}

#[test]
fn stored_type_against_affinity() {
    // The reviewer's case: text that converts, in an INT column.
    let l = case(
        "int",
        "CREATE TABLE t(a BLOB, b); CREATE INDEX i ON t(a); INSERT INTO t VALUES ('1', 1), ('2', 2);",
        &[("(a BLOB,", "(a INT,")],
    );
    assert_eq!(l, ["TEXT value in t.a", "TEXT value in t.a"]);
    // No index: the table is still checked row by row (and by
    // quick_check).
    let l = case(
        "noidx",
        "CREATE TABLE t(a BLOB, b); INSERT INTO t VALUES ('1', 1), ('x', 2), (3, 3);",
        &[("(a BLOB,", "(a NUM,")],
    );
    assert_eq!(l, ["TEXT value in t.a"]);
    // Numbers in a TEXT column; a blob and NULL pass.
    let l = case(
        "text",
        "CREATE TABLE t(a INT , b UNIQUE); \
         INSERT INTO t VALUES (1, 1), (2.5, 2), (x'01', 3), (NULL, 4), ('z', 5);",
        &[("(a INT ,", "(a TEXT,")],
    );
    assert_eq!(l, ["NUMERIC value in t.a", "NUMERIC value in t.a"]);
    // What NUMERIC conversion takes, and what it leaves text.
    case(
        "conv",
        "CREATE TABLE t(a BLOB, b UNIQUE); INSERT INTO t VALUES ('1', 1), ('2.5', 2), ('abc', 3), \
         (' 7 ', 4), ('1e400', 5), ('0x10', 6), ('', 7), ('1.', 8), ('.5', 9), ('-0', 10), \
         ('+3', 11), (char(49, 0, 50), 12), ('inf', 13), ('1e', 14), ('9223372036854775808', 15), \
         (' ', 16), ('1 2', 17), ('1_000', 18), (char(12288) || '3', 19);",
        &[("(a BLOB,", "(a REAL,")],
    );
    // Several columns, NOT NULL and the error budget interleaved;
    // the rowid alias is never checked.
    case(
        "mix",
        "CREATE TABLE t(a BLOB, b BLOB, c BLOB /*xxxxxxxx*/, d INTEGER PRIMARY KEY); \
         INSERT INTO t VALUES ('1', 2, NULL, NULL), ('x', 2.5, '3', NULL), (1, '2', NULL, NULL);",
        &[
            ("c BLOB /*xxxxxxxx*/,", "c INT NOT NULL,"),
            ("a BLOB", "a INT"),
            ("b BLOB", "b TEXT"),
        ],
    );
    // Generated columns (VIRTUAL, STORED) take their declared
    // affinity; a NOT NULL beside them.
    case(
        "gen",
        "CREATE TABLE g(a INT, b BLOB AS (a * 2) VIRTUAL, c BLOB AS (a || '') STORED, \
         d BLOB AS (a || ''), x BLOB /*xxxxxx*/); \
         INSERT INTO g(a, x) VALUES (1, '1'), (2.5, 2), ('7', NULL);",
        &[
            ("b BLOB", "b TEXT"),
            ("c BLOB", "c INT"),
            ("d BLOB", "d REAL"),
            ("x BLOB /*xxxxxx*/", "x INT NOT NULL"),
        ],
    );
    // A STRICT table made non-STRICT: its ANY column is NUMERIC
    // now. And a STRICT table's own findings, never both kinds.
    case(
        "unstrict",
        "CREATE TABLE t(a ANY, b TEXT, c INT, d INT) STRICT; CREATE INDEX si ON t(d); \
         INSERT INTO t VALUES ('5', '6', 7, 1), (8, 'x', NULL, 2);",
        &[(") STRICT", ")")],
    );
    case(
        "strict",
        "CREATE TABLE t(a ANY, b TEXT, c INT, d INT) STRICT; CREATE INDEX si ON t(d); \
         INSERT INTO t VALUES ('5', '6', 7, 1), (8, 'x', NULL, 2);",
        &[("b TEXT", "b INT"), ("c INT", "c ANY")],
    );
    // A WITHOUT ROWID table's columns.
    case(
        "wr",
        "CREATE TABLE t(a PRIMARY KEY, b BLOB /*xxxxxx*/) WITHOUT ROWID; \
         INSERT INTO t VALUES ('1', 2), (2, '3');",
        &[(
            "(a PRIMARY KEY, b BLOB /*xxxxxx*/)",
            "(a INT PRIMARY KEY, b TEXT)",
        )],
    );
    // A column added by ALTER TABLE reads its default, converted.
    case(
        "dflt",
        "CREATE TABLE t(a UNIQUE, b); INSERT INTO t VALUES (1, 2); \
         ALTER TABLE t ADD COLUMN g BLOB DEFAULT '5'; ALTER TABLE t ADD COLUMN h BLOB DEFAULT 5;",
        &[
            ("g BLOB DEFAULT", "g INT DEFAULT"),
            ("h BLOB DEFAULT", "h TEXT DEFAULT"),
        ],
    );
}

#[test]
fn not_null_strict_and_check_live() {
    let l = case(
        "nn",
        "CREATE TABLE t(a INT, b TEXT /*xxxxxxx*/); INSERT INTO t VALUES (1, NULL);",
        &[("b TEXT /*xxxxxxx*/", "b TEXT NOT NULL")],
    );
    assert_eq!(l, ["NULL value in t.b"]);
    case(
        "strict",
        "CREATE TABLE t(a INT, b TEXT, c REAL /*zzzzzzzz*/); \
         INSERT INTO t VALUES (1, 2, 'x'), ('y', 3.5, 4), (NULL, x'00', 2);",
        &[("c REAL /*zzzzzzzz*/)", "c REAL) STRICT--")],
    );
    case(
        "check",
        "CREATE TABLE t(a, b /*xxxxxxxxxxxx*/); INSERT INTO t VALUES (1, 2), (3, 4), (5, 6);",
        &[("b /*xxxxxxxxxxxx*/", "b CHECK (b <> 4)")],
    );
}

/// A small deterministic PRNG (xorshift64).
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
}

const VALUES: [&str; 16] = [
    "NULL",
    "1",
    "-7",
    "2.5",
    "1e3",
    "'1'",
    "' 7 '",
    "'2.5'",
    "'abc'",
    "'0x10'",
    "''",
    "x'01'",
    "'1e400'",
    "'+3'",
    "zeroblob(2)",
    "'-0'",
];
const DECLS: [&str; 14] = [
    "TEXT",
    "INT",
    "REAL",
    "NUM",
    "BLOB",
    "INT NOT NULL",
    "",
    "INTEGER NOT NULL",
    "TEXT NOT NULL",
    "VARCHAR(5)",
    "DOUBLE",
    "NUMERIC",
    "FLOAT NOT NULL",
    "NOT NULL",
];

/// Generated tables: up to four BLOB columns, one of them edited to
/// a random declared type (so a row has at most one column finding:
/// see the module doc on the pin's header re-parse), random values,
/// and one to three indexes (so the table is checked row by row),
/// some edited to a column their entries do not hold.
#[test]
fn generated_corpus() {
    let mut rng = Rng(0x1d2c_3b4a_5968_7786);
    for n in 0..120 {
        let ncols = 1 + rng.below(4);
        let mut cols = Vec::new();
        let mut edits: Vec<(String, String)> = Vec::new();
        for c in 0..ncols {
            // `cN BLOB /*xxxxxxxx*/` leaves room for every edit.
            cols.push(format!("c{c} BLOB /*xxxxxxxx*/"));
        }
        let c = rng.below(ncols);
        let d = DECLS[rng.below(DECLS.len())];
        edits.push((format!("c{c} BLOB /*xxxxxxxx*/"), format!("c{c} {d}")));
        let mut setup = format!("CREATE TABLE t({});", cols.join(", "));
        let nix = 1 + rng.below(3);
        for i in 0..nix {
            let a = rng.below(ncols);
            setup.push_str(&format!(" CREATE INDEX i{i} ON t(c{a});"));
            if rng.below(3) == 0 {
                let b = rng.below(ncols);
                edits.push((format!("i{i} ON t(c{a})"), format!("i{i} ON t(c{b})")));
            }
        }
        for _ in 0..1 + rng.below(5) {
            let row: Vec<&str> = (0..ncols)
                .map(|_| VALUES[rng.below(VALUES.len())])
                .collect();
            setup.push_str(&format!(" INSERT INTO t VALUES ({});", row.join(", ")));
        }
        let e: Vec<(&str, &str)> = edits
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        case(&format!("gen{n}"), &setup, &e);
    }
}
