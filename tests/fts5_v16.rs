//! v0.16 FTS5 full-text search (leaf `fts5`), differentially vs the
//! pinned oracle (SQLite 3.54.1, `SQLITE_ENABLE_FTS5`):
//!
//! - CREATE VIRTUAL TABLE ... USING fts5(...): columns, UNINDEXED,
//!   prefix=, tokenize= (unicode61 with remove_diacritics /
//!   tokenchars / separators / categories, ascii, porter, trigram),
//!   content= / content_rowid= / content='' / contentless_delete=,
//!   columnsize=, detail=; the shadow tables with the pin's text, the
//!   argument errors, DROP.
//! - The on-disk index byte for byte: %_data (structure record,
//!   averages, leaf pages, doclist indexes), %_idx, %_docsize,
//!   %_config, after inserts, deletes, updates, the special commands
//!   (delete-all, rebuild, optimize, merge, automerge, crisismerge,
//!   usermerge, pgsz, hashsize, rank, integrity-check, delete) and the
//!   pin's flush points (commit, savepoints, statement savepoints).
//! - Queries: the full query syntax, MATCH and `t = 'q'`, column
//!   filters, ORDER BY rank, bm25() with weights, highlight(),
//!   snippet(), fts5vocab (row / col / instance).
//! - Generated corpora of multi-script documents, random queries of
//!   every operator and write / delete / optimize sequences compare
//!   every output line and the shadow tables; each engine's
//!   integrity-check passes on the other's file; files move between
//!   the engines mid-way.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type FsError = <FileStorage as hipp::Storage>::Error;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// One row as the pin's shell prints it in list mode.
fn render(q: &hipp::Statement<'_, FileStorage>, line: &mut String) {
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

/// hipp on `path`: every statement, continuing past errors as the
/// shell does without `-bail`. The output lines, and per statement
/// whether it failed (with the first error).
fn hipp_run(
    path: &std::path::Path,
    stmts: &[String],
) -> (Vec<String>, Vec<bool>, Option<Error<FsError>>) {
    block_on(async {
        let mut db = open(path).await;
        let mut lines = Vec::new();
        let mut failed = Vec::new();
        let mut first = None;
        for stmt in stmts {
            let mut q = match db.prepare(stmt).await {
                Ok(q) => q,
                Err(e) => {
                    failed.push(true);
                    first.get_or_insert(e);
                    continue;
                }
            };
            let mut bad = false;
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
                    Err(e) => {
                        bad = true;
                        first.get_or_insert(e);
                        break;
                    }
                }
            }
            failed.push(bad);
        }
        (lines, failed, first)
    })
}

/// The pin's shell on `path`: the statements on stdin, its stdout
/// lines and the number of error lines on stderr.
fn pin_run(path: &std::path::Path, stmts: &[String]) -> (Vec<String>, usize) {
    let out = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            let mut sin = c.stdin.take().expect("stdin");
            for stmt in stmts {
                writeln!(sin, "{stmt};")?;
            }
            drop(sin);
            c.wait_with_output()
        })
        .expect("oracle runs");
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let errs = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| {
            (l.starts_with("Error")
                || l.starts_with("Parse error")
                || l.starts_with("Runtime error"))
                && l.contains(" near line ")
        })
        .count();
    (lines, errs)
}

/// Statements as owned strings.
fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// `stmts` on fresh twin files: the same stdout lines (a `#i` line
/// before statement i, so a difference names its statement) and the
/// same failing statements. Returns hipp's first error.
fn same(tag: &str, stmts: &[String]) -> Option<Error<FsError>> {
    let h = common::temp_db(&format!("v16fts-{tag}-h"));
    let o = common::temp_db(&format!("v16fts-{tag}-o"));
    same_on(&h, &o, stmts)
}

fn same_on(h: &std::path::Path, o: &std::path::Path, stmts: &[String]) -> Option<Error<FsError>> {
    let mut marked = Vec::new();
    for (i, s) in stmts.iter().enumerate() {
        marked.push(format!("SELECT '#{i}'"));
        marked.push(s.clone());
    }
    let (hl, hf, he) = hipp_run(h, &marked);
    let (ol, oerr) = pin_run(o, &marked);
    if hl != ol {
        let at = hl
            .iter()
            .zip(&ol)
            .position(|(a, b)| a != b)
            .unwrap_or(hl.len().min(ol.len()));
        let stmt = ol[..at.min(ol.len())]
            .iter()
            .rev()
            .find_map(|l| l.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()))
            .and_then(|i| stmts.get(i));
        panic!(
            "{h:?}: output differs at line {at} (statement {stmt:?})\npin : {:?}\nhipp: {:?}\nhipp error: {he:?}",
            ol.get(at..(at + 3).min(ol.len())),
            hl.get(at..(at + 3).min(hl.len())),
        );
    }
    let herr = hf.iter().filter(|b| **b).count();
    assert_eq!(
        herr, oerr,
        "{h:?}: failing statement count differs; hipp error: {he:?}"
    );
    he
}

/// Both refuse the last statement; hipp with `kind`.
fn refuse(tag: &str, stmts: &[&str], kind: SchemaKind) {
    let h = common::temp_db(&format!("v16fts-{tag}-h"));
    let o = common::temp_db(&format!("v16fts-{tag}-o"));
    let stmts = v(stmts);
    let (_, hf, he) = hipp_run(&h, &stmts);
    let (_, oerr) = pin_run(&o, &stmts);
    assert_eq!(oerr, 1, "the pin refuses {stmts:?}");
    assert_eq!(hf.iter().filter(|b| **b).count(), 1, "{stmts:?}: {he:?}");
    assert_eq!(hf.last(), Some(&true), "{stmts:?}");
    match he {
        Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "{stmts:?}"),
        other => panic!("expected {kind:?} for {stmts:?}, got {other:?}"),
    }
}

/// Every shadow table's rows (the index bytes as hex).
fn shadow(t: &str) -> Vec<String> {
    v(&[
        &format!("SELECT id, hex(block) FROM {t}_data ORDER BY id"),
        &format!("SELECT segid, hex(term), pgno FROM {t}_idx"),
        &format!("SELECT k, v FROM {t}_config"),
    ])
}

const CHECK: &str = "INSERT INTO t(t) VALUES('integrity-check')";

// ---------------- CREATE ----------------

#[test]
fn create_and_schema() {
    same(
        "create",
        &v(&[
            "CREATE VIRTUAL TABLE t USING fts5(a, b UNINDEXED, \"c d\", prefix='1,2 3', tokenize = 'porter unicode61 remove_diacritics 2')",
            "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY rowid",
            "SELECT id, hex(block) FROM t_data",
            "SELECT * FROM t_config",
            "PRAGMA table_info(t)",
            "PRAGMA table_xinfo(t)",
            "CREATE VIRTUAL TABLE IF NOT EXISTS t USING fts5(z)",
            "CREATE VIRTUAL TABLE e USING fts5(x, content='', columnsize=0, detail=none)",
            "CREATE VIRTUAL TABLE f USING fts5(x, content='', contentless_delete=1)",
            "CREATE TABLE src(k INTEGER PRIMARY KEY, x)",
            "CREATE VIRTUAL TABLE g USING fts5(x, content=src, content_rowid=k, detail=column)",
            "SELECT name, sql FROM sqlite_schema WHERE name GLOB '[efg]_*' ORDER BY name",
            "SELECT id, hex(block) FROM f_data",
            "SELECT * FROM pragma_table_list WHERE type IN ('virtual', 'shadow') ORDER BY name",
            "DROP TABLE e",
            "SELECT name FROM sqlite_schema ORDER BY name",
            "PRAGMA integrity_check",
        ]),
    );
    for (i, (sql, k)) in [
        (
            "CREATE VIRTUAL TABLE t USING fts5()",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, a)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(rank)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(rowid)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, detail=bogus)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, prefix=0)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, prefix='1000')",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, nosuch=1)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, columnsize=2)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a b)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, contentless_delete=1)",
            SchemaKind::Fts5Definition,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, tokenize='nosuch')",
            SchemaKind::Fts5Tokenizer,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, tokenize='unicode61 remove_diacritics 7')",
            SchemaKind::Fts5Tokenizer,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, tokenize='trigram bogus 1')",
            SchemaKind::Fts5Tokenizer,
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a, tokenize='porter nosuch')",
            SchemaKind::Fts5Tokenizer,
        ),
    ]
    .iter()
    .enumerate()
    {
        refuse(&format!("bad{i}"), &[sql], *k);
    }
}

/// ALTER TABLE RENAME of an fts5 table renames its shadow tables
/// (the pin's quoted text); pending terms follow the table.
#[test]
fn rename_tables() {
    same(
        "rename",
        &v(&[
            "CREATE VIRTUAL TABLE z USING fts5(a, \"b c\", prefix=2)",
            "INSERT INTO z VALUES('q', 'r')",
            "CREATE VIRTUAL TABLE z3 USING fts5(a, content='', columnsize=0)",
            "BEGIN",
            "INSERT INTO z VALUES('pend', 'ing')",
            "ALTER TABLE z RENAME TO \"Z\u{e9} 2\"",
            "INSERT INTO \"Z\u{e9} 2\" VALUES('more', 'x')",
            "SELECT rowid FROM \"Z\u{e9} 2\" WHERE \"Z\u{e9} 2\" MATCH 'pend OR more OR q'",
            "COMMIT",
            "ALTER TABLE z3 RENAME TO z4",
            "ALTER TABLE z4 RENAME TO \"Z\u{e9} 2\"",
            "ALTER TABLE z4 RENAME TO z4_data",
            "CREATE TABLE z5_idx(x)",
            "ALTER TABLE z4 RENAME TO z5",
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema",
            "SELECT * FROM \"Z\u{e9} 2\" WHERE \"Z\u{e9} 2\" MATCH 'q OR pend'",
            "INSERT INTO \"Z\u{e9} 2\"(\"Z\u{e9} 2\") VALUES('integrity-check')",
            "SELECT id, hex(block) FROM \"Z\u{e9} 2_data\"",
            "PRAGMA integrity_check",
        ]),
    );
}

// ---------------- tokenizers ----------------

