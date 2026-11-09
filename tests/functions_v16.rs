//! v0.16 scalar function library, differentially vs the pinned
//! oracle (SQLite 3.54.1). The semantics were probed on the pin
//! before implementation (STATUS.md, "Scalar function library");
//! the matrix the tests encode:
//!
//! - `printf()` / `format()` (`sql`): the pin's conversion grammar
//!   — flags `- + space 0 # , !` in any order, width and precision
//!   (digits accumulate in 32 bits; `*` reads an argument as a C
//!   int, a negative width left-justifies), `l` / `ll` ignored —
//!   over `%d %i %u %x %X %o %p %r %c %s %z %q %Q %w %j %J %f %e
//!   %E %g %G %% %n`; an unknown conversion (or `%T` / `%S`) ends
//!   the output; missing arguments read as NULL; format and text
//!   arguments end at a NUL. REAL conversions round the pin's own
//!   digits: a 17-digit `!` rounding is the REAL -> TEXT shortening,
//!   a %e / %g rounding to fewer than 17 digits reads a truncated
//!   scaled product (`printf('%.0e', 450.0)` is `4e+02`), %f rounds
//!   the 18 / 19 digits; `#` drops the sign of a negative zero
//!   result; precision is capped at 100,000,000; a result of
//!   1,000,000,000 bytes is "string or blob too big".
//! - `round()` (ungated fit): `%!.*f` of those digits read back.
//! - `quote char unicode zeroblob randomblob random concat
//!   concat_ws octet_length unhex likely unlikely likelihood if
//!   iif(2+ args) sqlite_version` (`sql`), `last_insert_rowid
//!   changes total_changes` with the pin's counting rules and the
//!   matching `Connection` methods.
//! - Ungated fixes: `coalesce` / `ifnull` / `iif` evaluate lazily;
//!   `length` / `substr` / `trim` / `instr` / `replace` / `upper` /
//!   `lower` treat text as the pin's C functions do (up to a NUL,
//!   characters by lead byte), blobs as bytes; unknown functions
//!   and wrong argument counts are prepare-time errors.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, FileStorage, SchemaKind, Step};

type HErr = Error<<FileStorage as hipp::Storage>::Error>;

async fn open(path: &std::path::Path) -> Connection<FileStorage> {
    Connection::open(FileStorage::open(path).expect("fs open"), Config::default())
        .await
        .expect("open")
}

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> String {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    String::from_utf8_lossy(t.as_slice()).into_owned()
}

/// The CLI's list rendering of a TEXT / BLOB: up to its first NUL
/// (the corpora below hex anything holding control characters,
/// which the CLI shows in caret notation).
fn fmt_bytes(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
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
                    line.push_str(&fmt_bytes(b));
                }
            }
        }
        rows.push(line);
    }
    Ok(rows)
}

/// One batch through the oracle CLI: (success, stdout lines).
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

/// A script through the oracle CLI's stdin (errors are reported
/// and skipped, as hipp's driver below skips them): stdout lines.
fn oracle_script(path: &std::path::Path, sql: &str) -> Vec<String> {
    use std::io::Write;
    let mut child = std::process::Command::new(common::oracle())
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oracle spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(sql.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("oracle finishes");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

struct Twins {
    hipp: std::path::PathBuf,
    oracle: std::path::PathBuf,
}

fn twins(tag: &str, setup: &str) -> Twins {
    let hipp = common::temp_db(&format!("v16fn-{tag}-h"));
    let oracle = common::temp_db(&format!("v16fn-{tag}-o"));
    common::cli(&oracle, if setup.is_empty() { "SELECT 1;" } else { setup });
    block_on(async {
        let mut db = open(&hipp).await;
        for stmt in setup.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                try_collect(&mut db, stmt).await.expect("setup");
            }
        }
    });
    Twins { hipp, oracle }
}

impl Twins {
    /// One statement on both twins: equal rows, or both refuse
    /// it (hipp's error returned).
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

    fn refuse(&self, sql: &str, kind: SchemaKind) {
        match self.same(sql) {
            Some(Error::Schema { kind: k }) => assert_eq!(k, kind, "statement: {sql}"),
            other => panic!("expected {kind:?} for {sql}, got {other:?}"),
        }
    }
}

