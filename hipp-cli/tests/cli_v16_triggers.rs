//! hipp-cli v0.16 triggers: byte-identical stdout against the pinned
//! oracle (SQLite 3.54.1) for scripts holding CREATE TRIGGER
//! statements over several lines, read as the pin's shell reads them
//! (a trigger ends at a `;` after `; END`, the `sqlite3_complete`
//! state machine: an `END` closing a CASE, a `;` in a string or a
//! comment do not end it), `.schema` and `.dump` with triggers (the
//! dump replays them after the data), and the pin's trigger depth
//! (1000 nested programs, on the CLI's large-stack thread). Errors
//! are compared by count only: hipp's messages are not the pin's.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let dir = std::env::temp_dir().join(format!("hipp-cli-v16trg-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("t.db")
}

/// Run `input` over stdin against `db`; (stdout, error lines).
fn run_stdin(bin: &Path, db: &Path, input: &str) -> (String, usize) {
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli spawns");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("cli finishes");
    let err = String::from_utf8_lossy(&out.stderr);
    let errors = err
        .lines()
        .filter(|l| l.starts_with("Error") || l.starts_with("Parse error"))
        .count();
    (String::from_utf8(out.stdout).expect("utf8 stdout"), errors)
}

/// Run one-shot SQL against `db`: (stdout, succeeded).
fn run_arg(bin: &Path, db: &Path, sql: &str) -> (String, bool) {
    let out = Command::new(bin)
        .arg(db)
        .arg(sql)
        .output()
        .expect("cli runs");
    (
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        out.status.success(),
    )
}

/// Both engines over stdin on fresh files: same stdout and errors.
fn same_stdin(tag: &str, input: &str) -> String {
    let h = temp_db(&format!("{tag}-h"));
    let o = temp_db(&format!("{tag}-o"));
    let hr = run_stdin(&hipp_bin(), &h, input);
    let or = run_stdin(&oracle(), &o, input);
    assert_eq!(hr, or, "{tag}:\n{input}");
    hr.0
}

#[test]
fn multi_line_triggers_fire() {
    let out = same_stdin(
        "multiline",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT);
CREATE TABLE log(x, y);
CREATE TRIGGER tr1 AFTER INSERT ON t BEGIN
  INSERT INTO log VALUES ('ins', new.a);  -- a comment ; with a semicolon
  INSERT INTO log VALUES ('end;', CASE WHEN new.a > 1 THEN 'big' ELSE 'small' END);
END;
create trigger tr0 before delete on t begin insert into log values ('del', old.a); end;
CREATE TRIGGER \"tr 2\" AFTER UPDATE OF a ON t
WHEN new.a > old.a
BEGIN
  UPDATE log SET y = y || '!' WHERE x = 'ins';
END
;
INSERT INTO t(a) VALUES (1), (2);
UPDATE t SET a = a + 10;
DELETE FROM t WHERE a = 11;
SELECT * FROM log;
SELECT * FROM t;
SELECT changes(), total_changes();
",
    );
    assert!(out.contains("ins|1!"), "{out}");
}

#[test]
fn incomplete_trigger_texts_read_as_the_pin_reads_them() {
    // `BEGIN END;` and an END not after `;` leave the trigger open:
    // the shell reads on until `; END ;`, and the whole chunk fails.
    same_stdin(
        "incomplete",
        "CREATE TABLE t(a);
CREATE TRIGGER e10 AFTER INSERT ON t BEGIN END;
SELECT 'swallowed';
CREATE TRIGGER e14 AFTER INSERT ON t BEGIN SELECT 1 ; SELECT 2 END;
SELECT 'also swallowed'; END;
SELECT 'after';
CREATE TRIGGER ok AFTER INSERT ON t BEGIN SELECT CASE WHEN 1 THEN 2 END; END; SELECT 'same line';
SELECT name FROM sqlite_schema;
",
    );
}

#[test]
fn raise_and_recursion() {
    same_stdin(
        "raise",
        "CREATE TABLE t(a);
CREATE TABLE u(x);
CREATE TRIGGER tr BEFORE INSERT ON t BEGIN
  INSERT INTO u VALUES (new.a);
  SELECT RAISE(IGNORE) WHERE new.a = 2;
  SELECT RAISE(FAIL, 'three') WHERE new.a = 3;
END;
INSERT INTO t VALUES (1), (2), (3), (4);
SELECT * FROM t;
SELECT * FROM u;
SELECT RAISE(ABORT, 'outside');
PRAGMA recursive_triggers = ON;
CREATE TABLE r(n);
CREATE TRIGGER rr AFTER INSERT ON r WHEN new.n < 1000 BEGIN INSERT INTO r VALUES (new.n + 1); END;
INSERT INTO r VALUES (1);
SELECT count(*), max(n) FROM r;
INSERT INTO r VALUES (0);
SELECT count(*), max(n) FROM r;
",
    );
}

