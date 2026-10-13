//! Integration tests for the `aipl check` command — the in-language test
//! runner. These drive the real CLI binary as a subprocess (the cases harness
//! only runs a program's `main`, so it can't exercise `check`), staging a
//! temp `.aipl` file — or a whole temp tree, for the batch mode a bare
//! `aipl check` runs — and asserting on `check`'s stdout and exit code.
//!
//! Every fixture source here is written in `aipl fmt`'s canonical format, since
//! `check` reports unformatted files: an offhandedly-spaced import would make a
//! test about test *counts* fail over whitespace. Reformat one by round-tripping
//! it through `aipl fmt` rather than by eye.

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

const PASSES: &str = "import { equal as == } from builtins;\n\n\
     fn a() -> i64 { 1 }.test({\n    assert(a() == 1);\n})\n";
const FAILS: &str = "import { equal as == } from builtins;\n\n\
     fn b() -> i64 { 5 }.test({\n    assert(b() == 6);\n})\n";
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

/// `PASSES` with the import's inner spacing squeezed — parses and passes, but
/// isn't what `aipl fmt` would write.
const UNFORMATTED: &str = "import { equal as ==} from builtins;\n\
     fn a() -> i64 { 1 }.test({ assert(a() == 1); })\n";

#[test]
fn an_unformatted_file_fails_even_when_its_tests_pass() {
    let (stdout, stderr, code) = check("unformatted", UNFORMATTED);
    // The tests still ran — formatting is reported alongside them, not instead.
    assert_eq!(stdout, "1 tests: 1 passed, 0 failed\n");
    assert!(
        stderr.contains("needs formatting"),
        "expected a formatting diagnostic, got:\n{stderr}"
    );
    // And it names the command that fixes it.
    assert!(stderr.contains("aipl fmt"), "got:\n{stderr}");
    assert_eq!(code, 1);
}

#[test]
fn formatting_and_test_failures_are_both_reported_from_one_run() {
    // Neither problem hides the other: the point of `check` is that one
    // invocation tells you everything standing between you and a green tree.
    let unformatted_and_failing = "import { equal as ==} from builtins;\n\
         fn b() -> i64 { 5 }.test({ assert(b() == 6); })\n";
    let (stdout, stderr, code) = check("unformatted_fail", unformatted_and_failing);
    assert!(stdout.contains("test b ... FAIL"), "got:\n{stdout}");
    assert!(stderr.contains("needs formatting"), "got:\n{stderr}");
    assert_eq!(code, 1);
}

#[test]
fn an_unformatted_file_does_not_stop_the_batch() {
    let (stdout, stderr, code) = check_tree(
        "fmt_keep_going",
        &[
            ("a_unformatted.aipl", UNFORMATTED),
            ("b_ok.aipl", PASSES),
            ("c_fails.aipl", FAILS),
        ],
        &[],
    );
    // The unformatted file sorts first, so the run would end there if it aborted.
    assert!(
        stderr.contains("./a_unformatted.aipl: needs formatting"),
        "expected a file-qualified formatting diagnostic, got:\n{stderr}"
    );
    assert!(
        stdout.contains("test ./c_fails.aipl::b ... FAIL"),
        "the run should continue past the unformatted file, got:\n{stdout}"
    );
    // Its own tests ran too, so all three files are in the tally.
    assert!(
        stdout.contains("3 files, 3 tests: 2 passed, 1 failed"),
        "got:\n{stdout}"
    );
    // Called out separately: an unformatted file's tests may all have passed, so
    // the tallies alone give no hint why the run is exiting non-zero.
    assert!(stdout.contains("1 file needs formatting"), "got:\n{stdout}");
    assert_eq!(code, 1);
}