/// Compare many scalar expressions, a few per SELECT.
fn exprs_same(tw: &Twins, exprs: &[String]) {
    for chunk in exprs.chunks(10) {
        tw.same(&format!("SELECT {}", chunk.join(", ")));
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

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------- printf() / format() ----------------

#[test]
fn printf_fixed_cases() {
    let tw = twins("pfix", "");
    tw.all(&[
        "SELECT printf('%d %s %5.2f %x %%', 42, 'hi', 3.14159, 255), format('%05d', 42)",
        "SELECT printf(), printf(NULL), printf(NULL, 1), typeof(printf('')), printf(12), printf(x'2564', 5), printf('%d')",
        "SELECT printf('%s|%q|%Q|%w|%f|%g|%d', NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
        "SELECT printf('%s|%q|%Q|%w|%f|%g|%d')",
        "SELECT printf('a%Tb', 1), printf('a%Sb', 1), printf('a%rb', 1), printf('a%r %r %r %r %r', 1, 2, 3, 11, 112)",
        "SELECT printf('a%pb', 255), printf('a%nb', 5), printf('a%yb'), printf('ab%'), printf('ab%5'), printf('ab%-')",
        "SELECT printf('%5%|%-5%|'), printf('%k%d', 3), printf('%lld %ld %lu %llx %hd', 1, 2, 3, 255, 4)",
        "SELECT printf('%5.2lf', 1.5), printf('%l5d', 3), printf('%-+ #0,d', 1234)",
        "SELECT printf('%d|%d|%d|%d|%5d|%-5d|%05d|%+d|% d|%.3d|%8.3d', 0, -1, 9223372036854775807, -9223372036854775808, 42, 42, -42, 42, 42, 7, 7)",
        "SELECT printf('%,d|%,d|%x|%X|%#x|%#X|%#o|%o|%u|%x|%#x|%,x', 1234567, -1234567, -1, 255, 255, 255, 8, 8, -1, 0, 0, 1234567)",
        "SELECT printf('%#p|%,p|%5p|%-5p|%05p|%p|%.4p', 255, 255, 255, 255, 255, -1, 10)",
        "SELECT printf('%r|%r|%r|%r|%,r|%#r|%+r|%5r|%05r|%.4r|%r|%r', -1, 0, 13, 101, 1234, 2, 3, 4, 5, 6, 111, -11)",
        "SELECT printf('%,o|%,u|%,i|%#d|%#u|%#5o|%#05x|%#.3x|%#x', 8, 1234567, -1234567, 5, 5, 8, 255, 1, -1)",
        "SELECT printf('%,05d|%,010d|%,.6d|%-,8d|', 12345, 1234, 1, 1234)",
        "SELECT printf('%d', '12abc'), printf('%d', 1.9), printf('%d', -1.9), printf('%d', 1e30), printf('%d', x'3132')",
        "SELECT printf('%f', '1.5x'), printf('%f', x'312e35'), printf('%s', 0.1), printf('%s', 1e100), printf('%c', 65)",
        "SELECT printf('%x', 1.5), printf('%f', 9223372036854775807), printf('%.0f', 1e22), printf('%f', '  3e2')",
        "SELECT printf('%*d|', -5, 1), printf('%.*f', -2, 1.5), printf('%.*s', -3, 'abcdef'), printf('%*.*f|', 9, 2, 3.14159)",
        "SELECT length(printf('%2147483648d', 1)), length(printf('%4294967297d', 1)), printf('%.4294967297d', 1)",
        "SELECT length(printf('%*d', -2147483648, 1)), length(printf('%.*d', -2147483648, 1)), length(printf('%*d', 4294967301, 1))",
        "SELECT printf('%5c|%-5c|%.3c|%5.3c|%-5.3c|%.0c|%.1c', 'x', 'x', 'x', 'x', 'x', 'x', 'x')",
        "SELECT printf('%!5c|%5c|%c', 'é', 'é', 'éa'), hex(printf('%c', '')), hex(printf('%c', NULL)), hex(printf('%c'))",
        "SELECT printf('%!.3s|%.3s|%!5s|%5s|%!-5s|', 'éèêë', 'éèêë', 'éè', 'éè', 'éè')",
        "SELECT printf('[%.2q|%.2Q|%.2w|%.2J|%.0J|%5J|%.2j|%-6J|%!.2J]', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
        "SELECT printf('%q|%Q|%w', 'it''s', 'it''s', 'a\"b'), printf('%!q', 'é'''), printf('%!Q|%!.1Q', 'é', 'éa')",
        "SELECT printf('%.2q|%!.1q|%.0Q|%.0q', 'é', 'éa', 'abc', 'abc')",
        "SELECT printf('a' || char(0) || '%d', 5), printf('%s|', 'x' || char(0) || 'y'), length(printf('%c%c', 'a', ''))",
    ]);
}

#[test]
fn printf_json_and_unistr_forms() {
    let tw = twins("pjson", "");
    tw.all(&[
        "SELECT printf('%j|%J', 'a\"b', 'c\"d'), printf('[%j]', NULL), printf('[%J]', NULL)",
        "SELECT printf('[%j]', 12), printf('[%j]', 1.5), printf('[%j]', x'41'), printf('[%j]', '\\/')",
        "SELECT printf('[%10j]', 'ab'), printf('[%-10J]', 'ab'), printf('[%.2j]', 'abcdef'), printf('[%.3J]', 'a\"bcd')",
        "SELECT hex(printf('[%j]', char(1, 9, 10, 13, 8, 12, 127, 31, 233, 8232)))",
        "SELECT hex(printf('[%J]', char(1, 9, 10, 13, 8, 12, 127, 31, 233, 8232)))",
        "SELECT hex(printf('[%!6j|%6j|%06J|%+j|%.0J|%!.2j|%.1j]', 'éè', 'éè', 'a', 'a', 'abc', 'éèx', 'éèx'))",
        "SELECT printf('[%#J|%#.1J]', char(9), char(9, 9)), hex(printf('%#j|%#J', char(10), char(10)))",
        "SELECT hex(printf('%#Q|%#q|%#w', 'a' || char(10) || 'b''\\', 'a' || char(10) || 'b''\\', 'a' || char(10) || 'b\"\\'))",
        "SELECT printf('%#Q', 'abc'), printf('%#Q', NULL), printf('%#q', NULL), printf('%#q', 'a\\b'), printf('%#Q', 'a\\b')",
        "SELECT hex(printf('%#Q', char(1, 127, 31, 128, 233))), hex(printf('%#.2Q', char(10, 10, 10))), hex(printf('%#10Q|', char(10)))",
        "SELECT printf('%#q', 'it''s\\'), printf('%#Q', 'it''s\\'), printf('%#Q', char(233)), printf('%#.1Q', 'a' || char(10))",
        "SELECT hex(printf('%#!.2Q|%#!8Q|%#8Q|%-#20Q|', 'é' || char(10) || 'x', 'é' || char(10), 'é' || char(10), char(9)))",
    ]);
}

#[test]
fn printf_real_conversions() {
    let tw = twins("preal", "");
    tw.all(&[
        "SELECT printf('%.20g|%!.20g|%.20f|%!.30f|%.17g|%!.26g|%!.26e', 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1)",
        "SELECT printf('%g|%g|%g|%g|%g|%g|%#g|%!g|%g', 100000, 1000000, 1e-4, 1e-5, 0.0, -0.0, 1.0, 1.0, 123456789)",
        "SELECT printf('%f|%e|%E|%G|%.0f|%.0e|%#.0f|%#.0e|%.3e|%10.3f|%-10.3f|%010.3f', 3.14159, 3.14159, 3.14159, 1e-10, 2.5, 2.5, 2.5, 2.5, 1234.5, 3.14159, 3.14159, -3.14159)",
        "SELECT printf('%+.2f|% .2f|%+010.2f|%,f|%,.2f', 2.0, 2.0, -2.0, 1234567.891, -1234567.891)",
        "SELECT printf('%f|%f|%f|%e|%g|%+f|% f|%!f|%.3f', 1e308*10, -1e308*10, 0.0/0, 1e308*10, -1e308*10, 1e308*10, 1e308*10, 1e308*10, 1e400)",
        "SELECT length(printf('%010f', 1e308*10)), length(printf('%-010f', -1e308*10)), printf('%010g|%-010e', 1e308*10, 1e308*10)",
        "SELECT printf('%!f|%!.3f|%!e|%!.0f|%!.0e|%!g|%!.3g|%#.3g|%#g|%!#g|%#f', 1.5, 2.0, 1.5, 2.0, 2.0, 100.0, 2.0, 2.0, 1e10, 3.0, 1.0)",
        "SELECT printf('%.0g|%.1g|%g|%g|%G|%.3G|%g|%g', 0.5, 0.05, 1e-5, 123456.5, 1.5e-7, 1234.5, 999999.5, 9999995.0)",
        // The `#` form's negative zero.
        "SELECT printf('[%.0f|%#.0f|%.0f|%.0f|%.0e|%.1f|%+.0f|%.0g]', -0.1, -0.1, -0.6, -1e-300, -1e-300, -0.01, 0.1, -0.1)",
        "SELECT printf('[%#.0f|%#.0f|%#.1f|%#.0e|%#.0g|%#.2f|%#f|%#.0f|%#.0f|%#5.0f|%#+.0f|%!.0f|%!.0f]', -0.6, -0.4, -0.01, -0.1, -0.1, -0.001, -1e-10, -1.5, -0.0, -0.3, -0.3, -0.3, -0.6)",
        "SELECT printf('[% #.0f|%#.0f|%#08.3f|%#-8.3f|%#,.1f|% .0f|%+.0f|%#!.0f|%#.3g|%#.0e|%#g]', -0.3, -0.5, -0.0001, -0.0001, -0.01, -0.3, -0.3, -0.3, -0.0001, -1e-300, -1e-300)",
        // The 17-digit `!` shortening.
        "SELECT printf('%!.16g|%!.17g|%!.18g|%.17g|%!.17e|%!.17f|%!.16e|%!.18e', -93e-18, -93e-18, -93e-18, -93e-18, -93e-18, -93e-18, -93e-18, -93e-18)",
        "SELECT printf('%!.17g|%!.17g|%!.17g|%!.17g|%!.19g', 0.1, 1.0/3, 59095.2975, 2705633e-17, 0.1)",
        "SELECT printf('%!.17e|%!.16e|%!.17f|%!.17f|%!.16f|%!.18f', 0.1, 0.1, 0.1, 0.3, 0.1, 0.01)",
        // Ties at fewer than 17 digits: the pin's truncated scale.
        "SELECT printf('%.0e|%.0e|%.0e|%.0e|%.0e|%.0e|%.0e|%.0e|%.0e', 15.0, 25.0, 45.0, 450.0, 4500.0, 2.5e3, 5.5e15, 4.6e15, 1.5e19)",
        "SELECT printf('%.0e|%.1e|%.1e|%.2e|%.2e|%.0e|%.0e', 4.5, 4250.0, 4350.0, 12350.0, 12450.0, 950.0, 850.0)",
        "SELECT printf('%.1g|%.1g|%.2g|%.2g|%.3g|%.2e|%.3e', 450.0, 45.0, 4450.0, 445.0, 44450.0, 123500.0, 1234500.0)",
        "SELECT printf('%.3f|%.8e|%.9g|%.2f|%.4f', 166162.7745, 166162.7745, 166162.7745, 16616.27745, 16616.27745)",
        "SELECT printf('%.0f|%.0f|%.0f|%.1f|%.1f|%.0f|%.2f', 0.5, 1.5, 2.5, 0.25, 0.35, 4.5, 0.125)",
        // The precision cap.
        "SELECT length(printf('%.*f', 1000000000, 1.0)), length(printf('|%.*E', 123456789012345678))",
    ]);
}

/// Format strings drawn from the whole conversion grammar over a
/// value pool of every storage class, compared as hex (NULs and
/// invalid UTF-8 included).
#[test]
fn printf_generated_corpus() {
    let tw = twins("pgen", "");
    let mut rng = Rng(0x5eed_0016_9f17_0001);
    let convs = [
        "d", "i", "u", "x", "X", "o", "f", "e", "E", "g", "G", "s", "c", "q", "Q", "w", "j", "J",
        "%", "p", "r", "n", "z", "T", "y",
    ];
    let flags = ["-", "+", " ", "#", "0", ",", "!"];
    let vals = [
        "0",
        "1",
        "-1",
        "42",
        "-42",
        "255",
        "9223372036854775807",
        "-9223372036854775808",
        "3.14159",
        "-2.5",
        "0.5",
        "1e100",
        "1e-100",
        "1e308*10",
        "-1e308*10",
        "123456789.123",
        "0.1",
        "1.0/3",
        "2.0/3*1e20",
        "5e-324",
        "99999.99",
        "0.000123456",
        "'abc'",
        "'héllo'",
        "'it''s'",
        "'a\"b\\c'",
        "char(10)||'x'",
        "char(1,9,31)",
        "''",
        "NULL",
        "x'414243'",
        "x'00'",
        "'12abc'",
        "' 3.5e2'",
        "-0.0",
        "1e15",
        "1e16",
        "450.0",
        "0.95",
        "0.05",
        "-3",
        "2",
        "5",
        "30",
    ];
    let mut exprs = Vec::new();
    for _ in 0..900 {
        let mut fmt = String::new();
        for _ in 0..1 + rng.below(3) {
            if rng.below(3) == 0 {
                fmt.push_str(rng.pick(&["x", " ", "|", "ab"]));
            }
            fmt.push('%');
            for _ in 0..rng.below(4) {
                fmt.push_str(rng.pick(&flags));
            }
            match rng.below(10) {
                0..=2 => fmt.push_str(&(1 + rng.below(24)).to_string()),
                3 => fmt.push('*'),
                _ => {}
            }
            match rng.below(10) {
                0..=3 => fmt.push_str(&format!(".{}", rng.below(31))),
                4 => fmt.push_str(".*"),
                _ => {}
            }
            if rng.below(20) == 0 {
                fmt.push_str(rng.pick(&["l", "ll"]));
            }
            fmt.push_str(rng.pick(&convs));
        }
        let mut call = format!("hex(printf({}", sql_str(&fmt));
        for _ in 0..rng.below(6) {
            call.push_str(", ");
            call.push_str(rng.pick(&vals));
        }
        call.push_str("))");
        exprs.push(call);
    }
    exprs_same(&tw, &exprs);
}

/// Random doubles (bit patterns, short decimals, ties) through
/// the REAL conversions, round() and quote(), compared value for
/// value — the digit generation and every rounding path.
#[test]
fn printf_real_corpus() {
    let tw = twins("prgen", "");
    let mut rng = Rng(0x00d1_6175_0016_7777);
    let specs = [
        "%f", "%.0f", "%.1f", "%.2f", "%.3f", "%.10f", "%.15f", "%.17f", "%e", "%.0e", "%.3e",
        "%.16e", "%.20e", "%g", "%.1g", "%.3g", "%.10g", "%.15g", "%.16g", "%.17g", "%.20g",
        "%!.15g", "%!.17g", "%!.20g", "%!.26g", "%!e", "%!.16e", "%!.20e", "%#g", "%#.3g", "%!f",
        "%!.3f", "%#.0f", "%,.2f", "%+.3e", "%012.4f", "%-12.4e|", "%G", "%E",
    ];
    let mut exprs = Vec::new();
    for _ in 0..700 {
        let lit = match rng.below(4) {
            0 => loop {
                let x = f64::from_bits(rng.next());
                if x.is_finite() {
                    break format!("{x:e}");
                }
            },
            1 => {
                let nd = 1 + rng.below(17);
                let m = rng.next() % 10u64.pow(nd as u32).max(2);
                let e = rng.below(61) as i64 - 30;
                format!(
                    "{}{}e{e}",
                    if rng.below(2) == 0 { "-" } else { "" },
                    m.max(1)
                )
            }
            2 => {
                // Exact ties (P5 * 10^z) and halfway decimals.
                let p = 1 + rng.below(99_999);
                let z = rng.below(6) as u32;
                format!("{}.0", (p as u64 * 10 + 5) * 10u64.pow(z))
            }
            _ => format!("{}.{}5", rng.below(1_000_000), rng.below(1000)),
        };
        let fmt: Vec<&str> = (0..3).map(|_| rng.pick(&specs)).collect();
        exprs.push(format!("printf('{}', {lit}, {lit}, {lit})", fmt.join("|")));
        exprs.push(format!("round({lit}, {})", rng.below(31)));
        exprs.push(format!("quote({lit})"));
    }
    exprs_same(&tw, &exprs);
}

#[test]
fn round_fits_the_pin() {
    let tw = twins("round", "");
    tw.all(&[
        "SELECT round(2.675, 2), round(1.005, 2), round(0.285, 2), round(0.49999999999999994), round(-0.5), round(-2.5)",
        "SELECT round(1e300, 2), round(5e-324, 30), round(1.0000000000000002, 15), round(123.456, 100), round(9.995, 2)",
        "SELECT round(x'31', 0), round('2.5'), typeof(round(3)), round(3, 'a'), round(NULL), round(1, NULL), round(1.5, -3), round(1.25, 1)",
        "SELECT round(-0.0001, 2), quote(round(-0.0001, 2)), round(-0.4), round(0.0001, 2), round(-1e-300, 5)",
        // Regressions: a value rounding to zero (hipp panicked in
        // the read-back), and digits the exact expansion got wrong.
        "SELECT round(-6.643632353073568e-275, 22), round(7.312903965700714e-168, 13), round(188.30472979193155, 13)",
        "SELECT round(166162.7745, 3), round(799.3565392695623, 25), round(-219.87892246138063, 19)",
    ]);
}

#[test]
fn printf_too_big() {
    let tw = twins("ptoo", "");
    tw.refuse("SELECT printf('%1000000001d', 1)", SchemaKind::TooBig);
    tw.refuse("SELECT printf('%.*c', 1000000001, 'x')", SchemaKind::TooBig);
    tw.refuse("SELECT zeroblob(1000000001)", SchemaKind::TooBig);
    tw.refuse("SELECT randomblob(1000000001)", SchemaKind::TooBig);
    tw.all(&["SELECT length(printf('%.*c', 5000, 'x')), length(printf('%9000d', 1))"]);
}

// ---------------- the value functions ----------------

#[test]
fn value_functions_fixed_cases() {
    let tw = twins("vfix", "");
    tw.all(&[
        "SELECT quote('it''s'), quote(1.5), quote(x'ab'), quote(NULL), quote(-0.0), quote(1e300), quote(1e-300), quote(0.1)",
        "SELECT quote(12), quote(x''), quote(''), quote(1e308*10), quote(-1e308*10), quote(9223372036854775807), quote(100.0), quote(123456789012345678.0)",
        "SELECT hex(quote(char(65, 0, 66))), quote(0.1 + 0.2), quote(1.0 / 3), quote(5e-324), quote(1e16), quote(1e15)",
        "SELECT char(), typeof(char()), char(72, 105), char(NULL), char(65, NULL, 66), hex(char(0)), char(1114111)",
        "SELECT hex(char(1114112)), hex(char(-1)), hex(char(55296)), char('65', 66.7, x'3637'), hex(char(9223372036854775807)), hex(char(128, 2048, 65536))",
        "SELECT unicode('H'), unicode(''), unicode(NULL), unicode(65), unicode(x'c3a9'), unicode(x'ff'), unicode(x'80'), unicode(x'e282')",
        "SELECT unicode(x'f09f9880'), unicode(1.5), unicode(x'eda080'), unicode(x'f4908080'), unicode(x'c0'), unicode(x'c1bf'), unicode(x'e08080')",
        "SELECT hex(zeroblob(3)), length(zeroblob(5)), typeof(zeroblob(0)), zeroblob(-1) = x'', hex(zeroblob('2')), hex(zeroblob(2.9)), typeof(zeroblob(NULL))",
        "SELECT likely(1), unlikely(0), likelihood(1, 0.5), likely(NULL), typeof(likely('a')), likelihood('x', 0.0), likelihood(2, 1.0)",
        "SELECT likelihood(1, (0.5)), likelihood(1, 5e-1), likelihood(1, 1.00000000000000001), unlikely(x'01') = x'01'",
        "SELECT concat('a', NULL, 'b', 1), concat_ws('-', 'a', NULL, 'b'), concat(NULL), typeof(concat(NULL)), concat(1.5, x'41', 2)",
        "SELECT typeof(concat(x'41')), concat_ws(NULL, 'a'), typeof(concat_ws(',', NULL)), concat_ws(',', NULL, NULL, 'x', NULL), concat_ws(1, 2, 3)",
        "SELECT concat_ws('', 'a', 'b'), concat_ws(',', 'a'), typeof(concat_ws(NULL, 'a')), concat(1e100, -0.0)",
        "SELECT octet_length('héllo'), octet_length(NULL), octet_length(12), octet_length(1.5), octet_length(x'0102'), octet_length(''), octet_length(-0.0), octet_length(1e100)",
        "SELECT hex(unhex('4142')), unhex('zz'), hex(unhex('')), typeof(unhex('')), unhex(NULL), hex(unhex('41 42', ' ')), hex(unhex('41-42', '-')), unhex('41-42')",
        "SELECT hex(unhex('a')), hex(unhex('4g')), hex(unhex(4142)), hex(unhex('41 42 ', ' ')), hex(unhex(' 4142', ' ')), hex(unhex('4 142', ' '))",
        "SELECT hex(unhex('41é42', 'é')), unhex('41', NULL), hex(unhex(x'3431')), hex(unhex('aBcD')), hex(unhex('41xx42', 'x')), hex(unhex('41', ''))",
        "SELECT iif(1, 'a'), iif(0, 'a'), if(1, 2, 3), if(0, 2), typeof(iif(0, 1)), iif(0, 1, 0, 2, 3), iif(0, 1, 0, 2), iif(0, 1, 1, 2, 3)",
        "SELECT if(NULL, 1, 2), iif('1x', 1, 2), iif(0.5, 1, 2), sqlite_version(), typeof(sqlite_version())",
    ]);
}

#[test]
fn argument_counts_and_unknown_functions() {
    let tw = twins("argc", "CREATE TABLE t(p)");
    for sql in [
        "SELECT quote()",
        "SELECT quote(1, 2)",
        "SELECT unicode()",
        "SELECT random(1)",
        "SELECT randomblob()",
        "SELECT zeroblob()",
        "SELECT changes(1)",
        "SELECT total_changes(1)",
        "SELECT last_insert_rowid(1)",
        "SELECT unhex()",
        "SELECT unhex(1, 2, 3)",
        "SELECT octet_length()",
        "SELECT likelihood(1)",
        "SELECT likely(1, 2)",
        "SELECT unlikely()",
        "SELECT concat()",
        "SELECT concat_ws(',')",
        "SELECT iif(1)",
        "SELECT if()",
        "SELECT sqlite_version(1)",
        "SELECT abs(1, 2)",
        "SELECT abs()",
        "SELECT min()",
        "SELECT length(1, 2)",
    ] {
        tw.refuse(sql, SchemaKind::WrongArgumentCount);
    }
    for sql in ["SELECT nosuch(1)", "SELECT p FROM t WHERE nosuch(p)"] {
        tw.refuse(sql, SchemaKind::UnknownFunction);
    }
    for sql in [
        "SELECT likelihood(1, 1)",
        "SELECT likelihood(1, 0)",
        "SELECT likelihood(1, 1.5)",
        "SELECT likelihood(1, -0.1)",
        "SELECT likelihood(1, -0.0)",
        "SELECT likelihood(1, '0.5')",
        "SELECT likelihood(1, NULL)",
        "SELECT likelihood(1, 0.5 + 0)",
        "SELECT likelihood(1, 1.0000000000000002)",
        "SELECT likelihood(1, p) FROM t",
    ] {
        tw.refuse(sql, SchemaKind::LikelihoodNotConstant);
    }
    // Prepare-time: refused before any row is read, even over no
    // rows. sqlite_source_id() names the pin's source check-in,
    // which hipp is not: left undefined (a documented gap).
    block_on(async {
        let mut db = open(&tw.hipp).await;
        assert!(matches!(
            db.prepare("SELECT sqlite_source_id()").await,
            Err(Error::Schema {
                kind: SchemaKind::UnknownFunction
            })
        ));
        assert!(matches!(
            db.prepare("SELECT nosuch(p) FROM t").await,
            Err(Error::Schema {
                kind: SchemaKind::UnknownFunction
            })
        ));
        assert!(matches!(
            db.prepare("SELECT abs(p, p) FROM t").await,
            Err(Error::Schema {
                kind: SchemaKind::WrongArgumentCount
            })
        ));
        // A bound probability is no literal: refused at prepare,
        // as on the pin (TK_VARIABLE is not TK_FLOAT).
        assert!(matches!(
            db.prepare("SELECT likelihood(1, ?)").await,
            Err(Error::Schema {
                kind: SchemaKind::LikelihoodNotConstant
            })
        ));
    });
}

/// A table of every storage class through the value functions.
#[test]
fn generated_value_function_corpus() {
    let setup = "CREATE TABLE v(x); INSERT INTO v VALUES (NULL), (0), (-7), (65), (9223372036854775807), (1.5), (-0.0), (1e100), (''), ('abc'), ('é'), ('12abc'), ('4142'), ('it''s'), (x''), (x'00'), (x'c3a9'), (x'ff41'), (x'3431'), ('a' || char(0) || 'b'), (char(1, 10)), ('  x  ')";
    let tw = twins("vgen", setup);
    let mut rng = Rng(0x0016_f00d_cafe_0003);
    let unary = [
        "quote(x)",
        "hex(quote(x))",
        "unicode(x)",
        "octet_length(x)",
        "hex(unhex(x))",
        "hex(char(x))",
        "hex(concat(x, x))",
        "hex(concat_ws(x, 'a', x))",
        "hex(concat_ws('-', x, NULL, x))",
        "likely(x) IS x",
        "typeof(zeroblob(x))",
        "length(zeroblob(x))",
        "hex(printf('%s|%q|%Q|%w|%j|%J|%c', x, x, x, x, x, x, x))",
        "hex(printf('%d|%f|%g|%x', x, x, x, x))",
        "length(x)",
        "hex(upper(x))",
        "hex(lower(x))",
        "hex(trim(x))",
        "hex(substr(x, 2))",
        "hex(substr(x, -1))",
        "instr(x, 'b')",
        "instr(x, x)",
        "hex(replace(x, 'b', 'Z'))",
        "iif(x, 'y', 'n')",
        "hex(coalesce(x, 'dflt'))",
    ];
    let mut items = Vec::new();
    for _ in 0..60 {
        items.push(rng.pick(&unary).to_string());
    }
    for chunk in items.chunks(6) {
        tw.same(&format!("SELECT {} FROM v", chunk.join(", ")));
    }
}

// ---------------- ungated fixes ----------------

#[test]
fn text_functions_read_text_as_the_pin() {
    let tw = twins("textfix", "");
    tw.all(&[
        // BLOB operands: instr searches bytes; replace / trim give TEXT.
        "SELECT instr(x'0102', x'02'), instr(x'010203', x'0203'), hex(replace(x'010203', x'02', x'ff')), typeof(replace(x'010203', x'02', x'ff'))",
        "SELECT hex(trim(x'000100', x'00')), typeof(trim(x'000100', x'00')), instr('héllo', 'l'), instr(x'68c3a96c6c6f', x'6c')",
        "SELECT instr(x'68c3a96c6c6f', 'l'), instr('héllo', x'6c'), typeof(ltrim(x'41')), hex(rtrim(x'4141', 'A')), instr(x'', x'')",
        "SELECT instr(NULL, 1), instr(12345, 34), instr(1.5, '.'), hex(replace('abc', 'b', x'00')), typeof(replace(x'41', 'z', 'y'))",
        "SELECT instr(x'00ff01', x'01'), instr(x'ff01', x'01'), instr('abc', ''), instr('', ''), instr(x'01', x'')",
        // trim's set is whole characters, up to a NUL.
        "SELECT hex(trim('ééa', 'é')), hex(trim('xéx', x'c3')), trim('  a  '), ltrim('aab', 'a'), rtrim(NULL), trim('abc', NULL), hex(trim(x'c3a9c3', 'é'))",
        "SELECT hex(trim(cast(x'200020' as text))), hex(trim('a' || char(0) || 'a', 'a'))",
        // upper / lower of a BLOB are TEXT.
        "SELECT upper(x'61'), typeof(upper(x'61')), lower(x'41'), typeof(lower(x'41')), upper('é')",
        // Text ends at a NUL; a lead byte takes its continuation bytes.
        "SELECT length(CAST(x'610062' AS TEXT)), hex(upper(CAST(x'610062' AS TEXT))), hex(substr(CAST(x'610062' AS TEXT), 2))",
        "SELECT length(CAST(x'C3' AS TEXT)), hex(substr(cast(x'c3a9c3' as text), 2)), hex(lower(cast(x'4100c3' as text))), length(x'610062')",
        "SELECT length('a' || char(0) || 'b'), length(char(0)), hex(substr(char(65, 0, 66), 1, 3))",
        // substr of a BLOB counts bytes.
        "SELECT hex(substr(x'c3a9', 1, 1)), length(x'c3a9'), hex(substr('é', 1, 1)), hex(substr(x'c3a9c3a9', -1)), typeof(substr(x'01', 1))",
        // replace with an empty or NUL-led pattern keeps the subject.
        "SELECT replace('abc', x'00', 'z'), hex(replace('a' || char(0) || 'b', char(0), 'z')), hex(replace(x'610062', x'00', x'7a')), typeof(replace(x'41', '', 'y'))",
    ]);
}

#[test]
fn coalesce_and_iif_are_lazy() {
    let tw = twins(
        "lazy",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1), (NULL)",
    );
    tw.all(&[
        "SELECT coalesce(1, abs(-9223372036854775808)), ifnull(1, abs(-9223372036854775808))",
        "SELECT iif(1, 2, abs(-9223372036854775808)), iif(0, abs(-9223372036854775808), 3)",
        "SELECT coalesce(a, 7, abs(-9223372036854775808)) FROM t",
        "SELECT CASE WHEN 1 THEN 2 ELSE abs(-9223372036854775808) END",
    ]);
    tw.refuse(
        "SELECT nullif(1, abs(-9223372036854775808))",
        SchemaKind::IntegerOverflow,
    );
    tw.refuse(
        "SELECT coalesce(NULL, abs(-9223372036854775808))",
        SchemaKind::IntegerOverflow,
    );
}

// ---------------- connection state ----------------

/// The statements of a script, split at top-level `;` (the test
/// scripts hold none inside literals).
fn statements(script: &str) -> Vec<String> {
    script
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{s};"))
        .collect()
}

/// Run `script` on a fresh hipp connection and through the
/// oracle's stdin, with a probe of the connection state after
/// every statement: the SELECT outputs must agree line for line,
/// and hipp's `Connection` methods must agree with the pin's
/// `changes()` / `total_changes()` / `last_insert_rowid()`.
fn script_same(tag: &str, script: &str) {
    let probe = "SELECT 'state', changes(), total_changes(), last_insert_rowid();";
    let stmts = statements(script);
    let mut oracle_sql = String::new();
    for s in &stmts {
        oracle_sql.push_str(s);
        oracle_sql.push('\n');
        oracle_sql.push_str(probe);
        oracle_sql.push('\n');
    }
    let opath = common::temp_db(&format!("v16fn-{tag}-o"));
    let want = oracle_script(&opath, &oracle_sql);
    let hpath = common::temp_db(&format!("v16fn-{tag}-h"));
    let got = block_on(async {
        let mut db = open(&hpath).await;
        let mut out = Vec::new();
        for s in &stmts {
            if let Ok(rows) = try_collect(&mut db, s).await {
                out.extend(rows);
            }
            let api = format!(
                "state|{}|{}|{}",
                db.changes(),
                db.total_changes(),
                db.last_insert_rowid()
            );
            let rows = try_collect(&mut db, probe).await.expect("probe");
            assert_eq!(rows, vec![api.clone()], "API vs SQL after {s}");
            out.extend(rows);
        }
        out
    });
    assert_eq!(got, want, "script {tag}");
    assert_eq!(oracle_raw(&hpath, "PRAGMA integrity_check;").1, ["ok"]);
}

#[test]
fn change_counters_follow_the_pin() {
    script_same(
        "counts",
        "CREATE TABLE t(a PRIMARY KEY, b);
         INSERT INTO t VALUES(1,1),(2,2),(3,3);
         INSERT OR REPLACE INTO t VALUES(1,10),(4,4);
         INSERT OR IGNORE INTO t VALUES(2,20),(5,5);
         INSERT OR IGNORE INTO t VALUES(2,20);
         INSERT INTO t VALUES(3,0) ON CONFLICT(a) DO UPDATE SET b=b+100;
         INSERT INTO t VALUES(3,0),(9,9) ON CONFLICT(a) DO NOTHING;
         REPLACE INTO t VALUES(9, 99);
         CREATE TABLE u(x);
         DELETE FROM t WHERE a > 3;
         DELETE FROM t;
         SELECT 1;
         DROP TABLE u;
         BEGIN;
         COMMIT;
         CREATE TABLE w(a INTEGER PRIMARY KEY, b UNIQUE);
         INSERT INTO w VALUES(1,1),(2,2);
         UPDATE OR REPLACE w SET b=1 WHERE a=2;
         UPDATE w SET b=b WHERE 0;
         UPDATE w SET b=b;
         INSERT INTO w VALUES(100, 100);
         INSERT INTO w VALUES(100, 5);
         INSERT INTO w VALUES(50, 7),(100, 8);
         SELECT rowid, * FROM w;
         BEGIN;
         INSERT INTO w VALUES(80,80);
         ROLLBACK;
         CREATE TABLE z AS SELECT * FROM w;
         INSERT INTO w VALUES(-5, -5) RETURNING last_insert_rowid(), changes(), total_changes();
         INSERT INTO w(b) SELECT 9 WHERE 0;
         UPDATE w SET b = b + 1000 WHERE a < 0 RETURNING changes(), b;
         DELETE FROM w WHERE a = -5 RETURNING total_changes();
         CREATE INDEX wi ON w(b);
         ALTER TABLE w ADD COLUMN c;
         CREATE VIEW v AS SELECT 1;
         DROP VIEW v",
    );
}

#[test]
fn last_insert_rowid_moves_per_row() {
    script_same(
        "lastrow",
        "CREATE TABLE s(x);
         INSERT INTO s VALUES(10),(20),(30);
         CREATE TABLE r(a);
         INSERT INTO r VALUES(last_insert_rowid()),(last_insert_rowid()),(last_insert_rowid());
         SELECT rowid, a FROM r;
         INSERT INTO r SELECT total_changes() FROM s;
         INSERT INTO r SELECT changes() FROM s;
         SELECT rowid, a FROM r;
         UPDATE r SET a = total_changes();
         SELECT a, changes() FROM r;
         INSERT INTO r(rowid, a) VALUES(-9223372036854775808, 2);
         INSERT INTO r VALUES(last_insert_rowid() + 1) RETURNING a, last_insert_rowid();
         CREATE TABLE f(a,b);
         INSERT INTO f VALUES(1,0),(2,0);
         CREATE TABLE g(x,y);
         INSERT INTO g VALUES(1,10),(1,20),(2,30);
         UPDATE f SET b=y FROM g WHERE g.x=f.a;
         CREATE TABLE d(a DEFAULT (last_insert_rowid()), b DEFAULT (typeof(random())));
         INSERT INTO d DEFAULT VALUES;
         INSERT INTO d(b) VALUES(1);
         INSERT INTO d DEFAULT VALUES;
         SELECT rowid, * FROM d;
         CREATE TABLE ck(a CHECK(a <> last_insert_rowid()));
         INSERT INTO ck VALUES(1);
         INSERT INTO ck VALUES(1);
         UPDATE ck SET a = 5 RETURNING changes(), last_insert_rowid();
         DELETE FROM ck RETURNING total_changes()",
    );
}

#[test]
fn random_functions_types_and_ranges() {
    let tw = twins(
        "rand",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1), (2), (3), (4)",
    );
    tw.all(&[
        "SELECT typeof(random()), length(randomblob(4)), length(randomblob(0)), length(randomblob(-5)), length(randomblob(NULL))",
        "SELECT typeof(randomblob(NULL)), length(randomblob('3')), length(randomblob(2.7)), random() = random()",
        "SELECT count(DISTINCT random()), count(DISTINCT randomblob(8)) FROM t",
        "SELECT count(DISTINCT x) FROM (SELECT random() AS x FROM t)",
        "SELECT random() BETWEEN -9223372036854775807 AND 9223372036854775807 FROM t",
    ]);
    // The stream is the connection's: deterministic for a seed.
    let draw = |seed: u64| -> Vec<i64> {
        let path = common::temp_db(&format!("v16fn-seed-{seed}"));
        block_on(async {
            let fs = FileStorage::open(&path).expect("fs open");
            let mut db = Connection::open(fs, Config::default().nonce_seed(seed))
                .await
                .expect("open");
            let mut out = Vec::new();
            for _ in 0..4 {
                let mut q = db.prepare("SELECT random()").await.expect("prepare");
                assert_eq!(q.step().await.expect("step"), Step::Row);
                out.push(q.column_i64(0).expect("int"));
            }
            out
        })
    };
    assert_eq!(draw(7), draw(7));
    assert_ne!(draw(7), draw(8));
    assert!(draw(9).iter().all(|r| *r != i64::MIN));
}