const MULTI: &[&str] = &[
    "The quick brown fox jumps over the lazy dog",
    "Caf\u{e9} na\u{ef}ve r\u{e9}sum\u{e9} Stra\u{df}e \u{c9}COLE \u{1e9e} \u{130}stanbul",
    "\u{3a3}\u{38a}\u{3a3}\u{3a5}\u{3a6}\u{39f}\u{3a3} \u{3c9}\u{3bc}\u{3ad}\u{3b3}\u{3b1} \u{3b1}\u{30c}",
    "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43c}\u{438}\u{440}, \u{401}\u{416}\u{418}\u{41a}",
    "\u{6771}\u{4eac} \u{65e5}\u{672c}\u{8a9e}\u{306e}\u{6587}\u{7ae0} \u{d55c}\u{ad6d}\u{c5b4}",
    "emoji \u{1f642}\u{1f680} mixed123abc 4.5e10 under_score hy-phen o'clock",
    "\u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{645}\u{631}\u{62d}\u{628}\u{627} \u{928}\u{92e}\u{938}\u{94d}\u{924}\u{947} \u{e2a}\u{e27}\u{e31}\u{e2a}\u{e14}\u{e35}",
    "A\u{301}B\u{308}C\u{327} \u{fb01}nance \u{2160}\u{2161} \u{ff21}\u{ff22}\u{ff23} \u{24b6}\u{24b7}",
    "tab\there new\nline \u{a0}nbsp \u{2028}sep \u{200b}zw",
    "running runs ran easily fairly happiness relational generalizations",
];

#[test]
fn tokenizers_index_alike() {
    for (i, tok) in [
        "unicode61",
        "unicode61 remove_diacritics 0",
        "unicode61 remove_diacritics 1",
        "unicode61 remove_diacritics 2",
        "unicode61 tokenchars '-_' separators 'xq'",
        "unicode61 categories 'L* N* Co Mn'",
        "ascii",
        "ascii tokenchars '_' separators 'e'",
        "porter",
        "porter ascii",
        "porter unicode61 remove_diacritics 0",
        "trigram",
        "trigram case_sensitive 1",
        "trigram remove_diacritics 1",
    ]
    .iter()
    .enumerate()
    {
        let mut s = vec![
            format!("CREATE VIRTUAL TABLE t USING fts5(x, tokenize=\"{tok}\")"),
            "CREATE VIRTUAL TABLE vi USING fts5vocab(t, instance)".to_string(),
        ];
        for d in MULTI {
            s.push(format!("INSERT INTO t VALUES('{}')", d.replace('\'', "''")));
        }
        s.push("INSERT INTO t VALUES('x' || char(0) || 'y z')".to_string());
        s.push("SELECT term, doc, col, offset FROM vi".to_string());
        s.extend(shadow("t"));
        s.push(CHECK.to_string());
        same(&format!("tok{i}"), &s);
    }
}

/// The porter stemmer on words built to reach every rule.
#[test]
fn porter_stems_alike() {
    let stems = [
        "cat",
        "run",
        "happy",
        "relate",
        "condition",
        "ration",
        "conform",
        "radical",
        "hope",
        "valen",
        "hesit",
        "predic",
        "formal",
        "sensitiv",
        "electr",
        "adopt",
        "adjust",
        "depend",
        "communic",
        "irrit",
        "effect",
        "gener",
        "controll",
        "roll",
        "bowdler",
        "fall",
        "hiss",
        "fizz",
        "fail",
        "fil",
        "agre",
        "plaster",
        "bled",
        "motor",
        "sing",
        "conflat",
        "troubl",
        "siz",
        "hop",
        "tann",
        "feed",
        "disabl",
        "matt",
        "sky",
        "by",
        "say",
        "oscill",
        "triplic",
        "revival",
        "allowanc",
        "airlin",
        "gyroscop",
        "defens",
        "replac",
        "homolog",
        "activat",
        "angular",
        "probat",
        "ceas",
        "abl",
        "ant",
        "ment",
        "ion",
        "ou",
        "ate",
        "iti",
        "ive",
    ];
    let sufs = [
        "", "s", "es", "ies", "sses", "ss", "ed", "ing", "eed", "ated", "ational", "tional",
        "enci", "anci", "izer", "bli", "alli", "entli", "eli", "ousli", "ization", "ation", "ator",
        "alism", "iveness", "fulness", "ousness", "aliti", "iviti", "biliti", "logi", "icate",
        "ative", "alize", "iciti", "ical", "ful", "ness", "al", "ance", "ence", "er", "ic", "able",
        "ible", "ant", "ement", "ment", "ent", "ion", "ou", "ism", "ate", "iti", "ous", "ive",
        "ize", "e", "ll", "y", "ly", "ingly", "edly", "izing", "ations",
    ];
    let mut words = Vec::new();
    for s in stems {
        for x in sufs {
            words.push(format!("{s}{x}"));
        }
    }
    let mut s = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='porter ascii')",
        "CREATE VIRTUAL TABLE vr USING fts5vocab(t, row)",
    ]);
    for chunk in words.chunks(60) {
        s.push(format!("INSERT INTO t VALUES('{}')", chunk.join(" ")));
    }
    s.push("SELECT term, doc, cnt FROM vr".to_string());
    same("porter", &s);
}

// ---------------- queries ----------------

const QDOCS: &[(&str, &str)] = &[
    ("one two three four five", "alpha beta"),
    ("two three", "gamma delta two"),
    ("three four five six seven eight nine ten", "one"),
    ("a b c d e f g h i j k l m n o p", "alpha one two"),
    ("one one one two", "beta beta gamma"),
    ("five four three two one", "delta alphabet alpine"),
    ("", "two"),
    ("seven seven eight", ""),
    ("one two. three four, five; six!", "Alpha BETA gamma"),
    ("the end of the line is near the end", "near end"),
];

const QUERIES: &[&str] = &[
    "one",
    "one two",
    "one AND two",
    "one OR alpha",
    "one NOT two",
    "one NOT two OR three",
    "(one OR alpha) AND (two OR beta)",
    "one AND two OR three AND four",
    "one OR two AND three",
    "\"one two\"",
    "\"two three\" OR \"four five\"",
    "one + two",
    "\"one\" \"two\"",
    "al*",
    "alph* OR t*",
    "\"three fo*\"",
    "^one",
    "^two",
    "^\"two three\"",
    "a: one",
    "b: two",
    "{a b}: alpha",
    "- a: two",
    "-{b}: one",
    "a: (one OR alpha) NOT b: beta",
    "b : \"alpha one\"",
    "NEAR(one five)",
    "NEAR(one five, 2)",
    "NEAR(one five, 0)",
    "NEAR(\"one two\" five, 3)",
    "NEAR(one two three, 4)",
    "a : NEAR(one two)",
    "NEAR(end line, 2) OR NEAR(the near)",
    "\"one\" AND NOT",
    "one AND",
    "(one",
    "one)",
    "nosuchcol: one",
    "NEAR(one",
    "\"unterminated",
    "one OR",
    "*",
    "",
    "AND",
    "NOT one",
    "one NOT (two OR three)",
    "seven eight",
    "\"seven eight\"",
    "x OR y OR one",
    "ONE",
    "one or two",
    "one OR NOT",
    "a:one AND b:alpha",
    "two NOT a:two",
];

fn query_script(cfg: &str) -> Vec<String> {
    let mut s = vec![format!("CREATE VIRTUAL TABLE t USING fts5(a, b{cfg})")];
    for (x, y) in QDOCS {
        s.push(format!("INSERT INTO t VALUES('{x}', '{y}')"));
    }
    for q in QUERIES {
        let q = q.replace('\'', "''");
        s.push(format!("SELECT rowid, rank FROM t WHERE t MATCH '{q}'"));
        s.push(format!("SELECT rowid FROM t WHERE t = '{q}' ORDER BY rank"));
        s.push(format!(
            "SELECT rowid, highlight(t, 0, '[', ']'), highlight(t, 1, '<', '>') FROM t('{q}') ORDER BY rowid DESC"
        ));
        s.push(format!(
            "SELECT snippet(t, -1, '[', ']', '...', 3), snippet(t, 1, '', '', '', 64) FROM t WHERE t MATCH '{q}'"
        ));
        s.push(format!(
            "SELECT rowid, bm25(t, 2.0, 0.5) FROM t WHERE b MATCH '{q}'"
        ));
    }
    s
}

#[test]
fn query_syntax_detail_full() {
    same("q-full", &query_script(""));
    same("q-prefix", &query_script(", prefix='1 2 3'"));
    same("q-porter", &query_script(", tokenize=porter, columnsize=0"));
}

#[test]
fn query_syntax_detail_column_and_none() {
    same("q-col", &query_script(", detail=column"));
    same("q-none", &query_script(", detail=none"));
    same("q-col-pfx", &query_script(", detail=column, prefix=1"));
}

#[test]
fn query_syntax_contentless_and_external() {
    same("q-ctless", &query_script(", content=''"));
    let mut s = v(&[
        "CREATE TABLE src(id INTEGER PRIMARY KEY, a, b)",
        "INSERT INTO src VALUES(3, 'one two', 'x'), (7, 'two three', 'one'), (9, 'z', 'z')",
        "CREATE VIRTUAL TABLE t USING fts5(a, b, content=src, content_rowid=id)",
        "INSERT INTO t(t) VALUES('rebuild')",
    ]);
    for q in ["one", "two OR z", "\"one two\"", "b:one", "t*"] {
        s.push(format!(
            "SELECT rowid, a, b, rank, highlight(t, 0, '(', ')') FROM t WHERE t MATCH '{q}' ORDER BY rank"
        ));
    }
    s.extend(shadow("t"));
    s.push(CHECK.to_string());
    same("q-ext", &s);
}

#[test]
fn query_plans_and_errors() {
    let mut s = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b)",
        "INSERT INTO t(rowid, a, b) VALUES(5, 'x y', 'z'), (10, 'y', 'x'), (-3, 'x', ''), (20, 'q', 'x x x')",
        "SELECT rowid, * FROM t",
        "SELECT rowid FROM t ORDER BY rowid DESC",
        "SELECT * FROM t WHERE rowid = 10",
        "SELECT rowid FROM t WHERE rowid > 0 AND rowid <= 10",
        "SELECT rowid FROM t WHERE rowid BETWEEN -5 AND 6 ORDER BY rowid DESC",
        "SELECT rowid FROM t WHERE t MATCH 'x' AND rowid < 15 ORDER BY rowid DESC",
        "SELECT rowid, rank FROM t WHERE t MATCH 'x' AND rank MATCH 'bm25(10.0, 1.0)' ORDER BY rank",
        "SELECT rowid FROM t WHERE t MATCH 'x' AND t MATCH 'y'",
        "SELECT rowid FROM t WHERE a MATCH 'x' AND b MATCH 'x'",
        "SELECT rowid, rank FROM t('x') ORDER BY rank DESC",
        "SELECT count(*) FROM t WHERE t MATCH 'x OR y OR z'",
        "SELECT t.rowid, u.rowid FROM t, t AS u WHERE t MATCH 'z' AND u MATCH 'q'",
        "SELECT rowid FROM t WHERE t MATCH 'x' ORDER BY rank LIMIT 2 OFFSET 1",
        "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'x' ORDER BY bm25(t)",
        "SELECT a FROM t WHERE t MATCH 'nocol: x'",
        "SELECT a FROM t WHERE t MATCH 'x AND'",
        "SELECT highlight(t, 5, '', '') FROM t WHERE t MATCH 'x'",
        "SELECT snippet(t, 0, '', '') FROM t WHERE t MATCH 'x'",
        "SELECT bm25(t) FROM t",
        "SELECT highlight(t, 0, '[', ']') FROM t",
        "SELECT rowid FROM t WHERE a MATCH 'b : x'",
        "SELECT rowid FROM t WHERE 'x' MATCH t",
        "SELECT rowid FROM t WHERE t MATCH NULL",
        "SELECT 1 WHERE 'a' MATCH 'a'",
        "SELECT rowid, rank FROM t WHERE t MATCH 'x' AND rank MATCH 'nosuchfn()'",
        "INSERT INTO t(t, rank) VALUES('rank', 'bm25(1.0, 10.0)')",
        "SELECT rowid, rank FROM t WHERE t MATCH 'x' ORDER BY rank",
        "SELECT k, v FROM t_config ORDER BY k",
        "INSERT INTO t(t, rank) VALUES('rank', 'nosuch(')",
    ]);
    s.push(CHECK.to_string());
    same("plans", &s);
}

