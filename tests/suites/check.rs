//! Integration tests for the `aipl check` command — the in-language test
//! runner. These drive the real CLI binary as a subprocess (the cases harness
//! only runs a program's `main`, so it can't exercise `check`), staging a
//! temp `.aipl` file — or a whole temp tree, for the batch mode a bare
//! `aipl check` runs — and asserting on `check`'s stdout and exit code.

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

/// Stage a tree of `(relative path, source)` files under a uniquely-named temp
/// directory and run `aipl check` in it with `args` (empty = the bare
/// whole-codebase form), returning `(stdout, stderr, exit_code)`. Paths may name
/// subdirectories (`"sub/b.aipl"`), which are created as needed.
fn check_tree(name: &str, files: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("check_tree")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    for (rel, src) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("file has a parent")).expect("create temp dir");
        fs::write(&path, src).expect("write temp source");
    }
    fs::create_dir_all(&dir).expect("create temp dir");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("check")
        .args(args)
        .current_dir(&dir)
        .output()
        .expect("run aipl check");
    let norm = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
    (
        norm(&out.stdout),
        norm(&out.stderr),
        out.status.code().unwrap_or(-1),
    )
}

const PASSES: &str =
    "import { equal as ==} from builtins;\nfn a() -> i64 { 1 }.test({ assert(a() == 1); })\n";
const FAILS: &str =
    "import { equal as ==} from builtins;\nfn b() -> i64 { 5 }.test({ assert(b() == 6); })\n";
/// Declared `i64`, returns `str` — fails the checker, so it never runs.
const BROKEN: &str = "pub fn c() -> i64 { \"oops\" }\n";

#[test]
fn bare_check_runs_every_file_under_the_working_directory() {
    let (stdout, _stderr, code) = check_tree(
        "whole_tree",
        &[("a.aipl", PASSES), ("sub/b.aipl", PASSES)],
        &[],
    );
    // Both files' tests ran, counted into one aggregate.
    assert_eq!(stdout, "2 files, 2 tests: 2 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn batch_failures_name_the_file_they_came_from() {
    let (stdout, _stderr, code) = check_tree(
        "attribute",
        &[("ok.aipl", PASSES), ("sub/bad.aipl", FAILS)],
        &[],
    );
    // With many files in play, a bare test name wouldn't locate the failure.
    assert!(
        stdout.contains("test ./sub/bad.aipl::b ... FAIL"),
        "expected a file-qualified FAIL header, got:\n{stdout}"
    );
    assert!(stdout.contains("2 files, 2 tests: 1 passed, 1 failed"));
    assert_eq!(code, 1);
}

#[test]
fn a_file_that_does_not_compile_is_reported_without_stopping_the_run() {
    let (stdout, stderr, code) = check_tree(
        "keep_going",
        &[
            ("a_broken.aipl", BROKEN),
            ("b_ok.aipl", PASSES),
            ("c_fails.aipl", FAILS),
        ],
        &[],
    );
    // The broken file sorts first, so the run would end there if it aborted.
    assert!(
        stderr.contains("declared return type is i64"),
        "expected the compile error, got:\n{stderr}"
    );
    assert!(
        stdout.contains("test ./c_fails.aipl::b ... FAIL"),
        "the run should continue past the broken file, got:\n{stdout}"
    );
    // A file that never compiled contributes no test counts, so it is called out
    // separately — otherwise "1 failed" would understate the damage.
    assert!(
        stdout.contains("3 files, 2 tests: 1 passed, 1 failed"),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("1 file failed to compile"),
        "got:\n{stdout}"
    );
    assert_eq!(code, 1);
}

#[test]
fn discovery_skips_hidden_directories_and_target() {
    // Both would fail to parse if they were picked up.
    let junk = "this is not valid aipl at all\n";
    let (stdout, _stderr, code) = check_tree(
        "exclusions",
        &[
            ("a.aipl", PASSES),
            (".hidden/x.aipl", junk),
            ("target/y.aipl", junk),
        ],
        &[],
    );
    assert_eq!(stdout, "1 file, 1 tests: 1 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn a_directory_argument_checks_the_tree_under_it() {
    let (stdout, _stderr, code) = check_tree(
        "dir_arg",
        &[("sub/a.aipl", PASSES), ("outside.aipl", FAILS)],
        &["sub"],
    );
    // Only the named subtree runs — the failing file outside it is untouched.
    assert_eq!(stdout, "1 file, 1 tests: 1 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn several_explicit_paths_are_checked_as_one_batch() {
    let (stdout, _stderr, code) = check_tree(
        "multi_path",
        &[
            ("a.aipl", PASSES),
            ("b.aipl", PASSES),
            ("unused.aipl", FAILS),
        ],
        &["a.aipl", "b.aipl"],
    );
    assert_eq!(stdout, "2 files, 2 tests: 2 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn finding_no_files_is_a_failure_not_a_silent_pass() {
    // A bare `aipl check` is meant to be a project's whole handoff gate, so
    // "checked nothing, all good" would quietly disarm it.
    let (_stdout, stderr, code) = check_tree("empty", &[], &[]);
    assert!(
        stderr.contains("no .aipl files found"),
        "expected an empty-tree diagnostic, got:\n{stderr}"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_missing_path_is_reported() {
    let (_stdout, stderr, code) = check_tree("missing", &[("a.aipl", PASSES)], &["nope.aipl"]);
    assert!(
        stderr.contains("no such file or directory"),
        "expected a missing-path diagnostic, got:\n{stderr}"
    );
    assert_eq!(code, 1);
}

#[test]
fn all_tests_pass_is_silent_and_exit_zero() {
    let (stdout, _stderr, code) = check(
        "all_pass",
        "import { equal as ==, greater_than as >} from builtins;\n\
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
        "import { equal as ==} from builtins;\n\
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
        "import { equal as ==} from builtins;\n\
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
        "import { print, equal as ==} from builtins;\n\
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