#[test]
fn nondeterministic_functions_in_definitions() {
    let tw = twins("nondet", "");
    for g in [
        "random()",
        "randomblob(2)",
        "changes()",
        "total_changes()",
        "last_insert_rowid()",
        "sqlite_version()",
        "abs(random())",
    ] {
        tw.refuse(
            &format!("CREATE TABLE g(a, b AS ({g}))"),
            SchemaKind::NonDeterministic,
        );
    }
    let tw = twins(
        "det",
        "CREATE TABLE t(a, b AS (printf('%05d', a)) STORED, c AS (quote(a)), d AS (hex(unhex(a))))",
    );
    tw.all(&[
        "INSERT INTO t(a) VALUES (5), ('41'), (NULL)",
        "SELECT * FROM t",
        "CREATE TABLE c(a CHECK (random() <> 0), b DEFAULT (length(randomblob(3))))",
        "INSERT INTO c(a) VALUES (1)",
        "SELECT * FROM c",
    ]);
}

// ---------------- review fixes ----------------

/// A record whose overflow tail outgrows the page cache wrote a
/// truncated chain (pages spilled while the chain was being
/// built were re-read clean and their writes lost): the pin's
/// integrity check reported "overflow list length is 15 but
/// should be 17" for `zeroblob(69530)`, and hipp read the row
/// back as CorruptRecord. The cut-off moved with the cache size,
/// so the tiny profile's 4-page cache is exercised too, with a
/// plain blob literal (no v16 function needed).
#[test]
fn large_records_write_whole_overflow_chains() {
    let tw = twins("ovf", "CREATE TABLE t(b)");
    tw.all(&[
        "INSERT INTO t VALUES (zeroblob(69529)), (zeroblob(69530)), (zeroblob(250000))",
        "INSERT INTO t VALUES (printf('%70000d', 1)), (randomblob(140000))",
        "INSERT INTO t SELECT concat(b, b) FROM t WHERE rowid = 4",
        "UPDATE t SET b = b || b || b WHERE rowid = 2",
        "SELECT rowid, length(b), typeof(b), hex(substr(b, length(b) - 2)) FROM t WHERE rowid <> 5",
        "SELECT length(b) FROM t WHERE rowid = 5",
        "DELETE FROM t WHERE rowid IN (1, 3)",
        "SELECT rowid, length(b) FROM t",
    ]);
    assert_eq!(oracle_raw(&tw.hipp, "PRAGMA integrity_check;").1, ["ok"]);
    // The tiny profile's cache size, a 30 KB literal.
    let path = common::temp_db("v16fn-ovf-tiny");
    let lit: String = (0..30_000).map(|i| format!("{:02x}", i % 251)).collect();
    block_on(async {
        let cfg = Config::default()
            .cache_pages(4)
            .statement_arena(1 << 16, 1 << 20);
        let mut db = Connection::open(FileStorage::open(&path).expect("fs open"), cfg)
            .await
            .expect("open");
        try_collect(&mut db, "CREATE TABLE t(b)")
            .await
            .expect("create");
        let r = try_collect(&mut db, &format!("INSERT INTO t VALUES (x'{lit}')")).await;
        assert!(r.is_ok(), "insert: {r:?}");
        let rows = try_collect(&mut db, "SELECT length(b) FROM t")
            .await
            .expect("read");
        assert_eq!(rows, ["30000"]);
    });
    assert_eq!(oracle_raw(&path, "PRAGMA integrity_check;").1, ["ok"]);
    assert_eq!(
        oracle_raw(&path, "SELECT length(b), hex(substr(b, 29999)) FROM t;").1,
        [format!("30000|{}", &lit[59996..])]
    );
}