// ---------------- auxiliary functions ----------------

#[test]
fn snippets_and_highlights() {
    let long = "Lorem ipsum dolor sit amet. Consectetur adipiscing elit, sed do eiusmod tempor. \
        Incididunt ut labore et dolore magna aliqua! Ut enim ad minim veniam? Quis nostrud \
        exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. Duis aute irure \
        dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur.";
    for (i, cfg) in ["", ", detail=column", ", detail=none", ", tokenize=trigram"]
        .iter()
        .enumerate()
    {
        let mut s = vec![
            format!("CREATE VIRTUAL TABLE t USING fts5(a, b, c UNINDEXED{cfg})"),
            format!("INSERT INTO t VALUES('{long}', 'dolor magna ut', 'dolor')"),
            "INSERT INTO t VALUES('ut ut ut', 'short dolor', NULL)".to_string(),
            "INSERT INTO t VALUES(X'616263', 12.5, 'x')".to_string(),
        ];
        for q in [
            "dolor",
            "ut OR magna",
            "\"dolor in\"",
            "dol*",
            "NEAR(dolor ut, 3)",
            "b : dolor",
            "ipsum + dolor",
            "lor",
        ] {
            if (cfg.contains("detail")
                && (q.contains('"') || q.contains('+') || q.contains("NEAR")))
                || (cfg.contains("none") && q.contains(':'))
            {
                continue;
            }
            s.push(format!("SELECT rowid, highlight(t, 0, '<b>', '</b>'), highlight(t, 1, '[', ']') FROM t WHERE t MATCH '{q}'"));
            if !cfg.contains("detail") {
                for n in [1, 4, 10, 64] {
                    s.push(format!("SELECT snippet(t, -1, '[', ']', '...', {n}), snippet(t, 0, '', '', '..', {n}) FROM t WHERE t MATCH '{q}'"));
                }
            }
            s.push(format!("SELECT rowid, bm25(t), bm25(t, 1), bm25(t, 0.5, 2, 9) FROM t WHERE t MATCH '{q}' ORDER BY rank"));
        }
        same(&format!("aux{i}"), &s);
    }
}

/// Trigram tables take LIKE / GLOB patterns as queries (the
/// auxiliary functions see their phrases), still checking them.
#[test]
fn trigram_like_and_glob() {
    for (i, cfg) in [
        "tokenize=trigram",
        "tokenize='trigram case_sensitive 1'",
        "tokenize=trigram, detail=column",
        "tokenize=trigram, detail=none",
        "tokenize='trigram remove_diacritics 1'",
        "tokenize=unicode61",
    ]
    .iter()
    .enumerate()
    {
        let mut s = vec![
            format!("CREATE VIRTUAL TABLE t USING fts5(a, b, {cfg})"),
            "INSERT INTO t VALUES('call the plumber', 'Plumbing works'), ('abcdef', 'xyz abc'), \
             ('ABCDEF', 'q\"uote'), ('caf\u{e9} na\u{ef}ve', '\u{6771}\u{4eac}\u{90fd}\u{5e9c}')"
                .to_string(),
        ];
        for w in [
            "a LIKE '%lumb%'",
            "b LIKE 'plumb%works'",
            "a GLOB '*bcd*'",
            "a GLOB '*B[CX]DEF'",
            "a GLOB 'ab?def'",
            "a GLOB '*[^q]cdef'",
            "a LIKE 'ab'",
            "t MATCH 'abc' AND b LIKE '%xyz%'",
            "b LIKE '%q\"uo%'",
            "a NOT LIKE '%lumb%'",
            "like('%lumb%', a)",
            "glob('*bcd*', a)",
            "a LIKE '%lumb%' ESCAPE 'x'",
            "a LIKE NULL",
            "a LIKE '%cde%' AND a LIKE '%abc%'",
            "a LIKE '%caf\u{e9}%'",
            "b LIKE '%\u{6771}\u{4eac}\u{90fd}%'",
            "a LIKE 12345",
        ] {
            s.push(format!(
                "SELECT rowid, highlight(t, 0, '[', ']'), highlight(t, 1, '[', ']'), bm25(t) FROM t WHERE {w}"
            ));
        }
        s.push("SELECT rowid, rank FROM t WHERE a LIKE '%bcde%' ORDER BY rank".to_string());
        same(&format!("pattern{i}"), &s);
    }
}

// ---------------- writes and special commands ----------------

#[test]
fn special_commands() {
    let mut s = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b, prefix=2)",
        "INSERT INTO t(t, rank) VALUES('pgsz', 64)",
        "INSERT INTO t(t, rank) VALUES('automerge', 2)",
        "INSERT INTO t(t, rank) VALUES('crisismerge', 3)",
        "INSERT INTO t(t, rank) VALUES('usermerge', 3)",
        "INSERT INTO t(t, rank) VALUES('hashsize', 500)",
        "SELECT k, v FROM t_config ORDER BY k",
    ]);
    for i in 0..30 {
        s.push(format!(
            "INSERT INTO t VALUES('w{} w{} common alpha{i}', 'b{} beta')",
            i % 7,
            i % 3,
            i % 5
        ));
    }
    s.extend(shadow("t"));
    s.extend(v(&[
        "INSERT INTO t(t, rank) VALUES('merge', 2)",
        "INSERT INTO t(t, rank) VALUES('merge', -5)",
        "INSERT INTO t(t, rank) VALUES('merge', 500)",
        "SELECT id, hex(block) FROM t_data ORDER BY id",
        "DELETE FROM t WHERE rowid % 4 = 0",
        "UPDATE t SET b = 'changed' WHERE rowid % 5 = 1",
        "INSERT INTO t(t) VALUES('optimize')",
        "SELECT id, hex(block) FROM t_data ORDER BY id",
        "INSERT INTO t(t) VALUES('rebuild')",
        "SELECT id, hex(block) FROM t_data ORDER BY id",
        "SELECT rowid FROM t WHERE t MATCH 'common AND w3'",
        "INSERT INTO t(t, rank) VALUES('pgsz', 31)",
        "INSERT INTO t(t, rank) VALUES('automerge', 65)",
        "INSERT INTO t(t, rank) VALUES('usermerge', 1)",
        "INSERT INTO t(t, rank) VALUES('nosuch', 1)",
        "INSERT INTO t(t) VALUES('nosuch')",
        "INSERT INTO t(t, rank) VALUES('integrity-check', 0)",
        "INSERT INTO t(t) VALUES('delete-all')",
        "SELECT count(*) FROM t",
        "INSERT INTO t(t, rowid, a, b) VALUES('delete', 1, 'x', 'y')",
        "SELECT k, v FROM t_config ORDER BY k",
    ]));
    s.extend(shadow("t"));
    s.push(CHECK.to_string());
    same("commands", &s);
    // Contentless tables: 'delete' with the old values, delete-all,
    // the refusals; contentless_delete inserts.
    let mut s = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b, content='')",
        "INSERT INTO t(rowid, a, b) VALUES(1, 'x y', 'z'), (2, 'y', 'y y')",
        "INSERT INTO t VALUES('auto', 'rowid')",
        "SELECT rowid, a, b FROM t",
        "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'y'",
        "INSERT INTO t(t, rowid, a, b) VALUES('delete', 1, 'x y', 'z')",
        "SELECT rowid FROM t WHERE t MATCH 'y OR x'",
        "DELETE FROM t WHERE rowid = 2",
        "UPDATE t SET a = 'q' WHERE rowid = 2",
        "INSERT INTO t(t) VALUES('rebuild')",
        "INSERT INTO t(t) VALUES('optimize')",
        "SELECT id, hex(block) FROM t_data ORDER BY id",
        "SELECT id, hex(sz) FROM t_docsize",
        CHECK,
        "INSERT INTO t(t) VALUES('delete-all')",
        "SELECT id, hex(block) FROM t_data ORDER BY id",
        "CREATE VIRTUAL TABLE u USING fts5(a, content='', contentless_delete=1)",
        "INSERT INTO u(rowid, a) VALUES(10, 'p q'), (11, 'q r')",
        "SELECT id, hex(block) FROM u_data ORDER BY id",
        "SELECT id, hex(sz), hex(origin) FROM u_docsize",
        "SELECT rowid FROM u WHERE u MATCH 'q'",
        "INSERT INTO u(u) VALUES('integrity-check')",
    ]);
    s.push("SELECT 1".to_string());
    same("contentless", &s);
}

#[test]
fn rowids_conflicts_and_types() {
    same(
        "rowids",
        &v(&[
            "CREATE VIRTUAL TABLE t USING fts5(a)",
            "INSERT INTO t(rowid, a) VALUES(-9223372036854775808, 'min')",
            "INSERT INTO t(rowid, a) VALUES(0, 'zero')",
            "INSERT INTO t(rowid, a) VALUES(0, 'again')",
            "INSERT OR REPLACE INTO t(rowid, a) VALUES(0, 'replaced')",
            "INSERT OR IGNORE INTO t(rowid, a) VALUES(0, 'ignored')",
            "INSERT INTO t(rowid, a) VALUES('12', 12)",
            "INSERT INTO t(rowid, a) VALUES(1.5, 'real')",
            "INSERT INTO t(rowid, a) VALUES('x', 'text')",
            "INSERT INTO t VALUES(X'00ff41')",
            "INSERT INTO t VALUES(NULL)",
            "INSERT INTO t VALUES(-0.0)",
            "SELECT rowid, typeof(a), quote(a) FROM t ORDER BY rowid",
            "INSERT INTO t(rowid, a) VALUES(9223372036854775807, 'max')",
            "SELECT rowid, a FROM t WHERE t MATCH 'max'",
            "UPDATE t SET rowid = 100 WHERE rowid = 12",
            "UPDATE t SET rowid = 0 WHERE rowid = 100",
            "SELECT rowid, a FROM t WHERE t MATCH 'replaced OR 12'",
            "SELECT last_insert_rowid(), changes()",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            CHECK,
        ]),
    );
}

