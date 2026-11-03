//! hipp-cli v0.16 differentials: PRAGMA statements and
//! `sqlite_schema` must print byte-identically to the pinned
//! oracle — introspection rows (`table_info` with its default
//! texts, `index_xinfo`, `foreign_key_list`, `table_list`),
//! header values and their text conversions, flag words, the
//! table-valued `pragma_*` functions, `sqlite_schema` queries,
//! and `integrity_check` messages (one multi-line value for the
//! page check, as the pin prints it) on files corrupted after
//! the oracle wrote them. Each batch runs one-shot and over
//! stdin; hipp-written files must pass the oracle's integrity
//! check.

use std::path::{Path, PathBuf};
use std::process::Command;

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

fn temp_db(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16prag-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> (bool, Vec<u8>) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (out.status.success(), out.stdout)
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("cli finishes").stdout
}

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let (hok, got) = run(&hipp_bin(), &a, batch);
    let (ook, want) = run(&oracle(), &b, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "{tag}"
    );
    assert_eq!(hok, ook, "{tag}");
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    // (A batch that stores CHECK-violating rows on purpose is
    // compared by its own integrity_check output instead.)
    if hok && !batch.contains("ignore_check_constraints = true") {
        assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;").1, b"ok\n");
    }
}

const INTRO: &str = "\
CREATE TABLE p(id INTEGER PRIMARY KEY, u TEXT NOT NULL DEFAULT 'x''y', v DEFAULT (1 + 2), w varchar(9) DEFAULT -1.5, UNIQUE(u, v));
CREATE TABLE c(a REFERENCES p, b INT REFERENCES p(u) ON DELETE CASCADE, x, y AS (x * 2) STORED, z AS (x || 'z'), FOREIGN KEY (x, b) REFERENCES p(u, v) ON UPDATE SET DEFAULT);
CREATE INDEX ic ON c(z DESC, x COLLATE NOCASE);
CREATE VIEW v AS SELECT id, u AS uu, CAST(v AS TEXT) AS vt, v + 1 FROM p;
PRAGMA table_info(p);
PRAGMA table_xinfo(c);
PRAGMA table_info(v);
PRAGMA index_list(p);
PRAGMA index_list(c);
PRAGMA index_xinfo(ic);
PRAGMA index_info(sqlite_autoindex_p_1);
PRAGMA foreign_key_list(c);
PRAGMA table_list;
PRAGMA table_list(c);
SELECT type, name, tbl_name, rootpage FROM sqlite_schema;
SELECT name, sql IS NULL FROM sqlite_master WHERE type = 'index' ORDER BY name;
SELECT * FROM pragma_table_info('p') WHERE \"notnull\" OR pk;
SELECT p.name, p.type FROM pragma_table_xinfo('c') AS p WHERE p.hidden;
SELECT count(*) FROM pragma_index_list('c');
";

const VALUES: &str = "\
PRAGMA user_version = 7; PRAGMA user_version;
PRAGMA user_version = '0x1f'; PRAGMA user_version;
PRAGMA user_version = -2147483648; PRAGMA user_version;
PRAGMA user_version = 2147483648; PRAGMA user_version;
PRAGMA application_id = '12abc'; PRAGMA application_id;
PRAGMA page_size; PRAGMA page_count; PRAGMA freelist_count; PRAGMA schema_version;
PRAGMA encoding; PRAGMA journal_mode; PRAGMA journal_mode = DELETE; PRAGMA synchronous; PRAGMA auto_vacuum;
PRAGMA busy_timeout = 250; PRAGMA busy_timeout;
PRAGMA foreign_keys = yes; PRAGMA foreign_keys; PRAGMA recursive_triggers = 2; PRAGMA recursive_triggers;
PRAGMA ignore_check_constraints = true; PRAGMA ignore_check_constraints;
PRAGMA case_sensitive_like = 1; SELECT 'a' LIKE 'A', like('b%', 'BC'), 'ab' LIKE 'a%';
PRAGMA case_sensitive_like = 0; SELECT 'a' LIKE 'A';
PRAGMA nosuch; PRAGMA nosuch = 1; PRAGMA main.nosuch(2);
SELECT * FROM pragma_user_version, pragma_application_id;
CREATE TABLE t(a CHECK (a > 0)); INSERT INTO t VALUES (-1); SELECT * FROM t;
PRAGMA ignore_check_constraints = 0; PRAGMA integrity_check; PRAGMA quick_check; PRAGMA integrity_check(t);
";