/// The pin's argument counts for coalesce (2+) and ifnull (2),
/// `f(*)` as `f()` and DISTINCT ignored on scalar functions, and
/// `count()` as `count(*)`.
#[test]
fn argument_count_review_fixes() {
    let tw = twins(
        "argc2",
        "CREATE TABLE t(a); INSERT INTO t VALUES (1), (NULL)",
    );
    for sql in [
        "SELECT coalesce(1)",
        "SELECT coalesce()",
        "SELECT coalesce(*)",
        "SELECT coalesce(DISTINCT 1)",
        "SELECT ifnull(1)",
        "SELECT ifnull(NULL, NULL, 5)",
        "SELECT ifnull(*)",
        "SELECT abs(*)",
        "SELECT abs(DISTINCT)",
        "SELECT concat(*)",
        "SELECT a FROM t WHERE ifnull(a)",
    ] {
        tw.refuse(sql, SchemaKind::WrongArgumentCount);
    }
    tw.refuse("SELECT nosuch(*)", SchemaKind::UnknownFunction);
    tw.refuse("SELECT nosuch(DISTINCT 1)", SchemaKind::UnknownFunction);
    tw.all(&[
        "SELECT coalesce(NULL, 2), ifnull(NULL, 5), coalesce(NULL, NULL, 5)",
        "SELECT typeof(random(*)), printf(*), char(*), changes(*), sqlite_version(*)",
        "SELECT typeof(last_insert_rowid(*)), total_changes(*), typeof(random(DISTINCT))",
        "SELECT abs(DISTINCT -1), upper(DISTINCT 'a'), coalesce(DISTINCT NULL, 2)",
        "SELECT ifnull(DISTINCT NULL, 2), iif(DISTINCT 1, 2), likelihood(DISTINCT 1, 0.5)",
        "SELECT likely(DISTINCT 3), min(DISTINCT 3, 1), max(DISTINCT 3, 1), nullif(DISTINCT 1, 2)",
        "SELECT printf(DISTINCT '%d-%d', 1, 1), char(DISTINCT 65, 65)",
        "SELECT abs(DISTINCT a) FROM t",
        "SELECT count(), count(*), count(a) FROM t",
        "SELECT count()",
    ]);
}

