//! v0.16 PRAGMA statements and `sqlite_schema`, differentially
//! vs the pinned oracle (SQLite 3.54.1, live). Probed on the pin
//! before implementation; the matrix the tests encode:
//!
//! - Grammar: `PRAGMA [main.]name [= value | (value)]`; the value
//!   is a signed number, a name or string, or ON / DELETE /
//!   DEFAULT; NULL, expressions, `()` and `1_000` are syntax
//!   errors; an unknown schema is an error, an unknown pragma
//!   name is silently ignored; a name the pin knows but hipp does
//!   not implement is refused (`Error::Unsupported`), as is
//!   anything against the (absent) temp database.
//! - Header values: `user_version` / `application_id` read and
//!   write the header (the pin's `sqlite3Atoi`: sign, hex, digits
//!   to the first non-digit, overflow 0), inside transactions;
//!   `page_size` changes only a pristine database; `page_count`
//!   is 0 there; `schema_version` writes are no-ops (the pin's
//!   shell is defensive); `encoding`, `auto_vacuum`,
//!   `journal_mode`, `synchronous`, `cache_size` report what hipp
//!   does and refuse what it cannot honour.
//! - Flags: `foreign_keys` (a no-op inside a transaction),
//!   `recursive_triggers`, `ignore_check_constraints` (skips
//!   CHECK on writes and in the integrity check),
//!   `case_sensitive_like` (changes LIKE and `like()`; no getter),
//!   `busy_timeout`; boolean words as the pin's `getSafetyLevel`.
//! - Introspection: `table_info`, `table_xinfo`, `index_list`,
//!   `index_info`, `index_xinfo`, `foreign_key_list`,
//!   `table_list`, `database_list` byte for byte (generated
//!   corpora), `collation_list` (hipp's three).
//! - `integrity_check` / `quick_check`: "ok", and the pin's
//!   message shapes, order and limit for corruptions built by
//!   editing oracle-written files.
//! - `sqlite_schema` / `sqlite_master` read as a table; writes
//!   refused; `sqlite_*` names reserved (ungated); the table-
//!   valued `pragma_*` functions in FROM.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
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
}

/// Split a script on `;` outside quotes (the corpora quote
/// nothing that holds `;` but strings).
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

/// Run a script on one hipp connection the way the CLI does:
/// statements in order, every row printed, stopping at the first
/// error. (ok, output lines, the error).
fn hipp_script(
    path: &std::path::Path,
    script: &str,
) -> (bool, Vec<String>, Option<Error<FsError>>) {
    block_on(async {
        let mut db = open(path).await;
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
                        // A multi-line value prints as lines.
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

fn oracle_script(path: &std::path::Path, script: &str) -> (bool, Vec<String>) {
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

/// A script on fresh twin files: same rows, same success.
/// Returns hipp's error, if both failed.
fn same(tag: &str, script: &str) -> Option<Error<FsError>> {
    let h = common::temp_db(&format!("v16pr-{tag}-h"));
    let o = common::temp_db(&format!("v16pr-{tag}-o"));
    same_on(&h, &o, script)
}

fn same_on(h: &std::path::Path, o: &std::path::Path, script: &str) -> Option<Error<FsError>> {
    let (hok, hl, he) = hipp_script(h, script);
    let (ook, ol) = oracle_script(o, script);
    assert_eq!(hl, ol, "script: {script}\nhipp error: {he:?}");
    assert_eq!(hok, ook, "success differs: {script}\nhipp error: {he:?}");
    he
}

/// Both refuse; hipp with schema error `kind`.
fn refuse(tag: &str, script: &str, kind: SchemaKind) {
    match same(tag, script) {
        Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "{script}"),
        other => panic!("expected {kind:?} for {script}, got {other:?}"),
    }
}

/// hipp refuses with `Error::Unsupported` what the oracle runs
/// (an honest refusal: hipp cannot honour it).
fn hipp_refuses(tag: &str, script: &str) {
    let h = common::temp_db(&format!("v16pr-{tag}-h"));
    let o = common::temp_db(&format!("v16pr-{tag}-o"));
    let (hok, _, he) = hipp_script(&h, script);
    let (ook, _) = oracle_script(&o, script);
    assert!(ook, "the oracle refused {script}");
    assert!(!hok, "hipp ran {script}");
    assert!(matches!(he, Some(Error::Unsupported)), "{script}: {he:?}");
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

// ---------------- grammar ----------------

#[test]
fn grammar_and_unknown_names() {
    for (i, s) in [
        "PRAGMA user_version",
        "PRAGMA 'user_version'; PRAGMA \"user_version\"; PRAGMA [user_version]",
        "PRAGMA user_version;;",
        "PRAGMA USER_VERSION; PRAGMA main.user_version; PRAGMA MAIN.user_version",
        "PRAGMA nosuch; PRAGMA nosuch = 5; PRAGMA nosuch(abc); PRAGMA main.nosuch; PRAGMA temp.nosuch",
        "PRAGMA user_version = \"7\"; PRAGMA user_version; PRAGMA user_version = [8]; PRAGMA user_version",
        "PRAGMA user_version = ON; PRAGMA user_version; PRAGMA user_version = DELETE; PRAGMA user_version = DEFAULT; PRAGMA user_version",
        "PRAGMA user_version = abc; PRAGMA user_version; PRAGMA user_version = 'x y'; PRAGMA user_version",
        "PRAGMA user_version = - 5; PRAGMA user_version; PRAGMA user_version = +6; PRAGMA user_version",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("g{i}"), s).is_none(), "{s}");
    }
    // Syntax errors on both.
    for (i, s) in [
        "PRAGMA",
        "PRAGMA user_version =",
        "PRAGMA user_version = NULL",
        "PRAGMA user_version = 1+1",
        "PRAGMA user_version = (1)",
        "PRAGMA user_version()",
        "PRAGMA user_version(1, 2)",
        "PRAGMA table_info(t) = 5",
        "PRAGMA user_version = -'5'",
        "PRAGMA user_version = x'01'",
        "PRAGMA user_version = 1_000",
        "PRAGMA user_version = SELECT",
        "PRAGMA a.b.c",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            matches!(same(&format!("ge{i}"), s), Some(Error::Parse { .. })),
            "{s}"
        );
    }
    refuse("gdb", "PRAGMA x.user_version", SchemaKind::UnknownDatabase);
    refuse("gdb2", "PRAGMA aux.nosuch", SchemaKind::UnknownDatabase);
}

#[test]
fn known_but_unimplemented_pragmas_are_refused() {
    for (i, name) in [
        "data_version",
        "max_page_count",
        "locking_mode",
        "temp_store",
        "secure_delete",
        "mmap_size",
        "function_list",
        "compile_options",
        "pragma_list",
        // Implemented by the `foreign_keys` leaf.
        if cfg!(feature = "foreign_keys") {
            "optimize"
        } else {
            "foreign_key_check"
        },
        "wal_checkpoint",
        "query_only",
        "writable_schema",
        "optimize",
    ]
    .iter()
    .enumerate()
    {
        hipp_refuses(&format!("ref{i}"), &format!("PRAGMA {name}"));
    }
    // The TEMP database (v0.16, tests/temp_v16.rs): a known pragma
    // against it runs there, as on the pin (refused before it).
    assert!(same("reftemp", "PRAGMA temp.user_version").is_none());
    assert!(same("reftemp2", "CREATE TABLE t(a); PRAGMA temp.table_info(t)").is_none());
    assert!(
        same(
            "reftemp3",
            "PRAGMA temp.user_version = 5; PRAGMA temp.user_version; PRAGMA user_version"
        )
        .is_none()
    );
}

// ---------------- header values ----------------

#[test]
fn header_values_read_and_write() {
    let mut rng = Rng(0x5eed_0016_0001);
    let fixed = [
        "7",
        "-3",
        "+3",
        "'12'",
        "' 5'",
        "'5x'",
        "1.9",
        "1e3",
        "0x10",
        "'0x10'",
        "'0XfF'",
        "0x7fffffff",
        "0x80000000",
        "'0x'",
        "2147483647",
        "2147483648",
        "-2147483648",
        "-2147483649",
        "4294967297",
        "9223372036854775807",
        "9223372036854775808",
        "'abc'",
        "abc",
        "ON",
        "'-0'",
        "'00012'",
        "'+-1'",
        "'1 2'",
        "0",
        "'-0x10'",
    ];
    let mut vals: Vec<String> = fixed.iter().map(|s| s.to_string()).collect();
    for _ in 0..60 {
        let v = (rng.next() as i64) >> rng.below(63);
        vals.push(match rng.below(3) {
            0 => v.to_string(),
            1 => format!("'{v}'"),
            _ => format!("'{v}{}'", rng.pick(&["x", " ", ".5", "e2", ""])),
        });
    }
    for (i, chunk) in vals.chunks(10).enumerate() {
        let mut s = String::new();
        for v in chunk {
            s.push_str(&format!(
                "PRAGMA user_version = {v}; PRAGMA user_version; PRAGMA application_id({v}); PRAGMA application_id; "
            ));
        }
        assert!(same(&format!("hv{i}"), &s).is_none(), "{s}");
    }
    // Transactions: a rollback undoes the write; the oracle reads
    // the hipp-written header.
    let h = common::temp_db("v16pr-hvtx-h");
    let o = common::temp_db("v16pr-hvtx-o");
    let s = "BEGIN; PRAGMA user_version = 3; PRAGMA user_version; ROLLBACK; PRAGMA user_version; \
             BEGIN; PRAGMA user_version = 4; CREATE TABLE t(a); PRAGMA application_id = -9; COMMIT; \
             PRAGMA user_version; PRAGMA application_id; PRAGMA schema_version";
    assert!(same_on(&h, &o, s).is_none());
    let q =
        "PRAGMA user_version; PRAGMA application_id; PRAGMA schema_version; PRAGMA integrity_check";
    assert_eq!(oracle_script(&h, q), oracle_script(&o, q));
    assert_eq!(oracle_script(&h, q).1, ["4", "-9", "1", "ok"]);
    // Read-only counters ignore an argument; schema_version
    // writes do nothing (the pin's defensive shell).
    for (i, s) in [
        "PRAGMA page_size; PRAGMA page_count; PRAGMA freelist_count; PRAGMA schema_version",
        "CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(30000)); PRAGMA page_count; DELETE FROM t; \
         PRAGMA freelist_count; PRAGMA page_count; PRAGMA page_count = 3; PRAGMA freelist_count = 9; \
         PRAGMA schema_version; PRAGMA schema_version = 77; PRAGMA schema_version; SELECT * FROM t",
        "PRAGMA encoding; PRAGMA journal_mode; PRAGMA synchronous; PRAGMA auto_vacuum; \
         PRAGMA busy_timeout; PRAGMA foreign_keys; PRAGMA recursive_triggers; \
         PRAGMA ignore_check_constraints; PRAGMA case_sensitive_like",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("hr{i}"), s).is_none(), "{s}");
    }
}

