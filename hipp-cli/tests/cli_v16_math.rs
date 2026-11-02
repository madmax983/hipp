//! hipp-cli v0.16 differentials: the math functions (leaf
//! feature `math`) must render byte-identically to the pinned
//! oracle — REAL results in the pin's list layout (`Inf`, `-Inf`,
//! a negative zero as `0.0`, subnormals, `1.0e+300`), NULL for
//! NaN-producing and non-numeric arguments, INTEGER results of
//! `ceil` / `floor` / `trunc`, and a generated corpus over the
//! functions whose results are exact on both engines (`sqrt`,
//! `mod`, `ceil` / `floor` / `trunc`, `degrees` / `radians`,
//! `pow` over powers of two, `log2` of powers of two). Each batch
//! runs one-shot and over stdin, and the hipp-written file must
//! pass the oracle's integrity check.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16math-{tag}-{}", std::process::id()));
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

/// One batch, byte-compared one-shot and over stdin (raw bytes:
/// a blob column renders unconverted, so not always UTF-8).
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(got, run(&oracle(), &b, batch));
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        run_stdin(&hipp_bin(), &a2, batch),
        run_stdin(&oracle(), &b2, batch)
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// Rendering of the special results and the argument rules.
const RENDER: &str = "\
SELECT pi(), -pi(), exp(1), exp(1000), -exp(1000), ln(0), sqrt(-1), sqrt(-0.0), ceil(-0.5), trunc(-0.0);
SELECT pow(2, -1074), pow(2, -1075), pow(2, 1023), pow(2, 1024), pow(10, 22), pow(10, -5), pow(-2, 3), pow(-8, 1.0 / 3);
SELECT ceil(5), typeof(ceil(5)), floor('-2.5'), trunc('7'), typeof(trunc('7')), ceil('5.0'), ceil(9223372036854775807), floor(-9223372036854775808);
SELECT ceil('abc'), ceil(x'31'), ceil(NULL), sqrt(' 4 '), sqrt('4x'), sin(''), log(2, x'38'), log(2, '8abc'), log(1, 8);
SELECT mod(7, 3), mod(-7, 3), mod(7.5, 2), mod(7, 0), mod(1e308, 3), mod(-0.0, 1), degrees(pi()), radians(180), log10(1000), log2(1024);
SELECT atan2(0, -1), atan2(-0.0, -1), atan2(1, 0), atan(1e400), acos(-1), asin(1), acosh(1), atanh(1), atanh(-1), tanh(30), cosh(710), sinh(-711);
SELECT sin(1e400), cos(-1e400), tan(1e300), sqrt(1e400), ln(1e400), exp(-1e400), exp(-746), sqrt(5e-324), sqrt(1e-310), ceil(-4.0e-320);
SELECT log(10, 1000), log(3, 81), log(8, 2), ln(1), exp(0), cos(0), sin(-0.0), sqrt(2), sqrt(1e300), power(0, 0), pow(0, -1);
CREATE TABLE g(a, b AS (sqrt(a)), c AS (ceil(a)) STORED);
INSERT INTO g(a) VALUES (4), (2.25), (-1), ('9'), (NULL);
SELECT a, b, c, typeof(c) FROM g;
";

/// xorshift64 over doubles: the exact functions on random
/// arguments of every magnitude.
fn exact_corpus() -> String {
    let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut rows = Vec::new();
    for i in 0..300 {
        let r = next();
        let x = match i % 3 {
            0 => loop {
                let v = f64::from_bits(next());
                if v.is_finite() {
                    break v;
                }
            },
            1 => ((r >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2e6,
            _ => ((r >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 20.0,
        };
        let y = ((next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 200.0;
        let k = (next() % 2100) as i64 - 1075;
        rows.push(format!("({x:e}, {y:e}, {k})"));
    }
    let mut out = String::from("CREATE TABLE c(x, y, k);\n");
    for ch in rows.chunks(50) {
        out.push_str(&format!("INSERT INTO c VALUES {};\n", ch.join(", ")));
    }
    out.push_str(
        "SELECT sqrt(abs(x)), ceil(x), floor(x), trunc(x), mod(x, y), degrees(x), radians(x) FROM c;\n\
         SELECT k, pow(2, k), pow(0.5, k), log2(pow(2, k)), pow(4, k / 2.0), sqrt(pow(2, k)) FROM c;\n",
    );
    out
}

#[test]
fn cli_v16_math_render_byte_identical() {
    byte_identical("render", RENDER);
}

#[test]
fn cli_v16_math_exact_corpus_byte_identical() {
    byte_identical("exact", &exact_corpus());
}