#[test]
fn transactions_and_savepoints() {
    same(
        "txn",
        &v(&[
            "CREATE VIRTUAL TABLE t USING fts5(a)",
            "INSERT INTO t(t, rank) VALUES('pgsz', 40)",
            "BEGIN",
            "INSERT INTO t VALUES('one two')",
            "SELECT rowid FROM t WHERE t MATCH 'one'",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            "SAVEPOINT a",
            "INSERT INTO t VALUES('three four')",
            "SAVEPOINT b",
            "INSERT INTO t VALUES('five')",
            "ROLLBACK TO b",
            "SELECT rowid FROM t WHERE t MATCH 'five OR three'",
            "RELEASE b",
            "INSERT INTO t VALUES('six'), ('seven')",
            "ROLLBACK TO a",
            "SELECT rowid, a FROM t",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            "RELEASE a",
            "COMMIT",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            "BEGIN",
            "INSERT INTO t VALUES('rolled back')",
            "INSERT INTO t(t) VALUES('optimize')",
            "ROLLBACK",
            "SELECT rowid, a FROM t WHERE t MATCH 'rolled OR one'",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            "BEGIN",
            "INSERT INTO t(rowid, a) VALUES(50, 'p'), (40, 'q')",
            "INSERT INTO t(rowid, a) VALUES(1, 'dup')",
            "DELETE FROM t WHERE rowid = 50",
            "INSERT INTO t(rowid, a) VALUES(50, 'p again')",
            "COMMIT",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            CHECK,
        ]),
    );
}

#[test]
fn external_content_with_triggers() {
    same(
        "ext",
        &v(&[
            "CREATE TABLE doc(id INTEGER PRIMARY KEY, title, body)",
            "CREATE VIRTUAL TABLE t USING fts5(title, body, content=doc, content_rowid=id)",
            "CREATE TRIGGER doc_ai AFTER INSERT ON doc BEGIN INSERT INTO t(rowid, title, body) VALUES(new.id, new.title, new.body); END",
            "CREATE TRIGGER doc_ad AFTER DELETE ON doc BEGIN INSERT INTO t(t, rowid, title, body) VALUES('delete', old.id, old.title, old.body); END",
            "CREATE TRIGGER doc_au AFTER UPDATE ON doc BEGIN INSERT INTO t(t, rowid, title, body) VALUES('delete', old.id, old.title, old.body); INSERT INTO t(rowid, title, body) VALUES(new.id, new.title, new.body); END",
            "INSERT INTO doc(title, body) VALUES('First', 'the quick brown fox'), ('Second', 'jumps over'), ('Third', 'the lazy dog')",
            "INSERT INTO doc VALUES(10, 'Tenth', 'fox and dog')",
            "SELECT rowid, title, highlight(t, 1, '*', '*') FROM t WHERE t MATCH 'fox OR dog' ORDER BY rank",
            "UPDATE doc SET body = 'a cat' WHERE id = 10",
            "DELETE FROM doc WHERE id = 1",
            "SELECT rowid, title FROM t WHERE t MATCH 'fox OR cat OR dog'",
            "BEGIN",
            "INSERT INTO doc(title, body) VALUES('In txn', 'quick fox')",
            "UPDATE doc SET title = 'Renamed' WHERE id = 2",
            "SELECT rowid FROM t WHERE t MATCH 'quick OR renamed'",
            "COMMIT",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            "SELECT id, hex(sz) FROM t_docsize",
            CHECK,
            "INSERT INTO t(t) VALUES('rebuild')",
            "SELECT id, hex(block) FROM t_data ORDER BY id",
            CHECK,
        ]),
    );
}

#[test]
fn vocab_tables() {
    let mut s = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b)",
        "CREATE VIRTUAL TABLE vr USING fts5vocab(t, row)",
        "CREATE VIRTUAL TABLE vc USING fts5vocab('t', 'col')",
        "CREATE VIRTUAL TABLE vi USING fts5vocab(t, instance)",
        "CREATE VIRTUAL TABLE v3 USING fts5vocab(main, t, instance)",
        "INSERT INTO t VALUES('x y z', 'x x'), ('y', ''), (NULL, 'z w')",
        "SELECT * FROM vr",
        "SELECT * FROM vc",
        "SELECT * FROM vi",
        "SELECT term, cnt FROM vr WHERE term >= 'x' AND term < 'z'",
        "SELECT * FROM vc WHERE term = 'x'",
        "PRAGMA table_info(vi)",
        "BEGIN",
        "INSERT INTO t VALUES('pending', 'terms')",
        "SELECT * FROM vr",
        "SELECT id, hex(block) FROM t_data ORDER BY id",
        "COMMIT",
        "CREATE VIRTUAL TABLE bad USING fts5vocab(t, nosuch)",
        "CREATE VIRTUAL TABLE bad USING fts5vocab(t)",
        "SELECT * FROM vr WHERE vr MATCH 'x'",
    ]);
    s.push("CREATE VIRTUAL TABLE n USING fts5(a, detail=none)".to_string());
    s.push("CREATE VIRTUAL TABLE nv USING fts5vocab(n, instance)".to_string());
    s.push("CREATE VIRTUAL TABLE nc USING fts5vocab(n, col)".to_string());
    s.push("INSERT INTO n VALUES('q r q')".to_string());
    s.push("SELECT * FROM nv".to_string());
    s.push("SELECT * FROM nc".to_string());
    same("vocab", &s);
}

// ---------------- regressions ----------------

/// Each a bug the generated corpora found, kept as its own case.
#[test]
fn regressions() {
    let cases: &[(&str, &[&str])] = &[
        // snippet() closes a highlight still open at the document's end.
        (
            "snippet-close",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t VALUES('x y z')",
                "SELECT snippet(t, 0, '[', ']', '...', 2) FROM t WHERE t MATCH '\"y z\"'",
            ],
        ),
        // ORDER BY rank on detail=column: the sorted cursor's phrase
        // poslists (all phrases tested), rank recomputed.
        (
            "rank-detail-column",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, b, detail=column)",
                "INSERT INTO t VALUES('p q', 'q'), ('q', 'p p'), ('r', 'p')",
                "SELECT rowid, rank, bm25(t) FROM t WHERE t MATCH 'p OR (q AND r)' ORDER BY rank",
                "SELECT rowid, highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'p OR q NOT r' ORDER BY rank",
            ],
        ),
        // `-col :` leaving no columns is not EOF; a nested filter
        // emptying the set is.
        (
            "empty-colset",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t VALUES('x')",
                "SELECT rowid FROM t WHERE t MATCH '- a : x'",
                "SELECT rowid FROM t WHERE t MATCH 'a : (- a : x)'",
                "SELECT rowid FROM t WHERE t MATCH 'x OR - a : x'",
            ],
        ),
        // ^ phrases: rows where every term is present but the phrase
        // fails stay candidates of the AND (nomatch exposure).
        (
            "caret-nomatch",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, b)",
                "INSERT INTO t VALUES('y x', 'x'), ('x y', 'y')",
                "SELECT rowid, highlight(t, 0, '[', ']'), highlight(t, 1, '[', ']') FROM t WHERE t MATCH '^x AND y'",
                "SELECT rowid, bm25(t) FROM t WHERE t MATCH '^x OR y'",
            ],
        ),
        // NEAR groups drop phrases a tokenizer makes empty (trigram).
        (
            "near-empty-trigram",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, tokenize=trigram)",
                "INSERT INTO t VALUES('abcdef ab')",
                "SELECT rowid FROM t WHERE t MATCH 'NEAR(ab abc)'",
                "SELECT rowid FROM t WHERE t MATCH 'NEAR(ab cde, 1)'",
            ],
        ),
        // Prefix queries read the prefix indexes as the pin does.
        (
            "prefix-index",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, prefix='1 2')",
                "INSERT INTO t VALUES('ab abc b'), ('a'), ('abd')",
                "SELECT rowid FROM t WHERE t MATCH 'a*'",
                "SELECT rowid FROM t WHERE t MATCH 'ab*'",
                "SELECT rowid FROM t WHERE t MATCH 'abc*'",
                "SELECT segid, hex(term), pgno FROM t_idx",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
            ],
        ),
        // porter step 1a: "-es" loses only the "s".
        (
            "porter-1a",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, tokenize=porter)",
                "INSERT INTO t VALUES('caresses ponies ties caress cats horses')",
                "CREATE VIRTUAL TABLE v USING fts5vocab(t, row)",
                "SELECT term FROM v",
            ],
        ),
        // Merges skip older duplicates in the pin's tournament order.
        (
            "merge-trim",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t(t, rank) VALUES('pgsz', 32)",
                "INSERT INTO t(t, rank) VALUES('automerge', 2)",
                "INSERT INTO t(rowid, a) VALUES(1, 'a b c d e f g h')",
                "INSERT INTO t(rowid, a) VALUES(2, 'a b c d e f g h')",
                "UPDATE t SET a = 'c d' WHERE rowid = 1",
                "INSERT INTO t(rowid, a) VALUES(3, 'b c')",
                "INSERT INTO t(t, rank) VALUES('merge', 1)",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
                "INSERT INTO t(t, rank) VALUES('merge', 1)",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
                CHECK,
            ],
        ),
        // Writes a trigger fires in autocommit reach the index.
        (
            "trigger-autocommit",
            &[
                "CREATE TABLE s(x)",
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "CREATE TRIGGER ai AFTER INSERT ON s BEGIN INSERT INTO t VALUES(new.x); END",
                "INSERT INTO s VALUES('hello world')",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
                "SELECT rowid FROM t WHERE t MATCH 'hello'",
            ],
        ),
        // A statement journal on another table flushes pending terms.
        (
            "stmt-journal-flush",
            &[
                "CREATE TABLE s(x NOT NULL)",
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "BEGIN",
                "INSERT INTO t VALUES('pending')",
                "INSERT INTO s VALUES(1), (2)",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
                "COMMIT",
            ],
        ),
        // fts5vocab syncs the pending terms and totals first.
        (
            "vocab-flush",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "CREATE VIRTUAL TABLE v USING fts5vocab(t, row)",
                "BEGIN",
                "INSERT INTO t VALUES('x')",
                "SELECT * FROM v",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
                "COMMIT",
            ],
        ),
        // Column filters on detail=none are an error.
        (
            "none-column-filter",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, b, detail=none)",
                "INSERT INTO t VALUES('x', 'y')",
                "SELECT rowid FROM t WHERE a MATCH 'x'",
                "SELECT rowid FROM t WHERE t MATCH 'b : y'",
                "SELECT rowid FROM t WHERE t MATCH 'x'",
            ],
        ),
        // A prefix scan of the pending terms resets the delete flag:
        // the next write of the same rowid flushes.
        (
            "prefix-scan-bdelete",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t(rowid, a) VALUES(1, 'xa')",
                "BEGIN",
                "DELETE FROM t WHERE rowid = 1",
                "SELECT rowid FROM t WHERE t MATCH 'x*'",
                "INSERT INTO t(rowid, a) VALUES(1, 'xb')",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
                "COMMIT",
                "SELECT id, hex(block) FROM t_data ORDER BY id",
            ],
        ),
        // Contentless tables without %_docsize: column sizes are -1
        // in bm25.
        (
            "contentless-columnsize0",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, b, columnsize=0, content='')",
                "INSERT INTO t VALUES('x y z', 'x'), ('y', 'y y y x')",
                "SELECT rowid, rank, bm25(t, 1, 2) FROM t WHERE t MATCH 'x OR y'",
                "SELECT rowid, rank FROM t WHERE t MATCH 'NEAR(x y, 4)'",
            ],
        ),
    ];
    for (tag, stmts) in cases {
        same(tag, &v(stmts));
    }
}