#[test]
fn page_size_encoding_and_auto_vacuum_on_a_fresh_database() {
    for (i, n) in [
        "512", "1024", "2048", "8192", "16384", "32768", "65536", "1000", "0", "-4096", "100000",
        "'4096'",
    ]
    .iter()
    .enumerate()
    {
        let h = common::temp_db(&format!("v16pr-ps{i}-h"));
        let o = common::temp_db(&format!("v16pr-ps{i}-o"));
        let s = format!(
            "PRAGMA page_size = {n}; PRAGMA page_size; PRAGMA page_count; CREATE TABLE t(a, b); \
             INSERT INTO t VALUES (1, zeroblob(3000)), (2, 'x'); PRAGMA page_size; PRAGMA page_count; \
             PRAGMA page_size = 4096; PRAGMA page_size; SELECT a, length(b) FROM t"
        );
        assert!(same_on(&h, &o, &s).is_none(), "{s}");
        // The file itself: the oracle reads hipp's page size.
        let q = "PRAGMA page_size; PRAGMA integrity_check; SELECT a FROM t";
        assert_eq!(oracle_script(&h, q), oracle_script(&o, q), "{s}");
    }
    for (i, s) in [
        "PRAGMA encoding = 'UTF-8'; PRAGMA encoding; PRAGMA encoding = utf8; PRAGMA encoding",
        "CREATE TABLE t(a); PRAGMA encoding = 'UTF-16'; PRAGMA encoding = 'nonsense'; PRAGMA encoding",
        "PRAGMA auto_vacuum = 0; PRAGMA auto_vacuum = none; PRAGMA auto_vacuum = 3; PRAGMA auto_vacuum = abc; PRAGMA auto_vacuum",
        "CREATE TABLE t(a); PRAGMA auto_vacuum = 1; PRAGMA auto_vacuum = incremental; PRAGMA auto_vacuum",
        "PRAGMA journal_mode = delete; PRAGMA journal_mode = DELETE; PRAGMA journal_mode = bogus; PRAGMA journal_mode(delete); PRAGMA main.journal_mode",
        "PRAGMA synchronous = FULL; PRAGMA synchronous = 2; PRAGMA synchronous",
        "PRAGMA busy_timeout = 100; PRAGMA busy_timeout; PRAGMA busy_timeout = -5; PRAGMA busy_timeout(2147483648); PRAGMA busy_timeout(77)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("pe{i}"), s).is_none(), "{s}");
    }
    refuse(
        "pebad",
        "PRAGMA encoding = 'nonsense'",
        SchemaKind::UnknownEncoding,
    );
    // What hipp cannot honour is refused, never pretended (a UTF-16
    // encoding is honoured with the `utf16` leaf: tests/utf16_v16.rs).
    for (i, s) in [
        #[cfg(not(feature = "utf16"))]
        "PRAGMA encoding = 'UTF-16le'",
        "PRAGMA auto_vacuum = 1",
        "PRAGMA auto_vacuum = incremental",
        "PRAGMA journal_mode = wal",
        "PRAGMA journal_mode = memory",
        "PRAGMA journal_mode = off",
        "PRAGMA journal_mode = truncate",
        "PRAGMA synchronous = OFF",
        "PRAGMA synchronous = normal",
        "PRAGMA synchronous = extra",
        "PRAGMA cache_size = -8000",
    ]
    .iter()
    .enumerate()
    {
        let h = common::temp_db(&format!("v16pr-hon{i}-h"));
        let (ok, _, e) = hipp_script(&h, s);
        assert!(!ok, "hipp pretended: {s}");
        assert!(
            matches!(e, Some(Error::Unsupported | Error::UnsupportedEncoding)),
            "{s}: {e:?}"
        );
    }
}

// ---------------- flags ----------------

#[test]
fn flags_and_their_words() {
    let words = [
        "1", "0", "ON", "off", "yes", "NO", "true", "FALSE", "2", "-1", "full", "extra", "abc",
        "'on'", "' on'", "0x1", "'1x'", "1.5", "7", "DEFAULT", "DELETE", "256",
    ];
    for (i, w) in words.iter().enumerate() {
        let s = format!(
            "PRAGMA foreign_keys = {w}; PRAGMA foreign_keys; PRAGMA recursive_triggers = {w}; \
             PRAGMA recursive_triggers; PRAGMA ignore_check_constraints({w}); PRAGMA ignore_check_constraints"
        );
        assert!(same(&format!("fw{i}"), &s).is_none(), "{s}");
    }
    // foreign_keys is a no-op inside a transaction; the others
    // are not.
    assert!(
        same(
            "fwtx",
            "BEGIN; PRAGMA foreign_keys = 1; PRAGMA foreign_keys; PRAGMA recursive_triggers = 1; \
             COMMIT; PRAGMA foreign_keys; PRAGMA recursive_triggers"
        )
        .is_none()
    );
}

#[test]
fn case_sensitive_like_changes_like() {
    let mut rng = Rng(0x5eed_0016_0002);
    let alpha = ["a", "A", "b", "B", "%", "_", "ä", "Ä", "z"];
    let mut pairs = Vec::new();
    for _ in 0..120 {
        let mut x = String::new();
        let mut p = String::new();
        for _ in 0..rng.below(4) + 1 {
            x.push_str(rng.pick(&alpha[..4]));
        }
        for _ in 0..rng.below(4) + 1 {
            p.push_str(rng.pick(&alpha));
        }
        pairs.push((x, p));
    }
    for (i, chunk) in pairs.chunks(20).enumerate() {
        let mut items = Vec::new();
        for (x, p) in chunk {
            items.push(format!("'{x}' LIKE '{p}'"));
            items.push(format!("like('{p}', '{x}')"));
            items.push(format!("'{x}' NOT LIKE '{p}'"));
        }
        let sel = format!("SELECT {}", items.join(", "));
        let s = format!(
            "{sel}; PRAGMA case_sensitive_like = 1; {sel}; PRAGMA case_sensitive_like = off; {sel}; \
             PRAGMA case_sensitive_like = yes; {sel}"
        );
        assert!(same(&format!("csl{i}"), &s).is_none(), "{s}");
    }
    // Through a CHECK and a WHERE, and the ESCAPE form.
    assert!(
        same(
            "cslrows",
            "CREATE TABLE t(a, b CHECK (b LIKE 'a%')); INSERT INTO t VALUES (1, 'Ab'), (2, 'ab'); \
             PRAGMA case_sensitive_like = 1; SELECT a FROM t WHERE b LIKE 'a%'; \
             SELECT 'aB' LIKE 'a_' ESCAPE 'x', 'AB' LIKE 'a_' ESCAPE 'x'; INSERT INTO t VALUES (3, 'Ax')"
        )
        .is_some()
    );
}

#[test]
fn ignore_check_constraints_skips_check() {
    for (i, s) in [
        "CREATE TABLE t(a CHECK (a > 0), b NOT NULL); PRAGMA ignore_check_constraints = 1; \
         INSERT INTO t VALUES (-1, 1); UPDATE t SET a = -5; INSERT OR IGNORE INTO t VALUES (-2, 2); \
         SELECT * FROM t; PRAGMA integrity_check; PRAGMA ignore_check_constraints = 0; \
         PRAGMA integrity_check; PRAGMA quick_check; UPDATE t SET b = 3; SELECT * FROM t",
        "CREATE TABLE t(a INT CHECK (a > 0)) STRICT; PRAGMA ignore_check_constraints = 1; \
         INSERT INTO t VALUES (-1); SELECT * FROM t; INSERT INTO t VALUES ('x')",
        "CREATE TABLE t(a CHECK (a > 0)); PRAGMA ignore_check_constraints = 1; INSERT INTO t VALUES (-1); \
         PRAGMA ignore_check_constraints = 0; INSERT INTO t VALUES (-2)",
        "CREATE TABLE t(a, b CHECK (b <> 5)); INSERT INTO t VALUES (1, 1); \
         PRAGMA ignore_check_constraints = 1; ALTER TABLE t ADD COLUMN c CHECK (c IS NOT NULL); \
         SELECT * FROM t",
    ]
    .iter()
    .enumerate()
    {
        same(&format!("icc{i}"), s);
    }
}

/// Without the `foreign_keys` leaf (`tests/foreign_keys_v16.rs`
/// covers the enforcement with it).
#[cfg(not(feature = "foreign_keys"))]
#[test]
fn foreign_keys_on_refuses_writes_hipp_cannot_check() {
    // The pin enforces foreign keys once the flag is on; hipp
    // does not, so every write to a table in a foreign key is
    // refused rather than run unchecked. Other tables stay
    // writable, and with the flag off nothing changes.
    let h = common::temp_db("v16pr-fk-h");
    let setup = "CREATE TABLE p(id INTEGER PRIMARY KEY); CREATE TABLE c(pid REFERENCES p(id)); \
                 CREATE TABLE o(x); INSERT INTO p VALUES (1); INSERT INTO c VALUES (1)";
    assert!(hipp_script(&h, setup).0);
    for s in [
        "PRAGMA foreign_keys = ON; INSERT INTO c VALUES (5)",
        "PRAGMA foreign_keys = ON; DELETE FROM p",
        "PRAGMA foreign_keys = ON; UPDATE p SET id = 2",
        "PRAGMA foreign_keys = ON; DROP TABLE c",
        "PRAGMA foreign_keys = ON; INSERT INTO p VALUES (2)",
    ] {
        let (ok, _, e) = hipp_script(&h, s);
        assert!(!ok && matches!(e, Some(Error::Unsupported)), "{s}: {e:?}");
    }
    let (ok, rows, _) = hipp_script(
        &h,
        "PRAGMA foreign_keys = ON; INSERT INTO o VALUES (1); PRAGMA foreign_keys = OFF; \
         INSERT INTO c VALUES (5); SELECT count(*) FROM c; SELECT * FROM o",
    );
    assert!(ok);
    assert_eq!(rows, ["2", "1"]);
}

// ---------------- introspection ----------------

