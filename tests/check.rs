//! Integration tests for the `aipl check` command — the in-language test
//! runner. These drive the real CLI binary as a subprocess (the cases harness
//! only runs a program's `main`, so it can't exercise `check`), staging a
//! temp `.aipl` file and asserting on `check`'s stdout and exit code.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Write `src` to a uniquely-named temp file, run `aipl check` on it, and return
/// `(stdout, stderr, exit_code)`. `name` keeps temp files distinct across tests
/// (which run in parallel).
fn check(name: &str, src: &str) -> (String, String, i32) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check");
    fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(format!("{name}.aipl"));
    fs::write(&path, src).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("check")
        .arg(&path)
        .output()
        .expect("run aipl check");
    let norm = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
    (
        norm(&out.stdout),
        norm(&out.stderr),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn all_tests_pass_is_silent_and_exit_zero() {
    let (stdout, _stderr, code) = check(
        "all_pass",
        "import { ==, > } from builtins;\n\
         fn a() -> i64 { 1 }.test({ assert(a() == 1); })\n\
         fn b() -> i64 { 2 }.test({ assert(b() == 2); assert(b() > 0); })\n",
    );
    // Passing tests print nothing; only the summary.
    assert_eq!(stdout, "2 tests: 2 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn a_failing_assert_reports_and_exits_one() {
    let (stdout, _stderr, code) = check(
        "one_fail",
        "import { == } from builtins;\n\
         fn foo() -> i64 { 42 }.test({ assert(foo() == 42); })\n\
         fn bar() -> i64 { 5 }.test({\n    assert(bar() == 6);\n})\n",
    );
    assert!(
        stdout.contains("test bar ... FAIL"),
        "expected a FAIL header, got:\n{stdout}"
    );
    // The location is the asserted condition's line and source text (line 4 with
    // the leading operator import).
    assert!(
        stdout.contains("assert failed at input:4: bar() == 6"),
        "expected the assert location, got:\n{stdout}"
    );
    assert!(stdout.contains("2 tests: 1 passed, 1 failed"));
    assert_eq!(code, 1);
}

#[test]
fn all_asserts_in_a_test_run_and_each_failure_is_reported() {
    let (stdout, _stderr, code) = check(
        "run_all",
        "import { == } from builtins;\n\
         fn bar() -> i64 { 5 }.test({\n    assert(bar() == 6);\n    assert(bar() == 7);\n})\n",
    );
    // Both failing asserts report — the first failure doesn't abort the test.
    // (Lines shift by one for the leading operator import.)
    assert!(stdout.contains("input:3: bar() == 6"), "got:\n{stdout}");
    assert!(stdout.contains("input:4: bar() == 7"), "got:\n{stdout}");
    assert!(stdout.contains("1 tests: 0 passed, 1 failed"));
    assert_eq!(code, 1);
}

#[test]
fn a_test_may_call_effectful_functions() {
    // "Implicitly allow all effects": a test can call a `!prints` function with
    // no effect annotation. (Its output lands in the check output.)
    let (stdout, _stderr, code) = check(
        "effects",
        "import { print, == } from builtins;\n\
         fn greet() !prints { print(\"hi\") }.test({ greet(); assert(1 == 1); })\n",
    );
    assert!(
        stdout.contains("hi"),
        "expected greet output, got:\n{stdout}"
    );
    assert!(stdout.contains("1 tests: 1 passed, 0 failed"));
    assert_eq!(code, 0);
}

#[test]
fn assert_outside_a_test_is_a_compile_error() {
    let (_stdout, stderr, code) = check(
        "assert_outside",
        "fn f() -> i64 { assert(true); 0 }\nfn main() -> i64 { f() }\n",
    );
    // `assert` is rewritten only inside `.test` bodies, so elsewhere it's an
    // ordinary (undefined) call.
    assert!(
        stderr.contains("undefined fn \"assert\""),
        "expected an undefined-assert error, got:\n{stderr}"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_program_with_no_tests_passes_zero_tests() {
    let (stdout, _stderr, code) = check("no_tests", "fn main() -> i64 { 0 }\n");
    assert_eq!(stdout, "0 tests: 0 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn a_type_error_in_a_test_body_is_reported() {
    // The test body is type-checked: asserting on a non-bool is an error.
    let (_stdout, stderr, code) =
        check("bad_assert", "fn f() -> i64 { 1 }.test({ assert(f()); })\n");
    assert!(!stderr.is_empty(), "expected a type error on stderr");
    assert_eq!(code, 1);
}