/// A subquery's evaluation error surfaces only if evaluation
/// reads it (the pin runs a subquery when it is reached): inside
/// coalesce / ifnull / iif / CASE, and on the undecided side of
/// an AND / OR (which the pin short-circuits when an operand has
/// a subquery; without one both sides run).
#[test]
fn subqueries_are_read_lazily() {
    let tw = twins(
        "lazysub",
        "CREATE TABLE z(a, b); INSERT INTO z VALUES (0, -9223372036854775808), (1, -9223372036854775808), (NULL, 2)",
    );
    let boom = "(SELECT abs(-9223372036854775808))";
    tw.all(&[
        &format!("SELECT coalesce(1, {boom}), ifnull(2, {boom}), iif(1, 3, {boom}), iif(0, {boom}, 4)"),
        &format!("SELECT CASE WHEN 1 THEN 1 ELSE {boom} END, CASE 2 WHEN 2 THEN 'x' ELSE {boom} END"),
        &format!("SELECT 0 AND {boom}, 1 OR {boom}, 1 IN (1, {boom})"),
        &format!("SELECT 1 WHERE 0 AND {boom}"),
        "SELECT coalesce(1, EXISTS (SELECT abs(-9223372036854775808))), coalesce(1, 5 IN (SELECT abs(-9223372036854775808)))",
        "SELECT coalesce(a, (SELECT abs(b))) FROM z WHERE a IS NOT NULL",
        "SELECT a AND (SELECT abs(b)) FROM z WHERE a = 0",
        "SELECT a OR (SELECT abs(b)) FROM z WHERE a = 1",
        &format!("SELECT coalesce(a, {boom}) FROM z WHERE a IS NOT NULL"),
        &format!("SELECT count(*) FROM z WHERE a = 0 OR (a = 1 AND {boom} IS NULL)"),
    ]);
    for sql in [
        "SELECT abs(-9223372036854775808)".to_string(),
        format!("SELECT {boom}"),
        format!("SELECT NULL AND {boom}"),
        format!("SELECT coalesce(NULL, {boom})"),
        "SELECT 5 IN (SELECT abs(-9223372036854775808))".to_string(),
        format!("SELECT EXISTS (SELECT 1) AND {boom}"),
        "SELECT coalesce(NULL, (SELECT abs(b))) FROM z WHERE a = 0".to_string(),
        "SELECT a = 1 OR (SELECT abs(b)) FROM z WHERE a = 0".to_string(),
        "SELECT a AND abs(b) FROM z WHERE a = 0".to_string(),
    ] {
        tw.refuse(&sql, SchemaKind::IntegerOverflow);
    }
}