// ---------------- generated corpora ----------------

/// A deterministic xorshift generator.
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
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "epsilon",
    "zeta",
    "eta",
    "theta",
    "iota",
    "kappa",
    "lambda",
    "mu",
    "nu",
    "xi",
    "omicron",
    "pi",
    "rho",
    "sigma",
    "tau",
    "upsilon",
    "phi",
    "chi",
    "psi",
    "omega",
    "running",
    "runner",
    "runs",
    "ran",
    "easily",
    "fairly",
    "happiness",
    "relational",
    "caf\u{e9}",
    "na\u{ef}ve",
    "r\u{e9}sum\u{e9}",
    "Stra\u{df}e",
    "\u{c9}COLE",
    "\u{3a9}mega",
    "\u{3a3}\u{38a}\u{3a3}\u{3a5}\u{3a6}\u{39f}\u{3a3}",
    "\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}",
    "\u{43c}\u{438}\u{440}",
    "\u{41f}\u{420}\u{418}\u{412}\u{415}\u{422}",
    "\u{6771}\u{4eac}",
    "\u{65e5}\u{672c}\u{8a9e}",
    "\u{4e2d}\u{6587}",
    "\u{d55c}\u{ad6d}\u{c5b4}",
    "\u{1f642}",
    "x",
    "y",
    "z",
    "a",
    "b",
    "c",
    "abc",
    "abcd",
    "abcde",
    "ab",
];

fn gen_doc(r: &mut Rng) -> String {
    let n = r.below(13);
    let mut out: Vec<String> = Vec::new();
    for _ in 0..n {
        let mut w = r.pick(WORDS).to_string();
        if r.chance(10) {
            w = w.to_uppercase();
        }
        out.push(w);
        if r.chance(15) {
            out.push(r.pick(&[",", ".", ":", "!", "-", " "]).to_string());
        }
    }
    out.join(" ").replace('\'', "''")
}

fn gen_query(r: &mut Rng, cols: &[String], detail: &str, depth: u32) -> String {
    let k = r.below(100);
    let full = detail == "full";
    let word = |r: &mut Rng| r.pick(WORDS).replace('"', "");
    let phrase = |r: &mut Rng| -> String {
        if !full || r.chance(60) {
            let t = word(r);
            if r.chance(20) {
                let c: Vec<char> = t.chars().collect();
                let n = (c.len() / 2).max(1);
                return format!("{}*", c[..n].iter().collect::<String>());
            }
            if r.chance(10) {
                return format!("\"{t}\"");
            }
            return t;
        }
        let n = r.range(2, 3);
        let ws: Vec<String> = (0..n).map(|_| word(r)).collect();
        format!("\"{}\"", ws.join(" "))
    };
    if depth > 2 || k < 35 {
        let mut p = phrase(r);
        if full && r.chance(10) {
            p = format!("^{p}");
        }
        if detail != "none" && r.chance(15) {
            p = format!("{} : {p}", r.pick(cols));
        }
        if detail != "none" && r.chance(5) {
            p = format!("- {} : {p}", r.pick(cols));
        }
        return p;
    }
    if k < 45 && full {
        let n = r.range(2, 3);
        let ps: Vec<String> = (0..n).map(|_| phrase(r)).collect();
        let dist = if r.chance(60) {
            format!(", {}", r.range(0, 4))
        } else {
            String::new()
        };
        return format!("NEAR({}{dist})", ps.join(" "));
    }
    let mut op = *r.pick(&["AND", "OR", "NOT", ""]);
    let a = gen_query(r, cols, detail, depth + 1);
    let b = gen_query(r, cols, detail, depth + 1);
    if op.is_empty() {
        if a.contains(['(', ')']) || b.contains(['(', ')']) {
            op = "AND";
        } else {
            return format!("{a} {b}");
        }
    }
    let s = format!("{a} {op} {b}");
    if r.chance(50) { format!("({s})") } else { s }
}