#[test]
fn schema_shows_triggers() {
    same_stdin(
        "schema",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT);
CREATE TRIGGER tr1 AFTER INSERT ON t BEGIN
  SELECT 1;
END;
CREATE TABLE u(x);
CREATE INDEX ui ON u(x);
create trigger tr0 after delete on u begin delete from t; end;
CREATE VIEW v AS SELECT * FROM t;
CREATE TRIGGER tv INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES (new.id, new.a); END;
INSERT INTO v VALUES (1, 2);
SELECT * FROM t;
.schema
.schema t
.schema v
CREATE TEMP TABLE tt(a);
CREATE TRIGGER ttr AFTER INSERT ON tt BEGIN SELECT 1; END;
.schema tt
",
    );
}

#[test]
fn dump_replays_triggers_after_the_data() {
    let src = temp_db("dump-src");
    let (_, ok) = run_arg(
        &hipp_bin(),
        &src,
        "CREATE TABLE t(a); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.a); END; \
         CREATE TRIGGER tc AFTER INSERT ON t BEGIN \
           -- a comment\n SELECT 1; END; \
         CREATE VIEW v AS SELECT * FROM t; CREATE INDEX ix ON log(x); \
         INSERT INTO t VALUES (1), (2);",
    );
    assert!(ok);
    let (dump, _) = run_stdin(&hipp_bin(), &src, ".dump\n");
    let (odump, _) = run_stdin(&oracle(), &src, ".dump\n");
    // The pin's object order and `--` rule; it also writes a PRAGMA
    // first.
    assert_eq!(
        odump.trim_start_matches("PRAGMA foreign_keys=OFF;\n"),
        dump,
        "{dump}"
    );
    let check = "INSERT INTO t VALUES (3); SELECT * FROM log; SELECT count(*) FROM t;";
    let mut outs = Vec::new();
    for (tag, bin) in [("h", hipp_bin()), ("o", oracle())] {
        let db = temp_db(&format!("dump-replay-{tag}"));
        run_stdin(&bin, &db, &dump);
        outs.push(run_arg(&bin, &db, check).0);
    }
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0], "1\n2\n3\n3\n");
}

#[test]
fn one_shot_scripts() {
    let sql = "CREATE TABLE t(a); CREATE TABLE log(x); \
               CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.a * 2); END; \
               INSERT INTO t VALUES (1), (2); SELECT * FROM log; \
               CREATE TRIGGER bad AFTER INSERT ON t BEGIN END; SELECT 'not reached';";
    let h = temp_db("oneshot-h");
    let o = temp_db("oneshot-o");
    assert_eq!(run_arg(&hipp_bin(), &h, sql), run_arg(&oracle(), &o, sql));
}

/// v0.16 review fixes, as the shell renders them: NEW's generated
/// columns in BEFORE UPDATE, part-way RAISE(FAIL) in a body write,
/// RETURNING through an INSTEAD OF view and with AFTER triggers,
/// REPLACE victims' order, the AND / OR literal rule, and a depth
/// failure that keeps a transaction's rows.
#[test]
fn review_fixes_render_as_the_pin() {
    same_stdin(
        "review",
        "CREATE TABLE g(id INTEGER PRIMARY KEY, a, b, c AS (a||b), d AS (b||a) STORED);
CREATE TABLE log(x);
CREATE TRIGGER gu BEFORE UPDATE ON g BEGIN
  INSERT INTO log VALUES(quote(new.c)||quote(new.b)||quote(new.d));
END;
INSERT INTO g(a, b) VALUES('x', 'y');
UPDATE g SET a = 'z';
SELECT * FROM log;
CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO u VALUES(1),(2),(3);
CREATE TRIGGER ti AFTER INSERT ON t BEGIN
  DELETE FROM u WHERE CASE WHEN b=2 THEN RAISE(FAIL,'two') ELSE 1 END;
END;
INSERT INTO t VALUES(1);
SELECT * FROM u;
SELECT total_changes();
CREATE TABLE r(a); CREATE VIEW v AS SELECT a FROM r;
CREATE TRIGGER vi INSTEAD OF INSERT ON v BEGIN INSERT INTO r VALUES(new.a); INSERT INTO log VALUES(new.a); END;
INSERT INTO v VALUES(5),(6) RETURNING a, (SELECT count(*) FROM log), typeof(a);
CREATE TRIGGER ra AFTER INSERT ON r BEGIN INSERT INTO log VALUES(-new.a); END;
INSERT INTO r VALUES(7),(8) RETURNING a, (SELECT count(*) FROM log), (SELECT count(*) FROM r);
PRAGMA recursive_triggers=1;
CREATE TABLE q(a UNIQUE, b UNIQUE);
CREATE TRIGGER qd AFTER DELETE ON q BEGIN INSERT INTO log VALUES('d'||old.a||old.b); END;
INSERT INTO q VALUES(1,1),(2,2);
INSERT OR REPLACE INTO q VALUES(1,2);
SELECT x FROM log WHERE x LIKE 'd%';
SELECT -1 OR abs(-9223372036854775808);
SELECT 0 AND abs(-9223372036854775808), (1) OR abs(-9223372036854775808);
CREATE TABLE z(a);
CREATE TRIGGER zr AFTER INSERT ON z BEGIN INSERT INTO z VALUES(new.a+1); END;
BEGIN;
INSERT INTO z VALUES(0);
COMMIT;
SELECT count(*), max(a) FROM z;
",
    );
}
