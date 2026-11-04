//! v0.16 JSON functions (leaf feature `json`), differentially vs the
//! pinned oracle (SQLite 3.54.1). The semantics were probed on the
//! pin before implementation (STATUS.md, "JSON functions"); the
//! matrix the tests encode:
//!
//! - `json()` / `jsonb()` and the JSONB format byte for byte (`hex()`
//!   of every `jsonb_` result), JSON5 input (comments, unquoted keys,
//!   hex, `+`, `.5` / `5.`, trailing commas, single quotes,
//!   Infinity / NaN, escapes) and its canonical output, the number
//!   texts the pin keeps or renders.
//! - Paths (`$`, `.key`, `."key"`, `[N]`, `[#]`, `[#-N]`), the `->` /
//!   `->>` operators (precedence of `||`, integer / label / `[N]`
//!   right-hand sides), every edit function including the pin's
//!   denormalized-header overwrite and `$[#]` appends, `json_patch`.
//! - The JSON subtype through CASE / coalesce / iif / nullif / min /
//!   max / likely / `+` / COLLATE / CAST AS TEXT, lost elsewhere.
//! - Errors agree statement by statement (and hipp's are the
//!   remedy-named kinds), the 1000-level nesting and path limits.
//! - `json_group_array` / `json_group_object` (DISTINCT, ORDER BY,
//!   FILTER, as window functions), `json_each` / `json_tree` with
//!   every column, hidden `json` / `root`, correlated (lateral) use.
//! - Generated corpora (xorshift PRNG) of random JSON / JSON5 /
//!   malformed text, random JSONB blobs (valid and corrupt) and
//!   random paths against the live pin.
//!
//! Every comparison runs the oracle CLI once per batch over stdin
//! (statement results split by marker rows, errors read from its
//! "near line N" reports) and hipp on an in-memory database.

mod common;

use hipp::storage::block_on;
use hipp::{Config, Connection, Error, MemStorage, SchemaKind, Step, Value};

type HErr = Error<<MemStorage as hipp::Storage>::Error>;

/// The backslash, spelled out of the corpora's literals.
const BS: &str = "\\";

/// `@BS@` in a hand-written corpus line is a backslash.
fn bs(s: &str) -> String {
    s.replace("@BS@", BS)
}

/// The CLI's list rendering of a REAL.
fn fmt_real(f: f64) -> Vec<u8> {
    let mut t = hipp::arena::AVec::new();
    hipp::record::render_real(if f == 0.0 { 0.0 } else { f }, &mut t);
    t.as_slice().to_vec()
}

/// Bytes up to the first NUL (the CLI prints C strings).
fn cbytes(b: &[u8]) -> &[u8] {
    match b.iter().position(|c| *c == 0) {
        Some(i) => &b[..i],
        None => b,
    }
}

/// The shell's byte rendering: control bytes but tab and newline
/// in caret notation.
fn shell_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    for &c in b {
        if c < 0x20 && c != b'\t' && c != b'\n' {
            out.push(b'^');
            out.push(c + 0x40);
        } else {
            out.push(c);
        }
    }
    out
}

/// One statement's rows in the CLI's list rendering (`|` between
/// values, a newline between rows), or the error.
async fn collect(db: &mut Connection<MemStorage>, sql: &str) -> Result<Vec<u8>, HErr> {
    let mut q = db.prepare(sql).await?;
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    while q.step().await? == Step::Row {
        if !first {
            out.push(b'\n');
        }
        first = false;
        for i in 0..q.column_count() {
            if i > 0 {
                out.push(b'|');
            }
            match q.column_value(i) {
                Some(Value::Null) | None => {}
                Some(Value::Integer(v)) => out.extend_from_slice(v.to_string().as_bytes()),
                Some(Value::Real(f)) => out.extend_from_slice(&fmt_real(f)),
                Some(Value::Text(b)) => out.extend(shell_bytes(cbytes(b))),
                Some(Value::Blob(b)) => out.extend(shell_bytes(b)),
            }
        }
    }
    Ok(out)
}

fn hipp_run(setup: &[String], stmts: &[String]) -> Vec<Result<Vec<u8>, HErr>> {
    block_on(async {
        // The CLI's statement arena: the depth corpus parses
        // literals of tens of KiB.
        let d = Config::default();
        let (init, max) = (d.stmt_init, d.stmt_max.max(4 * 1024 * 1024));
        let mut db = Connection::open(MemStorage::new(), d.statement_arena(init, max))
            .await
            .expect("open");
        for s in setup {
            collect(&mut db, s).await.expect("setup");
        }
        let mut out = Vec::new();
        for s in stmts {
            out.push(collect(&mut db, s).await);
        }
        out
    })
}

/// The batch through the oracle CLI's stdin: per statement, its
/// output (`Some`) or None when the pin reported an error for it.
fn oracle_run(setup: &[String], stmts: &[String]) -> Vec<Option<Vec<u8>>> {
    use std::io::Write;
    let mut script = String::new();
    for s in setup {
        assert!(!s.contains('\n'), "setup statements are one line each");
        script.push_str(s);
        script.push_str(";\n");
    }
    for (i, s) in stmts.iter().enumerate() {
        assert!(!s.contains('\n'), "statements are one line each: {s}");
        script.push_str(&format!("SELECT '#@#{i}';\n{s};\n"));
    }
    // The script goes through a file: a pipe would deadlock once
    // both directions fill.
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-json-v16-{n}.sql"));
    std::fs::File::create(&path)
        .and_then(|mut f| f.write_all(script.as_bytes()))
        .expect("write script");
    let out = std::process::Command::new(common::oracle())
        .arg(":memory:")
        .stdin(std::fs::File::open(&path).expect("script"))
        .output()
        .expect("oracle runs");
    let _ = std::fs::remove_file(&path);
    // Split stdout at the marker rows (each at a line start); a
    // statement's output is what lies between, less the newline
    // ending its last row (a value's own trailing newline stays).
    let mut res: Vec<Option<Vec<u8>>> = vec![Some(Vec::new()); stmts.len()];
    let o = &out.stdout;
    let mut marks: Vec<(usize, usize, usize)> = Vec::new();
    let mut at = 0usize;
    while at < o.len() {
        if o[at..].starts_with(b"#@#") && (at == 0 || o[at - 1] == b'\n') {
            let end = o[at..]
                .iter()
                .position(|c| *c == b'\n')
                .map_or(o.len(), |k| at + k);
            if let Some(k) = std::str::from_utf8(&o[at + 3..end])
                .ok()
                .and_then(|t| t.parse::<usize>().ok())
            {
                marks.push((k, at, (end + 1).min(o.len())));
            }
            at = end;
        }
        at += 1;
    }
    for (m, &(k, _, body)) in marks.iter().enumerate() {
        let stop = marks.get(m + 1).map_or(o.len(), |n| n.1);
        let mut chunk = o[body.min(stop)..stop].to_vec();
        if chunk.last() == Some(&b'\n') {
            chunk.pop();
        }
        if k < res.len() {
            res[k] = Some(chunk);
        }
    }
    let base = setup.len();
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        let Some(at) = line.find("near line ") else {
            continue;
        };
        let n: usize = line[at + 10..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        assert!(n > base, "setup failed on the oracle: {line}");
        let k = n - base;
        if k.is_multiple_of(2) && k / 2 >= 1 && k / 2 <= stmts.len() {
            res[k / 2 - 1] = None;
        }
    }
    res
}