/// A generated corpus of header-value conversions.
fn value_corpus() -> String {
    let mut s: u64 = 0x5eed_0016_00cc;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut out = String::new();
    for _ in 0..80 {
        let v = (next() as i64) >> (next() % 63);
        let lit = match next() % 4 {
            0 => v.to_string(),
            1 => format!("'{v}'"),
            2 => format!("'{v:#x}'"),
            _ => format!("'{v}{}'", ["x", ".9", "e3", " "][(next() % 4) as usize]),
        };
        out.push_str(&format!(
            "PRAGMA user_version = {lit}; PRAGMA user_version; PRAGMA application_id({lit}); PRAGMA application_id;\n"
        ));
    }
    out
}

#[test]
fn cli_v16_pragma_introspection_byte_identical() {
    byte_identical("intro", INTRO);
}

#[test]
fn cli_v16_pragma_values_byte_identical() {
    byte_identical("values", VALUES);
    byte_identical("corpus", &value_corpus());
}

#[test]
fn cli_v16_pragma_page_size_on_a_fresh_file() {
    byte_identical(
        "pagesize",
        "PRAGMA page_size = 2048; PRAGMA page_count; CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(5000)); \
         PRAGMA page_size; PRAGMA page_count; PRAGMA page_size = 8192; PRAGMA page_size;",
    );
}

/// The oracle writes `setup`, the bytes are edited, then both
/// engines check copies of the file.
fn corrupt_check(tag: &str, setup: &str, edit: impl Fn(&mut Vec<u8>)) {
    let src = temp_db(&format!("{tag}-src"));
    assert!(run(&oracle(), &src, setup).0, "{setup}");
    let mut d = std::fs::read(&src).expect("read");
    edit(&mut d);
    for q in [
        "PRAGMA integrity_check;",
        "PRAGMA quick_check;",
        "PRAGMA integrity_check(2);",
    ] {
        let a = temp_db(&format!("{tag}-h"));
        let b = temp_db(&format!("{tag}-o"));
        std::fs::write(&a, &d).expect("write");
        std::fs::write(&b, &d).expect("write");
        let got = run(&hipp_bin(), &a, q).1;
        let want = run(&oracle(), &b, q).1;
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want),
            "{tag}: {q}"
        );
        assert_ne!(got, b"ok\n", "{tag}: nothing found");
    }
}

#[test]
fn cli_v16_pragma_integrity_messages_byte_identical() {
    const PS: usize = 4096;
    corrupt_check("unused", "CREATE TABLE t(a);", |d| {
        d.extend(std::iter::repeat_n(0u8, 2 * PS));
        d[28..32].copy_from_slice(&4u32.to_be_bytes());
    });
    corrupt_check(
        "order",
        "CREATE TABLE t(a); INSERT INTO t VALUES ('x1'), ('x2'), ('x3');",
        |d| {
            let (a, b) = ([d[PS + 8], d[PS + 9]], [d[PS + 10], d[PS + 11]]);
            d[PS + 8..PS + 10].copy_from_slice(&b);
            d[PS + 10..PS + 12].copy_from_slice(&a);
        },
    );
    corrupt_check(
        "freelist",
        "CREATE TABLE t(a); INSERT INTO t VALUES (zeroblob(20000)); DELETE FROM t;",
        |d| d[36..40].copy_from_slice(&7u32.to_be_bytes()),
    );
    corrupt_check(
        "index",
        "CREATE TABLE t(a, b NOT NULL); CREATE INDEX i ON t(a); INSERT INTO t VALUES (1, 1), (2, 2), (3, 3);",
        |d| {
            let n = u16::from_be_bytes([d[2 * PS + 3], d[2 * PS + 4]]) - 1;
            d[2 * PS + 3..2 * PS + 5].copy_from_slice(&n.to_be_bytes());
        },
    );
}

// ---------------- review fixes (v0.16) ----------------

