//! v0.16 bound parameters in the tiny + `params` build, run from
//! the default (full) test run.
//!
//! The tiny grammar's parameter code (`Parser::param_here`, the
//! parameter forms of INSERT VALUES and `WHERE rowid = ?`) is
//! compiled only without `query`, and CI's test jobs build the
//! default features. So this test builds tests/params_v16.rs in a
//! `--no-default-features --features std,params` build of its own
//! (in its own target directory) and requires the three
//! tiny-grammar tests there to run and pass — against the tiny
//! parser, which no other test run reaches.
#![cfg(all(feature = "params", feature = "query"))]

use std::process::Command;

/// The tests of tests/params_v16.rs that a `std,params` build
/// compiles (the others need `query` / `sql`).
const TINY_TESTS: [&str; 3] = [
    "numbering_and_api_in_the_tiny_grammar",
    "literal_positions_of_the_tiny_grammar",
    "a_prepared_insert_is_re_executed_with_new_bindings",
];

#[test]
fn tiny_grammar_parameter_tests_pass_in_a_tiny_params_build() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = concat!(env!("CARGO_TARGET_TMPDIR"), "/tiny-params");
    let out = Command::new(cargo)
        .args([
            "test",
            "--manifest-path",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            "-p",
            "hipp",
            "--no-default-features",
            "--features",
            "std,params",
            "--test",
            "params_v16",
        ])
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("run cargo");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the std,params build of params_v16 failed:\n{stdout}\n{stderr}"
    );
    for name in TINY_TESTS {
        assert!(
            stdout.contains(&format!("test {name} ... ok")),
            "{name} did not run and pass in the std,params build:\n{stdout}"
        );
    }
    assert!(
        stdout.contains(&format!("{} passed", TINY_TESTS.len())),
        "the std,params build ran other tests than expected:\n{stdout}"
    );
}