/// Every statement agrees with the pin: equal output, or both
/// fail. Returns hipp's results for further checks.
fn agree(tag: &str, setup: &[&str], stmts: &[String]) -> Vec<Result<Vec<u8>, HErr>> {
    let setup: Vec<String> = setup.iter().map(|s| s.to_string()).collect();
    let got = hipp_run(&setup, stmts);
    let want = oracle_run(&setup, stmts);
    let mut bad = Vec::new();
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        let ok = match (g, w) {
            (Ok(a), Some(b)) => a == b,
            (Err(_), None) => true,
            _ => false,
        };
        if !ok {
            bad.push(format!(
                "{}\n  hipp:   {:?}\n  oracle: {:?}",
                stmts[i],
                g.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
                w.as_ref().map(|b| String::from_utf8_lossy(b).into_owned())
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{tag}: {} of {} differ from the pin:\n{}",
        bad.len(),
        stmts.len(),
        bad.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
    got
}

/// One `SELECT <item>` per non-empty line (`!` lines are whole
/// statements).
fn lines(src: &str) -> Vec<String> {
    src.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.strip_prefix('!') {
            Some(s) => bs(s),
            None => format!("SELECT {}", bs(l)),
        })
        .collect()
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
    fn chance(&mut self, p: u64) -> bool {
        self.next() % 100 < p
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[test]
fn oracle_is_the_pin() {
    assert!(
        common::oracle_is_pinned() || std::env::var("HIPP_ALLOW_UNPINNED").is_ok(),
        "oracle is {}",
        common::oracle_version()
    );
}

#[test]
fn jsonb_blobs_and_json5_text() {
    agree(
        "jsonb",
        &[],
        &lines(
            r#"
hex(jsonb('[1,2,3]')), hex(jsonb('{"a":1}')), hex(jsonb('"abc"')), hex(jsonb('1.5')), hex(jsonb('0x1F')), hex(jsonb('+5')), hex(jsonb('.5')), hex(jsonb('5.'))
hex(jsonb('Infinity')), hex(jsonb('-Infinity')), hex(jsonb('NaN')), hex(jsonb('''a"b''')), hex(jsonb('"a@BS@nb"')), hex(jsonb('"a@BS@x41"')), hex(jsonb('{a:1}'))
hex(jsonb('[+.5, +0x1, -5., 1.e5, 1.5e+3]')), hex(jsonb('{a@BS@u0062:1}')), hex(jsonb('"a' || char(1) || 'b"'))
hex(jsonb('[' || printf('%.300c', '1') || ']')), hex(jsonb(json_array(printf('%.70000c', 'x'))))
json('[0x1F, +5, .5, 5., -.5, -0x10, Infinity, -Infinity, NaN, ''a"b'', "@BS@v", "@BS@x41", "@BS@0", "@BS@''", 1e5, 1E+5, -0, 0.0e0]')
json('[1,2,]'), json('{"a":1,}'), json('[1 /*x*/, //y' || char(10) || ' 2]'), json(char(0xa0) || '1'), json(char(0xfeff) || '1'), json('[' || char(11) || '1]')
json('{$a:1, _b:2, c3:3, é:4}'), json('{a@BS@u0062:1}'), json('{truex:1}'), json('{a' || char(0xa0) || ':1}')
json('[nan]'),json('[NaN]'),json('[inf]'),json('[infinity]'),json('[QNaN]'),json('[SNaN]'),json('[-Infinity]'),json('[+Infinity]'),json('[-inf]'),json('[+inf]'), json('[-infinity]'), json('[INF]')
json('[+.5]'), json('[+0x1]'), json('[-0x1]'), json('[0X1f]'), json('[+1.5]'), json('[+1e3]'), json('[1.e5]'), json('[-.5e2]')
json('[0x8000000000000000]'), json('[0xffffffffffffffffff]'), json('"@BS@u0000x"'), json('"@BS@ud800"'), json('"a@BS@/b"')
json('{"a":1, a:2}'), json(' [1] '), json('{ "a" : [ ] , "b" : { } }'), json('/**/[1]'), json('[1]/**/'), json('[1]//x')
json(char(0x2028) || '1'), json(char(0x3000) || '1' || char(0x2003)), json(char(0x1680) || '1'), json(char(0x205f) || '1'), json('[1,' || char(0x2028) || '2]')
json('[1]' || char(0) || 'x'), json('"a' || char(9) || 'b"'), hex(json('"' || cast(x'ff' as text) || '"')), hex(json('"' || char(0x7f) || '"'))
json(1), json(1.5), json(x'31'), json(x'3133'), typeof(json('1')), json(null), typeof(jsonb('1')), jsonb(null), hex(jsonb(jsonb('[1]')))
json(x'0b'), json(x'3c171300'), json(x'1b00'), json(x'34307831'), json(x'262E35'), json(x'162E'), json(x'1531'), json(x'2a5c71'), json(x'295c71'), json(x'3a612262'), json(x'2a2271'), json(x'1a00')
json(x'262d2e'), json(x'262d35'), json(x'362d2e35'), json(x'166b'), json(x'266b2e'), json(x'262d2d'), json(x'1365'), json(x'132d')
json('[True]')
json('[-nan]')
json('[00]')
json('[1x]')
json('[true_]')
json('[,]')
json('{-a:1}')
json('"@BS@q"')
json('[1] x')
json('')
json(x'')
json(x'162d')
json(x'1b31')
json(x'295c78')
json(char(0x200b) || '1')
json('[/* unterminated')
json('[1]/*')
"#,
        ),
    );
}

#[test]
fn values_numbers_and_strings() {
    agree(
        "values",
        &[],
        &lines(
            r#"
json_extract('[Infinity]','$[0]'), json_extract('[0x10]','$[0]'), json_extract('[0xffffffffffffffff]','$[0]')
json_extract('[0x8000000000000000]','$[0]'), json_extract('[-0x8000000000000000]','$[0]'), json_extract('[-0x1]','$[0]')
json_extract('[1.5e999]','$[0]'), json_extract('[-0]','$[0]'), json_extract('[-0.0]','$[0]'), typeof(json_extract('[-0]','$[0]')), json_extract('[1e5]','$[0]'), json_extract('[.5]','$[0]'), json_extract('[5.]','$[0]')
json_extract('[9223372036854775808]','$[0]'),json_extract('[-9223372036854775808]','$[0]'),json_extract('[-9223372036854775809]','$[0]'), json_extract('[123456789012345678901234567890]','$[0]')
json_array(0.1+0.2, 1e300, 9e999, -9e999, 1.0, 100.0, 1e15, 1e16, 123456789.123456789), json_set('[]','$[0]',0.1+0.2), hex(jsonb_array(0.1+0.2)), hex(jsonb_set('[]','$[0]',0.1+0.2))
json_array(1e-5, 1e-4, 123e-20, 5e-324, -0.0, 0.0, 2e15, 4.5e15, 0.0/0.0)
json_array('a"b', 'x@BS@y', char(1,10,13,9,8,12,31,127), char(0x2028), 'é'), hex(jsonb_array('a"b')), hex(jsonb_set('[]','$[0]','a"b'))
json_quote('a"b'), json_quote(1), json_quote(1.5), json_quote(null), json_quote(json('[1]')), json_quote(x'00'), json_quote(1e999), json_quote(-1e999)
hex(jsonb_array(1e999)), hex(jsonb_set('[]','$[0]',1e999)), json_set('[]','$[0]',-1e999)
length(json_extract('"a@BS@u0000b"','$')), hex(json_extract('"a@BS@u0000b"','$')), hex(json_extract('"@BS@ud800x"','$')), hex(json_extract('"@BS@ud83d@BS@ude00"','$')), hex(json_extract('"@BS@x41@BS@0@BS@v"','$'))
hex(json_extract('"a@BS@' || char(10) || '"', '$')), hex(json_extract('"@BS@u12zz"', '$')), json_extract('"a@BS@/b"','$')
json_array('a' || char(0) || 'b'), json_quote('a' || char(0) || 'b'), json_object('a' || char(0) || 'b', 1)
json_extract('[0x10000000000000000]','$[0]')
json_object('a',1,'b','x','c',json('[1]'),'d',null), json_object(), hex(jsonb_object('a',1)), json_array(), json_array(null), json_array(x'00'), json_array(x'01'), hex(jsonb_array(1,'a'))
json_object(json('"x"'), 1), json_object('k', x'00')
json_object('a')
json_object(1,1)
json_object(null, 1)
json_object('k', x'ff')
json_array(x'ff')
json_quote(x'ff')
json_type('1'), json_type('[]'), json_type('{}','$.a'), json_type('{"a":0x1}','$.a'), json_type('{"a":.5}','$.a'), json_type('[1]', null), json_type(null), json_type(json_quote('a'))
json_type('[1,1.5,"a",null,true,false,[],{}]','$[0]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[1]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[2]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[3]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[4]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[5]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[6]'), json_type('[1,1.5,"a",null,true,false,[],{}]','$[7]')
json_type('[1]','x')
json_valid('{a:1}'), json_valid('{a:1}',1), json_valid('{a:1}',2), json_valid('[1]',4), json_valid(jsonb('[1]'),4), json_valid(jsonb('[1]'),1), json_valid(null), json_valid('',1),json_valid(jsonb('[1]'),8)
json_valid(x'2a2271', 8), json_valid(x'2a2271', 4), json_valid(x'1300', 8), json_valid(x'1300', 4), json_valid(x'13', 4), json_valid('[1]', 6), json_valid('{a:1}', 9), json_valid(x'5b315d', 1), json_valid(x'5b315d', 4), json_valid(x'5b315d', 5), json_valid(x'6b5b315d', 5), json_valid(123, 1), json_valid(1.5), json_valid('[1]', 3.9)
json_valid('[1]',0)
json_valid('[1]',16)
json_valid('[1]','abc')
json_valid('[1]', null)
json_valid(null, 0)
json_array_length('[1,2]'), json_array_length('{"a":[1]}'), json_array_length('{"a":[1]}','$.a'), json_array_length('{"a":[1]}','$.b'), json_array_length('[1]',null), json_array_length(null), json_array_length(x'5b312c325d')
json_array_length('[1]', 'a')
json_error_position('[1] x'), json_error_position('[1,'), json_error_position(''), json_error_position(null), json_error_position('[1]'), json_error_position('{"a":}'), json_error_position('  é x')
json_error_position('[1]    x'), json_error_position('[1] /* c */ x'),json_error_position('[1] // c'), json_error_position('{"a" 1}'), json_error_position('{"a":1 "b":2}'), json_error_position('[1 2]'), json_error_position('[1,,2]'), json_error_position('{a b:1}')
json_error_position('tru'), json_error_position('"abc'), json_error_position('[1'), json_error_position('{'), json_error_position('-'), json_error_position('01'), json_error_position('1.'), json_error_position('1e'), json_error_position('0x'), json_error_position('[-x]'), json_error_position('[.e1]')
json_error_position(x'00'), json_error_position(x'13'), json_error_position(x'1331'), json_error_position(jsonb('[1,2]')), json_error_position(x'2b1331'), json_error_position(x'6b13311332'), json_error_position(x'5b'), json_error_position(x'37612262'), json_error_position(x'338a3132')
json_error_position('[1 [x]'), json_error_position('{"a" /**/ [x]}'), json_error_position('{"a" [1 2]}'), json_error_position('{"a" [1]}'), json_error_position('{"a":1 /**/ x}'), json_error_position('{  '), json_error_position('   '), json_error_position('{true:1}'), json_error_position('{nan:1}')
json_error_position('[-infx]'), json_error_position('[infx]'), json_error_position('[-infinityx]'), json_error_position('[1' || char(0) || ']')
json_pretty('{"a":[1,2,{"b":[]}],"c":{}}'), json_pretty('[1,[2]]', '--'), json_pretty('[1]', null), json_pretty('[1]', ''), json_pretty('5'), json_array(json_pretty('[1]')), json_pretty('[1]',3), json_pretty(null)
json_pretty('[1')
"#,
        ),
    );
}

#[test]
fn paths_and_operators() {
    agree(
        "paths",
        &[],
        &lines(
            r#"
json_extract('{"a":1}'), json_extract('{"a":1}','$.b'), json_extract('{"a":1}','$.a','$.b'), json_extract('{"a":[1,2]}','$.a','$.a[0]')
json_extract('[1,2,3]','$[#-1]'), json_extract('[1,2,3]','$[#-3]'), json_extract('[1,2,3]','$[#-4]'), json_extract('[1,2,3]','$[#]')
json_extract('{"a.b":1,"a":{"b":2}}','$."a.b"', '$.a.b', '$.a."b"'), json_extract('{"":3}','$.""'), json_extract('{"a":1}','$[')
json_extract('{"a@BS@"b":1, "c":{"d":2}}','$."a@BS@"b"'), json_extract('{"a@BS@u0062":1}','$.ab'), json_extract('{"ab":1}','$."a@BS@u0062"'), json_extract('{"a@BS@"b":1}','$."a"b"')
json_extract('[1]', null), json_extract(null, '$'), json_extract('[1]', '$', null), json_extract('5', '$[x]'), json_extract('5', '$[0]'), json_extract('{}', '$[0')
json_extract('{"a":1}', '$.a' || char(0) || 'zz'), json_extract('[1,2]', x'245b315d'), json_extract(cast('{"a":2}' as blob), '$.a')
json_extract('[[1]]','$[0]'), json_array(json_extract('[[1]]','$[0]','$[0]')), json_array(jsonb_extract('[[1]]','$[0]')), hex(jsonb_extract('[[1]]','$[0]')), hex(jsonb_extract('[[1]]','$[0]', '$[0][0]')), jsonb_extract('[[1]]','$[0][0]')
json_extract('{"a":1}','a')
json_extract('[1,2]','$[1]x')
json_extract('{"a":1}','$.')
json_extract('{"a":1}','$a')
json_extract('{"a":1}','$.a.')
json_extract('[1]','$[0')
json_extract('{"a":1}', '$ .a')
json_extract('[1,2]','$[ 1]')
json_extract('{"a":1}', 12)
json_extract('{"a":5}', '$."b')
json_extract('["' || char(0) || '"]', '$[0]')
'{"a":{"b":3}}' -> '$.a', '{"a":{"b":3}}' ->> '$.a.b', '[1,2,3]' -> 1, '[1,2,3]' ->> -1, '{"a b":1}' ->> 'a b', '{"a":1}' -> 'a', '[1,[2]]' -> '[1]', '{"1.5":7}' ->> 1.5
'[1,2,3]' -> '1', '{"1":5}' -> '1', '{"x":[5]}' -> 'x' -> 0, '{"x":"y"}' -> 'x' || 'z', 1 + '[5]' ->> 0, '[5]' ->> 0 * 2
'{"a":1}' -> null, null -> '$', '{"a":"x"}' -> 'a', typeof('{"a":"x"}' ->> 'a'), '{"a":[1]}' ->> 'a', '{"a_b":1}' -> 'a_b', '{"a$":1}' -> 'a$'
'[1,2,3]' -> '#-1', '[1,2,3]' -> '[#-1]', '[1,2,3]' -> -4, '[1,2]' -> '$[#]', '[1,2]' -> '$[#-0]', '[1]' -> 0.0, '[1,2]' -> 1.0, '{"0.0":3}' -> 0.0, '{"a":3}' -> 'A'
-'[5]' ->> 0, '[5]' ->> 0 = 5, '[5]' -> 0 = '5', '[5]' ->> 0 + 1, '["a"]' ->> 0 collate nocase = 'A', '["a"]' ->> 0 || 'b', ~ '[5]' ->> 0
'[[1,2]]' -> 0 -> 1, '[[1,2]]' -> 0 ->> 1, '{"a":2}' ->> 'a' * 3, '[1]' -> '$' -> '$', '[1]' ->> 0 between 0 and 2, '[1]' -> 0 is not null, '["x"]' ->> 0 like 'X'
'[1,2]'->1, '[1,2]'->>1, '{"a":{"b":2}}'->'a'->>'b', json_array(1) ->> 0 ->> 0
'x' || '["a"]' ->> 0
'[1]' -> x'00'
!SELECT '[1]' - > 0
"#,
        ),
    );
}

#[test]
fn edits_patch_and_overwrite() {
    agree(
        "edits",
        &[],
        &lines(
            r#"
json_set('{"a":1}','$.b',2,'$.c.d',3,'$.a',json('[9]')), json_insert('{"a":1}','$.a',2,'$.e[0]',5), json_replace('{"a":1}','$.a',2,'$.z',3)
json_set('[1,2]','$[#]',3,'$[#]',4), json_set('[1,2]','$[5]',3), json_insert('[1,2]','$[2]',3), json_set('[1]','$[0].x',1), json_set('{}','$.a[1]',1), json_set('{}','$.a[0][0]',1), json_set('{}','$.a.b[#]',1)
json_set('[1]','$',2), json_insert('[1]','$',2), json_replace('[1]','$',json('{}')), json_set('[1]', null, 2), json_set('[1]','$[0]', null), json_set('[1]','$[0]',x'00'), json_set('[1]','$[0]',jsonb('{"a":1}'))
json_insert('{"a":1}', '$.a.b', 2), json_set('{"a":1}', '$.a.b', 2), json_set('{"a":[1]}', '$.a[#].b', 2), json_set('5', '$.a', 3), json_set('{"a":1, "a":2}', '$.a', 3), json_set('{"a":1}', '$.a' || char(0) || '.b', 5)
json_remove('[1,2,3]','$[1]','$[0]'), json_remove('{"a":1,"b":2}','$.a'), json_remove('[1]','$'), json_remove('[1]',null), json_remove('[1]','$[5]'), json_remove('[1,2,3]','$[#-1]'), json_remove('{"a":{"b":1}}','$.a.b')
json_array_insert('[1,2]','$[0]',0,'$[#]',9), json_array_insert('{"a":[1]}','$.a[0]',0), json_array_insert('[1]','$[5]',1), json_array_insert('[1]','$',0), json_array_insert('[1,2]','$[1]',json('[9]'))
json_array_insert('{}','$.b[0]',1), json_array_insert('{"b":[]}','$.b[0]',1), json_array_insert('[[1]]','$[0][0]',0,'$[0][#]',2), json_array_insert('[1]','$[#-1]',0), json_array_insert('[1]','$[0].a',1)
json_array_insert('{}','$.a[1]',1), json_array_insert('[]','$[1].k',1), json_array_insert('[]','$[1]',1), json_array_insert('{}','$.a.b[0]',1), json_array_insert('[]','$[0][0]',1), json_array_insert('[]','$[#-1]',1), json_array_insert('{}','$.a[#-1]',1), json_array_insert('[[1]]','$[0][5]',1)
json_array_insert('{"a":1}','$.a',0)
json_array_insert('{"a":1}','$.b',0)
json_array_insert('{}','$.a[1].k',1)
json_array_insert('{}','$.a[#-1].x',1)
json_array_insert('[[1]]','$[1].x',1)
json_array_insert('[1]','$[0]')
json_set('[1]','x',2)
json_set('[1]','$[0]')
json_set('[1]','$[0]',x'ff')
json_replace('[1]','$')
jsonb_insert('[1]','$')
json_set('[]', '$[0', 2)
json_set('{}', '$.a.', 2)
json_set('{}', '$.a', 2, '$.', 3)
json_set('{"a":5}', '$."a', 3)
json_remove('[1]','x')
json_remove(), json_set(), json_insert(), json_replace(), json_array_insert(), json_insert('[1]')
hex(jsonb_set('[1]','$[0]','abc')), hex(jsonb_set('{}','$.k','v')), hex(jsonb_set('{}','$."k@BS@"q"','v')), hex(jsonb_insert('[]','$[0]',json('{"x":1}'))), hex(jsonb_remove(jsonb('[1,2]'),'$[0]'))
hex(jsonb_set('[12345]','$[0]',1)), hex(jsonb_set('[1,2]','$',55)), hex(jsonb_set('[1,2]','$',5)), hex(jsonb_insert(x'cb021331','$[#]',2)), hex(jsonb_set(x'cb021331','$[0]',2)), hex(jsonb_set('[123456789]','$[0]',1))
hex(jsonb_set('[123456789012]','$[0]',1)), hex(jsonb_set('{"a":"xxxxxxxxxxxxxxx"}','$.a','y')), hex(jsonb_replace('[null]','$[0]',1)), hex(jsonb_set('["abcdefgh"]','$[0]',json('[]'))), hex(jsonb_set('[1234]','$[0]',null)), hex(jsonb_set('[[1,2,3]]','$[0]',json('[]')))
hex(jsonb_remove('[1,2,3]','$[1]')), hex(jsonb_remove(x'cb06133113321333','$[1]')), hex(jsonb_set(x'cb06133113321333','$[1]',9)), hex(jsonb(x'cb06133113321333')), hex(jsonb_set(x'cb06133113321333','$[1]',1234567))
json_patch('{"a":1,"b":{"c":2,"d":3}}','{"b":{"c":null,"e":4},"f":[1]}'), json_patch('[1]','{"a":1}'), json_patch('{"a":1}','[2]'), json_patch('{"a":1}','null'), json_patch('1','{"a":{"b":null,"c":1}}'), hex(jsonb_patch('{"a":1}','{"b":2}'))
json_patch('{"a":1}', '{"a":{"b":{"c":null}}}'), json_patch('{"a":"xyz"}','{"a":{"q":1}}'), json_patch('{"a":1}', '{"x":1,"x":{"y":2}}'), json_patch(null, '{}'), json_patch('{}', null)
hex(jsonb_patch('{"a":12345}','{"a":1}')), hex(jsonb_patch('{"a":"xx"}','{"a":{"b":1}}')), hex(jsonb_patch('{"a":12}','{"a":{}}')), hex(jsonb_patch('[1,2,3]','{"a":1}')), hex(jsonb_patch('{"a":[1,2]}','{"a":{"b":null}}'))
json_patch('{', '{}')
json_patch('{}', '{')
"#,
        ),
    );
}

#[test]
fn subtype_rules() {
    agree(
        "subtype",
        &["CREATE TABLE t(a)", "INSERT INTO t VALUES (json('[1]'))"],
        &lines(
            r#"
json_array((select json('[1]'))), json_array(x) from (select json('[1]') as x)
json_array(coalesce(json('[1]'),1)), json_array(case when 1 then json('[1]') end), json_array(ifnull(json('[1]'),1)), json_array(nullif(json('[1]'),1)), json_array(iif(1,json('[1]'),0)), json_array(+json('[1]')), json_array(json('[1]') collate nocase), json_array(likely(json('[1]')))
json_array(min(json('[1]'))), json_array(max(json('[1]'),json('[2]'))), json_array(cast(json('[1]') as text)), json_array(json('[1]') || ''), json_array(upper(json('[1]')))
json_array(cast(json('1') as integer)), json_array(substr(json('[1]'),1)), json_array(trim(json('[1]'))), json_array(coalesce(null, json('[1]'))), json_array(case 1 when 1 then json('[1]') else 2 end), json_array(case when 0 then 1 else json('[1]') end)
json_array(min(json('[1]'), json('[2]'))), json_array(iif(0, 1, json('[3]'))), json_array(nullif(json('[3]'), 'x')), json_array(unlikely(json('[3]'))), json_array(likelihood(json('[3]'), 0.5)), json_array((json('[3]'))), json_array(printf('%s', json('[3]')))
json_array(json('[3]') -> '$'), json_array(json('"x"')), json_array(json_quote(json_quote('x'))), json_object('a', json_object('b', 1)), json_array(json_array(1,2), '[3]'), json_array(jsonb('[1]')), json_array(jsonb_array(1))
json_array(json_object('a',1), json_set('[]','$[0]',1), json_patch('{}','{}'), json_remove('[1]'), json_insert('[1]'), json_quote('x')), json_array(json_type('[1]'), json_array_length('[1]'), json_valid('[1]'))
json_array('[1]' -> 0, '[[1]]' -> 0, '[[1]]' ->> 0, json_extract('[[1]]','$[0]'), json_extract('[1]','$[0]'))
json_array(a), json_array(json(a)) from t
json_set('[]', '$[0]', json('{"a":1}'), '$[1]', '{"a":1}', '$[2]', json_extract('[[2]]', '$[0]'), '$[3]', '[[2]]' ->> 0)
json_array(json_group_array(x)) from (select 1 x)
json_array(json_group_array(x) over ()) from (select 1 x)
json_array((select json_group_array(1)))
json_array(cast(json('[1]') as blob))
"#,
        ),
    );
}

#[test]
fn aggregates_and_windows() {
    agree(
        "aggregates",
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, g, k, v)",
            "INSERT INTO t VALUES (1, 1, 'a', 1), (2, 1, 'b', 'x'), (3, 2, null, 2.5), (4, 2, 'a', null), (5, 3, 'c', '[1]')",
        ],
        &lines(
            r#"
json_group_array(v), json_group_object(k, v), hex(jsonb_group_array(v)), hex(jsonb_group_object(k, v)) from t
g, json_group_array(v), json_group_object(k, id) from t group by g order by g
json_group_array(distinct g), json_group_array(distinct k) from t
json_group_array(distinct x) from (select null x union all select 1 union all select null)
json_group_array(distinct x) from (select 'a' x union all select 'A')
json_group_array(distinct x collate nocase) from (select 'a' x union all select 'A')
json_group_array(distinct x order by x desc) from (select 2 x union all select 1 union all select 2)
json_group_array(distinct x) from (select 1 x union all select 1.0 union all select 1)
json_group_object(x, y order by y) from (select 'a' x, 2 y union all select 'b', 1)
json_group_array(v order by id desc), json_group_array(id) filter (where g = 1) from t
json_group_array(json(v)) from t where json_valid(v)
json_group_array(x) from (select json('[1]') x)
json_group_array(1) from t where 0
json_group_object('a', 1) from t where 0
typeof(jsonb_group_array(1)), hex(jsonb_group_array(1)) from t where 0
json_group_object(x, 1) from (select null x union all select 'a')
json_group_object(x, y) from (select null x, x'ff' y union all select 'a', 2)
json_group_object(x, 1) from (select 'a' || char(0) || 'b' x)
json_group_object(x, 1) from (select 5 x union all select 1.5)
json_object('a', json_group_array(x)) from (select 1 x union all select 2)
!SELECT id, json_group_array(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id
!SELECT id, json_group_object(k, id) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id
!SELECT id, json_array(json_group_array(v) OVER (ORDER BY id)) FROM t ORDER BY id
!SELECT id, json_group_array(id) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING), json_group_array(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW) FROM t ORDER BY id
!SELECT id, hex(jsonb_group_array(id) OVER (ORDER BY id)) FROM t ORDER BY id
!SELECT json_group_array(value) OVER () FROM json_each('[1,2]')
json_group_array(x) from (select x'ff' x)
json_group_array()
json_group_object(1)
json_group_object(distinct x, x) from (select 1 x)
json_array(1) over ()
"#,
        ),
    );
}

#[test]
fn table_valued_functions() {
    agree(
        "tvf",
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a)",
            r#"INSERT INTO t VALUES (1, '[1,2]'), (2, '{"x":3}'), (3, null), (4, '[]'), (5, '[[1,2],[3]]'), (6, 'bad')"#,
        ],
        &lines(
            r#"
!SELECT * FROM json_each('{"a":1,"b":[2,3],"c d":{"e":null}, "":4, "1x":5, "x\"y":6}')
!SELECT *, json, root FROM json_each('[1,[2,3]]', '$[1]')
!SELECT *, json, root FROM json_each('{"a":{"b":1}}', '$.a')
!SELECT *, json, root FROM json_each('5')
!SELECT *, json, root FROM json_each('{"a":5}', '$.a')
!SELECT *, json, root FROM json_tree('{"a":5}', '$.a')
!SELECT *, json, root FROM json_tree('[[1,{"x":[2]}],3]')
!SELECT *, json, root FROM json_tree('[[1,{"x":[2]}],3]','$[0]')
!SELECT *, json, root FROM json_tree('{"a":[1,{"x":[2]}]}','$.a[1]')
!SELECT key,value,type,atom,id,parent,fullkey,path,json,root FROM json_tree('{"a":[1,{"b c":null}]}')
!SELECT * FROM json_tree('{"a":{"b":{"c":[1,{"d":2}]}}}')
!SELECT fullkey, path FROM json_tree('{"x y":{"1":[{"":3}]}}')
!SELECT * FROM json_each('[1,2]', '$[#-1]')
!SELECT * FROM json_each('{"a":[1]}', '$.a[0]')
!SELECT * FROM json_tree('{"a":[1]}', '$.a[0]')
!SELECT * FROM json_tree('[[1,2]]', '$[0][1]')
!SELECT * FROM json_tree('5', '$')
!SELECT * FROM json_each('{"a":1}', '$')
!SELECT * FROM json_each('[1]','x')
!SELECT * FROM json_each('[1')
!SELECT * FROM json_each('[1]',null)
!SELECT count(*) FROM json_each(null)
!SELECT count(*) FROM json_each('')
!SELECT count(*) FROM json_each(5)
!SELECT key FROM json_each('{"a":1}', '$.a')
!SELECT count(*) FROM json_each('[1]', '$[')
!SELECT key FROM json_each('[5,6]') WHERE root = '$'
!SELECT json_each.key, json_each.value FROM json_each('[5,6]')
!SELECT * FROM json_each('[5,6]') AS e ORDER BY e.key DESC
!SELECT * FROM json_each('[1]', '$', 3)
!SELECT * FROM json_each()
!SELECT * FROM json_each('[1]') JOIN json_tree('[2]')
!SELECT a.value, b.value FROM json_each('[1,2]') a, json_each('[3,4]') b
!SELECT typeof(key), typeof(value), typeof(id), typeof(parent), typeof(atom) FROM json_each('[1.5]')
!SELECT * FROM json_each(jsonb('[1,{"a":2}]'))
!SELECT key, hex(value), json FROM jsonb_each('[1,{"a":2}]')
!SELECT key, hex(value), hex(json) FROM jsonb_tree(jsonb('[1,{"a":2}]'))
!SELECT value, typeof(value) FROM json_each('[true,false,null,"x",1.0,0x10]')
!SELECT rowid, * FROM json_each('[5,6]')
!SELECT json_array(value) FROM json_each('[[1],{"a":2},3,"x"]')
!SELECT json_group_array(value) FROM json_each('[[1],[2],3]')
!SELECT * FROM json_each('{"a":{"b":[1,2]}}', '$.a.b') WHERE value > 1
!SELECT sum(value) FROM json_each('[1,2,3,4]') GROUP BY value % 2 ORDER BY 1
!SELECT * FROM json_each('{"a":1}') a JOIN json_each('{"a":2}') b USING (key)
!SELECT value FROM json_each(json_array(1, 2, 3)) LIMIT 2
!SELECT j.* FROM json_each('[1]') j
!SELECT count(*) FROM json_each('[1,2]') a, json_each('[1,2,3]') b WHERE a.value = b.value
!SELECT fullkey FROM json_tree('{"a":[1,{"b":2}]}') WHERE atom IS NOT NULL
!SELECT key, value FROM json_each(json_object('a', 1, 'b', json_array(2, 3)))
!SELECT * FROM json_each('[1,2]') WHERE key IN (SELECT 1)
!SELECT t.id, j.key, j.value FROM t, json_each(t.a) j WHERE t.id < 6 ORDER BY t.id, j.id
!SELECT t.id, key, value FROM t LEFT JOIN json_each(t.a) WHERE json_valid(t.a) OR t.a IS NULL ORDER BY t.id, json_each.id
!SELECT id, (SELECT count(*) FROM json_each(t.a)) FROM t WHERE id < 6 ORDER BY id
!SELECT t.id, x.value, y.value FROM t, json_each(t.a) x, json_each(x.value) y WHERE t.id = 5
!SELECT id FROM t WHERE json_valid(a) AND EXISTS (SELECT 1 FROM json_each(t.a) WHERE value = 3)
!SELECT id FROM t WHERE json_valid(a) AND 2 IN (SELECT value FROM json_each(t.a))
!SELECT t.id, j.value FROM t, json_each(t.a) j WHERE json_valid(t.a) ORDER BY 1, 2
!SELECT t.id, j.value FROM t JOIN json_each(t.a) j ON json_valid(t.a) ORDER BY 1, 2
!SELECT t.id, count(j.key) FROM t LEFT JOIN json_each(t.a) j WHERE json_valid(t.a) GROUP BY t.id ORDER BY 1
!SELECT value, count(*) FROM t, json_each(t.a) WHERE json_valid(t.a) GROUP BY value ORDER BY 1
!SELECT t.id, j.value FROM t, json_each(t.a) j
!SELECT t.id, j.value FROM t LEFT JOIN json_each(t.a) j ON json_valid(t.a)
"#,
        ),
    );
}