#[test]
fn a_file_that_does_not_parse_gets_no_formatting_complaint() {
    // The parse error is the diagnostic that helps; "needs formatting" stacked
    // on top of a source the formatter couldn't read is noise.
    let (_stdout, stderr, code) = check("unparseable", "this is not valid aipl at all\n");
    assert!(
        stderr.contains("expected an item or end of input"),
        "expected the parse error, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("needs formatting"),
        "expected no formatting complaint, got:\n{stderr}"
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
        "import { equal as ==, greater_than as > } from builtins;\n\n\
         fn a() -> i64 { 1 }.test({\n    assert(a() == 1);\n})\n\n\
         fn b() -> i64 { 2 }.test({\n    assert(b() == 2);\n    assert(b() > 0);\n})\n",
    );
    // Passing tests print nothing; only the summary.
    assert_eq!(stdout, "2 tests: 2 passed, 0 failed\n");
    assert_eq!(code, 0);
}

#[test]
fn a_failing_assert_reports_and_exits_one() {
    let (stdout, _stderr, code) = check(
        "one_fail",
        "import { equal as == } from builtins;\n\n\
         fn foo() -> i64 { 42 }.test({\n    assert(foo() == 42);\n})\n\n\
         fn bar() -> i64 { 5 }.test({\n    assert(bar() == 6);\n})\n",
    );
    assert!(
        stdout.contains("test bar ... FAIL"),
        "expected a FAIL header, got:\n{stdout}"
    );
    // The location is the asserted condition's line and source text (line 8 of
    // the canonically-formatted source above). Only the tail is pinned: the file
    // half is a path relative to the working directory, and this fixture lives
    // under `CARGO_TARGET_TMPDIR`, which moves with `CARGO_TARGET_DIR` — see
    // `a_failure_location_names_its_file` for the test that does pin it.
    assert!(
        stdout.contains(":8 bar() == 6"),
        "expected the assert location, got:\n{stdout}"
    );
    assert!(stdout.contains("2 tests: 1 passed, 1 failed"));
    assert_eq!(code, 1);
}

#[test]
fn a_failure_location_names_its_file_relative_to_where_check_was_invoked() {
    // `assert` and `trace` bake their locations through one formatter, so a
    // failure names its file the same way a trace does — which it could not do
    // while the rewrite lived in the parser, where the filename was the
    // placeholder `input`.
    let (stdout, _stderr, code) = check_tree("assert_label", &[("sub/fails.aipl", FAILS)], &[]);
    assert!(
        stdout.contains("assert failed at sub/fails.aipl:4 b() == 6"),
        "expected a project-relative assert location, got:\n{stdout}"
    );
    assert_eq!(code, 1);
}

#[test]
fn all_asserts_in_a_test_run_and_each_failure_is_reported() {
    let (stdout, _stderr, code) = check(
        "run_all",
        "import { equal as == } from builtins;\n\n\
         fn bar() -> i64 { 5 }.test({\n    assert(bar() == 6);\n    assert(bar() == 7);\n})\n",
    );
    // Both failing asserts report — the first failure doesn't abort the test.
    assert!(stdout.contains(":4 bar() == 6"), "got:\n{stdout}");
    assert!(stdout.contains(":5 bar() == 7"), "got:\n{stdout}");
    assert!(stdout.contains("1 tests: 0 passed, 1 failed"));
    assert_eq!(code, 1);
}