/// A random column definition for the introspection corpus.
fn coldef(rng: &mut Rng, i: usize, gens: &mut usize) -> String {
    let types = [
        "",
        "INTEGER",
        "INT",
        "TEXT",
        "varchar(10)",
        "REAL",
        "BLOB",
        "NUMERIC",
        "decimal(5, 2)",
        "STRING",
        "Big Int",
        "char",
    ];
    let mut s = format!("c{i} {}", rng.pick(&types));
    let n = rng.below(4);
    for _ in 0..n {
        match rng.below(9) {
            0 => s.push_str(" NOT NULL"),
            1 => s.push_str(&format!(
                " DEFAULT {}",
                rng.pick(&[
                    "1",
                    "-1",
                    "+5",
                    "1.50",
                    "'x'",
                    "'it''s'",
                    "NULL",
                    "x'0a'",
                    "(1+2)",
                    "( 4 )",
                    "CURRENT_TIMESTAMP",
                    "\"q\"",
                    "TRUE",
                    "abc",
                    "(  'a' || 'b'  )",
                    "-0x10",
                ])
            )),
            2 => s.push_str(" UNIQUE"),
            3 => s.push_str(&format!(
                " COLLATE {}",
                rng.pick(&["NOCASE", "RTRIM", "BINARY"])
            )),
            4 => s.push_str(" CHECK (1)"),
            5 if *gens < 2 && i > 0 => {
                *gens += 1;
                s.push_str(&format!(
                    " AS (c0) {}",
                    rng.pick(&["", "VIRTUAL", "STORED"])
                ));
            }
            6 => s.push_str(" REFERENCES p(x) ON DELETE CASCADE"),
            _ => {}
        }
    }
    s
}

#[test]
fn table_and_index_introspection_corpus() {
    let mut rng = Rng(0x5eed_0016_0003);
    for case in 0..40 {
        let ncols = rng.below(5) + 1;
        let mut gens = 0;
        let cols: Vec<String> = (0..ncols).map(|i| coldef(&mut rng, i, &mut gens)).collect();
        let mut defs = cols.join(", ");
        let pkc = rng.below(ncols);
        match rng.below(4) {
            0 => defs.push_str(&format!(", PRIMARY KEY (c{pkc})")),
            1 if ncols > 1 => defs.push_str(&format!(", PRIMARY KEY (c{}, c0 DESC)", ncols - 1)),
            2 => defs.push_str(&format!(", UNIQUE (c{pkc} COLLATE NOCASE)")),
            _ => {}
        }
        if rng.below(3) == 0 {
            defs.push_str(&format!(
                ", FOREIGN KEY (c0) REFERENCES q {}",
                rng.pick(&[
                    "",
                    "ON UPDATE SET NULL",
                    "ON DELETE RESTRICT ON UPDATE SET DEFAULT"
                ])
            ));
        }
        let mut s = format!("CREATE TABLE t({defs}); ");
        if rng.below(2) == 0 {
            s.push_str(&format!(
                "CREATE {} INDEX i1 ON t(c{} {}, c0 COLLATE RTRIM); ",
                rng.pick(&["", "UNIQUE"]),
                rng.below(ncols),
                rng.pick(&["", "DESC", "ASC"]),
            ));
        }
        s.push_str(
            "PRAGMA table_info(t); PRAGMA table_xinfo(t); PRAGMA index_list(t); \
             PRAGMA foreign_key_list(t); PRAGMA table_list(t); \
             SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name",
        );
        let h = common::temp_db(&format!("v16pr-intro{case}-h"));
        let o = common::temp_db(&format!("v16pr-intro{case}-o"));
        // The oracle may reject a generated definition (a default
        // on a generated column, ...); then both must.
        same_on(&h, &o, &s);
        let (_, names) = oracle_script(
            &o,
            "SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name",
        );
        for n in names {
            let q = format!("PRAGMA index_info({n}); PRAGMA index_xinfo('{n}')");
            same_on(&h, &o, &q);
        }
    }
}

#[test]
fn introspection_fixed_cases() {
    for (i, s) in [
        "CREATE TABLE t(a INTEGER, b TEXT NOT NULL DEFAULT 'x', c DEFAULT (1+2), d DEFAULT -1, \
         e AS (a+1), f INT AS (a*2) STORED, g DEFAULT CURRENT_TIMESTAMP, h varchar(10) DEFAULT x'0a', \
         i DEFAULT +5, j DEFAULT NULL, k DEFAULT 'it''s', l DEFAULT ( 4 ), m DEFAULT \"q\", \
         n DEFAULT 1.50, UNIQUE(b, c), PRIMARY KEY(c, a)); PRAGMA table_info(t); PRAGMA table_xinfo(t); \
         PRAGMA table_info('t'); PRAGMA main.table_info(T); PRAGMA table_info = t",
        "PRAGMA table_info(nosuch); PRAGMA table_info; PRAGMA table_info(sqlite_schema); \
         PRAGMA table_xinfo(sqlite_master); PRAGMA table_info(sqlite_temp_master); PRAGMA index_list; \
         PRAGMA index_info(nosuch); PRAGMA foreign_key_list(nosuch)",
        "CREATE TABLE p(id INTEGER PRIMARY KEY, x UNIQUE, y, z, UNIQUE(y COLLATE NOCASE DESC, z)); \
         CREATE INDEX ip ON p(z DESC, y COLLATE RTRIM); CREATE UNIQUE INDEX iu ON p(x, id); \
         CREATE TABLE q(a PRIMARY KEY, b, c AS (b) STORED, d AS (a)); CREATE TABLE r(a INTEGER PRIMARY KEY DESC); \
         PRAGMA index_list(p); PRAGMA index_list(q); PRAGMA index_list(r); PRAGMA index_info(iu); \
         PRAGMA index_xinfo(iu); PRAGMA index_xinfo(sqlite_autoindex_q_1); PRAGMA index_xinfo(sqlite_autoindex_r_1); \
         PRAGMA index_info(ip); PRAGMA index_xinfo(ip); PRAGMA index_xinfo(sqlite_autoindex_p_2); \
         PRAGMA index_info(p); PRAGMA index_list(ip)",
        "CREATE TABLE p(id INTEGER PRIMARY KEY, u, v, UNIQUE(u, v)); CREATE TABLE c(a REFERENCES p, \
         b REFERENCES p(u) ON DELETE CASCADE ON UPDATE SET NULL MATCH FULL, x, y, CONSTRAINT f FOREIGN KEY (x, y) \
         REFERENCES p(u, v) ON DELETE SET DEFAULT DEFERRABLE INITIALLY DEFERRED, FOREIGN KEY(A) REFERENCES \
         nosuch ON UPDATE RESTRICT ON DELETE NO ACTION ON INSERT CASCADE); PRAGMA foreign_key_list(c); \
         PRAGMA foreign_key_list(p)",
        // Up to three tables and views: the pin's hash order is
        // newest first, then sqlite_schema, then temp.
        "CREATE TABLE p(a); CREATE VIEW v AS SELECT 1, 2; CREATE TABLE s(a INT, b INT AS (a)) STRICT; \
         PRAGMA table_list; PRAGMA table_list(p); PRAGMA main.table_list; PRAGMA table_list(sqlite_schema); \
         PRAGMA table_list(sqlite_master); PRAGMA table_list(sqlite_temp_master); PRAGMA table_list(V)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("ifix{i}"), s).is_none(), "{s}");
    }
    // View column types (the pin's sqlite3SubqueryColumnTypes).
    for (i, s) in [
        "CREATE TABLE t(a INT, b varchar(5)); CREATE VIEW v AS SELECT CAST(1 AS VARCHAR(10)) a, \
         CAST(b AS foo) b, t.b d, +t.b e, (t.b) f, t.a AS g, (SELECT a FROM t) c FROM t; PRAGMA table_info(v); \
         CREATE VIEW u(p, q) AS SELECT a, b FROM t; PRAGMA table_xinfo(u); \
         CREATE VIEW s AS SELECT a FROM t UNION SELECT b FROM t; PRAGMA table_info(s); \
         CREATE VIEW r AS VALUES(1, 2); PRAGMA table_info(r)",
        "CREATE TABLE t(a, b BLOB, c STRING, d REAL, e INTEGER PRIMARY KEY); CREATE VIEW v AS SELECT a, b, c, \
         d, e, rowid, CAST(a AS BLOB), CAST(a AS REAL), 1, 'x', a||b, CAST(a AS INTEGER) FROM t; \
         PRAGMA table_info(v); CREATE VIEW w AS SELECT d FROM t UNION SELECT 'x'; PRAGMA table_info(w); \
         CREATE VIEW w2 AS SELECT CAST(a AS INT) FROM t UNION SELECT 'x'; PRAGMA table_info(w2); \
         CREATE VIEW w3 AS SELECT CAST(a AS INT) FROM t UNION SELECT 1; PRAGMA table_info(w3)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("vt{i}"), s).is_none(), "{s}");
    }
}

#[test]
fn table_list_order_and_contents() {
    // Beyond three objects the pin's order is its hash table's,
    // which hipp does not reproduce: the rows themselves match.
    let mut rng = Rng(0x5eed_0016_0004);
    for case in 0..6 {
        let mut s = String::new();
        let n = rng.below(8) + 1;
        for k in 0..n {
            if rng.below(3) == 0 {
                s.push_str(&format!("CREATE VIEW v{k} AS SELECT {k} AS x, 2; "));
            } else {
                s.push_str(&format!(
                    "CREATE TABLE t{k}({}){}; ",
                    (0..=rng.below(4))
                        .map(|c| format!("c{c} INT"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    rng.pick(&["", " STRICT"])
                ));
            }
        }
        s.push_str("PRAGMA table_list");
        let h = common::temp_db(&format!("v16pr-tl{case}-h"));
        let o = common::temp_db(&format!("v16pr-tl{case}-o"));
        let (hok, mut hl, _) = hipp_script(&h, &s);
        let (ook, mut ol) = oracle_script(&o, &s);
        assert!(hok && ook, "{s}");
        if n <= 3 {
            assert_eq!(hl, ol, "{s}");
        }
        hl.sort();
        ol.sort();
        assert_eq!(hl, ol, "{s}");
    }
}

#[test]
fn database_and_collation_lists() {
    let h = common::temp_db("v16pr-dbl-h");
    let (ok, rows, _) = hipp_script(&h, "PRAGMA database_list");
    assert!(ok);
    let full = std::fs::canonicalize(&h).expect("canonical path");
    assert_eq!(rows, [format!("0|main|{}", full.display())]);
    // The oracle prints the same for its own file.
    let o = common::temp_db("v16pr-dbl-o");
    let (_, orows) = oracle_script(&o, "PRAGMA database_list");
    let ofull = std::fs::canonicalize(&o).expect("canonical path");
    assert_eq!(orows, [format!("0|main|{}", ofull.display())]);
    // hipp's collations are the pin's built-in three; the pin's
    // shell adds two of its own (decimal, uint).
    let (_, rows, _) = hipp_script(&h, "PRAGMA collation_list");
    assert_eq!(rows, ["0|BINARY", "1|NOCASE", "2|RTRIM"]);
    let (_, orows) = oracle_script(&o, "PRAGMA collation_list");
    let names: Vec<&str> = orows.iter().filter_map(|r| r.split('|').nth(1)).collect();
    for n in ["BINARY", "NOCASE", "RTRIM"] {
        assert!(names.contains(&n));
    }
}

// ---------------- integrity check ----------------

#[test]
fn integrity_check_ok_and_constraint_findings() {
    for (i, s) in [
        "PRAGMA integrity_check; PRAGMA quick_check",
        "CREATE TABLE t(a UNIQUE, b NOT NULL, c CHECK (c <> 5)); CREATE INDEX i ON t(b, c); \
         INSERT INTO t VALUES (1, 2, 3), (4, 5, 6), (NULL, 7, 8); PRAGMA integrity_check; \
         PRAGMA quick_check; PRAGMA integrity_check(t); PRAGMA integrity_check(1); PRAGMA quick_check(T); \
         PRAGMA integrity_check(sqlite_schema); PRAGMA integrity_check(+2); PRAGMA integrity_check = 3",
        "CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(50000)); DELETE FROM t; \
         CREATE TABLE u(x, y AS (x * 2), z TEXT AS (x || 'z') STORED); CREATE INDEX iu ON u(y); \
         INSERT INTO u(x) VALUES (1), (2), (3); PRAGMA integrity_check; PRAGMA freelist_count",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("ic{i}"), s).is_none(), "{s}");
    }
    refuse(
        "icnt",
        "PRAGMA integrity_check(nosuch)",
        SchemaKind::UnknownTable,
    );
    refuse(
        "icnt2",
        "PRAGMA integrity_check('2x')",
        SchemaKind::UnknownTable,
    );
}