#[test]
fn schema_and_write_paths() {
    let stmts = lines(
        r#"
!CREATE TABLE g(j, a AS (j ->> '$.a'), b AS (json_extract(j, '$.b')) STORED)
!INSERT INTO g(j) VALUES ('{"a":1,"b":[2]}'), ('{"a":"x"}')
!SELECT a, b, typeof(b) FROM g
!CREATE TABLE c(j CHECK (json_valid(j)))
!INSERT INTO c VALUES ('[1]')
!INSERT INTO c VALUES ('[1')
!SELECT * FROM c
!CREATE TABLE d(j DEFAULT (json_array(1,2)))
!INSERT INTO d DEFAULT VALUES
!SELECT j FROM d
!CREATE TABLE u(a)
!INSERT INTO u VALUES ('{"k":[1,2]}')
!UPDATE u SET a = json_set(a, '$.k[#]', 3)
!UPDATE u SET a = (SELECT json_group_array(value * 2) FROM json_each(u.a, '$.k'))
!SELECT a FROM u
!INSERT INTO u SELECT value FROM json_each('[1,"x",null,2.5]')
!SELECT a, typeof(a) FROM u
!DELETE FROM u WHERE json_valid(a) AND json_array_length(a) = 3
!SELECT count(*) FROM u
!CREATE VIEW v AS SELECT u.a -> '$' x, json_each.value v FROM u, json_each(json_array(u.a))
!SELECT * FROM v
!WITH q(x) AS (SELECT json_object('a', 1)) SELECT x -> 'a', json_type(x) FROM q
!CREATE TABLE w(id INTEGER PRIMARY KEY, tags)
!INSERT INTO w VALUES (1, '["a","b"]'), (2, '["b"]')
!SELECT value, json_group_array(id) FROM w, json_each(w.tags) GROUP BY value ORDER BY value
!UPDATE w SET tags = json_insert(tags, '$[#]', id) WHERE id IN (SELECT w2.id FROM w w2, json_each(w2.tags) WHERE value = 'b')
!SELECT * FROM w
"#,
    );
    agree("schema", &[], &stmts);
}