#[test]
fn a_test_may_call_effectful_functions() {
    // "Implicitly allow all effects": a test can call a `!prints` function with
    // no effect annotation. (Its output lands in the check output.)
    let (stdout, _stderr, code) = check(
        "effects",
        "import { equal as ==, print } from builtins;\n\n\
         fn greet() !prints { print(\"hi\") }.test({\n    greet();\n    assert(1 == 1);\n})\n",
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
        "fn f() -> i64 {\n    assert(true);\n    0\n}\n\nfn main() -> i64 { f() }\n",
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
    let (_stdout, stderr, code) = check(
        "bad_assert",
        "fn f() -> i64 { 1 }.test({\n    assert(f());\n})\n",
    );
    assert!(!stderr.is_empty(), "expected a type error on stderr");
    assert_eq!(code, 1);
}

// ---------- trace(..) ----------

/// A trace inside a `.test` body: the test passes, so nothing but `trace` itself
/// stands between this file and a green run.
///
/// That `check` prints the trace, reports the passing test, and *still* exits
/// non-zero is pinned in the corpus — `tests/cases/debug/trace_fails_check.aipl`,
/// whose `--- check ---` and `--- check exit code ---` sections are that claim
/// written out. What stays here is everything a case cannot reach: `check`'s
/// stderr, and the working directory it was invoked from.
const TRACED: &str = "import { equal as ==, wrapping_add as + } from builtins;\n\n\
     fn bump(v: i64) -> i64 { trace(v + 2) }.test({\n    assert(bump(5) == 7);\n})\n";

#[test]
fn a_trace_names_its_file_relative_to_where_check_was_invoked() {
    // The regression this guards: `check` stages each file's companions into a
    // scratch directory and runs the tests from *there*, so a label taken from
    // the working directory at load time would name the scratch directory
    // instead of the project.
    let (stdout, _stderr, code) = check_tree("trace_label", &[("sub/traced.aipl", TRACED)], &[]);
    assert!(
        stdout.contains("sub/traced.aipl:3 v + 2 = 7"),
        "expected a project-relative trace label, got:\n{stdout}"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_trace_in_an_imported_file_fails_the_importing_files_check() {
    // "the file or any file it imports": the trace is nowhere in `importer.aipl`,
    // and checking it still fails.
    let (stdout, stderr, code) = check_tree(
        "trace_imported",
        &[
            (
                "importer.aipl",
                "import { equal as == } from builtins;\n\
                 import { bump } from \"./sub/lib.aipl\";\n\n\
                 fn use_bump() -> i64 { bump(5) }.test({\n    assert(use_bump() == 7);\n})\n",
            ),
            (
                "sub/lib.aipl",
                "import { wrapping_add as + } from builtins;\n\n\
                 pub fn bump(v: i64) -> i64 { trace(v + 2) }\n",
            ),
        ],
        &["importer.aipl"],
    );
    assert!(
        stdout.contains("sub/lib.aipl:3 v + 2 = 7"),
        "expected the imported file's trace output, got:\n{stdout}"
    );
    assert!(
        stderr.contains("`trace(..)` prints for debugging"),
        "expected the trace diagnostic, got:\n{stderr}"
    );
    // The caret underlines the traced expression, not the whole `trace(..)` call.
    assert!(stderr.contains("trace(v + 2)"), "got:\n{stderr}");
    assert_eq!(code, 1);
}

#[test]
fn a_trace_does_not_stop_the_batch_and_is_counted_separately() {
    let (stdout, stderr, code) = check_tree(
        "trace_keep_going",
        &[
            ("a_traced.aipl", TRACED),
            ("b_ok.aipl", PASSES),
            ("c_fails.aipl", FAILS),
        ],
        &[],
    );
    // The traced file sorts first, so the run would end there if it aborted.
    assert!(
        stdout.contains("test ./c_fails.aipl::b ... FAIL"),
        "the run should continue past the traced file, got:\n{stdout}"
    );
    assert!(
        stderr.contains("`trace(..)` prints for debugging"),
        "got:\n{stderr}"
    );
    // Called out separately for the same reason "needs formatting" is: the traced
    // file's own tests passed, so the tallies give no hint why this exits
    // non-zero.
    assert!(
        stdout.contains("1 file still calls trace()"),
        "got:\n{stdout}"
    );
    assert_eq!(code, 1);
}

// ---------------------------------------------------------------------------
// Recorded sections: `check` validates a file's trailing `--- section ---`
// blocks, so a fixture is checked by the command a user would run.
//
// These are written to be read as examples: each states the whole of what `check`
// printed, so this is what a mismatch (or a clean run) looks like.
// ---------------------------------------------------------------------------

/// Write `src` to `<name>.aipl` in a scratch directory and run `aipl check` on it
/// *from that directory*, by its bare name — so what `check` prints names the file
/// `name.aipl`, not a path that depends on where the target directory is.
fn check_here(name: &str, src: &str) -> (String, String, i32) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check_here");
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = format!("{name}.aipl");
    fs::write(dir.join(&file), src).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("check")
        .arg(&file)
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

/// The summary a single-file `check` always ends with; a file with no `.test`
/// blocks has none to count.
const NO_TESTS: &str = "0 tests: 0 passed, 0 failed\n";

const GREETS: &str =
    "import { print } from builtins;\n\nfn main() !prints {\n    print(\"hi\");\n}\n";

#[test]
fn a_file_without_sections_does_not_have_its_main_run() {
    // `main` here exits 3 and would print; with no section asking for a run it is
    // not executed, so the file passes. Running it unasked would make `check` a
    // way to start a server.
    let (stdout, stderr, code) = check_here(
        "sections_none",
        "import { print } from builtins;\n\nfn main() !prints -> i64 {\n    print(\"hi\");\n    3\n}\n",
    );
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));
}

#[test]
fn matching_stdout_and_exit_code_sections_pass() {
    let src = format!("{GREETS}--- stdout ---\nhi\n--- exit code ---\n0\n");
    let (stdout, stderr, code) = check_here("sections_match", &src);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));
}