/// An oracle-written file with byte edits applied.
fn corrupted(tag: &str, setup: &str, edit: impl Fn(&mut Vec<u8>)) -> std::path::PathBuf {
    let p = common::temp_db(&format!("v16pr-cor-{tag}"));
    let (ok, _) = oracle_script(&p, setup);
    assert!(ok, "{setup}");
    let mut d = std::fs::read(&p).expect("read");
    edit(&mut d);
    std::fs::write(&p, &d).expect("write");
    p
}

/// Both engines' integrity / quick check of copies of `p`.
fn check_both(p: &std::path::Path, tag: &str) {
    for (k, q) in [
        "PRAGMA integrity_check",
        "PRAGMA quick_check",
        "PRAGMA integrity_check(2)",
        "PRAGMA integrity_check(1)",
    ]
    .iter()
    .enumerate()
    {
        let h = common::temp_db(&format!("v16pr-cc-{tag}-{k}-h"));
        let o = common::temp_db(&format!("v16pr-cc-{tag}-{k}-o"));
        std::fs::copy(p, &h).expect("copy");
        std::fs::copy(p, &o).expect("copy");
        let (_, hl, he) = hipp_script(&h, q);
        let (_, ol) = oracle_script(&o, q);
        assert_eq!(hl, ol, "{tag}: {q} (hipp error {he:?})");
        // Every corruption is found (the edit took).
        if k == 0 {
            assert_eq!(tag == "ml-ok", hl == ["ok"], "{tag}: {hl:?}");
        }
    }
}

fn be16(d: &[u8], o: usize) -> usize {
    usize::from(u16::from_be_bytes([d[o], d[o + 1]]))
}

fn put16(d: &mut [u8], o: usize, v: usize) {
    d[o..o + 2].copy_from_slice(&(v as u16).to_be_bytes());
}

fn put32(d: &mut [u8], o: usize, v: u32) {
    d[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

const PS: usize = 4096;

#[test]
fn integrity_check_page_level_corruptions() {
    // Pages past the end that nothing uses.
    let p = corrupted("unused", "CREATE TABLE t(a);", |d| {
        d.extend(std::iter::repeat_n(0u8, 2 * PS));
        put32(d, 28, 4);
    });
    check_both(&p, "unused");
    // Rowids out of order in a leaf.
    let p = corrupted(
        "order",
        "CREATE TABLE t(a); INSERT INTO t VALUES ('x1'), ('x2'), ('x3');",
        |d| {
            let a = be16(d, PS + 8);
            let b = be16(d, PS + 10);
            put16(d, PS + 8, b);
            put16(d, PS + 10, a);
        },
    );
    check_both(&p, "order");
    // A cell pointer off the content area.
    let p = corrupted(
        "cellptr",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1), (2);",
        |d| put16(d, PS + 8, 5000),
    );
    check_both(&p, "cellptr");
    // A short overflow chain.
    let p = corrupted(
        "ovfl",
        "CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(10000));",
        |d| put32(d, 2 * PS, 0),
    );
    check_both(&p, "ovfl");
    // A freelist that is shorter than the header says.
    let p = corrupted(
        "free",
        "CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(20000)); DELETE FROM t;",
        |d| put32(d, 36, 7),
    );
    check_both(&p, "free");
    // A freelist trunk off the file.
    let p = corrupted(
        "trunk",
        "CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(20000)); DELETE FROM t;",
        |d| put32(d, 32, 77),
    );
    check_both(&p, "trunk");
    // A page that is not a B-tree page.
    let p = corrupted(
        "type",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1);",
        |d| d[PS] = 7,
    );
    check_both(&p, "type");
    // An index missing an entry (its leaf's cell count cut).
    let p = corrupted(
        "count",
        "CREATE TABLE t(a); CREATE INDEX i ON t(a); INSERT INTO t VALUES (1), (2), (3);",
        |d| {
            let n = be16(d, 2 * PS + 3);
            put16(d, 2 * PS + 3, n - 1);
        },
    );
    check_both(&p, "count");
    // A table missing a row its index still has.
    let p = corrupted(
        "extra",
        "CREATE TABLE t(a); CREATE INDEX i ON t(a); INSERT INTO t VALUES (1), (2), (3);",
        |d| {
            let n = be16(d, PS + 3);
            put16(d, PS + 3, n - 1);
        },
    );
    check_both(&p, "extra");
    // A table value changed under its index (row ordinal, not
    // rowid, in the message).
    let p = corrupted(
        "idxval",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(a); INSERT INTO t VALUES ('aaaa', 1), ('bbbb', 2), ('cccc', 3); \
         DELETE FROM t WHERE b = 1; INSERT INTO t VALUES ('dddd', 4);",
        |d| {
            let k = d[PS..]
                .windows(4)
                .position(|w| w == b"cccc")
                .expect("value")
                + PS;
            d[k..k + 4].copy_from_slice(b"zzzz");
        },
    );
    check_both(&p, "idxval");
    // A UNIQUE index holding a duplicate.
    let p = corrupted(
        "uniq",
        "CREATE TABLE t(a UNIQUE, b); INSERT INTO t VALUES ('aaaa', 1), ('bbbb', 2), ('cccc', 3);",
        |d| {
            let k = d[PS..2 * PS]
                .windows(4)
                .position(|w| w == b"bbbb")
                .expect("value")
                + PS;
            d[k..k + 4].copy_from_slice(b"aaaa");
        },
    );
    check_both(&p, "uniq");
    // Two roots sharing a page.
    let p = corrupted(
        "dup",
        "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO t VALUES (1);",
        |d| {
            // u's root page number in its schema record (the byte
            // before its `uu` names and SQL) becomes t's.
            let k = d[..PS]
                .windows(b"tableuu".len())
                .position(|w| w == b"tableuu")
                .expect("schema row")
                + 7;
            assert_eq!(d[k], 3);
            d[k] = 2;
        },
    );
    check_both(&p, "dup");
}

#[test]
fn integrity_check_multi_level_trees() {
    let setup = "CREATE TABLE t(a, b); CREATE INDEX ti ON t(b); \
                 WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000) \
                 INSERT INTO t SELECT x, printf('%080d', x * 7) FROM c;";
    let kinds = |d: &[u8]| -> Vec<(usize, u8)> {
        (1..=d.len() / PS)
            .map(|n| (n, d[(n - 1) * PS + if n == 1 { 100 } else { 0 }]))
            .collect()
    };
    let p = corrupted("ml-ok", setup, |_| {});
    check_both(&p, "ml-ok");
    // The root's right child points at its first child.
    let p = corrupted("ml-2nd", setup, |d| {
        let off = PS;
        let c0 = be16(d, off + 12);
        let first = u32::from_be_bytes([
            d[off + c0],
            d[off + c0 + 1],
            d[off + c0 + 2],
            d[off + c0 + 3],
        ]);
        put32(d, off + 8, first);
    });
    check_both(&p, "ml-2nd");
    // A leaf's fragment count wrong.
    let p = corrupted("ml-frag", setup, |d| {
        let leaf = kinds(d)
            .iter()
            .filter(|(_, k)| *k == 13)
            .nth(5)
            .expect("leaf")
            .0;
        d[(leaf - 1) * PS + 7] = 9;
    });
    check_both(&p, "ml-frag");
    // A leaf's first and last cells swapped.
    let p = corrupted("ml-swap", setup, |d| {
        let leaf = kinds(d)
            .iter()
            .filter(|(_, k)| *k == 13)
            .nth(7)
            .expect("leaf")
            .0;
        let off = (leaf - 1) * PS;
        let n = be16(d, off + 3);
        let a = be16(d, off + 8);
        let b = be16(d, off + 8 + 2 * (n - 1));
        put16(d, off + 8, b);
        put16(d, off + 8 + 2 * (n - 1), a);
    });
    check_both(&p, "ml-swap");
    // An interior divider key wrong.
    let p = corrupted("ml-div", setup, |d| {
        let c = be16(d, PS + 12 + 2);
        d[PS + c + 4] = 1;
    });
    check_both(&p, "ml-div");
    // An index interior child pointing elsewhere (depth differs).
    let p = corrupted("ml-depth", setup, |d| {
        let k = kinds(d);
        let ii: Vec<usize> = k.iter().filter(|(_, t)| *t == 2).map(|x| x.0).collect();
        let off = (ii[0] - 1) * PS;
        let c0 = be16(d, off + 12);
        put32(d, off + c0, *ii.last().expect("index interior") as u32);
    });
    check_both(&p, "ml-depth");
    // A freeblock over cell content.
    let p = corrupted("ml-free", setup, |d| {
        let leaf = kinds(d)
            .iter()
            .filter(|(_, k)| *k == 13)
            .nth(3)
            .expect("leaf")
            .0;
        let off = (leaf - 1) * PS;
        let cs = be16(d, off + 5);
        put16(d, off + 1, cs + 2);
        put16(d, off + cs + 2, 0);
        put16(d, off + cs + 4, 10);
    });
    check_both(&p, "ml-free");
}