#[test]
fn nesting_limits() {
    fn node(t: u8, payload: &[u8]) -> Vec<u8> {
        let n = payload.len();
        let mut h = if n <= 11 {
            vec![t | ((n as u8) << 4)]
        } else if n <= 0xff {
            vec![t | 0xc0, n as u8]
        } else if n <= 0xffff {
            vec![t | 0xd0, (n >> 8) as u8, n as u8]
        } else {
            let mut v = vec![t | 0xe0];
            v.extend_from_slice(&(n as u32).to_be_bytes());
            v
        };
        h.extend_from_slice(payload);
        h
    }
    let hex = |b: &[u8]| b.iter().map(|c| format!("{c:02x}")).collect::<String>();
    let mut stmts = Vec::new();
    for n in [999usize, 1000, 1001] {
        let mut arr = vec![0x13, 0x31];
        let mut empty = vec![0x0b];
        let mut obj = vec![0x13, 0x31];
        for k in 0..n {
            arr = node(11, &arr);
            if k + 1 < n {
                empty = node(11, &empty);
            }
            let mut p = vec![0x17, b'a'];
            p.extend_from_slice(&obj);
            obj = node(12, &p);
        }
        for (b, step) in [(&arr, "[0]"), (&empty, "[0]"), (&obj, ".a")] {
            let a = format!("x'{}'", hex(b));
            let p = format!("'${}'", step.repeat(n));
            let q = format!("'${}'", step.repeat(n - 1));
            for t in [
                "length(json(A))",
                "length(json_pretty(A))",
                "json_extract(A,P)",
                "json_extract(A,Q) IS NOT NULL",
                "json_valid(A,8)",
                "json_error_position(A)",
                "length(json_set(A,P,2))",
                "length(json_remove(A,P))",
                "length(json_patch('{}',A))",
                "length(json_patch(A,'{}'))",
                "json_type(A,P)",
                "length(json_array(A))",
                "length(jsonb(A))",
                "length(json_insert(A,'$[9]',1))",
                "length(jsonb_patch(A,A))",
                "length(jsonb_array(A))",
                "length(json_set('[]', P, 1))",
                "json_array_length(A, Q)",
                "(SELECT count(*) FROM json_tree(A))",
                "(SELECT max(length(value)) FROM json_tree(A))",
                "(SELECT count(*) FROM json_each(A, Q))",
            ] {
                stmts.push(format!(
                    "SELECT {}",
                    t.replace('A', &a).replace('P', &p).replace('Q', &q)
                ));
            }
        }
        let open = "[".repeat(n);
        let close = "]".repeat(n);
        stmts.push(format!("SELECT json_error_position('{open}1{close}')"));
        stmts.push(format!("SELECT length(json('{open}1{close}'))"));
        let o = "{\"a\":".repeat(n);
        let c = "}".repeat(n);
        stmts.push(format!("SELECT json_error_position('{o}1{c}')"));
        stmts.push(format!(
            "SELECT length(jsonb_array(json('{open}1{close}')))"
        ));
    }
    agree("depth", &[], &stmts);
}