#[test]
fn a_stdout_mismatch_names_the_file_the_section_and_both_sides() {
    let src = format!("{GREETS}--- stdout ---\nbye\n");
    let (stdout, stderr, code) = check_here("sections_stdout_wrong", &src);
    assert_eq!(stdout, NO_TESTS);
    assert_eq!(
        stderr,
        "\
sections_stdout_wrong.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
"
    );
    assert_eq!(code, 1);
}

#[test]
fn an_absent_section_records_silence_and_a_clean_exit() {
    // Only `exit code` is written, but a run is now being checked, so the
    // missing `stdout` means "prints nothing" — and `hi` is a finding.
    let src = format!("{GREETS}--- exit code ---\n0\n");
    let (stdout, stderr, code) = check_here("sections_absent_means_empty", &src);
    assert_eq!(stdout, NO_TESTS);
    assert_eq!(
        stderr,
        "\
sections_absent_means_empty.aipl: `stdout` mismatch
--- expected ---

--- actual ---
hi
"
    );
    assert_eq!(code, 1);
}

#[test]
fn an_exit_code_section_is_compared_with_the_status_main_returns() {
    let src = "fn main() -> i64 { 3 }\n--- exit code ---\n3\n";
    let (stdout, stderr, code) = check_here("sections_exit_ok", src);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));

    let src = "fn main() -> i64 { 3 }\n--- exit code ---\n4\n";
    let (stdout, stderr, code) = check_here("sections_exit_wrong", src);
    assert_eq!(stdout, NO_TESTS);
    assert_eq!(
        stderr,
        "\
sections_exit_wrong.aipl: `exit code` mismatch
--- expected ---
4
--- actual ---
3
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_cli_section_is_what_main_receives() {
    let src = "import { len, print, to_str } from builtins;\n\n\
               fn main(args: str[]) !prints {\n    print(to_str(len(args)));\n}\n\
               --- cli ---\none\ntwo\n--- stdout ---\n2\n";
    let (stdout, stderr, code) = check_here("sections_cli", src);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));
}