/// One generated FTS5 workload: a table of random configuration, its
/// writes, transactions, commands and queries, and the shadow tables.
fn workload(seed: u64, n: usize) -> Vec<String> {
    let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let ncol = r.range(1, 3) as usize;
    let cols: Vec<String> = (0..ncol).map(|i| format!("c{i}")).collect();
    let detail = *r.pick(&["full", "full", "column", "none"]);
    let tok = *r.pick(&[
        "unicode61",
        "unicode61 remove_diacritics 2",
        "ascii",
        "porter",
        "porter ascii",
        "unicode61 remove_diacritics 0",
        "trigram",
    ]);
    let mut opts = format!(", detail={detail}, tokenize='{tok}'");
    if r.chance(30) {
        opts.push_str(", prefix='2 3'");
    }
    let colsize0 = r.chance(20);
    if colsize0 {
        opts.push_str(", columnsize=0");
    }
    let content = *r.pick(&["normal", "normal", "normal", "ext", "none"]);
    let ext = content == "ext";
    let none = content == "none";
    if ext {
        opts.push_str(", content=ext, content_rowid=id");
    }
    if none {
        opts.push_str(", content=''");
    }
    let unindexed = ncol > 1 && r.chance(20);
    let cols_def: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if unindexed && i == ncol - 1 {
                format!("{c} UNINDEXED")
            } else {
                c.clone()
            }
        })
        .collect();
    let cl = cols.join(", ");
    let newc: Vec<String> = cols.iter().map(|c| format!("new.{c}")).collect();
    let oldc: Vec<String> = cols.iter().map(|c| format!("old.{c}")).collect();
    let (newc, oldc) = (newc.join(", "), oldc.join(", "));
    let mut out = Vec::new();
    if ext {
        out.push(format!("CREATE TABLE ext(id INTEGER PRIMARY KEY, {cl})"));
        out.push(format!("CREATE TRIGGER ext_ai AFTER INSERT ON ext BEGIN INSERT INTO t(rowid, {cl}) VALUES (new.id, {newc}); END"));
        out.push(format!("CREATE TRIGGER ext_ad AFTER DELETE ON ext BEGIN INSERT INTO t(t, rowid, {cl}) VALUES ('delete', old.id, {oldc}); END"));
        out.push(format!("CREATE TRIGGER ext_au AFTER UPDATE ON ext BEGIN INSERT INTO t(t, rowid, {cl}) VALUES ('delete', old.id, {oldc}); INSERT INTO t(rowid, {cl}) VALUES (new.id, {newc}); END"));
    }
    out.push(format!(
        "CREATE VIRTUAL TABLE t USING fts5({}{opts})",
        cols_def.join(", ")
    ));
    let vocab = r.chance(40);
    if vocab {
        out.push(format!(
            "CREATE VIRTUAL TABLE v USING fts5vocab(t, {})",
            r.pick(&["row", "col", "instance"])
        ));
    }
    let tgt = if ext { "ext" } else { "t" };
    out.push(format!(
        "INSERT INTO t(t, rank) VALUES('pgsz', {})",
        r.pick(&[32, 48, 64, 100, 200, 4050])
    ));
    if r.chance(50) {
        out.push(format!(
            "INSERT INTO t(t, rank) VALUES('automerge', {})",
            r.pick(&[0, 2, 4, 8])
        ));
    }
    if r.chance(30) {
        out.push(format!(
            "INSERT INTO t(t, rank) VALUES('crisismerge', {})",
            r.pick(&[2, 3, 4])
        ));
    }
    if r.chance(20) {
        out.push(format!(
            "INSERT INTO t(t, rank) VALUES('usermerge', {})",
            r.pick(&[2, 3, 8])
        ));
    }
    if r.chance(20) {
        out.push(format!(
            "INSERT INTO t(t, rank) VALUES('hashsize', {})",
            r.pick(&[100, 1000, 5000])
        ));
    }
    if r.chance(10) {
        out.push("INSERT INTO t(t, rank) VALUES('rank', 'bm25(2.0, 1.0)')".to_string());
    }
    let mut intx = false;
    for _ in 0..n {
        let k = r.below(100);
        let vals = |r: &mut Rng| -> String {
            (0..ncol)
                .map(|_| format!("'{}'", gen_doc(r)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if k < 35 {
            let rid = if ext { "id" } else { "rowid" };
            if r.chance(30) {
                let id = r.range(-5, 300);
                let vs = vals(&mut r);
                out.push(format!("INSERT INTO {tgt}({rid}, {cl}) VALUES({id}, {vs})"));
            } else if r.chance(30) {
                let m = r.range(2, 4);
                let rows: Vec<String> = (0..m).map(|_| format!("({})", vals(&mut r))).collect();
                out.push(format!("INSERT INTO {tgt}({cl}) VALUES{}", rows.join(", ")));
            } else if content == "normal" && r.chance(10) {
                let id = r.range(1, 20);
                let vs = vals(&mut r);
                out.push(format!(
                    "INSERT OR REPLACE INTO t(rowid, {cl}) VALUES({id}, {vs})"
                ));
            } else if r.chance(10) {
                let m = r.range(50, 400);
                let big: Vec<&str> = (0..m)
                    .map(|_| *r.pick(&["alpha", "beta", "gamma"]))
                    .collect();
                let rest = ", ''".repeat(ncol - 1);
                out.push(format!(
                    "INSERT INTO {tgt}({cl}) VALUES('{}'{rest})",
                    big.join(" ")
                ));
            } else {
                let vs = vals(&mut r);
                out.push(format!("INSERT INTO {tgt}({cl}) VALUES({vs})"));
            }
        } else if k < 45 {
            if none {
                out.push(if r.chance(20) {
                    "INSERT INTO t(t) VALUES('delete-all')".to_string()
                } else {
                    "SELECT 1".to_string()
                });
            } else if r.chance(50) {
                out.push(format!(
                    "DELETE FROM {tgt} WHERE rowid = {}",
                    r.range(-5, 300)
                ));
            } else {
                out.push(format!(
                    "DELETE FROM {tgt} WHERE rowid % {} = {}",
                    r.range(3, 7),
                    r.range(0, 2)
                ));
            }
        } else if k < 52 {
            if none {
                out.push("SELECT 2".to_string());
            } else if content == "normal" && r.chance(20) {
                out.push(format!(
                    "UPDATE t SET rowid = rowid + {} WHERE rowid % 9 = 1",
                    r.range(1, 3)
                ));
            } else {
                let c = r.pick(&cols).clone();
                let d = gen_doc(&mut r);
                out.push(format!(
                    "UPDATE {tgt} SET {c} = '{d}' WHERE rowid = {}",
                    r.range(-5, 300)
                ));
            }
        } else if k < 55 {
            let opt = "INSERT INTO t(t) VALUES('optimize')";
            let choices = [
                opt,
                opt,
                if none {
                    "SELECT 3"
                } else {
                    "INSERT INTO t(t) VALUES('rebuild')"
                },
                if vocab { "SELECT * FROM v" } else { "SELECT 4" },
            ];
            out.push(r.pick(&choices).to_string());
        } else if k < 58 {
            out.push(format!(
                "INSERT INTO t(t, rank) VALUES('merge', {})",
                r.pick(&[-10, 1, 5, 50, 500])
            ));
        } else if k < 62 && !intx {
            out.push("BEGIN".to_string());
            intx = true;
        } else if k < 66 && intx {
            out.push(r.pick(&["COMMIT", "ROLLBACK", "COMMIT"]).to_string());
            intx = false;
        } else if k < 68 && intx {
            out.push(
                r.pick(&[
                    "SAVEPOINT s1",
                    "RELEASE s1",
                    "ROLLBACK TO s1",
                    "SAVEPOINT s2",
                ])
                .to_string(),
            );
        } else {
            let q = gen_query(&mut r, &cols, detail, 0).replace('\'', "''");
            let mut sels = vec![
                "rowid".to_string(),
                "rowid, *".to_string(),
                "rowid, rank".to_string(),
                "rowid, bm25(t)".to_string(),
                "highlight(t, 0, '[', ']')".to_string(),
                "count(*)".to_string(),
            ];
            if detail == "full" || !unindexed {
                sels.push(format!(
                    "snippet(t, -1, '<', '>', '...', {})",
                    r.range(1, 8)
                ));
            }
            let sel = r.pick(&sels).clone();
            let mut order = *r.pick(&["", " ORDER BY rank", " ORDER BY rowid DESC", ""]);
            if sel == "count(*)" {
                order = "";
            }
            let mut wh = match r.below(4) {
                0 | 1 => format!("t MATCH '{q}'"),
                2 => format!("t = '{q}'"),
                _ => format!("{} MATCH '{q}'", r.pick(&cols)),
            };
            if r.chance(10) {
                let ws: Vec<String> = (0..ncol)
                    .map(|_| r.pick(&["0.5", "1", "2", "10"]).to_string())
                    .collect();
                wh.push_str(&format!(" AND rank MATCH 'bm25({})'", ws.join(", ")));
            }
            if r.chance(10) {
                wh.push_str(&format!(" AND rowid > {}", r.range(0, 100)));
            }
            out.push(format!("SELECT {sel} FROM t WHERE {wh}{order}"));
        }
        if r.chance(25) {
            out.push("SELECT id, hex(block) FROM t_data".to_string());
            out.push("SELECT segid, hex(term), pgno FROM t_idx".to_string());
        }
    }
    if intx {
        out.push("COMMIT".to_string());
    }
    out.push("SELECT id, hex(block) FROM t_data".to_string());
    out.push("SELECT segid, hex(term), pgno FROM t_idx".to_string());
    if !colsize0 {
        out.push("SELECT id, hex(sz) FROM t_docsize".to_string());
    }
    if !none {
        out.push("SELECT rowid, * FROM t".to_string());
    }
    out.push(CHECK.to_string());
    out
}

/// The pin's `integrity-check` (and PRAGMA integrity_check) on a file.
fn pin_checks(db: &std::path::Path) -> Vec<String> {
    pin_run(
        db,
        &v(&["PRAGMA integrity_check", CHECK, "SELECT 'checked'"]),
    )
    .0
}

/// hipp's on a file.
fn hipp_checks(db: &std::path::Path) -> Vec<String> {
    let (l, f, e) = hipp_run(
        db,
        &v(&["PRAGMA integrity_check", CHECK, "SELECT 'checked'"]),
    );
    assert!(f.iter().all(|b| !b), "{db:?}: {e:?}");
    l
}

#[test]
fn generated_workloads_match_byte_for_byte() {
    let n: u64 = std::env::var("HIPP_FTS5_CORPUS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in 0..n {
        let w = workload(seed, 70);
        let h = common::temp_db(&format!("v16fts-gen{seed}-h"));
        let o = common::temp_db(&format!("v16fts-gen{seed}-o"));
        let _ = std::fs::write(h.with_extension("sql"), w.join(";\n"));
        same_on(&h, &o, &w);
        let ok = v(&["ok", "checked"]);
        assert_eq!(
            pin_checks(&h),
            ok,
            "seed {seed}: the pin checks hipp's file"
        );
        assert_eq!(
            hipp_checks(&o),
            ok,
            "seed {seed}: hipp checks the pin's file"
        );
    }
}

/// A file moves between the engines mid-workload: one engine writes
/// the first half, then each engine writes the second half on its own
/// copy; the outputs and shadow tables agree, both ways round.
#[test]
fn files_move_between_engines() {
    for seed in [1001u64, 1002, 1003, 1004, 1005, 1006] {
        let w = workload(seed, 60);
        // Split outside a transaction.
        let mut cut = w.len() / 2;
        let mut depth = 0i32;
        let mut safe = Vec::new();
        for (i, s) in w.iter().enumerate() {
            if s == "BEGIN" {
                depth += 1;
            } else if s == "COMMIT" || s == "ROLLBACK" {
                depth = 0;
            }
            if depth == 0 {
                safe.push(i + 1);
            }
        }
        if let Some(c) = safe.iter().copied().min_by_key(|c| c.abs_diff(cut)) {
            cut = c;
        }
        let (first, second) = w.split_at(cut);
        for pin_first in [true, false] {
            let base = common::temp_db(&format!("v16fts-mv{seed}-{pin_first}-base"));
            if pin_first {
                pin_run(&base, first);
            } else {
                hipp_run(&base, first);
            }
            let h = common::temp_db(&format!("v16fts-mv{seed}-{pin_first}-h"));
            let o = common::temp_db(&format!("v16fts-mv{seed}-{pin_first}-o"));
            std::fs::copy(&base, &h).expect("copy");
            std::fs::copy(&base, &o).expect("copy");
            let mut rest = second.to_vec();
            rest.extend(shadow("t"));
            let _ = std::fs::write(
                base.with_extension("sql"),
                format!("{};\n-- second\n{};\n", first.join(";\n"), rest.join(";\n")),
            );
            same_on(&h, &o, &rest);
            let ok = v(&["ok", "checked"]);
            assert_eq!(pin_checks(&h), ok, "seed {seed}");
            assert_eq!(hipp_checks(&o), ok, "seed {seed}");
        }
    }
}

// ---------------- Review fixes (v0.16) ----------------

/// The pin's own continuation of a file versus hipp's: `setup` runs on
/// the pin alone, then `rest` (and the shadow tables) on a copy each.
fn continue_pin_file(tag: &str, setup: &[String], rest: &[String]) {
    let base = common::temp_db(&format!("v16fts-{tag}-base"));
    pin_run(&base, setup);
    let h = common::temp_db(&format!("v16fts-{tag}-h"));
    let o = common::temp_db(&format!("v16fts-{tag}-o"));
    std::fs::copy(&base, &h).expect("copy");
    std::fs::copy(&base, &o).expect("copy");
    let mut all = rest.to_vec();
    all.extend(shadow("t"));
    let _ = std::fs::write(
        base.with_extension("sql"),
        format!("{};\n-- rest\n{};\n", setup.join(";\n"), all.join(";\n")),
    );
    same_on(&h, &o, &all);
    assert_eq!(pin_checks(&h), v(&["ok", "checked"]), "{tag}");
}

/// A contentless_delete table the pin deleted rows of: its tombstone
/// pages hide them from queries and fts5vocab, merges drop them, and
/// hipp's merges keep the structure counters the pin keeps (random
/// write / delete / merge sequences, every detail mode).
#[test]
fn pin_tombstones_are_honoured() {
    for seed in 0..24u64 {
        let mut r = Rng(0x7041_0000 + seed * 7919 + 1);
        let detail = *r.pick(&["full", "column", "none"]);
        let mut setup = vec![format!(
            "CREATE VIRTUAL TABLE t USING fts5(a, b, content='', contentless_delete=1, detail={detail})"
        )];
        setup.push(format!(
            "INSERT INTO t(t, rank) VALUES('pgsz', {})",
            r.pick(&[64, 128, 4050])
        ));
        if r.chance(50) {
            setup.push(format!(
                "INSERT INTO t(t, rank) VALUES('deletemerge', {})",
                r.pick(&[0, 5, 10, 30])
            ));
        }
        if r.chance(50) {
            setup.push(format!(
                "INSERT INTO t(t, rank) VALUES('automerge', {})",
                r.pick(&[0, 2, 4, 8])
            ));
        }
        let words: Vec<String> = (0..30).map(|i| format!("w{i}")).collect();
        let mut rid = 0i64;
        for _ in 0..r.range(3, 20) {
            setup.push("BEGIN".into());
            for _ in 0..r.range(1, 8) {
                rid += r.range(1, 3);
                let a: Vec<&str> = (0..r.range(1, 6))
                    .map(|_| r.pick(&words).as_str())
                    .collect();
                setup.push(format!(
                    "INSERT INTO t(rowid, a, b) VALUES({rid}, '{}', '{}')",
                    a.join(" "),
                    r.pick(&words)
                ));
            }
            setup.push("COMMIT".into());
            if r.chance(45) {
                let ids: Vec<String> = (0..r.range(1, 6))
                    .map(|_| r.range(1, rid).to_string())
                    .collect();
                setup.push(format!("DELETE FROM t WHERE rowid IN ({})", ids.join(",")));
            }
        }
        let mut rest = Vec::new();
        for w in words.iter().take(8) {
            rest.push(format!(
                "SELECT '{w}', group_concat(rowid) FROM t WHERE t MATCH '{w}'"
            ));
        }
        rest.push("CREATE VIRTUAL TABLE v USING fts5vocab(t, row)".into());
        rest.push("SELECT * FROM v".into());
        for _ in 0..r.range(1, 8) {
            match r.below(10) {
                0..=5 => {
                    rid += 1;
                    rest.push(format!(
                        "INSERT INTO t(rowid, a, b) VALUES({rid}, '{} {}', 'x')",
                        r.pick(&words),
                        r.pick(&words)
                    ));
                }
                6 => rest.push("INSERT INTO t(t) VALUES('optimize')".into()),
                7 | 8 => rest.push(format!(
                    "INSERT INTO t(t, rank) VALUES('merge', {})",
                    r.pick(&[-5, 5, 100, -100, 1])
                )),
                _ => rest.push("INSERT INTO t(t, rank) VALUES('automerge', 2)".into()),
            }
        }
        for w in words.iter().take(8) {
            rest.push(format!(
                "SELECT '{w}', group_concat(rowid) FROM t WHERE t MATCH '{w}'"
            ));
        }
        continue_pin_file(&format!("tomb{seed}"), &setup, &rest);
    }
}

/// A pin-set `secure-delete`: without the v0.17 `fts5_delete` leaf
/// hipp refuses the deletes it cannot make in place (the file stays as
/// it was); with it, it makes them as the pin does. Either way it
/// deletes once the setting is off as the pin does, and setting it
/// through hipp stores it as the pin does.
#[test]
fn secure_delete_is_not_ignored() {
    let setup = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(a)",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
        "INSERT INTO t SELECT 'w'||value||' common' FROM generate_series(1, 30)",
    ]);
    let base = common::temp_db("v16fts-sd-base");
    pin_run(&base, &setup);
    let deletes = [
        "DELETE FROM t WHERE rowid = 10",
        "UPDATE t SET a = 'x' WHERE rowid = 10",
        "INSERT OR REPLACE INTO t(rowid, a) VALUES(10, 'y')",
    ];
    // With the v0.17 `fts5_delete` leaf hipp deletes in place as the
    // pin does (tests/fts5_delete_v17.rs has the corpus).
    #[cfg(feature = "fts5_delete")]
    for (i, stmt) in deletes.iter().enumerate() {
        let h = common::temp_db(&format!("v16fts-sd{i}-h"));
        let o = common::temp_db(&format!("v16fts-sd{i}-o"));
        std::fs::copy(&base, &h).expect("copy");
        std::fs::copy(&base, &o).expect("copy");
        let mut rest = v(&[stmt]);
        rest.extend(shadow("t"));
        same_on(&h, &o, &rest);
    }
    #[cfg(not(feature = "fts5_delete"))]
    let before = pin_run(&base, &shadow("t")).0;
    #[cfg(not(feature = "fts5_delete"))]
    for stmt in deletes {
        let (_, f, e) = hipp_run(&base, &v(&[stmt]));
        assert_eq!(f, vec![true], "{stmt}");
        assert!(
            matches!(
                e,
                Some(Error::Schema {
                    kind: SchemaKind::Fts5SecureDelete
                })
            ),
            "{stmt}: {e:?}"
        );
        assert_eq!(pin_run(&base, &shadow("t")).0, before, "{stmt}");
    }
    let h = common::temp_db("v16fts-sd-h");
    let o = common::temp_db("v16fts-sd-o");
    std::fs::copy(&base, &h).expect("copy");
    std::fs::copy(&base, &o).expect("copy");
    let mut rest = v(&[
        "INSERT INTO t(t, rank) VALUES('secure-delete', 0)",
        "DELETE FROM t WHERE rowid = 10",
        "SELECT rowid FROM t WHERE t MATCH 'w10 OR w11'",
    ]);
    rest.extend(shadow("t"));
    same_on(&h, &o, &rest);
    // The setting's values as the pin stores them (numeric text as
    // its integer; bad values refused).
    let mut cfg = v(&[
        "CREATE VIRTUAL TABLE t USING fts5(a)",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 1)",
        "INSERT INTO t(t, rank) VALUES('secure-delete', -1)",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 'x')",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 2.0)",
        "INSERT INTO t(t, rank) VALUES('secure-delete', '3')",
        "INSERT INTO t(t, rank) VALUES('pgsz', '64')",
        "INSERT INTO t(t, rank) VALUES('automerge', ' 3 ')",
        "INSERT INTO t(t, rank) VALUES('deletemerge', '7')",
        "INSERT INTO t(t, rank) VALUES('rank', 'bm25(1)')",
        "SELECT k, quote(v) FROM t_config",
        "INSERT INTO t VALUES('a b')",
        "INSERT INTO t(t, rank) VALUES('secure-delete', 0)",
        "DELETE FROM t",
    ]);
    cfg.extend(shadow("t"));
    same("sd-config", &cfg);
}