/// length() / substr() / trim() / LIKE over large texts: walked
/// in place, not as a per-character list (which took 16 bytes per
/// character and was OOM-killed on the pin's largest legal
/// value); answers checked against the pin.
#[test]
fn text_functions_on_large_values() {
    let tw = twins("bigtext", "");
    tw.all(&[
        "SELECT length(printf('%*.*s', 20000000, 5, 'x')), length(hex(zeroblob(5000000)))",
        "SELECT length(substr(hex(zeroblob(3000000)), 5, 4000000)), substr(printf('%.*c', 2000000, 'é'), 1999999)",
        "SELECT length(substr(printf('%.*c', 2000000, 'é'), -3)), hex(substr(zeroblob(3000000), 2999999))",
        "SELECT length(trim(printf('%2000000s', 'y'))), hex(zeroblob(2000000)) LIKE '%01', printf('%.*c', 1000000, 'é') GLOB '*é'",
        "SELECT printf('%.*c', 1000000, 'é') LIKE '%é_é', instr(printf('%1000000s', 'q'), 'q')",
    ]);
}

/// A DEFAULT reading connection state is evaluated for each row
/// inserted: `last_insert_rowid()` sees the previous row's rowid
/// (VALUES, INSERT ... SELECT, OR REPLACE, upsert), `random()`
/// draws per row. hipp evaluated it once at prepare.
#[test]
fn connection_state_defaults_per_row() {
    script_same(
        "defrow",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b DEFAULT (last_insert_rowid()), c DEFAULT (random()));
         INSERT INTO t(a) VALUES(11),(12),(13);
         SELECT a, b, typeof(c) FROM t;
         SELECT count(DISTINCT c) FROM t;
         INSERT OR REPLACE INTO t(a) VALUES(12),(20);
         SELECT a, b FROM t;
         CREATE TABLE s(x);
         INSERT INTO s VALUES(100),(101);
         INSERT INTO t(a) SELECT x FROM s;
         SELECT a, b FROM t;
         INSERT INTO t(a) VALUES(30) RETURNING b;
         INSERT INTO t(a, b) VALUES(40, last_insert_rowid()),(41, last_insert_rowid());
         INSERT INTO t(a) VALUES(50),(51) ON CONFLICT(a) DO NOTHING;
         SELECT a, b FROM t WHERE a >= 40;
         SELECT count(DISTINCT c) = count(*) FROM t;
         CREATE TABLE u(k INTEGER PRIMARY KEY, d DEFAULT (changes()), e DEFAULT (total_changes() + 1));
         INSERT INTO u(k) VALUES(1),(2);
         SELECT * FROM u;
         CREATE TABLE w(a INTEGER PRIMARY KEY, n NOT NULL DEFAULT (last_insert_rowid()));
         INSERT OR REPLACE INTO w VALUES(5, NULL),(6, NULL);
         SELECT * FROM w",
    );
}