#[test]
fn an_expect_file_section_checks_what_the_run_wrote() {
    let writer = "import { print, to_str, write_string_to_file } from builtins;\n\n\
                  fn main() !prints !write_files {\n    print(to_str(write_string_to_file(\"out.txt\", \"written\")));\n}\n\
                  --- stdout ---\nok()\n";
    let (stdout, stderr, code) = check_here(
        "sections_expect_ok",
        &format!("{writer}--- expect file: out.txt ---\nwritten\n"),
    );
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));

    let (stdout, stderr, code) = check_here(
        "sections_expect_wrong",
        &format!("{writer}--- expect file: out.txt ---\nother\n"),
    );
    assert_eq!(stdout, NO_TESTS);
    assert_eq!(
        stderr,
        "\
sections_expect_wrong.aipl: `expect file: out.txt` mismatch
--- expected ---
other
--- actual ---
written
"
    );
    assert_eq!(code, 1);

    let (stdout, stderr, code) = check_here(
        "sections_expect_missing",
        &format!("{writer}--- expect file: never.txt ---\nx\n"),
    );
    assert_eq!(stdout, NO_TESTS);
    assert_eq!(
        stderr,
        "sections_expect_missing.aipl: expected output file \"never.txt\" was not written: No such file or directory (os error 2)\n"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_run_section_on_a_file_with_no_main_is_reported() {
    let src = "fn f() -> i64 { 1 }\n--- stdout ---\nx\n";
    let (stdout, stderr, code) = check_here("sections_no_main", src);
    assert_eq!(stdout, NO_TESTS);
    assert_eq!(
        stderr,
        "sections_no_main.aipl: a file with no `main` runs only its `.test` blocks — it can't have a stdout/stderr/exit/cli/expect-file section\n"
    );
    assert_eq!(code, 1);
}

/// What compiling `fn bad() -> i64 { true }` reports, as an `errors` section
/// records it: the file is named `./`-relative to where `check` was started.
const BAD_ERRORS: &str = "\
error: fn \"bad\": body returns bool, but the declared return type is i64
 --> ./bad.aipl:1:19
  |
1 | fn bad() -> i64 { true }
  |                   ^^^^";

#[test]
fn an_errors_section_makes_a_failing_compile_the_expected_outcome() {
    let program = "fn bad() -> i64 { true }\n";

    // Written as `?`, the section is wrong, and the mismatch shows what to write.
    let (stdout, stderr, code) = check_tree(
        "sections_errors_record",
        &[("bad.aipl", &format!("{program}--- errors ---\n?\n"))],
        &[],
    );
    assert_eq!(
        stdout,
        "1 file, 0 tests: 0 passed, 0 failed\n1 file has mismatched sections\n"
    );
    assert_eq!(
        stderr,
        format!(
            "./bad.aipl: `errors` mismatch\n--- expected ---\n?\n--- actual ---\n{BAD_ERRORS}\n"
        )
    );
    assert_eq!(code, 1);

    // Written as what it showed, it passes, however the file is named on the
    // command line (or found by walking the tree). A named file is checked on its
    // own and has no tests to summarize, since it never compiled; a walk or a
    // directory is a batch, which always ends in its one-line summary.
    for (args, summary) in [
        (&[][..], "1 file, 0 tests: 0 passed, 0 failed\n"),
        (&["."][..], "1 file, 0 tests: 0 passed, 0 failed\n"),
        (&["bad.aipl"][..], ""),
        (&["./bad.aipl"][..], ""),
    ] {
        let (stdout, stderr, code) = check_tree(
            "sections_errors_ok",
            &[(
                "bad.aipl",
                &format!("{program}--- errors ---\n{BAD_ERRORS}\n"),
            )],
            args,
        );
        assert_eq!(
            (stdout.as_str(), stderr.as_str(), code),
            (summary, "", 0),
            "args {args:?}"
        );
    }
}

#[test]
fn an_errors_section_on_a_file_that_compiles_is_reported() {
    let src = "fn ok() -> i64 { 1 }\n--- errors ---\nerror: nope\n";
    let (stdout, stderr, code) = check_here("sections_errors_compiles", src);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "sections_errors_compiles.aipl: expected an error, but compilation succeeded\n"
    );
    assert_eq!(code, 1);
}