#[test]
fn large_documents() {
    agree(
        "large",
        &[
            "CREATE TABLE n(i INTEGER PRIMARY KEY, s)",
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 3000) INSERT INTO n SELECT i, printf('k%d', i) FROM c",
        ],
        &lines(
            r#"
length(json_group_array(i)), length(json_group_object(s, i)), length(jsonb_group_array(s)) from n
(select count(*) from json_each((select json_group_array(i) from n)))
(select sum(value) from json_each((select json_group_array(i) from n)))
(select count(*) from json_tree((select json_group_object(s, json_array(i, s)) from n)))
json_extract((select json_group_object(s, i) from n), '$.k2999')
length(json_set((select json_group_array(i) from n), '$[#]', 1, '$[1500]', json('[1,2]')))
length(json_remove((select json_group_array(i) from n), '$[0]', '$[#-1]', '$[1000]'))
length(json_patch((select json_group_object(s, i) from n), (select json_group_object(s, null) from n where i % 2 = 0)))
length(json_pretty((select json_group_object(s, json_array(i)) from n)))
length(json(printf('"%.100000c"', 'x'))), hex(substr(jsonb(printf('"%.100000c"', 'x')), 1, 5))
json_array_length((select json_group_array(json_array(i)) from n))
"#,
        ),
    );
}

/// Review regression: documents of 50,000 to 100,000 elements stay
/// linear. Parsing text used to copy the whole blob each time a
/// container's header shrank (O(n²) in the containers: seconds per
/// call at 1 MB), and `json_each` / `json_tree` copied the input
/// document into every row's hidden `json` column, and the table's
/// row into every joined row (O(rows × size): gigabytes for a 100 KB
/// array, an allocation-failure abort beyond).
#[test]
fn large_documents_scale() {
    let start = std::time::Instant::now();
    agree(
        "large-scale",
        &[
            "CREATE TABLE d(j)",
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 100000) INSERT INTO d SELECT json_group_array(json_object('a', i)) FROM c",
            "CREATE TABLE e(j)",
            "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < 20000) INSERT INTO e SELECT json_group_array(i) FROM c",
            "CREATE TABLE f(j)",
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 50000) INSERT INTO f SELECT json_group_object('k' || i, json_array(i, i + 1)) FROM c",
        ],
        &lines(
            r#"
json_valid(j), length(j), length(jsonb(j)), j ->> '$[99999].a' from d
json_array_length(j), json_extract(j, '$[50000]') from d
count(*), sum(value) from e, json_each(e.j)
count(*) from json_each((select j from e))
count(*), sum(key), min(json), max(length(json)) from e, json_each(e.j) where key % 5000 = 0
count(*), sum(value ->> 1) from f, json_tree(f.j) where type = 'array'
count(*), sum(length(fullkey)) from f, json_tree(f.j)
j ->> '$.k49999[1]', length(json_set(j, '$.k7[0]', 99)), length(json_pretty(j)) from f
count(*) from d, json_each(d.j) where value ->> 'a' > 99990
"#,
        ),
    );
    let took = start.elapsed();
    assert!(
        took < std::time::Duration::from_secs(120),
        "large documents took {took:?}"
    );
}

#[test]
fn errors_are_remedy_named() {
    let cases: &[(&str, SchemaKind)] = &[
        ("SELECT json('[')", SchemaKind::JsonMalformed),
        ("SELECT json_extract('[1]', 'x')", SchemaKind::JsonBadPath),
        ("SELECT json_array(x'ff')", SchemaKind::JsonBlobValue),
        ("SELECT json_object('a')", SchemaKind::JsonObjectOddArgs),
        ("SELECT json_object(1, 2)", SchemaKind::JsonObjectLabel),
        ("SELECT json_set('[]', '$[0]')", SchemaKind::JsonOddArgs),
        ("SELECT json_valid('[]', 0)", SchemaKind::JsonValidFlags),
        (
            "SELECT json_array_insert('{}', '$.a', 1)",
            SchemaKind::JsonNotArrayElement,
        ),
        (
            "SELECT json_set('[]', '$' || printf('%.1001c', '#'), 1)",
            SchemaKind::JsonBadPath,
        ),
        ("SELECT json()", SchemaKind::WrongArgumentCount),
        (
            "SELECT * FROM json_each('[1]', '$', 3)",
            SchemaKind::WrongArgumentCount,
        ),
    ];
    let stmts: Vec<String> = cases.iter().map(|(s, _)| s.to_string()).collect();
    let got = agree("errors", &[], &stmts);
    for ((sql, kind), g) in cases.iter().zip(got.iter()) {
        match g {
            Err(Error::Schema { kind: k }) => assert_eq!(k, kind, "{sql}"),
            other => panic!("{sql}: expected {kind:?}, got {other:?}"),
        }
    }
    // The depth errors, on a path and on a rendering.
    let deep_path = format!("'${}'", "[0]".repeat(1001));
    let deep = format!("SELECT json_set('[]', {deep_path}, 1)");
    let render = format!(
        "SELECT json(jsonb_set('[]', '${}', jsonb_set('[]', '${}', 1)))",
        "[0]".repeat(600),
        "[0]".repeat(600)
    );
    let got = agree("depth errors", &[], &[deep.clone(), render.clone()]);
    assert!(
        matches!(
            got[0],
            Err(Error::Schema {
                kind: SchemaKind::JsonPathTooDeep
            })
        ),
        "{deep}"
    );
    assert!(
        matches!(
            got[1],
            Err(Error::Schema {
                kind: SchemaKind::JsonNestedTooDeep
            })
        ),
        "{render}"
    );
}