#[test]
fn integrity_check_schema_constraint_findings() {
    // The schema text is edited in place (same length) to add
    // constraints the stored rows break.
    // (tag, stored definition, edited definition, rows)
    let cases = [
        (
            "nn",
            "CREATE TABLE t(a, b)",
            "CREATE TABLE t(a NOT NULL, b NOT NULL)",
            "INSERT INTO t VALUES (NULL, 1), (2, NULL), (NULL, NULL);",
        ),
        (
            "ck",
            "CREATE TABLE t(a)",
            "CREATE TABLE t(a CHECK (a > 0), CHECK (a<10))",
            "INSERT INTO t VALUES (-1), (1), (NULL), (20);",
        ),
        (
            "st",
            "CREATE TABLE t(a INT, b TEXT, c REAL, d BLOB, e ANY)",
            "CREATE TABLE t(a INT, b TEXT, c REAL, d BLOB, e ANY) STRICT",
            "INSERT INTO t VALUES ('x', 5, 'y', 1, 2), (1, 'a', 2, x'00', NULL), (1.5, NULL, 2.5, 'z', 'q');",
        ),
        (
            "uq",
            "CREATE INDEX ix ON t(a)",
            "CREATE UNIQUE INDEX ix ON t(a)",
            "INSERT INTO t VALUES (1), (1), (2), (NULL), (NULL);",
        ),
        (
            "gen",
            "CREATE TABLE t(a, b AS (a*2))",
            "CREATE TABLE t(a, b NOT NULL AS (a*2))",
            "INSERT INTO t(a) VALUES (NULL), (2);",
        ),
    ];
    for (tag, def, edited, rows) in cases {
        // Pad the stored text so the edit keeps its length.
        let pad = edited.len() - def.len();
        let close = def.rfind(')').expect("definition");
        let from = format!("{}{}{}", &def[..close], " ".repeat(pad), &def[close..]);
        let setup = if def.starts_with("CREATE INDEX") {
            format!("CREATE TABLE t(a); {rows} {from};")
        } else {
            format!("{from}; {rows}")
        };
        let (from, to) = (from.as_str(), edited);
        assert_eq!(from.len(), to.len(), "{tag}");
        let p = corrupted(tag, &setup, |d| {
            let k = d[..PS]
                .windows(from.len())
                .position(|w| w == from.as_bytes())
                .expect("schema text");
            d[k..k + from.len()].copy_from_slice(to.as_bytes());
            // The pin re-reads the schema only on a cookie change.
            let c = u32::from_be_bytes([d[40], d[41], d[42], d[43]]);
            put32(d, 40, c + 1);
        });
        check_both(&p, tag);
    }
    // Multiple tables: order (newest first for small schemas) and
    // the limit across them.
    let p = corrupted(
        "two",
        "CREATE TABLE b(a         ); CREATE TABLE a(a         ); INSERT INTO a VALUES (NULL), (NULL); \
         INSERT INTO b VALUES (NULL);",
        |d| {
            for _ in 0..2 {
                let k = d[..PS]
                    .windows(b"(a         )".len())
                    .position(|w| w == b"(a         )")
                    .expect("schema text");
                d[k..k + 12].copy_from_slice(b"(a NOT NULL)");
            }
            let c = u32::from_be_bytes([d[40], d[41], d[42], d[43]]);
            put32(d, 40, c + 1);
        },
    );
    check_both(&p, "two");
}

// ---------------- sqlite_schema and table-valued pragmas ----------------

#[test]
fn sqlite_schema_reads_like_a_table() {
    let setup = "CREATE TABLE t(a UNIQUE, b); CREATE INDEX i ON t(b); CREATE VIEW v AS SELECT 1; ";
    for (i, q) in [
        "SELECT * FROM sqlite_schema",
        "SELECT * FROM sqlite_master WHERE type = 'index' ORDER BY name",
        "SELECT * FROM main.sqlite_master",
        "SELECT * FROM SQLITE_SCHEMA s JOIN t",
        "SELECT * FROM sqlite_temp_schema",
        "SELECT * FROM sqlite_temp_master",
        "SELECT rowid, * FROM sqlite_schema",
        "SELECT typeof(rootpage), typeof(sql) FROM sqlite_schema",
        "SELECT * FROM \"sqlite_schema\"",
        "SELECT sqlite_schema.name FROM sqlite_schema",
        "SELECT s.name FROM sqlite_master s WHERE s.type = 'table'",
        "SELECT count(*), max(rootpage) FROM sqlite_schema",
        "SELECT name FROM sqlite_schema WHERE name IN (SELECT name FROM sqlite_master) ORDER BY 1",
        "SELECT type, count(*) FROM sqlite_schema GROUP BY type ORDER BY type",
        "SELECT (SELECT count(*) FROM sqlite_schema WHERE tbl_name = 't')",
        "INSERT INTO t SELECT name, type FROM sqlite_schema; SELECT * FROM t",
        // (The pin's CTAS also sees its own placeholder schema
        // row, all NULL; hipp's does not — see the gaps.)
        "CREATE TABLE x AS SELECT * FROM sqlite_schema; PRAGMA table_info(x); SELECT name FROM x WHERE name NOT NULL",
        "CREATE VIEW w AS SELECT name FROM sqlite_schema; SELECT * FROM sqlite_schema WHERE name = 'w'",
    ]
    .iter()
    .enumerate()
    {
        let s = format!("{setup}{q}");
        assert!(same(&format!("ss{i}"), &s).is_none(), "{s}");
    }
    for (i, q) in [
        "INSERT INTO sqlite_schema VALUES ('table', 'x', 'x', 0, '')",
        "DELETE FROM sqlite_master",
        "UPDATE sqlite_master SET name = 'q'",
        "DROP TABLE sqlite_master",
        "CREATE INDEX j ON sqlite_master(name)",
        "ALTER TABLE sqlite_schema ADD COLUMN z",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("ssw{i}"),
            &format!("{setup}{q}"),
            SchemaKind::SchemaTableReadOnly,
        );
    }
}

#[test]
fn reserved_names_are_refused() {
    for (i, s) in [
        "CREATE TABLE sqlite_x(a)",
        "CREATE TABLE SQLITE_master(a)",
        "CREATE TABLE \"sqlite_q\"(a)",
        "CREATE TABLE IF NOT EXISTS sqlite_x(a)",
        "CREATE TABLE sqlite_(a)",
        "CREATE TABLE sqlite_y AS SELECT 1",
        "CREATE VIEW sqlite_v AS SELECT 1",
        "CREATE TABLE t(a); CREATE INDEX SQLITE_i ON t(a)",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("rn{i}"), s, SchemaKind::ReservedName);
    }
    for (i, s) in [
        "CREATE TABLE sqlitex(a); CREATE TABLE t(sqlite_a); ALTER TABLE t ADD COLUMN sqlite_b; PRAGMA table_info(t)",
        "CREATE TABLE \"sqlite\"(a); SELECT name FROM sqlite_schema",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("rnok{i}"), s).is_none(), "{s}");
    }
}

#[test]
fn table_valued_pragma_functions() {
    let setup = "CREATE TABLE t(a INT PRIMARY KEY, b TEXT NOT NULL DEFAULT 'q'); CREATE INDEX i ON t(b); \
                 CREATE TABLE c(x REFERENCES t(a)); ";
    for (i, q) in [
        "SELECT * FROM pragma_table_info('t')",
        "SELECT * FROM pragma_table_info('t', 'main')",
        "SELECT * FROM pragma_table_info",
        "SELECT name FROM pragma_table_info('t') WHERE pk",
        "SELECT * FROM pragma_user_version",
        "SELECT * FROM pragma_user_version()",
        "SELECT * FROM pragma_page_size",
        "SELECT * FROM pragma_index_list('t')",
        "SELECT * FROM main.pragma_table_info('t')",
        "SELECT * FROM pragma_table_info('t') AS x WHERE x.pk",
        "SELECT * FROM pragma_foreign_keys",
        "SELECT * FROM pragma_journal_mode",
        "SELECT * FROM pragma_table_list",
        "SELECT count(*) FROM pragma_table_info('t')",
        "SELECT * FROM pragma_table_xinfo('t')",
        "SELECT * FROM pragma_busy_timeout",
        "SELECT * FROM pragma_encoding",
        "SELECT * FROM pragma_table_info(NULL)",
        "SELECT pragma_table_info.name FROM pragma_table_info('t')",
        "SELECT * FROM pragma_index_xinfo('i')",
        "SELECT * FROM pragma_foreign_key_list('c')",
        "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list('c')",
        "SELECT t2.name, p.name FROM pragma_table_list t2 JOIN pragma_table_info('t') p WHERE t2.name = 't'",
        "SELECT name, type FROM pragma_table_info('t') ORDER BY name DESC",
        "SELECT * FROM pragma_table_info('t', 'temp')",
        "SELECT * FROM pragma_table_info(1)",
        "SELECT * FROM pragma_application_id; SELECT * FROM pragma_freelist_count; SELECT * FROM pragma_schema_version",
        "SELECT (SELECT count(*) FROM pragma_index_list('t'))",
    ]
    .iter()
    .enumerate()
    {
        let s = format!("{setup}{q}");
        assert!(same(&format!("tvf{i}"), &s).is_none(), "{s}");
    }
    refuse(
        "tvfnone",
        "SELECT * FROM pragma_nosuch",
        SchemaKind::UnknownTable,
    );
    refuse(
        "tvfcsl",
        "SELECT * FROM pragma_case_sensitive_like",
        SchemaKind::UnknownTable,
    );
    refuse(
        "tvfargs",
        "CREATE TABLE t(a); SELECT * FROM pragma_table_info('t', 'main', 3)",
        SchemaKind::WrongArgumentCount,
    );
    // A correlated argument (the pin runs it per row) is refused.
    hipp_refuses(
        "tvfcorr",
        "CREATE TABLE t(a); SELECT m.name, p.name FROM sqlite_schema m JOIN pragma_table_info(m.name) p",
    );
}

#[test]
fn regression_view_rowid_column_is_named_by_its_alias() {
    // Found while probing table_info over views: a view's rowid
    // column was named `rowid`; the pin names it by the table's
    // rowid-alias column (`e`, deduplicated to `e:1`).
    assert!(
        same(
            "vrowid",
            "CREATE TABLE t(e INTEGER PRIMARY KEY, x); CREATE TABLE u(y); \
             CREATE VIEW v AS SELECT e, rowid, x FROM t; CREATE VIEW w AS SELECT rowid, oid FROM u; \
             PRAGMA table_info(v); PRAGMA table_info(w)"
        )
        .is_none()
    );
}

// ---------------- review fixes (v0.16) ----------------