/// One regression case per review finding (and per bug the fixes'
/// own fuzzing found), compared with the pin.
#[test]
fn review_regressions() {
    let cases: &[(&str, &[&str])] = &[
        // A failing multi-row statement in a transaction drops the
        // terms its trigger programs added (they were left in the
        // index of an external-content table).
        (
            "trigger-stmt-rollback",
            &[
                "CREATE TABLE src(id INTEGER PRIMARY KEY, x)",
                "CREATE VIRTUAL TABLE e USING fts5(x, content='src', content_rowid='id')",
                "CREATE TRIGGER s_ai AFTER INSERT ON src BEGIN INSERT INTO e(rowid, x) VALUES(new.id, new.x); END",
                "CREATE TRIGGER s_ad AFTER DELETE ON src BEGIN INSERT INTO e(e, rowid, x) VALUES('delete', old.id, old.x); END",
                "INSERT INTO src VALUES(2, 'two')",
                "BEGIN",
                "INSERT INTO src VALUES(3, 'three')",
                "INSERT INTO src VALUES(6, 'six'), (2, 'bad')",
                "INSERT INTO src VALUES(7, 'seven'), (8, 'eight')",
                "SAVEPOINT a",
                "INSERT INTO src VALUES(9, 'nine'), (3, 'bad')",
                "DELETE FROM src WHERE id IN (3, 7)",
                "INSERT INTO src VALUES(10, 'ten'), (10, 'bad')",
                "RELEASE a",
                "COMMIT",
                "SELECT rowid FROM e WHERE e MATCH 'six OR three OR seven OR eight OR nine OR ten OR two'",
                "INSERT INTO e(e, rank) VALUES('integrity-check', 1)",
                "SELECT id, hex(block) FROM e_data",
            ],
        ),
        // A NEAR group failing inside an OR keeps the phrase hits the
        // pin's test left (bm25 / highlight / snippet count them).
        (
            "near-in-or",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(x, y)",
                "INSERT INTO t VALUES('e b', 'ef')",
                "SELECT rowid, bm25(t), highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'e OR y:NEAR(ef b)'",
                "CREATE VIRTUAL TABLE u USING fts5(x)",
                "INSERT INTO u VALUES('cd b ef ab c')",
                "SELECT bm25(u), highlight(u, 0, '<', '>') FROM u WHERE u MATCH 'NEAR(b \"cd ef\", 3) OR ab'",
                "SELECT bm25(u), snippet(u, 0, '<', '>', '..', 3) FROM u WHERE u MATCH 'ab OR NEAR(b \"cd ef\", 3)' ORDER BY rowid DESC",
            ],
        ),
        // A row lacking a later phrase's term is no NEAR candidate.
        (
            "near-candidate",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(x, y)",
                "INSERT INTO t VALUES('c ef c cd e cd', 'a cd ef b')",
                "INSERT INTO t VALUES('a a b f ef a', 'f a')",
                "INSERT INTO t VALUES('ab', 'b c')",
                "SELECT rowid, highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'c OR NEAR(a \"ef e\" f, 4)'",
                "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'NEAR(a \"ef e\" f, 4) OR c'",
            ],
        ),
        // A column-filtered prefix scan drops the rows the filter
        // empties (they are no NEAR candidates).
        (
            "prefix-colset",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(x, y)",
                "INSERT INTO t VALUES('cd cd f ab f a', 'b ef ab')",
                "INSERT INTO t VALUES('ef ab f', '')",
                "INSERT INTO t VALUES('c ab f c ef b c', 'ef c ef')",
                "SELECT rowid, bm25(t) FROM t WHERE t MATCH '(\"b\" AND NEAR(\"c ef\" \"c*\")) NOT -x:(NEAR(ab* ab, 0))'",
                "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'b NOT y:(ab* c)'",
            ],
        ),
        // snippet()'s token count is clamped to 0..INT_MAX.
        (
            "snippet-ntoken",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t VALUES('Cat. CAT! cAt?'), ('x cat y. z cat w')",
                "SELECT snippet(t, -1, '<', '>', '...', -1) FROM t WHERE t MATCH 'cat'",
                "SELECT snippet(t, -1, '<', '>', '...', -4294967297) FROM t WHERE t MATCH 'cat'",
                "SELECT snippet(t, -1, '<', '>', '...', 4294967295) FROM t WHERE t MATCH 'cat'",
                "SELECT snippet(t, -1, '<', '>', '...', 4294967296) FROM t WHERE t MATCH 'cat'",
                "SELECT snippet(t, 0, '<', '>', '...', '-3') FROM t WHERE t MATCH 'cat'",
            ],
        ),
        // External content missing the row: an error when the
        // statement reads a content column or its text.
        (
            "missing-content-row",
            &[
                "CREATE TABLE docs(id INTEGER PRIMARY KEY, title, body)",
                "CREATE VIRTUAL TABLE f USING fts5(title, body, content='docs', content_rowid='id')",
                "INSERT INTO f(rowid, title, body) VALUES(99, 'ghost', 'ghost')",
                "SELECT rowid, title FROM f WHERE f MATCH 'ghost'",
                "SELECT rowid FROM f WHERE f MATCH 'ghost'",
                "SELECT count(*) FROM f WHERE f MATCH 'ghost'",
                "SELECT rowid, bm25(f) FROM f WHERE f MATCH 'ghost'",
                "SELECT rowid, highlight(f, 0, '[', ']') FROM f WHERE f MATCH 'ghost'",
                "SELECT rowid FROM f WHERE f MATCH 'ghost' ORDER BY rank",
                "SELECT (SELECT title FROM f WHERE f MATCH 'ghost')",
                "SELECT rowid FROM f WHERE f MATCH 'ghost' ORDER BY title",
                "DELETE FROM f WHERE f MATCH 'ghost'",
                "SELECT rowid FROM f WHERE f MATCH 'ghost'",
            ],
        ),
        // An INSERT's rowid passes OP_MustBeInt; an UPDATE's does not.
        (
            "real-rowids",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t(rowid, a) VALUES(7.0, 'x')",
                "INSERT INTO t(rowid, a) VALUES(CAST(8 AS REAL), 'x')",
                "INSERT INTO t(rowid, a) VALUES(8.5, 'x')",
                "INSERT INTO t(rowid, a) VALUES('9.0', 'x')",
                "INSERT INTO t(rowid, a) VALUES(' 12 ', 'x')",
                "INSERT INTO t(rowid, a) VALUES(9223372036854775807.0, 'x')",
                "INSERT INTO t(rowid, a) VALUES(-9223372036854775808.0, 'x')",
                "INSERT INTO t(rowid, a) VALUES(1e30, 'x')",
                "INSERT INTO t(rowid, a) VALUES(x'31', 'x')",
                "UPDATE t SET rowid = 10.0 WHERE rowid = 7",
                "UPDATE t SET rowid = '20' WHERE rowid = 9",
                "INSERT OR REPLACE INTO t(rowid, a) VALUES(20.0, 'y')",
                "SELECT rowid, a FROM t",
            ],
        ),
        // INSERT ... RETURNING and UPDATE ... FROM.
        (
            "returning-update-from",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a, b)",
                "INSERT INTO t VALUES('alpha beta', 'x') RETURNING rowid, a, b, t, rank",
                "INSERT INTO t(rowid, a) VALUES(10, 'gamma') RETURNING rowid, a, b, *",
                "INSERT INTO t(a) VALUES('d'), ('e') RETURNING rowid, upper(a), last_insert_rowid()",
                "INSERT INTO t(rowid, a) SELECT 20, 'sel' RETURNING rowid",
                "INSERT INTO t(t) VALUES('optimize') RETURNING rowid, a",
                "INSERT OR REPLACE INTO t(rowid, a) VALUES(10, 'gamma2') RETURNING rowid, a",
                "INSERT INTO t VALUES('zz', 'q') RETURNING (SELECT count(*) FROM t)",
                "INSERT INTO t VALUES('z', 'q') RETURNING bm25(t)",
                "INSERT INTO t VALUES('z', 'q') RETURNING t.*",
                "UPDATE t SET a = 'from' FROM (SELECT 2 AS k) WHERE t.rowid = k",
                "UPDATE t SET b = s.v FROM (SELECT 1 AS k, 'vv' AS v UNION ALL SELECT 10, 'ww') AS s WHERE t.rowid = s.k",
                "UPDATE t AS z SET a = s.v || z.a FROM (SELECT 1 AS k, 'p' AS v UNION ALL SELECT 1, 'q') AS s WHERE z.rowid = s.k",
                "UPDATE t SET rowid = s.k + 30 FROM (SELECT 20 AS k) s WHERE t.rowid = s.k",
                "UPDATE t SET a = 'z' WHERE rowid = 1 RETURNING rowid",
                "DELETE FROM t WHERE rowid = 1 RETURNING rowid",
                "SELECT rowid, * FROM t",
                "SELECT rowid FROM t WHERE t MATCH 'from OR sel OR qalpha'",
                CHECK,
            ],
        ),
        // UPDATE ... FROM writes in the join's order (the FROM rows
        // outer, the table probed by rowid), with the pin's flushes.
        (
            "update-from-order",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t(rowid, a) VALUES(1, 'one'), (2, 'two'), (3, 'three')",
                "BEGIN",
                "INSERT INTO t(rowid, a) VALUES(5, 'five')",
                "UPDATE t SET a = 'x' FROM (SELECT 2 AS k) WHERE t.rowid = k",
                "INSERT INTO t(rowid, a) VALUES(6, 'six')",
                "UPDATE t SET a = 'y' FROM (SELECT 3 AS k UNION ALL SELECT 1) s WHERE t.rowid = s.k",
                "COMMIT",
                "SELECT id, hex(block) FROM t_data",
                CHECK,
            ],
        ),
        // MATCH under an OR (the pin's multi-index OR) and in a LEFT
        // JOIN's ON.
        (
            "match-or-and-left-join",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t(rowid, a) VALUES(1, 'red green'), (2, 'green blue'), (3, 'blue red'), (4, 'pink'), (5, 'red pink')",
                "SELECT rowid FROM t WHERE t MATCH 'pink' OR t MATCH 'red'",
                "SELECT rowid FROM t WHERE rowid = 4 OR t MATCH 'red'",
                "SELECT rowid, bm25(t), highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'pink' OR t MATCH 'red'",
                "SELECT rowid, rank FROM t WHERE t MATCH 'pink' OR t MATCH 'red' ORDER BY rank",
                "SELECT rowid FROM t WHERE (t MATCH 'pink' OR t MATCH 'red') AND rowid > 2",
                "SELECT rowid FROM t WHERE t MATCH 'pink' OR a MATCH 'blue'",
                "SELECT rowid FROM t WHERE t MATCH 'pink' OR rowid > 3",
                "SELECT rowid FROM t WHERE t MATCH 'pink' OR a = 'red green'",
                "SELECT rowid, bm25(t) FROM t WHERE rowid = 4 OR t MATCH 'red'",
                "SELECT rowid FROM t WHERE t MATCH 'pink' OR (t MATCH 'red' AND rowid < 3)",
                "SELECT rowid FROM t WHERE rowid = 3.0 OR rowid = '2' OR rowid = 2.5 OR t MATCH 'pink'",
                "SELECT rowid FROM t WHERE t = 'pink' OR t MATCH 'blue'",
                "SELECT count(*) FROM t WHERE t MATCH 'pink' OR t MATCH 'red'",
                "SELECT rowid FROM t WHERE NOT (t MATCH 'pink' OR t MATCH 'red')",
                "CREATE TABLE m(q)",
                "INSERT INTO m VALUES('red'), ('nothing'), ('blue')",
                "SELECT m.q, t.rowid FROM m, t WHERE t MATCH m.q OR t MATCH 'pink'",
                "SELECT m.q, t.rowid FROM m LEFT JOIN t ON t MATCH m.q || 'x'",
                "SELECT m.q, t.rowid FROM m LEFT JOIN t ON t MATCH m.q",
                "SELECT m.q, t.rowid, highlight(t, 0, '[', ']') FROM m LEFT JOIN t ON t MATCH m.q WHERE t.rowid < 3",
                "SELECT m.q, count(t.rowid) FROM m LEFT JOIN t ON t MATCH m.q GROUP BY m.q",
                "SELECT m.q, value FROM m LEFT JOIN generate_series ON start = length(m.q) AND stop = 6",
            ],
        ),
        // Auxiliary functions in an aggregate query: the pin's
        // "unable to use function ... in the requested context".
        (
            "aux-in-aggregate",
            &[
                "CREATE VIRTUAL TABLE t USING fts5(a)",
                "INSERT INTO t VALUES('x y'), ('x')",
                "SELECT sum(bm25(t)) FROM t WHERE t MATCH 'x'",
                "SELECT group_concat(highlight(t, 0, '[', ']')) FROM t WHERE t MATCH 'x'",
                "SELECT count(*), bm25(t) FROM t WHERE t MATCH 'x'",
                "SELECT max(rank) FROM t WHERE t MATCH 'x'",
                "SELECT bm25(t) FROM t WHERE t MATCH 'x' GROUP BY rowid",
                "SELECT count(*) FROM t WHERE t MATCH 'x' HAVING max(bm25(t)) < 0",
                "SELECT (SELECT bm25(t)) FROM t WHERE t MATCH 'x'",
                "SELECT sum(rowid) OVER (ORDER BY bm25(t)) FROM t WHERE t MATCH 'x'",
            ],
        ),
    ];
    for (tag, stmts) in cases {
        same(tag, &v(stmts));
    }
}