/// Regressions for what the corpora found during the unit (each
/// against the pin).
#[test]
fn regressions() {
    agree(
        "regressions",
        &[],
        &lines(
            r#"
json_array('[1]'), json_array(json('[1]')), json('[1]')
json_patch('[1]','{"a":1}'), hex(jsonb_patch('{"a":12}','{"a":{}}'))
json_array_insert('{"k":{},"c":[]}','$.""[1].k',x'00')
json_array_insert('{}','$.a[1].k',1)
(select group_concat(quote(atom),';') from json_tree(x'695c758c303431'))
json(x'162d')
json_error_position(x'37612262'), json_error_position(x'338a3132')
(select count(*) from json_each(x'37612262','$'))
json_set('[]','$[0]',x'37612262')
(select count(*) from json_tree(x'c4143078666666666666666666666666666666666666','$'))
(select count(*) from json_tree(x'3b8f1331'))
(select count(*) from json_each(x'3c17618f'))
(select fullkey||'|'||id from json_tree(x'7c136b8f00136b01'))
json_group_array(x) from (select 1 x)
json_type(1.5), json_type('1.5'), pi() > 3
case when x = 1 and json('x') then 1 else 2 end, iif(x = 1 and json('x'), 1, 2), iif(x = 0 or json('x'), 1, 2) from (select 0 x)
count(*) from (select 0 x) where x = 1 and abs(-9223372036854775808) > 0
count(*) from (select 0 x) where x = 1 and (x = 2 or json('[') is null)
count(*) from (select 0 x) where x = 1 and json_valid(x) and random() > 0
count(*) from (select 0 x union all select 1) where x = 0 or json('[' || x) is null
json_group_array(x) filter (where x > 0 and json(x) = 1) from (select 0 x union all select 1)
x = 1 AND json('x') from (select 0 x)
"#,
        ),
    );
}

/// Review regressions: TVF arguments reading a trigger's NEW / OLD,
/// the AND / OR / NOT jump code everywhere a condition is tested
/// (aggregate FILTER over a table, HAVING, CHECK, NOT, subquery
/// operands tested last, correlated-subquery WHERE terms after the
/// others), WHERE constant terms computed once up front (over no
/// rows too, after constant propagation, never for an OR operand or
/// an outer join's ON), and a compound scalar subquery's JSON
/// subtype.
#[test]
fn review_regressions() {
    agree(
        "review",
        &[
            "CREATE TABLE t(a, b)",
            "INSERT INTO t VALUES (1, 'x'), (2, '[1]')",
            "CREATE TABLE u(a)",
            "INSERT INTO u VALUES (1)",
            "CREATE TABLE e(a)",
            "CREATE TABLE ti(a INTEGER PRIMARY KEY, b)",
            "INSERT INTO ti VALUES (1, 'x'), (2, '[1]')",
            "CREATE TABLE tx(a, b)",
            "CREATE INDEX txa ON tx(a)",
            "INSERT INTO tx VALUES (1, 'x'), (2, '[1]')",
        ],
        &lines(
            r#"
count(*) filter (where json_valid(b) and b ->> 0 = 1) from t
json_group_array(a) filter (where json_valid(b) and b ->> 0 = 1) from t
sum(a) filter (where json_valid(b) and b ->> 0 = 1), (select 1) from t
a, count(*) filter (where not json_valid(b) or b ->> 0 = 1) over () from t
!SELECT a FROM t WHERE NOT (json_valid(b) AND b ->> 0 = 1)
!SELECT a FROM t WHERE NOT NOT (json_valid(b) AND b ->> 0 = 1)
!SELECT a FROM t WHERE NOT (NOT json_valid(b) OR b ->> 0 <> 1)
!SELECT a FROM t WHERE (json_valid(b) AND b ->> 0 = 1) IS NOT 0
!SELECT a FROM t GROUP BY a HAVING json_valid(max(b)) AND max(b) ->> 0 = 1
!SELECT a FROM t GROUP BY a HAVING NOT json_valid(max(b)) OR max(b) ->> 0 = 1
!SELECT * FROM t WHERE EXISTS (SELECT 1 FROM json_each(t.b) WHERE value = 1) AND json_valid(b)
!SELECT * FROM t WHERE (SELECT count(*) FROM json_each(t.b)) AND json_valid(b)
!SELECT * FROM t WHERE (SELECT count(*) FROM json_each(t.b)) OR NOT json_valid(b)
!SELECT * FROM t WHERE b ->> 0 = (SELECT 1) AND json_valid(b)
!SELECT * FROM t WHERE b ->> 0 = (SELECT a FROM u WHERE u.a = t.a) AND json_valid(b)
!SELECT * FROM t WHERE t.a IN (SELECT key FROM json_each(t.b)) AND json_valid(b)
!SELECT * FROM t WHERE t.a NOT IN (SELECT key FROM json_each(t.b)) AND json_valid(b)
!SELECT * FROM t WHERE EXISTS (SELECT 1 FROM json_each(t.b)) AND a = 2
!SELECT * FROM t WHERE NOT (EXISTS (SELECT 1 FROM json_each(t.b)) AND json_valid(b))
!SELECT * FROM t, u WHERE (SELECT count(*) FROM json_each(t.b)) AND json_valid(t.b)
!SELECT * FROM t LEFT JOIN u ON json_valid(t.b) AND EXISTS (SELECT 1 FROM json_each(t.b))
!SELECT * FROM ti WHERE b -> 0 = 1 AND a = 2
!SELECT * FROM tx WHERE b -> 0 = 1 AND a = 2
!SELECT * FROM t WHERE b -> 0 = 1 AND a = 2
!SELECT * FROM u WHERE a = 1 OR json('[') IS NULL
!SELECT * FROM u WHERE a = 2 OR json('[') IS NULL
!SELECT * FROM u WHERE NOT (a = 1 OR json('[') IS NULL)
!SELECT * FROM e WHERE (a = 1 OR json('[') IS NULL)
!SELECT * FROM e WHERE a = 1 AND json('[') IS NULL
!SELECT * FROM e WHERE NOT (json('[') IS NULL)
!SELECT * FROM e WHERE 1 = 2 AND json('[') IS NULL
!SELECT * FROM e WHERE json('[') IS NULL AND 1 = 2
!SELECT * FROM u WHERE abs(-1) = 2 AND json('[') IS NULL
!SELECT * FROM e WHERE abs(-9223372036854775808) AND length('x') = 2
!SELECT * FROM e, u WHERE e.a = u.a AND json('[') IS NULL
!SELECT * FROM e JOIN u ON json('[') IS NULL
!SELECT * FROM e LEFT JOIN t ON json('[') IS NULL
!SELECT * FROM u LEFT JOIN t ON json('[') IS NULL
!SELECT * FROM u LEFT JOIN t ON t.a = 5 AND json('[') IS NULL
!SELECT * FROM u WHERE a = 2 AND (a = 1 OR json('[') IS NULL)
!SELECT * FROM e WHERE a = 2 AND (a = 1 OR json('[') IS NULL)
!SELECT * FROM u WHERE a = 1 AND json('[') IS NULL AND a = 2
!SELECT * FROM e WHERE a = 1 AND json('[') IS NULL AND a = 2
!SELECT * FROM u WHERE a = 1 AND (a <> 2 OR json('[') IS NULL)
!SELECT * FROM u WHERE a = 1 AND (a + 1 = 3 OR json('[') IS NULL)
!SELECT * FROM t WHERE a = 1 COLLATE nocase AND (a = 2 OR json('[') IS NULL)
!SELECT count(*) FROM e WHERE a = 1 AND json('[') IS NULL
!SELECT (SELECT 1 FROM e WHERE json('[') IS NULL) FROM u
!SELECT (SELECT 1 FROM e WHERE json('[') IS NULL) FROM e
count(*) filter (where a = 2 and json('[') is null) from u
case when a = 2 and json('[') is null then 1 end from u
iif(json_valid(b) and b ->> 0 = 1, 'y', 'n') from t
json_array((select json('[1]') union all select 2)), json_array((select json('[1]') union select 2))
json_array((select json('[1]') except select 2)), json_array((select json('[1]') intersect select json('[1]')))
json_array((select json('[1]') union all select 2 limit 1)), json_array((select 2 union all select json('[1]')))
json_array((select json_array(1) from u union all select 2)), json_array((values (json('[1]')) union all values (2)))
json_array((select json_quote('a') union all select null)), json_array((select json_group_array(1) union all select 2))
json_array((select '[1]' -> '$' union all select 2)), json_array((select '[1]' ->> '$' union all select 2))
json_array((select json('[1]') limit 1)), json_array((select b from t where a = 2 union all select 2))
"#,
        ),
    );
    // DML: the WHERE constant terms and the jump code; CHECK.
    for (tag, stmts) in [
        (
            "review-dml-empty",
            &[
                "DELETE FROM e WHERE a = 1 AND json('[') IS NULL",
                "UPDATE e SET a = 1 WHERE a = 1 AND json('[') IS NULL",
                "DELETE FROM u WHERE a = 2 AND json('[') IS NULL",
                "UPDATE u SET a = 1 WHERE a = 2 AND json('[') IS NULL",
                "SELECT * FROM u",
            ][..],
        ),
        (
            "review-dml-jump",
            &[
                "DELETE FROM t WHERE json_valid(b) AND b ->> 0 = 1",
                "SELECT * FROM t",
                "UPDATE t SET a = 9 WHERE NOT (json_valid(b) AND b ->> 0 = 1)",
                "UPDATE t SET a = 7 WHERE EXISTS (SELECT 1 FROM json_each(t.b)) AND json_valid(b)",
                "SELECT * FROM t",
            ][..],
        ),
        (
            "review-check",
            &[
                "CREATE TABLE c1(x, CHECK (NOT json_valid(x) OR x ->> 0 > 0))",
                "INSERT INTO c1 VALUES ('zz'), ('[5]')",
                "INSERT INTO c1 VALUES ('[0]')",
                "CREATE TABLE c2(x, CHECK (json_valid(x) AND x ->> 0 > 0))",
                "INSERT INTO c2 VALUES ('zz')",
                "CREATE TABLE c3(x, CHECK (x IS NULL OR json_valid(x) AND x ->> 0 > 0))",
                "INSERT INTO c3 VALUES (NULL), ('[3]')",
                "SELECT * FROM c1, c3",
            ][..],
        ),
    ] {
        let stmts: Vec<String> = stmts.iter().map(|s| s.to_string()).collect();
        agree(
            tag,
            &[
                "CREATE TABLE t(a, b)",
                "INSERT INTO t VALUES (1, 'x'), (2, '[1]')",
                "CREATE TABLE u(a)",
                "INSERT INTO u VALUES (1)",
                "CREATE TABLE e(a)",
            ],
            &stmts,
        );
    }
}