#[test]
fn an_errors_section_cannot_sit_with_a_section_that_observes_a_run() {
    let src = "fn bad() -> i64 { true }\n--- errors ---\nx\n--- stdout ---\ny\n";
    let (stdout, stderr, code) = check_here("sections_errors_conflict", src);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
sections_errors_conflict.aipl: `errors` section is mutually exclusive with stdout/stderr/exit code
sections_errors_conflict.aipl: `errors` mismatch
--- expected ---
x
--- actual ---
error: fn \"bad\": body returns bool, but the declared return type is i64
 --> ./sections_errors_conflict.aipl:1:19
  |
1 | fn bad() -> i64 { true }
  |                   ^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn companions_are_staged_so_the_file_can_import_them() {
    // `helper.aipl` exists only as a `--- file: ---` section; the import resolves
    // because the file is loaded from a staged copy beside it.
    let src = "import { print, to_str } from builtins;\n\
               import { answer } from \"./helper.aipl\";\n\n\
               fn main() !prints {\n    print(to_str(answer()));\n}\n\
               --- file: helper.aipl ---\npub fn answer() -> i64 { 42 }\n\
               --- stdout ---\n42\n";
    let (stdout, stderr, code) = check_here("sections_companion", src);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));
}

#[test]
fn companions_are_staged_for_the_tests_too() {
    let src = "import { equal as == } from builtins;\n\
               import { answer } from \"./helper.aipl\";\n\n\
               fn f() -> i64 { answer() }.test({\n    assert(f() == 42);\n})\n\
               --- file: helper.aipl ---\npub fn answer() -> i64 { 42 }\n";
    let (stdout, stderr, code) = check_here("sections_companion_tests", src);
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        ("1 tests: 1 passed, 0 failed\n", "", 0)
    );
}

#[test]
fn a_mismatched_section_does_not_hide_a_failing_test() {
    let src = "import { equal as ==, print } from builtins;\n\n\
               fn f() -> i64 { 1 }.test({\n    assert(f() == 2);\n})\n\n\
               fn main() !prints {\n    print(\"hi\");\n}\n\
               --- stdout ---\nbye\n";
    let (stdout, stderr, code) = check_here("sections_with_failing_test", src);
    assert_eq!(
        stdout,
        "\
test f ... FAIL
  assert failed at sections_with_failing_test.aipl:4 f() == 2
1 tests: 0 passed, 1 failed
"
    );
    assert_eq!(
        stderr,
        "\
sections_with_failing_test.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
"
    );
    assert_eq!(code, 1);
}

#[test]
fn the_batch_counts_files_with_mismatched_sections_and_keeps_going() {
    let (stdout, stderr, code) = check_tree(
        "sections_batch",
        &[
            ("a_wrong.aipl", &format!("{GREETS}--- stdout ---\nbye\n")),
            ("b_ok.aipl", &format!("{GREETS}--- stdout ---\nhi\n")),
            ("c_wrong.aipl", &format!("{GREETS}--- stdout ---\nbye\n")),
        ],
        &[],
    );
    assert_eq!(
        stderr,
        "\
./a_wrong.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
./c_wrong.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
"
    );
    assert_eq!(
        stdout,
        "3 files, 0 tests: 0 passed, 0 failed\n2 files have mismatched sections\n"
    );
    assert_eq!(code, 1);
}

#[test]
fn ignore_sections_checks_the_tests_alone() {
    let src = format!("{GREETS}--- stdout ---\nbye\n");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check_here");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(dir.join("sections_ignored.aipl"), src).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(["check", "--ignore-sections", "sections_ignored.aipl"])
        .current_dir(&dir)
        .output()
        .expect("run aipl check");
    assert_eq!(
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code()
        ),
        (NO_TESTS.to_string(), String::new(), Some(0))
    );
}

#[test]
fn a_malformed_section_is_the_files_error_not_a_dropped_expectation() {
    let src = format!("{GREETS}--- stdotu ---\nhi\n");
    let (stdout, stderr, code) = check_here("sections_unknown", &src);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "sections_unknown.aipl: unknown section name \"stdotu\"\n"
    );
    assert_eq!(code, 1);
}

