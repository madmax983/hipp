//! hipp-cli v0.16 virtual tables (leaves `vtab`, `rtree`): byte-for-
//! byte the pinned oracle's stdout for `generate_series`, `dbstat`
//! (on the same file), R*Tree tables (float32 coordinates rendered as
//! REAL text, `rtreenode()`, `rtreecheck()`) and `.schema`'s comment
//! line for a virtual table. (List mode: the output modes are the
//! shell's, tested elsewhere.)

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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hipp-cli-v16vt-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("test.db")
}

fn run_stdin(bin: &Path, db: &Path, input: &str) -> String {
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
    let out = child.wait_with_output().expect("cli finishes");
    assert!(
        out.status.success(),
        "{} failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// `script` on fresh twin files: identical stdout.
fn same(tag: &str, script: &str) {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let a = run_stdin(&oracle(), &o, script);
    let b = run_stdin(&hipp_bin(), &h, script);
    assert_eq!(b, a, "script:\n{script}");
}

const SERIES: &str = "\
SELECT * FROM generate_series(1, 5);
SELECT value, start, stop, step FROM generate_series(10, 1, -3);
SELECT value FROM generate_series(0.5, 3);
SELECT value FROM generate_series(1, 20) WHERE value > '3' LIMIT 2;
SELECT value FROM generate_series WHERE value BETWEEN 3 AND 5;
SELECT typeof(value), value FROM generate_series(1.5, 3.5);
SELECT * FROM generate_series(1, 2);
SELECT *, step FROM generate_series(1, 2);
SELECT value, value * 1.5 AS r FROM generate_series(1, 3);
";

#[test]
fn generate_series_output() {
    same("series", SERIES);
}

const RTREE: &str = "\
CREATE VIRTUAL TABLE r USING rtree(id, x0, x1, y0, y1, +name);
INSERT INTO r VALUES (1, 0.1, 0.2, 5, 6, 'a'), (2, 1e10, 3e10, -1.5, 2.25, NULL);
INSERT INTO r VALUES ('7', '1.5', '2', 3, 4, 'x');
INSERT INTO r SELECT value + 10, value / 3.0, value / 3.0 + 0.1, value, value * 2, 'g' || value FROM generate_series(1, 30);
SELECT * FROM r WHERE id < 15;
SELECT id, x0 * 2, typeof(x1) FROM r WHERE x0 >= 2 AND y1 < 50 ORDER BY id;
SELECT rtreenode(2, data) FROM r_node WHERE nodeno = 1;
SELECT rtreecheck('r');
SELECT count(*), sum(x1), max(y1) FROM r;
DELETE FROM r WHERE id % 2 = 0;
UPDATE r SET x1 = x1 + 1 WHERE id > 30;
SELECT * FROM r WHERE id > 30;
.schema
CREATE VIRTUAL TABLE i USING rtree_i32(id, a, b);
INSERT INTO i VALUES (1, 1.7, 2.2), (2, -5000000000, 5000000000);
SELECT * FROM i;
SELECT * FROM r WHERE id = 1;
";

#[test]
fn rtree_output() {
    same("rtree", RTREE);
}

#[test]
fn dbstat_on_the_same_file() {
    let base = temp_db("dbstat-base");
    run_stdin(
        &oracle(),
        &base,
        "PRAGMA page_size = 1024;
CREATE TABLE t(a, b); CREATE INDEX ti ON t(b);
INSERT INTO t SELECT value, randomblob(value * 7) FROM generate_series(1, 150);
CREATE TABLE big(x); INSERT INTO big VALUES (zeroblob(4000));
DELETE FROM t WHERE a % 4 = 0;
CREATE VIRTUAL TABLE s USING dbstat;
",
    );
    let script = "\
SELECT * FROM dbstat;
SELECT * FROM dbstat WHERE aggregate = 1;
SELECT name, path, pagetype FROM s WHERE name = 'big';
.schema s
";
    let h = temp_db("dbstat-h");
    let o = temp_db("dbstat-o");
    std::fs::copy(&base, &h).expect("copy");
    std::fs::copy(&base, &o).expect("copy");
    let a = run_stdin(&oracle(), &o, script);
    let b = run_stdin(&hipp_bin(), &h, script);
    assert_eq!(b, a);
}

/// `.dump` of virtual tables (v0.16 review fix): the pin shell's
/// script byte for byte — its defensive-mode WARNING header, the
/// `writable_schema` rows, `x'..'` blobs, `CREATE TABLE IF NOT EXISTS`
/// for quoted names, `PRAGMA writable_schema=OFF` last — and `.dump
/// PATTERN` (a virtual table bringing its shadow tables). Each
/// engine's dump replays in both engines (hipp's own through `PRAGMA
/// writable_schema`).
#[test]
fn dump_of_virtual_tables() {
    let base = temp_db("dump-base");
    run_stdin(
        &oracle(),
        &base,
        "create table t(a); insert into t values (1), (x'ab01'); \
         create virtual table r using rtree(id, a, b, +x); \
         insert into r values (1, 2, 3, 'p'), (2, 0.5, 9.25, x'00ff'); \
         create table \"u\"(b); insert into u values (2); create view v as select 1; \
         create index ui on u(b); \
         create virtual table f using fts5(x, y); insert into f values ('hello world', 'zz'); \
         create virtual table g using fts4(body); insert into g values ('one two');\n",
    );
    for args in [
        "", " t", " r", " r_node", " u", " 'r%'", " f", " g", " nosuch", " t u",
    ] {
        let pin = run_stdin(&oracle(), &base, &format!(".dump{args}\n"));
        let pin = pin.trim_start_matches("PRAGMA foreign_keys=OFF;\n");
        let ours = run_stdin(&hipp_bin(), &base, &format!(".dump{args}\n"));
        assert_eq!(ours, pin, ".dump{args}");
    }
    let dump = run_stdin(&hipp_bin(), &base, ".dump\n");
    let pin_dump = run_stdin(&oracle(), &base, ".dump\n");
    let check = "select id, a, b, hex(x) from r; select rtreecheck('r'); select * from f; \
                 select * from g_content; select hex(a) from t; pragma integrity_check;\n";
    for (i, script) in [&dump, &pin_dump].iter().enumerate() {
        let o = temp_db(&format!("dump-into-pin{i}"));
        run_stdin(&oracle(), &o, script);
        let h = temp_db(&format!("dump-into-hipp{i}"));
        run_stdin(&hipp_bin(), &h, script);
        let want = run_stdin(&oracle(), &base, check);
        assert_eq!(
            run_stdin(&oracle(), &o, check),
            want,
            "pin replays dump {i}"
        );
        assert_eq!(
            run_stdin(&oracle(), &h, check),
            want,
            "pin reads hipp's replay {i}"
        );
        assert_eq!(
            run_stdin(&hipp_bin(), &h, ".dump\n"),
            dump,
            "hipp's replay {i} dumps the same"
        );
    }
}