/// Review regression: a table-valued function's arguments read the
/// trigger's NEW / OLD row (they were left unbound: "no such
/// column" when the trigger fired), with only the sources before it
/// in scope, and ALTER TABLE RENAME COLUMN edits them.
#[test]
fn tvf_in_triggers() {
    let cases: &[&[&str]] = &[
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(a, k, v)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT new.a, key, value FROM json_each(new.a); END",
            "INSERT INTO t VALUES ('[1,2]'), ('{\"x\":3}')",
            "SELECT * FROM log",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(a, k)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.a, (SELECT group_concat(key) FROM json_each(new.a))); END",
            "INSERT INTO t VALUES ('{\"x\":1,\"y\":2}')",
            "SELECT * FROM log",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE k(k, n)",
            "INSERT INTO k VALUES (0, 0), (1, 0), (5, 0)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE k SET n = n + 1 WHERE k IN (SELECT key FROM json_each(new.a)); END",
            "INSERT INTO t VALUES ('[7,8]')",
            "SELECT * FROM k",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE k(k, n)",
            "INSERT INTO k VALUES ('x', 0), ('y', 0)",
            "CREATE TRIGGER tr AFTER UPDATE ON t BEGIN UPDATE k SET n = e.value FROM json_each(new.a) e WHERE e.key = k.k; END",
            "INSERT INTO t VALUES ('{}')",
            "UPDATE t SET a = '{\"x\":4}'",
            "SELECT * FROM k",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(n)",
            "CREATE TRIGGER tr BEFORE DELETE ON t BEGIN INSERT INTO log SELECT count(*) FROM json_tree(old.a); END",
            "INSERT INTO t VALUES ('[1,[2]]')",
            "DELETE FROM t",
            "SELECT * FROM log",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(x)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT value FROM json_each(new.a) AS new; END",
            "INSERT INTO t VALUES ('[1]')",
            "SELECT * FROM log",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(x)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT y.value FROM json_each(new.a) new, json_each(new.value) y; END",
            "INSERT INTO t VALUES ('[[1,2],[3]]')",
            "SELECT * FROM log",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(x)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log WITH c AS (SELECT value v FROM json_each(new.a)) SELECT sum(v) FROM c; END",
            "INSERT INTO t VALUES ('[1,5,9]')",
            "SELECT * FROM log",
        ],
        &[
            "CREATE TABLE t(a)",
            "CREATE TABLE log(a, k, v)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT new.a, key, value FROM json_each(new.a) WHERE new.a IS NOT NULL; END",
            "ALTER TABLE t RENAME COLUMN a TO b",
            "SELECT sql FROM sqlite_schema WHERE type = 'trigger'",
            "INSERT INTO t VALUES ('[3]')",
            "SELECT * FROM log",
        ],
    ];
    for (i, c) in cases.iter().enumerate() {
        let stmts: Vec<String> = c.iter().map(|s| s.to_string()).collect();
        agree(&format!("trigger-tvf-{i}"), &[], &stmts);
    }
}

// ---------------------------------------------------------------
// Generated corpora.

fn gen_str(r: &mut Rng) -> String {
    let parts: Vec<String> = vec![
        "a".into(),
        "b".into(),
        "x".into(),
        "\"".into(),
        "'".into(),
        format!("{BS}n"),
        format!("{BS}\""),
        format!("{BS}{BS}"),
        format!("{BS}u0041"),
        format!("{BS}ud83d{BS}ude00"),
        "é".into(),
        format!("{BS}x41"),
        format!("{BS}v"),
        format!("{BS}0"),
        " ".into(),
        format!("{BS}/"),
        format!("{BS}t"),
        "{".into(),
        "[".into(),
        ":".into(),
        ",".into(),
        format!("{BS}u00e9"),
        format!("{BS}'"),
    ];
    let n = r.below(6);
    (0..n)
        .map(|_| parts[r.below(parts.len())].clone())
        .collect()
}

fn gen_num(r: &mut Rng) -> &'static str {
    r.pick(&[
        "0",
        "1",
        "-1",
        "12",
        "1.5",
        "-0.5",
        "1e5",
        "1E-3",
        "0x1F",
        "-0x10",
        "+3",
        ".5",
        "5.",
        "Infinity",
        "-Infinity",
        "NaN",
        "123456789012345678901234",
        "9223372036854775807",
        "9223372036854775808",
        "-9223372036854775808",
        "0.0",
        "1.0e+300",
        "3.14159",
    ])
}

fn gen_ws(r: &mut Rng, j5: bool) -> &'static str {
    if j5 {
        r.pick(&["", "", " ", "/*c*/", "\t", "//x\n", " \u{a0}"])
    } else {
        r.pick(&["", " ", "", "\n"])
    }
}

fn gen_key(r: &mut Rng, j5: bool) -> String {
    if j5 && r.chance(30) {
        return r.pick(&["a", "b", "_x", "$y", "k1", "é"]).to_string();
    }
    let q = if j5 && r.chance(20) { "'" } else { "\"" };
    let k = r.pick(&["a", "b", "c", "a b", "", "1", "k"]);
    format!("{q}{k}{q}")
}

fn gen_json(r: &mut Rng, d: u32, j5: bool) -> String {
    let x = r.below(100);
    if d > 3 || x < 35 {
        let c = r.below(100);
        if c < 40 {
            return gen_num(r).to_string();
        }
        if c < 70 {
            let q = if j5 && r.chance(25) { "'" } else { "\"" };
            let mut s = gen_str(r);
            if q == "\"" {
                s = s.replace('"', &format!("{BS}\""));
            }
            return format!("{q}{s}{q}");
        }
        return r.pick(&["true", "false", "null"]).to_string();
    }
    if x < 65 {
        let n = r.below(5);
        let items: Vec<String> = (0..n)
            .map(|_| {
                let a = gen_ws(r, j5);
                let v = gen_json(r, d + 1, j5);
                let b = gen_ws(r, j5);
                format!("{a}{v}{b}")
            })
            .collect();
        let t = if j5 && n > 0 && r.chance(30) { "," } else { "" };
        return format!("[{}{t}]", items.join(","));
    }
    let n = r.below(5);
    let items: Vec<String> = (0..n)
        .map(|_| {
            let w1 = gen_ws(r, j5);
            let k = gen_key(r, j5);
            let w2 = gen_ws(r, j5);
            let w3 = gen_ws(r, j5);
            let v = gen_json(r, d + 1, j5);
            format!("{w1}{k}{w2}:{w3}{v}")
        })
        .collect();
    let t = if j5 && n > 0 && r.chance(30) { "," } else { "" };
    format!("{{{}{t}}}", items.join(","))
}

fn mutate(r: &mut Rng, s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    if b.is_empty() {
        return String::new();
    }
    let i = r.below(b.len());
    let op = r.below(10);
    let mut out: String = b[..i].iter().collect();
    if op < 4 {
        out.extend(&b[i + 1..]);
    } else if op < 8 {
        let ins = ['[', ']', '{', '}', ':', ',', '"', '\'', '\\', '\u{1}', ' '];
        out.push(ins[r.below(ins.len())]);
        out.extend(&b[i..]);
    }
    out
}

fn gen_path(r: &mut Rng) -> String {
    let mut p = String::from("$");
    for _ in 0..r.below(4) {
        let c = r.below(100);
        if c < 40 {
            p.push('.');
            p.push_str(r.pick(&["a", "b", "c", "k", "x"]));
        } else if c < 55 {
            p.push_str(&format!(".\"{}\"", r.pick(&["a b", "a", "1", ""])));
        } else if c < 85 {
            p.push_str(&format!(
                "[{}]",
                r.pick(&["0", "1", "2", "#-1", "#", "#-2", "5"])
            ));
        } else {
            p.push_str(r.pick(&[".", "[", "[x]", "a", "[ 1]"]));
        }
    }
    p
}

