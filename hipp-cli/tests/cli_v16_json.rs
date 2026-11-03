//! hipp-cli v0.16 differentials: the JSON functions (leaf feature
//! `json`) must render byte-identically to the pinned oracle —
//! `json_pretty`'s multi-line text, JSONB blobs printed raw (control
//! bytes in the shell's caret notation), REALs inside JSON in the
//! pin's REAL text, `json_each` / `json_tree` rows, the aggregates and
//! a generated corpus of valid JSON / JSON5 documents through
//! `json()`, `jsonb()`, paths, edits and `json_tree`. Each batch runs
//! one-shot and over stdin, and the hipp-written file must pass the
//! oracle's integrity check.

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16json-{tag}-{}", std::process::id()));
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

/// One batch, byte-compared one-shot and over stdin.
fn byte_identical(tag: &str, batch: &str) {
    let a = temp_db(&format!("{tag}-hipp"));
    let b = temp_db(&format!("{tag}-ora"));
    let got = run(&hipp_bin(), &a, batch);
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&run(&oracle(), &b, batch))
    );
    assert!(!got.is_empty());
    let a2 = temp_db(&format!("{tag}-hipp-stdin"));
    let b2 = temp_db(&format!("{tag}-ora-stdin"));
    assert_eq!(
        String::from_utf8_lossy(&run_stdin(&hipp_bin(), &a2, batch)),
        String::from_utf8_lossy(&run_stdin(&oracle(), &b2, batch))
    );
    // The hipp-written file reads clean on the oracle.
    assert_eq!(run(&oracle(), &a, "PRAGMA integrity_check;"), b"ok\n");
}

/// Rendering: pretty text, raw blobs, numbers, tables, aggregates.
const RENDER: &str = r#"
SELECT json_pretty('{"a":[1,2,{"b":[]}],"c":{},"d":"x' || char(9) || 'y"}');
SELECT json_pretty('[1,[2,{"k":null}]]', '--'), json_pretty('5');
SELECT jsonb('[1,2,3]'), jsonb('{"a":"b"}'), jsonb('"' || char(1) || '"'), hex(jsonb('[1.5e3, 0x1f, .5]'));
SELECT json('[0x1F, +5, .5, 5., -.5, Infinity, -Infinity, NaN]'), json_array(0.1 + 0.2, 1e300, 1e999, -0.0, 5e-324);
SELECT json_object('a', 1, 'b', json('[true,false,null]'), 'c', 2.5, 'd', 'x"y'), json_quote('a' || char(1) || 'b');
SELECT json_extract('{"a":{"b":[10,20]}}', '$.a.b[#-1]', '$.a'), '{"a":{"b":[10,20]}}' -> '$.a', '{"a":{"b":[10,20]}}' ->> '$.a.b[0]';
SELECT json_set('{"a":1}', '$.b', json('{"c":[1]}'), '$.d[#]', 9), json_remove('[1,2,3]', '$[1]'), json_patch('{"a":1,"b":2}', '{"b":null,"c":3}');
CREATE TABLE t(id INTEGER PRIMARY KEY, g, doc);
INSERT INTO t VALUES (1, 1, '{"tags":["a","b"],"n":1}'), (2, 1, '{"tags":["b"],"n":2.5}'), (3, 2, '{"tags":[],"n":null}');
SELECT g, json_group_array(doc -> '$.n'), json_group_object(id, doc ->> '$.n') FROM t GROUP BY g;
SELECT t.id, e.value, e.fullkey FROM t, json_each(t.doc, '$.tags') e ORDER BY t.id, e.id;
SELECT key, value, type, atom, id, parent, fullkey, path FROM json_tree('{"a":[1,{"b c":null}],"d":"e"}');
SELECT id, json_group_array(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t;
SELECT value, json_group_array(t.id) FROM t, json_each(t.doc, '$.tags') GROUP BY value ORDER BY value;
UPDATE t SET doc = json_set(doc, '$.n', id * 10) WHERE json_valid(doc) AND doc ->> '$.n' IS NOT NULL;
SELECT id, doc FROM t;
SELECT jsonb_group_array(id), hex(jsonb_group_object('k' || id, id)) FROM t;
"#;

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

fn doc(r: &mut Rng, d: u32) -> String {
    let x = r.below(10);
    if d > 3 || x < 4 {
        return r
            .pick(&[
                "0", "-1", "1.5", "1e5", "0x1F", ".5", "5.", "true", "false", "null", "\"s\"",
                "'q'", "\"a b\"", "Infinity", "1E-3",
            ])
            .to_string();
    }
    let n = r.below(4);
    if x < 7 {
        let v: Vec<String> = (0..n).map(|_| doc(r, d + 1)).collect();
        return format!("[{}]", v.join(", "));
    }
    let v: Vec<String> = (0..n)
        .map(|_| {
            let k = r.pick(&["a", "\"b\"", "'c'", "$d", "\"e f\""]);
            format!("{k}: {}", doc(r, d + 1))
        })
        .collect();
    format!("{{{}}}", v.join(", "))
}

fn path(r: &mut Rng) -> String {
    let mut p = String::from("$");
    for _ in 0..r.below(3) {
        p.push_str(r.pick(&[".a", ".b", ".c", ".$d", "[0]", "[1]", "[#-1]"]));
    }
    p
}

/// Valid JSON / JSON5 documents through the rendering paths.
fn corpus() -> String {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut out = String::from("CREATE TABLE c(d);\n");
    for _ in 0..120 {
        let d = doc(&mut r, 0).replace('\'', "''");
        out.push_str(&format!("INSERT INTO c VALUES ('{d}');\n"));
    }
    for _ in 0..40 {
        let p = path(&mut r);
        out.push_str(&format!(
            "SELECT json(d), hex(jsonb(d)), json_type(d, '{p}'), d -> '{p}', json_set(d, '{p}', 1), json_remove(d, '{p}') FROM c WHERE rowid % 40 = {};\n",
            r.below(40)
        ));
    }
    out.push_str(
        "SELECT c.rowid, fullkey, type, quote(atom), id, parent FROM c, json_tree(c.d) ORDER BY c.rowid, json_tree.id;\n\
         SELECT json_group_array(json(d)) FROM c;\n\
         SELECT json_pretty(d) FROM c WHERE rowid < 20;\n",
    );
    out
}

#[test]
fn cli_v16_json_render_byte_identical() {
    byte_identical("render", RENDER);
}

#[test]
fn cli_v16_json_corpus_byte_identical() {
    byte_identical("corpus", &corpus());
}