/// OR FAIL: a constraint violation (UNIQUE / PRIMARY KEY, NOT
/// NULL, CHECK) keeps the rows the statement changed before it —
/// in autocommit and in an explicit transaction, for INSERT
/// (VALUES, SELECT, upsert) and UPDATE — and `changes()` counts
/// them; other errors (overflow, datatype, STRICT) still undo the
/// statement. hipp used to treat FAIL as ABORT.
#[test]
fn or_fail_keeps_earlier_rows() {
    script_same(
        "orfail",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE, c NOT NULL DEFAULT 5, d CHECK(d > 0));
         INSERT INTO t VALUES(1,'a',1,1);
         INSERT OR FAIL INTO t VALUES(2,'b',1,1),(3,'a',1,1),(4,'d',1,1);
         INSERT OR FAIL INTO t VALUES(5,'e',1,1),(6,'f',NULL,1),(7,'g',1,1);
         INSERT OR FAIL INTO t VALUES(8,'h',1,1),(9,'i',1,-1),(10,'j',1,1);
         INSERT OR FAIL INTO t VALUES(11,'k',1,1),(1,'z',1,1);
         UPDATE OR FAIL t SET b = 'q' WHERE a >= 2;
         SELECT * FROM t;
         BEGIN;
         INSERT OR FAIL INTO t VALUES(20,'t',1,1),(21,'a',1,1);
         SELECT * FROM t WHERE a >= 20;
         COMMIT;
         SELECT * FROM t WHERE a >= 20;
         UPDATE OR FAIL t SET c = NULL WHERE a >= 11;
         INSERT OR FAIL INTO t SELECT a + 100, b, c, d FROM t ORDER BY a;
         INSERT OR FAIL INTO t VALUES(30,'x',1,1),(31, abs(-9223372036854775808),1,1);
         INSERT OR FAIL INTO t VALUES(32,'x2',1,1),('abc','y',1,1);
         INSERT OR ROLLBACK INTO t VALUES(33,'x3',1,1),(34,'a',1,1);
         INSERT OR ABORT INTO t VALUES(35,'x4',1,1),(36,'a',1,1);
         INSERT OR FAIL INTO t VALUES(37,'x5',1,1),(38,'x5',1,1) RETURNING a;
         SELECT count(*) FROM t;
         CREATE TABLE u(a INTEGER PRIMARY KEY, b UNIQUE);
         INSERT INTO u VALUES(1,'a'),(2,'b'),(3,'c');
         UPDATE OR FAIL u SET b = 'z' WHERE a >= 1;
         UPDATE OR FAIL u SET b = b || 'q';
         INSERT OR FAIL INTO u VALUES(9,'n'),(10,'a'),(1,'w') ON CONFLICT(a) DO UPDATE SET b = 'zq';
         INSERT OR FAIL INTO u VALUES(4,'d'),(5,'c') ON CONFLICT(b) DO NOTHING;
         INSERT OR FAIL INTO u VALUES(6,'e'),(7,'zq') ON CONFLICT(a) DO NOTHING;
         SELECT * FROM u;
         CREATE TABLE s(a INT) STRICT;
         INSERT OR FAIL INTO s VALUES(1),('x');
         SELECT count(*) FROM s;
         CREATE TABLE g(a, b AS (a * 2) CHECK(b < 10));
         INSERT OR FAIL INTO g(a) VALUES(1),(7),(2);
         SELECT * FROM g",
    );
}