/// Random JSON / JSON5 / malformed text through every function,
/// with random paths and values.
#[test]
fn generated_text_corpus() {
    let n: usize = std::env::var("HIPP_JSON_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut stmts = Vec::new();
    for _ in 0..n {
        let j5 = r.chance(40);
        let mut doc = gen_json(&mut r, 0, j5);
        if r.chance(15) {
            doc = mutate(&mut r, &doc);
        }
        let doc = doc.replace('\n', " ");
        let d = sq(&doc);
        let p = sq(&gen_path(&mut r));
        let p2 = sq(&gen_path(&mut r));
        let patch = sq(&gen_json(&mut r, 0, j5).replace('\n', " "));
        let v = r.pick(&[
            "1",
            "'x'",
            "null",
            "1.5",
            "json('[1,2]')",
            "json('{\"q\":1}')",
            "x'00'",
            "'[3]'",
            "-2",
        ]);
        let forms = [
            format!("json({d})"),
            format!("hex(jsonb({d}))"),
            format!("json_valid({d})"),
            format!("json_valid({d}, 2)"),
            format!("json_error_position({d})"),
            format!("json_extract({d}, {p})"),
            format!("json_extract({d}, {p}, {p2})"),
            format!("hex(jsonb_extract({d}, {p}))"),
            format!("{d} -> {p}"),
            format!("{d} ->> {p}"),
            format!("json_set({d}, {p}, {v})"),
            format!("json_insert({d}, {p}, {v})"),
            format!("json_replace({d}, {p}, {v})"),
            format!("json_remove({d}, {p})"),
            format!("hex(jsonb_set({d}, {p}, {v}))"),
            format!("hex(jsonb_remove({d}, {p}, {p2}))"),
            format!("json_array_insert({d}, {p}, {v})"),
            format!("json_type({d}, {p})"),
            format!("json_array_length({d}, {p})"),
            format!("json_pretty({d})"),
            format!("json_patch({d}, {patch})"),
            format!("hex(jsonb_patch({d}, {patch}))"),
            format!("json_quote({d})"),
            format!("json_array({d}, json({d}))"),
            format!("json_object('k', json({d}))"),
            format!(
                "(SELECT group_concat(fullkey || '|' || ifnull(key, 'N') || '|' || type || '|' || id || '|' || ifnull(parent, 'N') || '|' || path, ';') FROM json_tree({d}))"
            ),
            format!(
                "(SELECT group_concat(fullkey || '|' || ifnull(key, 'N') || '|' || type || '|' || id || '|' || quote(value), ';') FROM json_each({d}, {p}))"
            ),
            format!("(SELECT group_concat(quote(atom), ';') FROM json_tree({d}, {p}))"),
            format!("json(jsonb_set(jsonb({d}), {p}, {v}))"),
            format!("json_valid(jsonb({d}), 8)"),
            format!("json(json_set(jsonb({d}), {p}, {v}, {p2}, {v}))"),
        ];
        stmts.push(format!("SELECT {}", forms[r.below(forms.len())]));
    }
    agree("generated text", &[], &stmts);
}

/// Random JSONB blobs — denormalized headers, wrong types, corrupt
/// bytes — through every function.
#[test]
fn generated_blob_corpus() {
    fn hdr(r: &mut Rng, t: u8, n: usize, big: bool) -> Vec<u8> {
        if n <= 11 && !big {
            return vec![t | ((n as u8) << 4)];
        }
        let c = if big {
            r.below(4)
        } else if n <= 0xff {
            1
        } else {
            2
        };
        if c == 0 && n <= 11 {
            return vec![t | ((n as u8) << 4)];
        }
        if c <= 1 && n <= 0xff {
            return vec![t | 0xc0, n as u8];
        }
        if c <= 2 && n <= 0xffff {
            return vec![t | 0xd0, (n >> 8) as u8, n as u8];
        }
        if r.chance(70) {
            let mut v = vec![t | 0xe0];
            v.extend_from_slice(&(n as u32).to_be_bytes());
            return v;
        }
        let mut v = vec![t | 0xf0];
        v.extend_from_slice(&(n as u64).to_be_bytes());
        v
    }
    fn blob(r: &mut Rng, d: u32) -> Vec<u8> {
        let mut t = [0u8, 1, 2, 3, 3, 4, 5, 6, 7, 7, 8, 9, 10, 11, 11, 12, 12][r.below(17)];
        if d > 3 && t >= 11 {
            t = 3;
        }
        let big = r.chance(20);
        let esc = |s: &str| s.replace('~', "\\").into_bytes();
        let p: Vec<u8> = match t {
            0..=2 => Vec::new(),
            3 => esc(r.pick(&[
                "1",
                "-12",
                "0",
                "123456789012345678901",
                "9223372036854775808",
                "-9223372036854775808",
                "1a",
                "",
            ])),
            4 => esc(r.pick(&["0x1f", "-0x10", "0xffffffffffffffffff", "0x", "1"])),
            5 => esc(r.pick(&["1.5", "1e5", "-0.0", "9e999", "1.", ".5", "x"])),
            6 => esc(r.pick(&[".5", "5.", "-.5", "1.e3", "-"])),
            7..=10 => {
                let mut v = esc(r.pick(&[
                    "abc", "a\"b", "a~nb", "~x41", "~'", "~q", "", "é", "a~", "~\n",
                ]));
                if r.chance(10) {
                    v.push(1);
                }
                if r.chance(5) {
                    v.extend_from_slice(b"~u0041");
                    let n = v.len();
                    v[n - 6] = b'\\';
                }
                v
            }
            11 => {
                let k = r.below(4);
                let mut v = Vec::new();
                for _ in 0..k {
                    v.extend(blob(r, d + 1));
                }
                v
            }
            _ => {
                let k = r.below(4);
                let mut v = Vec::new();
                for _ in 0..k {
                    let key = esc(r.pick(&["a", "b", "k", "a b", "~x61"]));
                    let kt = [7u8, 7, 10, 8, 9, 3][r.below(6)];
                    v.extend(hdr(r, kt, key.len(), false));
                    v.extend(key);
                    v.extend(blob(r, d + 1));
                }
                v
            }
        };
        let mut b = hdr(r, t, p.len(), big);
        b.extend(p);
        if r.chance(5) && b.len() > 1 {
            let i = r.below(b.len());
            b[i] = r.next() as u8;
        }
        b
    }
    let n: usize = std::env::var("HIPP_JSON_CORPUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut stmts = Vec::new();
    for _ in 0..n {
        let b = blob(&mut r, 0);
        let bl = format!(
            "x'{}'",
            b.iter().map(|c| format!("{c:02x}")).collect::<String>()
        );
        let p = sq(&gen_path(&mut r));
        let v = [
            "1".to_string(),
            "'x'".to_string(),
            "null".to_string(),
            "1.5".to_string(),
            "json('[1,2]')".to_string(),
            bl.clone(),
            "x'00'".to_string(),
        ][r.below(7)]
        .clone();
        let forms = [
            format!("json({bl})"),
            format!("hex(jsonb({bl}))"),
            format!("json_valid({bl}, 4)"),
            format!("json_valid({bl}, 8)"),
            format!("json_error_position({bl})"),
            format!("json_extract({bl}, {p})"),
            format!("hex(jsonb_extract({bl}, {p}))"),
            format!("{bl} ->> {p}"),
            format!("json_type({bl}, {p})"),
            format!("hex(jsonb_set({bl}, {p}, {v}))"),
            format!("hex(jsonb_insert({bl}, {p}, {v}))"),
            format!("hex(jsonb_remove({bl}, {p}))"),
            format!("hex(jsonb_array_insert({bl}, {p}, {v}))"),
            format!("hex(jsonb_patch({bl}, {bl}))"),
            format!("hex(jsonb_patch({bl}, '{{\"a\":1,\"b\":null}}'))"),
            format!("json_pretty({bl})"),
            format!("json_array({bl}, {v})"),
            format!("json_quote({bl})"),
            format!("json_array_length({bl})"),
            format!(
                "(SELECT group_concat(fullkey || '|' || ifnull(key, 'N') || '|' || type || '|' || id || '|' || ifnull(parent, 'N') || '|' || path || '|' || quote(atom), ';') FROM json_tree({bl}))"
            ),
            format!("(SELECT group_concat(quote(value), ';') FROM json_each({bl}, {p}))"),
            format!("(SELECT count(*) FROM json_tree({bl}, {p}))"),
            format!("json_object('k', {bl})"),
            format!("hex(jsonb_array({bl}))"),
            format!("json_set('[]', '$[0]', {bl})"),
        ];
        stmts.push(format!("SELECT {}", forms[r.below(forms.len())]));
    }
    agree("generated blobs", &[], &stmts);
}

/// Random documents in a table, read through the aggregates, the
/// window forms, lateral `json_each` / `json_tree` joins and the
/// edit functions per row.
#[test]
fn generated_table_corpus() {
    let mut r = Rng(0xd1b5_4a32_d192_ed03);
    let mut rows = Vec::new();
    for i in 0..14 {
        let j5 = r.chance(20);
        let mut d = gen_json(&mut r, 0, j5).replace('\n', " ");
        if r.chance(15) {
            d = r.pick(&["[1,2", "{\"a\":}", "x", ""]).to_string();
        }
        let g = r.pick(&["1", "2", "3", "null"]);
        let k = sq(r.pick(&["a", "b", "c", "A"]));
        let v = r.pick(&["1", "2.5", "null", "'x'", "'[1]'", "x'00'"]);
        rows.push(format!("({i}, {}, {g}, {k}, {v})", sq(&d)));
    }
    let setup = [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, j, g, k, v)".to_string(),
        format!("INSERT INTO t VALUES {}", rows.join(", ")),
    ];
    let mut stmts = Vec::new();
    for _ in 0..300 {
        let p = sq(&gen_path(&mut r));
        let forms = [
            format!("SELECT id, j -> {p}, j ->> {p} FROM t WHERE json_valid(j, 2) ORDER BY id"),
            "SELECT g, json_group_array(v) FROM t GROUP BY g ORDER BY g".to_string(),
            "SELECT g, json_group_array(json(j)) FROM t WHERE json_valid(j) GROUP BY g ORDER BY g".to_string(),
            "SELECT json_group_object(k, v) FROM t".to_string(),
            "SELECT json_group_object(k || id, j) FROM t WHERE id < 5 AND json_valid(j, 2)".to_string(),
            "SELECT json_group_array(DISTINCT k) FROM t".to_string(),
            "SELECT json_group_array(v ORDER BY id DESC) FROM t".to_string(),
            "SELECT json_group_array(id) FILTER (WHERE g = 1) FROM t".to_string(),
            "SELECT id, json_group_array(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id".to_string(),
            "SELECT id, json_group_object(k, id) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id".to_string(),
            "SELECT id, json_array(json_group_array(v) OVER (ORDER BY id)) FROM t ORDER BY id".to_string(),
            "SELECT t.id, e.key, e.type FROM t, json_each(t.j) e WHERE json_valid(t.j) ORDER BY 1, e.id".to_string(),
            format!("SELECT t.id, e.fullkey, e.atom FROM t, json_tree(t.j, {p}) e WHERE json_valid(t.j) ORDER BY 1, e.id"),
            "SELECT t.id, count(e.key) FROM t LEFT JOIN json_each(t.j) e WHERE json_valid(t.j) GROUP BY t.id ORDER BY 1".to_string(),
            "SELECT id, (SELECT json_group_array(value) FROM json_each(t.j)) FROM t WHERE json_valid(t.j) ORDER BY id".to_string(),
            format!("SELECT id, json_type(j, {p}), json_array_length(j, {p}) FROM t WHERE json_valid(j) ORDER BY id"),
            "SELECT id, json_set(j, '$.n', v) FROM t WHERE json_valid(j) ORDER BY id".to_string(),
            format!("SELECT id, json_insert(j, {p}, json_object('k', k)) FROM t WHERE json_valid(j) ORDER BY id"),
            format!("SELECT id, json_remove(j, {p}) FROM t WHERE json_valid(j) ORDER BY id"),
            "SELECT id, hex(jsonb(j)) FROM t WHERE json_valid(j, 2) ORDER BY id".to_string(),
            format!("SELECT k, json_group_array(j -> {p}) FROM t WHERE json_valid(j) GROUP BY k ORDER BY k"),
            "SELECT json_patch(a.j, b.j) FROM t a, t b WHERE a.id = 1 AND json_valid(a.j) AND json_valid(b.j) ORDER BY b.id".to_string(),
            "SELECT id, json_quote(v), json_array(v, k, g) FROM t ORDER BY id".to_string(),
            "SELECT count(*) FROM t, json_tree(t.j) WHERE json_valid(t.j)".to_string(),
            "SELECT value, count(*) FROM t, json_each(t.j) WHERE json_valid(t.j) GROUP BY value ORDER BY 1, 2".to_string(),
        ];
        stmts.push(forms[r.below(forms.len())].clone());
    }
    let setup: Vec<&str> = setup.iter().map(String::as_str).collect();
    agree("generated tables", &setup, &stmts);
}