/// hipp writes `script` on a fresh file; the oracle must then read
/// the file cleanly and see the same `check` rows as hipp.
fn pin_reads_hipp_file(tag: &str, script: &str, check: &str) {
    let h = common::temp_db(&format!("v16pr-rt-{tag}"));
    let (hok, _, he) = hipp_script(&h, script);
    assert!(hok, "{script}: {he:?}");
    let (ook, ol) = oracle_script(&h, "PRAGMA integrity_check");
    assert!(ook && ol == ["ok"], "{tag}: the pin reads {ol:?}");
    let (_, hl, _) = hipp_script(&h, check);
    let (ook, ol) = oracle_script(&h, check);
    assert!(ook, "{tag}: {check}");
    assert_eq!(hl, ol, "{tag}: {check}");
}

/// A generated corpus of declared types, quoted every way the pin
/// tokenizes: `table_info` shows the pin's dequoted form (standard
/// names in upper case), affinity reads the dequoted form, a view
/// carries it, and a quoted `INTEGER PRIMARY KEY` is the rowid
/// alias (it used to get an autoindex the pin calls orphan).
#[test]
fn regression_declared_types_are_dequoted() {
    let bases = [
        "int",
        "INTEGER",
        "Text",
        "text ",
        "real",
        "blob",
        "any",
        "varchar(10)",
        "my type",
        "x] TEXT",
        "a b",
        "",
        "CHAR",
        "doub",
        "floating",
        "Integer",
    ];
    let quotes = [("", ""), ("\"", "\""), ("[", "]"), ("`", "`"), ("'", "'")];
    let mut rng = Rng(0x0016_dec1_7e5e);
    for round in 0..6 {
        let mut cols = Vec::new();
        for i in 0..10 {
            let b = rng.pick(&bases);
            let (l, r) = quotes[rng.below(quotes.len())];
            // Keep each type a span the pin can lex (the harness
            // splits statements on `;` outside ' " [ quotes, so the
            // fixed cases below hold the quote-in-quote spellings).
            let ty = match (l, b) {
                ("", b) | ("[", b) if b.contains(']') => format!("\"{b}\""),
                (l, b) => format!("{l}{b}{r}"),
            };
            cols.push(format!("c{i} {ty}"));
        }
        let vals = ["'1'", "'1.5'", "'x'", "1", "2.0"];
        let mut ins = Vec::new();
        for _ in 0..3 {
            let row: Vec<&str> = (0..10).map(|_| rng.pick(&vals)).collect();
            ins.push(format!("({})", row.join(", ")));
        }
        let types: Vec<String> = (0..10).map(|i| format!("typeof(c{i})")).collect();
        let s = format!(
            "CREATE TABLE t({}); INSERT INTO t VALUES {}; PRAGMA table_info(t); PRAGMA table_xinfo(t); \
             SELECT {} FROM t; CREATE VIEW v AS SELECT * FROM t; PRAGMA table_info(v)",
            cols.join(", "),
            ins.join(", "),
            types.join(", ")
        );
        assert!(same(&format!("dq{round}"), &s).is_none(), "{s}");
    }
    for (i, s) in [
        "CREATE TABLE t(a \"te\"\"xt\", b [a] b, c \"a\" \"b\", d 'text', e `int`, f \"\", g [x]y, \
         h 'my' type, i \"int\"(3), k \"INTEGER\", l [Real], m 'blob ', n \"a'b\", p [a], q 'it''s', \
         r `a``b`, s [x] TEXT); PRAGMA table_info(t)",
        "SELECT typeof(CAST('1' AS [x] TEXT)), typeof(CAST('1' AS [a] CHARX)), typeof(CAST('1' AS \"text\")), \
         typeof(CAST('1' AS \"int\"(3))), typeof(CAST('1' AS \"\"))",
        "CREATE TABLE t(a \"ANY\", b [int], c 'TEXT') STRICT; INSERT INTO t VALUES ('1', '2', 3); \
         SELECT typeof(a), typeof(b), typeof(c) FROM t; PRAGMA table_info(t)",
        "CREATE TABLE t(a INT, b varchar(5)); CREATE VIEW v AS SELECT a, a COLLATE NOCASE, b FROM t; \
         PRAGMA table_info(v)",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("dqf{i}"), s).is_none(), "{s}");
    }
    // A quoted type the pin's STRICT check reads as unknown.
    assert!(same("dqs", "CREATE TABLE u(a [x] TEXT) STRICT").is_some());
    for (i, s) in [
        "CREATE TABLE t(a \"INTEGER\" PRIMARY KEY, b); INSERT INTO t VALUES (5, 1)",
        "CREATE TABLE t(a [integer], b, PRIMARY KEY (a)); INSERT INTO t VALUES (6, 1)",
        "CREATE TABLE t(a 'Integer' PRIMARY KEY); INSERT INTO t VALUES (7)",
        "CREATE TABLE t(a \"INTEGER\"(3) PRIMARY KEY); INSERT INTO t VALUES (8)",
    ]
    .iter()
    .enumerate()
    {
        pin_reads_hipp_file(
            &format!("dqa{i}"),
            s,
            "SELECT rowid, * FROM t; SELECT name FROM sqlite_schema",
        );
    }
}

/// A table-level `PRIMARY KEY (a DESC)` on an INTEGER column is the
/// rowid alias on the pin (only the column constraint's DESC is
/// the quirk); hipp wrote an autoindex the pin calls orphan.
#[test]
fn regression_table_level_pk_desc_is_the_rowid_alias() {
    for (i, s) in [
        "CREATE TABLE t(a INTEGER, b, PRIMARY KEY(a DESC)); INSERT INTO t VALUES (5, 1)",
        "CREATE TABLE t(a INTEGER, b, PRIMARY KEY(a COLLATE nocase DESC)); INSERT INTO t VALUES (5, 1)",
        "CREATE TABLE t(a INTEGER PRIMARY KEY DESC, b); INSERT INTO t VALUES (5, 1)",
        "CREATE TABLE t(a INTEGER, b, PRIMARY KEY(a DESC, b)); INSERT INTO t VALUES (5, 1)",
        "CREATE TABLE t(a TEXT, b, PRIMARY KEY(a DESC)); INSERT INTO t VALUES ('5', 1)",
    ]
    .iter()
    .enumerate()
    {
        pin_reads_hipp_file(
            &format!("pkd{i}"),
            s,
            "SELECT rowid, * FROM t; SELECT name FROM sqlite_schema; PRAGMA index_list(t); PRAGMA table_info(t)",
        );
    }
}

/// A user view or table named like a table-valued pragma function
/// shadows it; given arguments, any table or view is "not a
/// function" (hipp ignored the arguments).
#[test]
fn regression_schema_objects_shadow_pragma_functions() {
    for (i, s) in [
        "CREATE TABLE t(a); INSERT INTO t VALUES (7); CREATE VIEW pragma_user_version AS SELECT a FROM t; \
         SELECT * FROM pragma_user_version",
        "CREATE TABLE pragma_table_info(x); INSERT INTO pragma_table_info VALUES (9); \
         SELECT * FROM pragma_table_info",
        "CREATE TABLE t(a); CREATE VIEW pragma_table_list AS SELECT 1 AS n; SELECT * FROM pragma_table_list",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("sh{i}"), s).is_none(), "{s}");
    }
    for (i, s) in [
        "CREATE TABLE pragma_table_info(x); SELECT * FROM pragma_table_info('pragma_table_info')",
        "CREATE TABLE t(a); INSERT INTO t VALUES (3); SELECT * FROM t(1)",
        "CREATE TABLE t(a); SELECT * FROM main.t(1)",
        "CREATE TABLE t(a); SELECT * FROM t()",
        "CREATE VIEW v AS SELECT 1; SELECT * FROM v(1)",
        "WITH c AS (SELECT 1) SELECT * FROM c(1)",
        "CREATE VIEW pragma_user_version AS SELECT 5; SELECT * FROM pragma_user_version()",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("shf{i}"), s, SchemaKind::UnknownFunction);
    }
}

/// Each table-valued pragma takes the pin's hidden columns only
/// (`arg` for a name, `schema` for a database), checks the schema
/// name, and ignores nothing (pragma_user_version(5) wrote
/// nothing and answered).
#[test]
fn regression_table_valued_pragma_arities() {
    let setup = "CREATE TABLE t(a); CREATE INDEX i ON t(a); ";
    let prags = [
        "application_id",
        "auto_vacuum",
        "busy_timeout",
        "cache_size",
        "database_list",
        "encoding",
        "foreign_key_list",
        "foreign_keys",
        "freelist_count",
        "ignore_check_constraints",
        "index_info",
        "index_list",
        "index_xinfo",
        "journal_mode",
        "page_count",
        "page_size",
        "recursive_triggers",
        "schema_version",
        "synchronous",
        "table_info",
        "table_list",
        "table_xinfo",
        "user_version",
    ];
    let args = [
        "('t', 'main', 'x')",
        "('nosuch')",
        "('t', 'nosuch')",
        "",
        "(NULL)",
        "('t')",
        "('main')",
        "('i', 'main')",
        "('t', NULL)",
        "(5)",
        "('i', 'MAIN')",
    ];
    for (i, p) in prags.iter().enumerate() {
        for (j, a) in args.iter().enumerate() {
            let s = format!("{setup}SELECT count(*) FROM pragma_{p}{a}");
            let _ = same(&format!("ar{i}-{j}"), &s);
        }
    }
    refuse(
        "ar-uv",
        "SELECT * FROM pragma_user_version(5)",
        SchemaKind::WrongArgumentCount,
    );
    refuse(
        "ar-ps",
        "SELECT * FROM pragma_page_size(1024)",
        SchemaKind::UnknownDatabase,
    );
    assert!(
        same(
            "ar-uv2",
            "PRAGMA user_version = 3; SELECT * FROM pragma_user_version"
        )
        .is_none()
    );
    // The temp database's header values are not hipp's to report.
    hipp_refuses("ar-temp", "SELECT * FROM pragma_page_size('temp')");
}