// ---------------------------------------------------------------------------
// `.performance` blocks and `measure`: run by `check`, in an instrumented build.
// The corpus (`tests/cases/performance/`) holds the claims that pass and the one
// that is pinned failing; what lives here is what a case cannot reach — the
// diagnostics a block's compile errors print to stderr, and batch labelling.
// ---------------------------------------------------------------------------

/// A function whose `.performance` block makes one claim, parameterized on the
/// claim. Canonical format, so no fixture here fails over whitespace.
fn performance_block(claim: &str) -> String {
    format!(
        "import {{ equal as ==, measure }} from builtins;\n\n\
         fn work() {{}}.performance({{\n    {claim}\n}})\n"
    )
}

#[test]
fn a_performance_block_that_holds_passes_and_is_counted() {
    let (stdout, stderr, code) = check_here(
        "performance_holds",
        &performance_block("assert(measure(\"scalar\", 3, |n| n).allocations == 0);"),
    );
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        ("1 tests: 1 passed, 0 failed\n", "", 0)
    );
}

#[test]
fn a_performance_claim_that_does_not_hold_fails_the_check() {
    let (stdout, stderr, code) = check_here(
        "performance_fails",
        &performance_block("assert(measure(\"scalar\", 3, |n| n).instructions == 0);"),
    );
    assert_eq!(
        stdout,
        "\
test work.performance ... FAIL
  assert failed at performance_fails.aipl:4 measure(\"scalar\", 3, |n| n).instructions == 0
1 tests: 0 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn performance_and_ordinary_tests_share_one_summary_and_a_failure_of_either_fails() {
    let src = "import { equal as ==, measure } from builtins;\n\n\
               fn one() -> i64 { 1 }.test({\n    assert(one() == 1);\n}).performance({\n    \
               assert(measure(\"scalar\", 3, |n| n).allocations == 0);\n})\n\n\
               fn two() -> i64 { 2 }.test({\n    assert(two() == 3);\n})\n";
    let (stdout, stderr, code) = check_here("performance_summary", src);
    // Three tests — `one`, `one.performance`, `two` — and the one that fails is
    // the ordinary one; the performance block passes silently.
    assert_eq!(
        stdout,
        "\
test two ... FAIL
  assert failed at performance_summary.aipl:10 two() == 3
3 tests: 2 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn a_performance_failure_in_a_batch_names_its_file_and_does_not_stop_the_run() {
    let (stdout, stderr, code) = check_tree(
        "performance_batch",
        &[
            (
                "a_fails.aipl",
                &performance_block("assert(measure(\"scalar\", 3, |n| n).instructions == 0);"),
            ),
            (
                "b_holds.aipl",
                &performance_block("assert(measure(\"scalar\", 3, |n| n).allocations == 0);"),
            ),
        ],
        &[],
    );
    // The second file ran, and both blocks are in the one aggregate.
    assert_eq!(
        stdout,
        "\
test ./a_fails.aipl::work.performance ... FAIL
  assert failed at a_fails.aipl:4 measure(\"scalar\", 3, |n| n).instructions == 0
2 files, 2 tests: 1 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn measure_outside_a_performance_block_is_refused() {
    // Not in a `.test` block either: a performance block is the only place that
    // may count anything.
    let in_test = "import { equal as ==, measure } from builtins;\n\n\
                   fn one() -> i64 { 1 }.test({\n    assert(measure(\"x\", 1, |n| n).allocations == 0);\n})\n";
    let (stdout, stderr, code) = check_here("measure_in_test", in_test);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
error: fn \"measure\" can only be called from a `.performance` block
 --> measure_in_test.aipl:4:12
  |
4 |     assert(measure(\"x\", 1, |n| n).allocations == 0);
  |            ^^^^^^^^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);

    let in_fn = "import { measure } from builtins;\n\n\
                 fn main() {\n    measure(\"x\", 1, |n| n);\n}\n";
    let (stdout, stderr, code) = check_here("measure_in_main", in_fn);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
error: fn \"measure\" can only be called from a `.performance` block
 --> measure_in_main.aipl:4:5
  |
4 |     measure(\"x\", 1, |n| n);
  |     ^^^^^^^^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_performance_block_may_not_use_an_effect_directly_or_through_what_it_measures() {
    let src = "\
import { measure, print } from builtins;

fn noisy(n: i64) !prints -> i64 {
    print(\"hi\");
    n
}

fn work() {}.performance({
    print(\"direct\");
    measure(\"named\", 1, noisy);
    measure(
        \"closure\",
        1,
        |n| {
            print(\"inside\");
            n
        },
    );
})
";
    let (stdout, stderr, code) = check_here("performance_effects", src);
    // Each of the three uses is refused, and each says why: printing in the block
    // itself, handing it a function that prints, and a closure that prints.
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
error: effects are not supported in a `.performance` block yet: fn \"print\" has effect \"!prints\"
 --> performance_effects.aipl:9:5
  |
9 |     print(\"direct\");
  |     ^^^^^^^^^^^^^^^

error: effects are not supported in a `.performance` block yet: fn \"noisy\" has effect \"!prints\"
  --> performance_effects.aipl:10:25
   |
10 |     measure(\"named\", 1, noisy);
   |                         ^^^^^

error: effects are not supported in a `.performance` block yet: fn \"print\" has effect \"!prints\"
  --> performance_effects.aipl:15:13
   |
15 |             print(\"inside\");
   |             ^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_literal_measurement_label_used_twice_in_one_block_is_refused() {
    let src = "import { measure } from builtins;\n\n\
               fn work() {}.performance({\n    measure(\"same\", 1, |n| n);\n    measure(\"same\", 2, |n| n);\n})\n";
    let (stdout, stderr, code) = check_here("performance_duplicate_label", src);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
error: duplicate measurement label \"same\" in this `.performance` block — a report could not tell the two apart
 --> performance_duplicate_label.aipl:5:13
  |
5 |     measure(\"same\", 2, |n| n);
  |             ^^^^^^
"
    );
    assert_eq!(code, 1);

    // The same label in two blocks is two different reports, and fine.
    let two_blocks = "import { measure } from builtins;\n\n\
                      fn a() {}.performance({\n    measure(\"same\", 1, |n| n);\n})\n\n\
                      fn b() {}.performance({\n    measure(\"same\", 1, |n| n);\n})\n";
    let (stdout, stderr, code) = check_here("performance_label_per_block", two_blocks);
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        ("2 tests: 2 passed, 0 failed\n", "", 0)
    );
}

#[test]
fn a_type_error_in_a_performance_block_is_reported_with_its_source() {
    let (stdout, stderr, code) = check_here(
        "performance_type_error",
        &performance_block("assert(measure(\"x\", 1, |n| n).allocations == true);"),
    );
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
error: \"==\" between u64 and bool: both sides must be the same type
 --> performance_type_error.aipl:4:12
  |
4 |     assert(measure(\"x\", 1, |n| n).allocations == true);
  |            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_performance_block_is_left_out_of_what_run_and_build_make() {
    // Same program with and without a block that calls `measure`: `run` behaves
    // identically. (The IR and binary being byte-identical is checked by hand
    // when the mechanism changes; this is the behavioural half.)
    let src = "import { measure, print } from builtins;\n\n\
               fn work() {}.performance({\n    measure(\"x\", 1, |n| n);\n})\n\n\
               fn main() !prints {\n    print(\"hi\");\n}\n";
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check_here");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(dir.join("performance_run.aipl"), src).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(["run", "performance_run.aipl"])
        .current_dir(&dir)
        .output()
        .expect("run aipl run");
    assert_eq!(
        (
            String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code()
        ),
        ("hi\n0\n".to_string(), String::new(), Some(0))
    );
}