/// Generated queries mixing NEAR groups, phrases, prefixes, column
/// filters and AND / OR / NOT over small random documents: every
/// row's bm25, highlights and snippet as the pin's, in both rowid
/// orders and in rank order (the NEAR-in-OR finding's own fuzz).
#[test]
fn generated_near_or_queries_match() {
    const W: &[&str] = &["a", "b", "c", "ab", "cd", "ef", "e", "f"];
    fn phrase(r: &mut Rng) -> String {
        let n = *r.pick(&[1, 1, 1, 2, 2, 3]);
        let t: Vec<&str> = (0..n).map(|_| *r.pick(W)).collect();
        let mut s = t.join(" ");
        if n == 1 && r.chance(10) {
            s.push('*');
        }
        if n > 1 || r.chance(20) {
            s = format!("\"{s}\"");
        }
        if r.chance(10) {
            s = format!("^{s}");
        }
        s
    }
    fn q(r: &mut Rng, d: u32) -> String {
        let c = r.below(100);
        let mut p = if d > 2 || c < 30 {
            phrase(r)
        } else if c < 50 {
            let k = r.range(2, 3);
            let ps: Vec<String> = (0..k)
                .map(|_| phrase(r).trim_start_matches('^').to_string())
                .collect();
            let n = if r.chance(60) {
                format!(", {}", r.below(5))
            } else {
                String::new()
            };
            format!("NEAR({}{n})", ps.join(" "))
        } else if c < 65 {
            format!("({} OR {})", q(r, d + 1), q(r, d + 1))
        } else if c < 80 {
            format!("({} AND {})", q(r, d + 1), q(r, d + 1))
        } else if c < 90 {
            format!("({} NOT {})", q(r, d + 1), q(r, d + 1))
        } else {
            format!("{} {}", q(r, d + 1), q(r, d + 1))
        };
        if r.chance(15) && !p.starts_with('^') {
            let f = *r.pick(&["x", "y", "{x y}", "-x", "-y"]);
            p = if p.contains(' ') && !p.starts_with("NEAR") {
                format!("{f}:({p})")
            } else {
                format!("{f}:{p}")
            };
        }
        p
    }
    for seed in 0..40u64 {
        let mut r = Rng(0x4e45_4152 ^ (seed * 104_729 + 7));
        let mut stmts = v(&["CREATE VIRTUAL TABLE t USING fts5(x, y)"]);
        for _ in 0..r.range(3, 12) {
            let a: Vec<&str> = (0..r.range(1, 8)).map(|_| *r.pick(W)).collect();
            let b: Vec<&str> = (0..r.range(0, 5)).map(|_| *r.pick(W)).collect();
            stmts.push(format!(
                "INSERT INTO t VALUES('{}', '{}')",
                a.join(" "),
                b.join(" ")
            ));
        }
        for _ in 0..8 {
            let qq = q(&mut r, 0);
            stmts.push(format!(
                "SELECT rowid, bm25(t), highlight(t, 0, '[', ']'), highlight(t, 1, '[', ']'), snippet(t, -1, '<', '>', '..', 3) FROM t WHERE t MATCH '{qq}'"
            ));
            stmts.push(format!(
                "SELECT rowid, highlight(t, 0, '[', ']') FROM t WHERE t MATCH '{qq}' ORDER BY rowid DESC"
            ));
            stmts.push(format!(
                "SELECT rowid, rank FROM t WHERE t MATCH '{qq}' ORDER BY rank"
            ));
        }
        same(&format!("nearor{seed}"), &stmts);
    }
}