/// The first write transaction fixes an empty database's page
/// size, encoding and auto-vacuum mode, even when rolled back
/// (the pin's BTS_PAGESIZE_FIXED); BEGIN IMMEDIATE / EXCLUSIVE
/// start one at once.
#[test]
fn regression_page_size_is_fixed_by_a_rolled_back_write() {
    for (i, s) in [
        "BEGIN; CREATE TABLE t(a); ROLLBACK; PRAGMA page_size = 1024; CREATE TABLE u(a); PRAGMA page_size",
        "BEGIN; PRAGMA user_version = 1; ROLLBACK; PRAGMA page_size = 1024; CREATE TABLE u(a); PRAGMA page_size",
        "BEGIN IMMEDIATE; ROLLBACK; PRAGMA page_size = 1024; CREATE TABLE u(a); PRAGMA page_size",
        "BEGIN EXCLUSIVE TRANSACTION; PRAGMA page_size = 1024; CREATE TABLE u(a); COMMIT; PRAGMA page_size",
        "BEGIN DEFERRED; PRAGMA page_size = 1024; CREATE TABLE u(a); COMMIT; PRAGMA page_size",
        "BEGIN; ROLLBACK; PRAGMA page_size = 2048; CREATE TABLE u(a); PRAGMA page_size",
        "BEGIN; SELECT 1; ROLLBACK; PRAGMA page_size = 2048; CREATE TABLE u(a); PRAGMA page_size",
        "BEGIN; CREATE TABLE t(a); ROLLBACK; PRAGMA page_count; PRAGMA freelist_count; \
         PRAGMA encoding = 'UTF-16le'; PRAGMA encoding; PRAGMA auto_vacuum = 1; PRAGMA auto_vacuum",
    ]
    .iter()
    .enumerate()
    {
        let h = common::temp_db(&format!("v16pr-psf{i}-h"));
        let o = common::temp_db(&format!("v16pr-psf{i}-o"));
        assert!(same_on(&h, &o, s).is_none(), "{s}");
        // (hipp writes page 1 at create; the pin's empty file has
        // no pages, so only files with a table compare.)
        if s.contains("CREATE TABLE u") {
            assert_eq!(
                std::fs::metadata(&h).expect("hipp file").len(),
                std::fs::metadata(&o).expect("pin file").len(),
                "{s}"
            );
        }
    }
    assert!(matches!(
        same("psf-syn", "BEGIN DEFERRED IMMEDIATE"),
        Some(Error::Parse { .. })
    ));
}

/// FOREIGN KEY definitions the pin refuses (hipp wrote them, and
/// the pin then could not read the file); PRIMARY KEY / UNIQUE
/// lists must name columns too.
#[test]
fn regression_foreign_key_definitions_are_validated() {
    for (i, s) in [
        "CREATE TABLE c(x REFERENCES p(a, b))",
        "CREATE TABLE c(x REFERENCES c(x, x))",
        "CREATE TABLE c(x, FOREIGN KEY (x, x) REFERENCES p(a))",
        "CREATE TABLE c(x, FOREIGN KEY (x) REFERENCES p(a, b))",
        "CREATE TABLE c(x, FOREIGN KEY (nosuch) REFERENCES p(a, b))",
        "CREATE TABLE c(x); ALTER TABLE c ADD COLUMN y REFERENCES p(a, b)",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("fk{i}"), s, SchemaKind::ForeignKeyColumns);
    }
    for (i, s) in [
        "CREATE TABLE c(x, FOREIGN KEY (nosuch) REFERENCES p)",
        "CREATE TABLE c(x, FOREIGN KEY (rowid) REFERENCES p(a))",
        "CREATE TABLE c(x, FOREIGN KEY (x, y) REFERENCES p)",
        "CREATE TABLE t(a, PRIMARY KEY (nosuch))",
        "CREATE TABLE t(a, UNIQUE (a), PRIMARY KEY (rowid))",
        "CREATE TABLE t(a, b, UNIQUE (b, zz))",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("fku{i}"), s, SchemaKind::UnknownColumn);
    }
    for (i, s) in [
        "CREATE TABLE c(x, y, FOREIGN KEY (x, y) REFERENCES p)",
        "CREATE TABLE c(x, FOREIGN KEY (X) REFERENCES p)",
        "CREATE TABLE c(x, FOREIGN KEY (\"x\") REFERENCES p(a))",
        "CREATE TABLE c(x, FOREIGN KEY (x, x) REFERENCES p(a, b))",
        "CREATE TABLE c(x REFERENCES p(a) ON DELETE CASCADE, y REFERENCES p)",
        "CREATE TABLE c(a, UNIQUE (\"A\"))",
    ]
    .iter()
    .enumerate()
    {
        pin_reads_hipp_file(
            &format!("fkok{i}"),
            s,
            "PRAGMA foreign_key_list(c); PRAGMA table_list(c)",
        );
    }
}

/// `case_sensitive_like` reaches generated columns, and once it has
/// been set at all the pin counts LIKE non-deterministic: a
/// generated column may not use it (CREATE TABLE, and ALTER TABLE,
/// which re-checks the whole table).
#[test]
fn regression_case_sensitive_like_and_generated_columns() {
    assert!(
        same(
            "csg",
            "CREATE TABLE t(a, b AS (a LIKE 'A%'), c AS (like('A%', a)) STORED, d AS (a LIKE 'A%' ESCAPE 'x')); \
             PRAGMA case_sensitive_like = 1; INSERT INTO t(a) VALUES ('abc'), ('Abc'); SELECT * FROM t; \
             PRAGMA case_sensitive_like = 0; SELECT * FROM t; SELECT a FROM t WHERE b"
        )
        .is_none()
    );
    for (i, s) in [
        "PRAGMA case_sensitive_like = 1; PRAGMA case_sensitive_like = 0; CREATE TABLE u(a, b AS (a LIKE 'A%'))",
        "PRAGMA case_sensitive_like = 0; CREATE TABLE u(a, b AS (like('A%', a)))",
        "PRAGMA case_sensitive_like = 1; CREATE TABLE u(a, d AS (a LIKE 'x' ESCAPE 'y'))",
        "CREATE TABLE u(a, b AS (a LIKE 'A%')); PRAGMA case_sensitive_like = 1; ALTER TABLE u ADD COLUMN c AS (a LIKE 'x')",
        "CREATE TABLE u(a, b AS (a LIKE 'A%')); PRAGMA case_sensitive_like = 1; ALTER TABLE u ADD COLUMN c",
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("csn{i}"), s, SchemaKind::NonDeterministic);
    }
    for (i, s) in [
        "PRAGMA case_sensitive_like; CREATE TABLE u(a, b AS (like('A%', a))); INSERT INTO u(a) VALUES ('a'); SELECT * FROM u",
        "PRAGMA case_sensitive_like = 1; CREATE TABLE u(a, b AS (a GLOB 'x'), c CHECK (c LIKE 'a')); \
         INSERT INTO u(a, c) VALUES ('x', 'a'); SELECT * FROM u",
    ]
    .iter()
    .enumerate()
    {
        assert!(same(&format!("csok{i}"), s).is_none(), "{s}");
    }
}

/// `table_list` counts a view's planned columns (it reported 0 for
/// a view over a derived table or CTE); one that cannot be planned
/// has none, as on the pin.
#[test]
fn regression_table_list_counts_view_columns() {
    assert!(
        same(
            "tlv",
            "CREATE TABLE t(a, b); CREATE VIEW v AS SELECT * FROM (SELECT a FROM t); \
             CREATE VIEW w AS WITH c AS (SELECT a, b FROM t) SELECT * FROM c; CREATE VIEW x AS SELECT * FROM nosuch; \
             CREATE VIEW y(p, q, r) AS SELECT 1; CREATE VIEW z(p) AS SELECT 1, 2; CREATE VIEW vv AS SELECT * FROM v, w; \
             PRAGMA table_list(v); PRAGMA table_list(w); PRAGMA table_list(x); PRAGMA table_list(y); \
             PRAGMA table_list(z); PRAGMA table_list(vv); \
             SELECT * FROM pragma_table_list WHERE type = 'view' ORDER BY name"
        )
        .is_none()
    );
}

/// A file whose only change is a header pragma (the pin leaves the
/// encoding and schema format unset) opens; a later CREATE TABLE
/// sets them, as the pin's does.
#[test]
fn regression_pin_file_with_only_a_header_write() {
    let p = common::temp_db("v16pr-hdronly");
    let (ok, _) = oracle_script(&p, "PRAGMA user_version = 77; PRAGMA application_id = 5");
    assert!(ok);
    let q = "PRAGMA user_version; PRAGMA application_id; PRAGMA encoding; PRAGMA schema_version; \
             SELECT count(*) FROM sqlite_schema";
    let (hok, hl, he) = hipp_script(&p, q);
    assert!(hok, "{he:?}");
    assert_eq!(hl, oracle_script(&p, q).1);
    let (hok, _, he) = hipp_script(
        &p,
        "CREATE TABLE t(a); CREATE INDEX i ON t(a DESC); INSERT INTO t VALUES (2), (1)",
    );
    assert!(hok, "{he:?}");
    let d = std::fs::read(&p).expect("read");
    assert_eq!(&d[44..48], &4u32.to_be_bytes(), "schema format");
    assert_eq!(&d[56..60], &1u32.to_be_bytes(), "encoding");
    let q = "PRAGMA integrity_check; PRAGMA user_version; PRAGMA index_xinfo(i); SELECT a FROM t ORDER BY a DESC";
    let (_, hl, _) = hipp_script(&p, q);
    assert_eq!(hl, oracle_script(&p, q).1);
}

/// `integrity_check(N)` with N outside 32 bits checks a table of
/// that name (the pin's `sqlite3GetInt32` failing).
#[test]
fn regression_integrity_check_limit_outside_32_bits() {
    for (i, a) in ["2147483648", "-2147483648", "99999999999"]
        .iter()
        .enumerate()
    {
        refuse(
            &format!("ic32-{i}"),
            &format!("CREATE TABLE t(a); PRAGMA integrity_check({a})"),
            SchemaKind::UnknownTable,
        );
    }
    for (i, a) in ["2147483647", "-2147483647", "1e3", "0x10", "+5", "1.5"]
        .iter()
        .enumerate()
    {
        assert!(
            same(
                &format!("ic32ok{i}"),
                &format!("CREATE TABLE t(a); PRAGMA integrity_check({a})")
            )
            .is_none()
        );
    }
}

/// The main schema table answers to both of its names as a
/// qualifier.
#[test]
fn regression_sqlite_master_qualifies_sqlite_schema() {
    for (i, q) in [
        "SELECT sqlite_master.name FROM sqlite_schema",
        "SELECT SQLITE_SCHEMA.name FROM sqlite_master",
        "SELECT sqlite_master.* FROM sqlite_schema",
        "SELECT sqlite_schema.rowid FROM main.sqlite_master",
        "SELECT name FROM sqlite_schema WHERE sqlite_master.type = 'table'",
        "SELECT sqlite_master.a FROM t AS sqlite_master",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            same(&format!("smq{i}"), &format!("CREATE TABLE t(a); {q}")).is_none(),
            "{q}"
        );
    }
    for (i, q) in [
        "SELECT sqlite_temp_master.name FROM sqlite_schema",
        "SELECT sqlite_schema.a FROM t AS sqlite_master",
        "SELECT sqlite_schema.name FROM sqlite_master s",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("smqe{i}"),
            &format!("CREATE TABLE t(a); {q}"),
            SchemaKind::UnknownColumn,
        );
    }
    for (i, q) in [
        "SELECT sqlite_master.name FROM sqlite_master, sqlite_schema",
        "SELECT * FROM sqlite_master JOIN sqlite_schema USING (name)",
    ]
    .iter()
    .enumerate()
    {
        refuse(
            &format!("smqa{i}"),
            &format!("CREATE TABLE t(a); {q}"),
            SchemaKind::AmbiguousColumn,
        );
    }
}