const REVIEW: &str = "\
CREATE TABLE t(a int, b varchar(10), c Integer, d \"text\", e [my type], f `x`, g any, h Text, i \"a\" \"b\", j [x] TEXT, k 'it''s');
PRAGMA table_info(t);
INSERT INTO t VALUES ('1', '1', '1', '1', '1', '1', '1', '1', '1', '1', '1');
SELECT typeof(a), typeof(d), typeof(e), typeof(j), typeof(k) FROM t;
CREATE VIEW v AS SELECT a, a COLLATE NOCASE, d, j FROM t;
PRAGMA table_info(v);
CREATE VIEW w AS SELECT * FROM (SELECT a FROM t);
CREATE VIEW x AS WITH c AS (SELECT a, b FROM t) SELECT * FROM c;
PRAGMA table_list(w); PRAGMA table_list(x);
CREATE TABLE r(a INTEGER, b, PRIMARY KEY(a DESC)); INSERT INTO r VALUES (5, 1);
SELECT rowid, * FROM r; PRAGMA index_list(r);
CREATE TABLE q(a \"INTEGER\" PRIMARY KEY, b); INSERT INTO q VALUES (6, 1); SELECT rowid, a FROM q;
SELECT sqlite_master.name FROM sqlite_schema WHERE sqlite_schema.type = 'view' ORDER BY 1;
SELECT count(*) FROM pragma_index_list('t', 'main');
SELECT * FROM pragma_page_size('main');
CREATE TABLE u(a, b AS (a LIKE 'A%'));
PRAGMA case_sensitive_like = 1; INSERT INTO u(a) VALUES ('abc'), ('Abc'); SELECT * FROM u;
PRAGMA integrity_check; PRAGMA quick_check(u);
";

#[test]
fn cli_v16_pragma_review_fixes_byte_identical() {
    byte_identical("review", REVIEW);
    // Each failing statement stops the batch the same way.
    for (i, b) in [
        "CREATE TABLE t(a); SELECT 1; PRAGMA integrity_check(2147483648); SELECT 2;",
        "SELECT * FROM pragma_user_version(5); PRAGMA user_version;",
        "CREATE TABLE t(a); SELECT 1; SELECT * FROM t(1);",
        "CREATE TABLE c(x REFERENCES p(a, b)); SELECT 1;",
        "SELECT 1; CREATE TABLE c(x, FOREIGN KEY (nosuch) REFERENCES p);",
        "PRAGMA case_sensitive_like = 0; SELECT 1; CREATE TABLE u(a, b AS (a LIKE 'x'));",
        "BEGIN; CREATE TABLE t(a); ROLLBACK; PRAGMA page_size = 1024; CREATE TABLE u(a); PRAGMA page_size;",
    ]
    .iter()
    .enumerate()
    {
        let a = temp_db(&format!("rev{i}-hipp"));
        let b2 = temp_db(&format!("rev{i}-ora"));
        let (hok, got) = run(&hipp_bin(), &a, b);
        let (ook, want) = run(&oracle(), &b2, b);
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want),
            "{b}"
        );
        assert_eq!(hok, ook, "{b}");
    }
}

#[test]
fn cli_v16_pragma_review_integrity_messages_byte_identical() {
    const PS: usize = 4096;
    // A record header the pin's row check cannot read: one row,
    // "database disk image is malformed", after the page check's.
    corrupt_check(
        "malformed",
        "CREATE TABLE t(a, b NOT NULL); CREATE INDEX i ON t(b); INSERT INTO t VALUES (2, 'x'), (1, 'helloworld');",
        |d| {
            let k = d
                .windows(11)
                .position(|w| &w[1..] == b"helloworld")
                .expect("row")
                - 2;
            d[k] = 0x7f;
            d.extend(std::iter::repeat_n(0u8, PS));
            let n = (d.len() / PS) as u32;
            d[28..32].copy_from_slice(&n.to_be_bytes());
        },
    );
    // The incremental-vacuum flag without auto-vacuum.
    corrupt_check(
        "incrvac",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1);",
        |d| d[67] = 1,
    );
    // An auto-vacuum file's header root page and pointer map.
    corrupt_check(
        "ptrmap",
        "PRAGMA page_size = 1024; PRAGMA auto_vacuum = 1; CREATE TABLE t(a, b); CREATE INDEX i ON t(b); \
         WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 60) \
         INSERT INTO t SELECT x, printf('%.*c', 300, char(65 + x % 26)) FROM c;",
        |d| {
            d[55] = 3;
            d[1024 + 5] = 5;
        },
    );
}
