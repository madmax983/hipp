//! hipp-cli v0.16 differentials: UTF-16 databases (leaf feature
//! `utf16`) must render byte-identically to the pinned oracle — text
//! read from UTF-16le / UTF-16be files (BMP, astral, combining, NUL,
//! invalid UTF-16), `PRAGMA encoding`, `.schema` / `.tables` of
//! non-ASCII names, the BINARY order over the UTF-16 bytes, the text
//! functions, blobs rendered raw, and a generated corpus. Each batch
//! runs one-shot and over stdin; the hipp-written file must pass the
//! oracle's integrity check, and the CLI must read the oracle's file.

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
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16u16-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run(bin: &Path, db: &Path, sql: &str) -> Vec<u8> {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    assert!(
        out.status.success(),
        "{} failed on {sql:?}: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One batch, byte-compared one-shot and over stdin; then the
/// oracle's integrity check on hipp's file and both CLIs reading the
/// other's file with `check`.
fn byte_identical(tag: &str, batch: &str, check: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(got, run(&oracle(), &b, batch), "{tag}");
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch),
        "{tag} (stdin)"
    );
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
    assert_eq!(
        run(&hipp_bin(), &b, check),
        run(&oracle(), &a, check),
        "{tag}: each CLI on the other's file"
    );
    for dot in [".schema", ".tables"] {
        assert_eq!(
            run(&hipp_bin(), &a, dot),
            run(&oracle(), &b, dot),
            "{tag}: {dot}"
        );
    }
}

const BASICS: &str = "
PRAGMA encoding = '@ENC@';
PRAGMA encoding;
CREATE TABLE \"t\u{e9}\"(x TEXT, \"y\u{1f600}\" COLLATE NOCASE, n INT);
INSERT INTO \"t\u{e9}\" VALUES ('a', 'A', 1), ('\u{100}', '\u{100}', 2), ('\u{1f600}', '\u{1f600}', 3), ('\u{e000}', 'e', 4), ('e\u{301}', 'E\u{301}', 5), ('', '', 6), ('tab\there', 'x', 7), ('nul' || char(0) || 'cut', 'z', 8), ('\u{10ffff}', 'Z', 9);
INSERT INTO \"t\u{e9}\" VALUES (CAST(x'00D8' AS TEXT), CAST(x'41' AS TEXT), 10);
SELECT * FROM \"t\u{e9}\";
SELECT x FROM \"t\u{e9}\" ORDER BY x;
SELECT \"y\u{1f600}\" FROM \"t\u{e9}\" ORDER BY 1, n;
SELECT hex(x), length(x), octet_length(x), upper(x), quote(x) FROM \"t\u{e9}\";
SELECT x'41004200', CAST(x'41004200' AS TEXT), x'00D8', typeof(CAST(x AS BLOB)) FROM \"t\u{e9}\" LIMIT 1;
CREATE INDEX ix ON \"t\u{e9}\"(x);
SELECT x FROM \"t\u{e9}\" WHERE x > '';
SELECT min(x), max(x), group_concat(x, '|') FROM \"t\u{e9}\";
SELECT sql FROM sqlite_schema;
VACUUM;
PRAGMA encoding;
PRAGMA integrity_check;
";

#[test]
fn basics_both_orders() {
    for (k, order) in ["UTF-16le", "UTF-16be", "UTF-16"].iter().enumerate() {
        byte_identical(
            &format!("basics{k}"),
            &BASICS.replace("@ENC@", order),
            "PRAGMA encoding; SELECT x, hex(x) FROM \"t\u{e9}\" ORDER BY x;",
        );
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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const ALPHABET: [&str; 14] = [
    "a",
    "Q",
    " ",
    "\u{e9}",
    "\u{100}",
    "\u{4e2d}",
    "\u{e000}",
    "\u{fffd}",
    "\u{1f600}",
    "\u{10000}",
    "e\u{301}",
    "\u{3000}",
    "'",
    "|",
];

#[test]
fn generated_rendering() {
    let n: usize = std::env::var("HIPP_UTF16_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    for case in 0..n {
        let mut r = Rng(0xdead_beef_u64 ^ ((case as u64 + 1) * 0x9e37_79b9));
        let order = if case % 2 == 0 {
            "UTF-16le"
        } else {
            "UTF-16be"
        };
        let mut b = format!("PRAGMA encoding = '{order}';\nCREATE TABLE t(x, y);\n");
        if r.below(2) == 0 {
            b.push_str("CREATE INDEX i ON t(x);\n");
        }
        for i in 0..30 {
            let mut s = String::new();
            for _ in 0..r.below(6) {
                s.push_str(ALPHABET[r.below(ALPHABET.len() as u64) as usize]);
            }
            let s = s.replace('\'', "''");
            b.push_str(&format!(
                "INSERT INTO t VALUES ('{s}', char({}) || '{i}');\n",
                r.below(0x11_0000)
            ));
        }
        b.push_str(
            "SELECT x, y FROM t ORDER BY x, rowid;
SELECT x FROM t WHERE x > 'a' ORDER BY x DESC, rowid;
SELECT upper(x), substr(x, 2), replace(x, ' ', '_'), trim(x) FROM t;
SELECT x, count(*) FROM t GROUP BY x;
SELECT hex(y), unicode(y) FROM t;
PRAGMA integrity_check;
",
        );
        byte_identical(
            &format!("gen{case}"),
            &b,
            "SELECT x, y FROM t ORDER BY x, rowid;",
        );
    }
}