/// The full check of a table with an expression or partial index
/// answers as the pin does (v0.16, `expr_index`); without it, it is
/// refused before anything is reported (it printed a false "wrong #
/// of entries", or "ok", then failed). The quick check, which reads
/// no index keys, answers as the pin does either way.
#[test]
fn regression_integrity_check_expression_and_partial_indexes() {
    for (i, ix) in [
        "CREATE INDEX ip ON t(a) WHERE b > 0",
        "CREATE INDEX ie ON t(a + b)",
    ]
    .iter()
    .enumerate()
    {
        let p = common::temp_db(&format!("v16pr-opq{i}"));
        let setup = format!("CREATE TABLE t(a, b); {ix}; INSERT INTO t VALUES (1, 2), (2, -1)");
        assert!(oracle_script(&p, &setup).0);
        for q in ["PRAGMA quick_check", "PRAGMA quick_check(t)"] {
            let (hok, hl, _) = hipp_script(&p, q);
            assert!(hok);
            assert_eq!(hl, oracle_script(&p, q).1, "{ix}: {q}");
        }
        for q in ["PRAGMA integrity_check", "PRAGMA integrity_check(t)"] {
            let (hok, hl, he) = hipp_script(&p, q);
            if cfg!(feature = "expr_index") {
                assert!(hok, "{ix}: {q}: {he:?}");
                assert_eq!(hl, oracle_script(&p, q).1, "{ix}: {q}");
            } else {
                assert!(!hok && hl.is_empty(), "{ix}: {q}: {hl:?}");
                assert!(matches!(he, Some(Error::Unsupported)), "{he:?}");
            }
        }
    }
}

/// Copies of the oracle file `base` with byte edits: both engines'
/// checks; true when hipp's matched (hipp failing outright is
/// returned as false).
fn edited_check(base: &[u8], edits: &[(usize, u8)], q: &str, tag: &str) -> bool {
    let mut d = base.to_vec();
    for (o, v) in edits {
        d[*o] = *v;
    }
    let h = common::temp_db(&format!("v16pr-ed-{tag}-h"));
    let o = common::temp_db(&format!("v16pr-ed-{tag}-o"));
    std::fs::write(&h, &d).expect("write");
    std::fs::write(&o, &d).expect("write");
    let hres = block_on(async {
        let Ok(mut db) =
            Connection::open(FileStorage::open(&h).expect("fs"), Config::default()).await
        else {
            return None;
        };
        let mut lines = Vec::new();
        let Ok(mut st) = db.prepare(q).await else {
            return None;
        };
        loop {
            match st.step().await {
                Ok(Step::Row) => {
                    let mut l = String::new();
                    render(&st, &mut l);
                    lines.extend(l.split('\n').map(str::to_string));
                }
                Ok(Step::Done) => return Some(lines),
                Err(_) => return None,
            }
        }
    });
    let (ook, ol) = oracle_script(&o, q);
    match hres {
        Some(hl) => {
            assert!(ook, "{tag}: hipp answered {hl:?} where the pin failed");
            assert_eq!(hl, ol, "{tag}: {q}");
            true
        }
        None => false,
    }
}

/// The record-header test the pin's integrity check makes on each
/// row ("database disk image is malformed", one row, and the check
/// stops), over every header-size byte class, with and without
/// indexes and row constraints, under limits.
#[test]
fn regression_integrity_check_reads_record_headers() {
    for (k, setup) in [
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (2, NULL), (1, 'helloworld'), (3, NULL)",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(b); INSERT INTO t VALUES (2, NULL), (1, 'helloworld'), (3, NULL)",
        "CREATE TABLE t(a, b CHECK (b <> 'q')); INSERT INTO t VALUES (2, NULL), (1, 'helloworld'), (3, NULL)",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); INSERT INTO t VALUES (2, NULL), (1, 'helloworld'), (3, NULL)",
    ]
    .iter()
    .enumerate()
    {
        let p = common::temp_db(&format!("v16pr-rh{k}"));
        assert!(oracle_script(&p, setup).0);
        let base = std::fs::read(&p).expect("read");
        let at = base
            .windows(11)
            .position(|w| &w[1..] == b"helloworld")
            .expect("record")
            - 2;
        for (j, v) in [0x00u8, 0x01, 0x02, 0x03, 0x05, 0x0d, 0x0e, 0x7f, 0x80, 0xff]
            .iter()
            .enumerate()
        {
            for q in ["PRAGMA integrity_check", "PRAGMA quick_check", "PRAGMA integrity_check(1)"] {
                assert!(
                    edited_check(&base, &[(at, *v)], q, &format!("rh{k}-{j}")),
                    "{setup}: header byte {v:#x}: {q}"
                );
            }
        }
    }
}

/// The index probe of the full check searches each page as the
/// pin's seek does (binary search, raw-record comparison, cells
/// decoded only when visited), so damaged index keys report the
/// pin's rows; a deterministic corpus of single-byte edits.
#[test]
fn regression_integrity_check_damaged_index_keys() {
    let p = common::temp_db("v16pr-dik");
    let setup = "PRAGMA page_size = 512; CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c); CREATE INDEX i ON t(b); \
                 CREATE TABLE u(x UNIQUE, y); \
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 199) \
                 INSERT INTO t SELECT i, substr('ssssssssssssssssssssssssssssssssssssssssssssssssss', 1, i % 50), \
                 printf('%.*c', (i * 37) % 900, 'q') FROM c; \
                 WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 59) \
                 INSERT INTO u SELECT i, zeroblob(i * 3) FROM c; DELETE FROM t WHERE a % 7 = 0";
    assert!(oracle_script(&p, setup).0);
    let base = std::fs::read(&p).expect("read");
    // The review's case: one byte of an index key.
    assert!(edited_check(
        &base,
        &[(113_040, 0xf4)],
        "PRAGMA integrity_check",
        "dik0"
    ));
    let mut rng = Rng(0x0016_1d3e_5eed);
    let (mut ran, mut answered) = (0, 0);
    for k in 0..150 {
        let off = 512 + rng.below(base.len() - 512);
        let v = rng.below(256) as u8;
        ran += 1;
        if edited_check(
            &base,
            &[(off, v)],
            "PRAGMA integrity_check",
            &format!("dik{}", k + 1),
        ) {
            answered += 1;
        }
    }
    // hipp stops with an error only where the pin reads a cell
    // that lies off its page (see the gaps); everything it answers
    // is the pin's answer.
    assert!(answered * 100 >= ran * 95, "{answered} of {ran}");
}

/// Auto-vacuum files: the header's max-root-page field and the
/// pointer map are checked (messages, prefixes and order as the
/// pin's); a non-auto-vacuum file with the incremental-vacuum flag
/// set is reported.
#[test]
fn regression_integrity_check_pointer_maps() {
    let p = common::temp_db("v16pr-av");
    let setup = "PRAGMA page_size = 1024; PRAGMA auto_vacuum = 1; CREATE TABLE t(a, b); CREATE INDEX i ON t(b); \
                 WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 60) \
                 INSERT INTO t SELECT x, printf('%.*c', 300, char(65 + x % 26)) FROM c; CREATE TABLE big(x); \
                 INSERT INTO big VALUES (printf('%.*c', 3000, 'z')); DELETE FROM t WHERE a % 3 = 0";
    assert!(oracle_script(&p, setup).0);
    let base = std::fs::read(&p).expect("read");
    for (k, e) in [
        vec![(67, 1)],
        vec![(55, 3)],
        vec![(55, 7)],
        vec![(55, 0)],
        vec![(1029, 5)],
        vec![(1030, 9)],
        vec![(1029, 0)],
        vec![(1029, 9)],
        vec![(1034, 5)],
        vec![(1040, 2)],
        vec![(1044, 4)],
        vec![(1049, 1)],
    ]
    .iter()
    .enumerate()
    {
        for q in [
            "PRAGMA integrity_check",
            "PRAGMA quick_check",
            "PRAGMA integrity_check(2)",
            "PRAGMA integrity_check(t)",
        ] {
            assert!(edited_check(&base, e, q, &format!("av{k}")), "{e:?}: {q}");
        }
    }
    let mut rng = Rng(0x0016_a0a0_a0a0);
    let (mut ran, mut answered) = (0, 0);
    for k in 0..120 {
        let off = 1024 + rng.below(base.len() - 1024);
        ran += 1;
        if edited_check(
            &base,
            &[(off, rng.below(256) as u8)],
            "PRAGMA integrity_check",
            &format!("avf{k}"),
        ) {
            answered += 1;
        }
    }
    assert!(answered * 100 >= ran * 95, "{answered} of {ran}");
    let p = common::temp_db("v16pr-nav");
    assert!(oracle_script(&p, "CREATE TABLE t(a); INSERT INTO t VALUES (1)").0);
    let base = std::fs::read(&p).expect("read");
    for (k, e) in [vec![(67, 1)], vec![(55, 2)], vec![(55, 1)]]
        .iter()
        .enumerate()
    {
        for q in [
            "PRAGMA integrity_check",
            "PRAGMA integrity_check(t)",
            "PRAGMA quick_check",
        ] {
            assert!(edited_check(&base, e, q, &format!("nav{k}")), "{e:?}: {q}");
        }
    }
    // Header fields the pin reads at open: a page count beyond the
    // file is malformed there (hipp read past the end as empty
    // pages); an encoding's high bits are ignored.
    for (k, e) in [
        vec![(30, 0x20)],
        vec![(31, 0x80)],
        vec![(58, 0x42)],
        vec![(57, 0x4d)],
    ]
    .iter()
    .enumerate()
    {
        let mut d = base.clone();
        for (o, v) in e {
            d[*o] = *v;
        }
        let h = common::temp_db(&format!("v16pr-hdr{k}"));
        std::fs::write(&h, &d).expect("write");
        let opened = block_on(async {
            Connection::open(FileStorage::open(&h).expect("fs"), Config::default())
                .await
                .is_ok()
        });
        let (ook, _) = oracle_script(&h, "PRAGMA integrity_check");
        assert_eq!(opened, ook, "{e:?}");
        if opened {
            assert!(edited_check(
                &base,
                e,
                "PRAGMA integrity_check",
                &format!("hdrc{k}")
            ));
        }
    }
}
