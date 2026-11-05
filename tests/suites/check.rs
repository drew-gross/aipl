//! Integration tests for the `aipl check` command — the in-language test
//! runner. These drive the real CLI binary as a subprocess (the cases harness
//! only runs a program's `main`, so it can't exercise `check`), staging a
//! temp `.aipl` file — or a whole temp tree, for the batch mode a bare
//! `aipl check` runs — and asserting on `check`'s stdout and exit code.
//!
//! Every fixture source here is written in the formatter's canonical format, since
//! `check` reports unformatted files: an offhandedly-spaced import would make a
//! test about test *counts* fail over whitespace. Reformat one by round-tripping
//! it through `aipl check --fix` rather than by eye.

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
    assert_eq!(
        stdout,
        "./a.aipl:\n    a: 1 assertion passed\n./sub/b.aipl:\n    a: 1 assertion passed\n\
         2 files, 2 tests: 2 passed, 0 failed\n"
    );
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
        stdout.contains("declared return type is i64"),
        "expected the compile error, got:\n{stdout}"
    );
    assert_eq!(stderr, "");
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

/// A file with `--- section ---` blocks is a case of its own, so another file may
/// not import it: the importer would load its code and drop the sections, its
/// `.test` blocks and its `file:` companions. (A case cannot show this: the cases
/// harness stages only a case's `file:` companions, and a companion cannot hold
/// sections.)
#[test]
fn a_file_with_sections_cannot_be_imported() {
    let (stdout, stderr, code) = check_tree(
        "import_sectioned",
        &[
            (
                "main.aipl",
                "import { shared } from \"./sectioned.aipl\";\n\nfn main() -> i64 { shared(1) }\n",
            ),
            (
                "sectioned.aipl",
                "import { wrapping_add as + } from builtins;\n\n\
                 pub fn shared(n: i64) -> i64 { n + 1 }\n--- stdout ---\n",
            ),
        ],
        &["main.aipl"],
    );
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "error: import \"./sectioned.aipl\": that file has a `--- stdout ---` section, which makes it \
         a case of its own, and a file with sections cannot be imported — move what is shared into \
         a file without sections\n \
         --> ./main.aipl:1:24\n  \
         |\n\
         1 | import { shared } from \"./sectioned.aipl\";\n  \
         |                        ^^^^^^^^^^^^^^^^^^\n"
    );
    assert_eq!(code, 1);
}

/// A `.test` body declares no effect, so one that uses an effect it has not
/// shimmed is refused, and a `shim` inside the body discharges it.
#[test]
fn a_test_cannot_let_an_effect_escape() {
    let unshimmed = "import { print } from builtins;\n\n\
         fn greet() !prints {\n    print(\"hello\");\n}.test({\n    greet();\n})\n";
    let (stdout, stderr, code) = check_here("test_unshimmed_effect", unshimmed);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "error: fn \"greet\" has effect \"!prints\" but the calling function does not declare it\n \
         --> ./test_unshimmed_effect.aipl:6:5\n  \
         |\n\
         6 |     greet();\n  \
         |     ^^^^^^^\n"
    );
    assert_eq!(code, 1);

    let shimmed = "import { print } from builtins;\n\n\
         fn greet() !prints {\n    print(\"hello\");\n}.test({\n    \
         shim prints { print = |text| {}, put = |text| {} } {\n        greet();\n    };\n})\n";
    let (stdout, stderr, code) = check_here("test_shimmed_effect", shimmed);
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        (
            "./test_shimmed_effect.aipl:\n    greet: 0 assertions passed\n1 tests: 1 passed, 0 failed\n",
            "",
            0
        )
    );
}

/// Files checked in one process do not see each other's cached constants: the
/// first file's shim closure keeps a `str[]` in a cell, and the second file's
/// own constant must not read it.
#[test]
fn one_files_cached_constants_do_not_reach_the_next() {
    let (stdout, stderr, code) = check_tree(
        "constants_across_files",
        &[
            (
                "a.aipl",
                "import { equal as ==, push, write_string_to_file } from builtins;\n\n\
                 fn save() !write_files -> !Error { write_string_to_file(\"x\", \"y\") }.test({\n    \
                 mut written: str[] = [];\n    \
                 shim write_files {\n        write_string_to_file = |path, contents| {\n            \
                 set written.push(contents);\n            ok()\n        },\n    } {\n        \
                 assert(save() == ok());\n    };\n    assert(written == [\"y\"]);\n})\n",
            ),
            (
                "b.aipl",
                "import { equal as == } from builtins;\n\nfn make() -> str[] { [\"p\", \"q\"] }\n\n\
                 let NAMES: str[] = make();\n\n\
                 fn names() -> str[] { NAMES }.test({\n    assert(names() == [\"p\", \"q\"]);\n})\n",
            ),
        ],
        &["a.aipl", "b.aipl"],
    );
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        (
            "./a.aipl:\n    save: 2 assertions passed\n./b.aipl:\n    names: 1 assertion passed\n\
             2 files, 2 tests: 2 passed, 0 failed\n",
            "",
            0
        )
    );
}

/// A top-level `test({ .. })` is a test attached to no function: it runs with the
/// file's other tests and is reported by the line it starts on.
#[test]
fn a_top_level_test_block_runs_and_is_reported_by_its_position() {
    let src = "import { equal as == } from builtins;\n\ntest({\n    assert(1 == 1);\n})\n\n\
               test({\n    assert(1 == 2);\n})\n";
    let (stdout, stderr, code) = check_here("top_level_test_blocks", src);
    assert_eq!(
        stdout,
        "./top_level_test_blocks.aipl:\n    standalone tests: 1 assertion passed, 1 failed\n\
         test failures:\ntest block 2 ... FAIL\n  assert failed at ./top_level_test_blocks.aipl:8 1 == 2\n\
         2 tests: 1 passed, 1 failed\n"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

/// Failures are gathered at the bottom, so nobody scrolls past the passing files to
/// find them: a failing file that comes first still reports after the one that
/// passes.
#[test]
fn failures_come_after_every_file_that_passed() {
    let (stdout, stderr, code) = check_tree(
        "failures_last",
        &[("a_fails.aipl", FAILS), ("b_passes.aipl", PASSES)],
        &[],
    );
    assert_eq!(
        stdout,
        "./a_fails.aipl:\n    b: 0 assertions passed, 1 failed\n\
         ./b_passes.aipl:\n    a: 1 assertion passed\n\
         test failures:\ntest ./a_fails.aipl::b ... FAIL\n  assert failed at ./a_fails.aipl:4 b() == 6\n\
         2 files, 2 tests: 1 passed, 1 failed\n"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

/// `PASSES` with the import's inner spacing squeezed — parses and passes, but
/// isn't what the formatter would write.
const UNFORMATTED: &str = "import { equal as ==} from builtins;\n\
     fn a() -> i64 { 1 }.test({ assert(a() == 1); })\n";

#[test]
fn an_unformatted_file_fails_even_when_its_tests_pass() {
    let (stdout, stderr, code) = check_here("unformatted", UNFORMATTED);
    // The tests still ran — formatting is reported alongside them, not instead — and the
    // diagnostic names the command that fixes it.
    assert_eq!(
        stdout,
        "./unformatted.aipl: needs formatting (run `aipl check --fix ./unformatted.aipl`)\n\
         ./unformatted.aipl:\n    a: 1 assertion passed\n1 tests: 1 passed, 0 failed\n"
    );
    assert_eq!(stderr, "");
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
    assert!(stdout.contains("needs formatting"), "got:\n{stdout}");
    assert_eq!(stderr, "");
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
        stdout.contains("./a_unformatted.aipl: needs formatting"),
        "expected a file-qualified formatting diagnostic, got:\n{stdout}"
    );
    assert_eq!(stderr, "");
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
    let (stdout, stderr, code) = check("unparseable", "this is not valid aipl at all\n");
    assert!(
        stdout.contains("expected an item or end of input"),
        "expected the parse error, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("needs formatting"),
        "expected no formatting complaint, got:\n{stdout}"
    );
    assert_eq!(stderr, "");
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
    assert_eq!(
        stdout,
        "./a.aipl:\n    a: 1 assertion passed\n1 file, 1 tests: 1 passed, 0 failed\n"
    );
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
    assert_eq!(
        stdout,
        "./sub/a.aipl:\n    a: 1 assertion passed\n1 file, 1 tests: 1 passed, 0 failed\n"
    );
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
    assert_eq!(
        stdout,
        "./a.aipl:\n    a: 1 assertion passed\n./b.aipl:\n    a: 1 assertion passed\n\
         2 files, 2 tests: 2 passed, 0 failed\n"
    );
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
    // Passing tests are listed under their file, with how many assertions held. (The
    // file is named by the absolute path it was staged at.)
    let (file, rows) = stdout.split_once(":\n").expect("a file heading");
    assert!(file.ends_with("all_pass.aipl"), "got {file:?}");
    assert_eq!(
        rows,
        "    a: 1 assertion passed\n    b: 2 assertions passed\n2 tests: 2 passed, 0 failed\n"
    );
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
        stdout.contains("assert failed at ./sub/fails.aipl:4 b() == 6"),
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
fn assert_outside_a_test_is_a_compile_error() {
    let (stdout, stderr, code) = check(
        "assert_outside",
        "fn f() -> i64 {\n    assert(true);\n    0\n}\n\nfn main() -> i64 { f() }\n",
    );
    // `assert` is rewritten only inside `.test` bodies, so elsewhere it's an
    // ordinary (undefined) call.
    assert!(
        stdout.contains("undefined fn \"assert\""),
        "expected an undefined-assert error, got:\n{stdout}"
    );
    assert_eq!(stderr, "");
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
    let (stdout, stderr, code) = check(
        "bad_assert",
        "fn f() -> i64 { 1 }.test({\n    assert(f());\n})\n",
    );
    assert!(
        stdout.starts_with("error:"),
        "expected a type error, got:\n{stdout}"
    );
    assert_eq!(stderr, "");
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
/// stream, and the working directory it was invoked from.
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
        stdout.contains("`trace(..)` prints for debugging"),
        "expected the trace diagnostic, got:\n{stdout}"
    );
    // The caret underlines the traced expression, not the whole `trace(..)` call.
    assert!(stdout.contains("trace(v + 2)"), "got:\n{stdout}");
    assert_eq!(stderr, "");
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
        stdout.contains("`trace(..)` prints for debugging"),
        "got:\n{stdout}"
    );
    assert_eq!(stderr, "");
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
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./sections_stdout_wrong.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
handoff-fill: section\tsections_stdout_wrong.aipl\tstdout
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

#[test]
fn a_mismatch_a_rewrite_repairs_asks_handoff_for_it_by_path_from_the_root() {
    // The same line a failing case prints, so `cargo handoff` reads `check` and the
    // harness alike. The case is the file's path from the directory `check` is run in.
    let src = format!("{GREETS}--- stdout ---\nbye\n");
    let (stdout, _, _) = check_tree("sections_fill_marker", &[("sub/wrong.aipl", &src)], &[]);
    let (asked, unreadable) = handoff::protocol::Fill::scan(&stdout);
    assert_eq!(
        asked,
        vec![handoff::protocol::Fill::Section {
            case: "sub/wrong.aipl".to_string(),
            section: "stdout".to_string(),
        }]
    );
    assert!(unreadable.is_empty(), "{unreadable:?}");

    // A finding no rewrite repairs asks for nothing: `errors` and `report` cannot
    // sit together, and the conflict itself has no fix.
    let src = "fn bad() -> i64 { true }\n--- errors ---\nx\n--- report ---\ny\n";
    let (stdout, _, _) = check_here("sections_fill_marker_conflict", src);
    let conflict = stdout.lines().next().unwrap_or_default();
    assert!(conflict.contains("cannot coexist"), "{conflict}");
    assert!(!conflict.contains("handoff-fill"), "{conflict}");
}

#[test]
fn a_lint_hit_fails_the_file_like_a_compile_error() {
    let src = "fn f(x: i64) -> i64 {\n    match (x) {\n        0 => { 1 },\n        _ => 2,\n    }\n}.test({\n    assert(f(0) == 1);\n})\n";
    let (stdout, stderr, code) = check_here("lint_hit", src);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
error: this arm's block holds nothing but its value — write the value as the arm's body, without the braces (or append #[allow] to this line to keep it)
 --> ./lint_hit.aipl:3:18
  |
3 |         0 => { 1 },
  |                  ^
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
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./sections_absent_means_empty.aipl: `stdout` mismatch
--- expected ---

--- actual ---
hi
handoff-fill: section\tsections_absent_means_empty.aipl\tstdout
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

#[test]
fn an_exit_code_section_is_compared_with_the_status_main_returns() {
    let src = "fn main() -> i64 { 3 }\n--- exit code ---\n3\n";
    let (stdout, stderr, code) = check_here("sections_exit_ok", src);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));

    let src = "fn main() -> i64 { 3 }\n--- exit code ---\n4\n";
    let (stdout, stderr, code) = check_here("sections_exit_wrong", src);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./sections_exit_wrong.aipl: `exit code` mismatch
--- expected ---
4
--- actual ---
3
handoff-fill: section\tsections_exit_wrong.aipl\texit code
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
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
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./sections_expect_wrong.aipl: `expect file: out.txt` mismatch
--- expected ---
other
--- actual ---
written
handoff-fill: section\tsections_expect_wrong.aipl\texpect file: out.txt
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);

    let (stdout, stderr, code) = check_here(
        "sections_expect_missing",
        &format!("{writer}--- expect file: never.txt ---\nx\n"),
    );
    // What the file reports, then the summary: one stream, in that order.
    let findings = "./sections_expect_missing.aipl: expected output file \"never.txt\" was not written: No such file or directory (os error 2)\n";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

#[test]
fn a_run_section_on_a_file_with_no_main_is_reported() {
    let src = "fn f() -> i64 { 1 }\n--- stdout ---\nx\n";
    let (stdout, stderr, code) = check_here("sections_no_main", src);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "./sections_no_main.aipl: a file with no `main` runs only its `.test` blocks — it can't have a stdout/stderr/exit/cli/expect-file section\n";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
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
        format!(
            "./bad.aipl: `errors` mismatch\n--- expected ---\n?\n--- actual ---\n{BAD_ERRORS}\nhandoff-fill: section\tbad.aipl\terrors\n\
             1 file, 0 tests: 0 passed, 0 failed\n1 file has mismatched sections\n"
        )
    );
    assert_eq!(stderr, "");
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
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "./sections_errors_compiles.aipl: expected an error, but compilation succeeded\n"
    );
    assert_eq!(code, 1);
}

#[test]
fn an_errors_section_cannot_sit_with_a_section_that_observes_a_run() {
    let src = "fn bad() -> i64 { true }\n--- errors ---\nx\n--- stdout ---\ny\n";
    let (stdout, stderr, code) = check_here("sections_errors_conflict", src);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
./sections_errors_conflict.aipl: `errors` section is mutually exclusive with stdout/stderr/exit code
./sections_errors_conflict.aipl: `errors` mismatch
--- expected ---
x
--- actual ---
error: fn \"bad\": body returns bool, but the declared return type is i64
 --> ./sections_errors_conflict.aipl:1:19
  |
1 | fn bad() -> i64 { true }
  |                   ^^^^
handoff-fill: section\tsections_errors_conflict.aipl\terrors
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
        (
            "./sections_companion_tests.aipl:\n    f: 1 assertion passed\n1 tests: 1 passed, 0 failed\n",
            "",
            0
        )
    );
}

#[test]
fn a_mismatched_section_does_not_hide_a_failing_test() {
    let src = "import { equal as ==, print } from builtins;\n\n\
               fn f() -> i64 { 1 }.test({\n    assert(f() == 2);\n})\n\n\
               fn main() !prints {\n    print(\"hi\");\n}\n\
               --- stdout ---\nbye\n";
    let (stdout, stderr, code) = check_here("sections_with_failing_test", src);
    // The file's own report (rows, then its findings), then the aggregate (the failing tests'
    // text and the count).
    assert_eq!(
        stdout,
        "\
./sections_with_failing_test.aipl:
    f: 0 assertions passed, 1 failed
./sections_with_failing_test.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
handoff-fill: section\tsections_with_failing_test.aipl\tstdout
test failures:
test f ... FAIL
  assert failed at ./sections_with_failing_test.aipl:4 f() == 2
1 tests: 0 passed, 1 failed
"
    );
    assert_eq!(stderr, "");
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
        stdout,
        "\
./a_wrong.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
handoff-fill: section\ta_wrong.aipl\tstdout
./c_wrong.aipl: `stdout` mismatch
--- expected ---
bye
--- actual ---
hi
handoff-fill: section\tc_wrong.aipl\tstdout
3 files, 0 tests: 0 passed, 0 failed
2 files have mismatched sections
"
    );
    assert_eq!(stderr, "");
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
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "./sections_unknown.aipl: unknown section name \"stdotu\"\n"
    );
    assert_eq!(code, 1);
}

/// What `aipl report` prints for `fn main() {}` — the figures are this platform's,
/// as the corpus's own sections are.
const REPORT_OF_EMPTY_MAIN: &str = "\
allocations       0
deallocations     0
reallocations     0
bytes allocated   0
peak live bytes   0
instructions      3
binary size       488 bytes (code 8, data 1, metadata 479)

function      calls  code bytes
main              1           8";

#[test]
fn a_report_section_that_matches_what_report_prints_passes() {
    let src = format!("fn main() {{}}\n--- report ---\n{REPORT_OF_EMPTY_MAIN}\n");
    let (stdout, stderr, code) = check_here("sections_report_ok", &src);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));
}

#[test]
fn a_report_section_that_does_not_match_shows_what_report_printed() {
    // The mismatch carries the whole actual report, so the section can be written
    // by copying it — or by running `aipl report`, which prints the same text.
    let src = "fn main() {}\n--- report ---\n?\n";
    let (stdout, stderr, code) = check_here("sections_report_wrong", src);
    // What the file reports, then the summary: one stream, in that order.
    let findings = format!(
            "./sections_report_wrong.aipl: `report` mismatch\n--- expected ---\n?\n--- actual ---\n{REPORT_OF_EMPTY_MAIN}\nhandoff-fill: section\tsections_report_wrong.aipl\treport\n"
        );
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

#[test]
fn a_report_is_of_the_run_the_cli_section_asks_for() {
    // The arguments come from `--- cli ---`, so this is a report of a run with two:
    // the array of them is what the program allocates, and the whole of what `check`
    // says about the mismatched section is the report that would have matched.
    let src = "import { len, print, to_str } from builtins;\n\n\
               fn main(args: str[]) !prints {\n    print(to_str(len(args)));\n}\n\
               --- cli ---\none\ntwo\n--- stdout ---\n2\n--- report ---\n?\n";
    let (stdout, stderr, code) = check_here("sections_report_cli", src);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./sections_report_cli.aipl: `report` mismatch
--- expected ---
?
--- actual ---
allocations       2
deallocations     2
reallocations     0
bytes allocated   97
peak live bytes   97
instructions      44
binary size       1400 bytes (code 400, data 1, metadata 999)

function            calls  code bytes
__to_str_u64            1         204
aipl_array_dec          1    external
aipl_array_new          1    external
aipl_shim_get           1    external
aipl_u64_len            2    external
aipl_write_u64          1    external
main                    1         196
handoff-fill: section\tsections_report_cli.aipl\treport
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

#[test]
fn a_report_section_cannot_sit_with_an_errors_section() {
    let src = "fn bad() -> i64 { true }\n--- errors ---\nx\n--- report ---\ny\n";
    let (stdout, stderr, code) = check_here("sections_report_errors", src);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
./sections_report_errors.aipl: `report` section requires a running program, so it cannot coexist with `errors`
./sections_report_errors.aipl: `errors` mismatch
--- expected ---
x
--- actual ---
error: fn \"bad\": body returns bool, but the declared return type is i64
 --> ./sections_report_errors.aipl:1:19
  |
1 | fn bad() -> i64 { true }
  |                   ^^^^
handoff-fill: section\tsections_report_errors.aipl\terrors
"
    );
    assert_eq!(code, 1);
}

// ---------------------------------------------------------------------------
// `.benchmark` blocks and `measure`: run by `check`, in an instrumented build.
// The corpus (`tests/cases/benchmark/`) holds the claims that pass and the one
// that is pinned failing; what lives here is what a case cannot reach — the
// diagnostics a block's compile errors print to stderr, and batch labelling.
// ---------------------------------------------------------------------------

/// A function whose `.benchmark` block makes one claim, parameterized on the
/// claim. Canonical format, so no fixture here fails over whitespace.
fn benchmark_block(claim: &str) -> String {
    format!(
        "import {{ equal as ==, measure }} from builtins;\n\n\
         fn work() {{}}.benchmark({{\n    {claim}\n}})\n"
    )
}

#[test]
fn a_benchmark_block_that_holds_passes_and_is_counted() {
    let (stdout, stderr, code) = check_here(
        "benchmark_holds",
        &benchmark_block("assert(measure(\"scalar\", 3, |n| n).allocations == 0);"),
    );
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        ("1 tests: 1 passed, 0 failed\n", "", 0)
    );
}

#[test]
fn a_benchmark_claim_that_does_not_hold_fails_the_check() {
    let (stdout, stderr, code) = check_here(
        "benchmark_fails",
        &benchmark_block("assert(measure(\"scalar\", 3, |n| n).instructions == 0);"),
    );
    assert_eq!(
        stdout,
        "\
test failures:
test work.benchmark ... FAIL
  assert failed at ./benchmark_fails.aipl:4 measure(\"scalar\", 3, |n| n).instructions == 0
1 tests: 0 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

/// `check` with `AIPL_INJECT_LEAK` set (a switch of the instrumented runtime that stops it counting
/// frees, since nothing in the language can leak on purpose): its stdout, stderr and exit status.
fn check_leaking(name: &str, src: &str, leak: bool) -> (String, String, Option<i32>) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check_here");
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = format!("{name}.aipl");
    fs::write(dir.join(&file), src).expect("write temp source");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aipl"));
    command.args(["check", &file]).current_dir(&dir);
    if leak {
        command.env("AIPL_INJECT_LEAK", "1");
    }
    let out = command.output().expect("run aipl check");
    (
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// The leak gate holds every file with tests to freeing what its tests allocate, whether or not the
/// file pins anything: the tests are run once more, as the synthesized driver, in an instrumented
/// build.
#[test]
fn the_leak_gate_covers_the_tests_of_a_file_that_pins_nothing() {
    let src = "import { equal as ==, heap_str } from builtins;\n\n\
               fn word() -> str { heap_str(\"hello\") }.test({\n    assert(word() == \"hello\");\n})\n";
    assert_eq!(
        check_leaking("leak_gate_tests", src, true),
        (
            "\
./leak_gate_tests.aipl:
    word: 1 assertion passed
test failures:
test memory leak in tests ... FAIL
  leak: 1 of 1 allocations were never freed (0 deallocations)
2 tests: 1 passed, 1 failed
"
            .to_string(),
            String::new(),
            Some(1)
        )
    );
    assert_eq!(
        check_leaking("leak_gate_tests", src, false),
        (
            "./leak_gate_tests.aipl:\n    word: 1 assertion passed\n1 tests: 1 passed, 0 failed\n"
                .to_string(),
            String::new(),
            Some(0)
        )
    );
}

/// `main` is run only when the file records what its run does, so only then can the gate hold it
/// to freeing what it allocates.
#[test]
fn the_leak_gate_holds_a_main_that_check_runs_and_leaves_a_plain_one_alone() {
    let recorded = "import { len, print, to_str } from builtins;\n\n\
                    fn main() !prints {\n    print(to_str([1, 2, 3].len()));\n}\n--- stdout ---\n3\n";
    assert_eq!(
        check_leaking("leak_gate_main", recorded, true),
        (
            "\
test failures:
test memory leak in main ... FAIL
  leak: 1 of 1 allocations were never freed (0 deallocations)
1 tests: 0 passed, 1 failed
"
            .to_string(),
            String::new(),
            Some(1)
        )
    );
    // The same program with nothing recorded is never run, so there is nothing to hold.
    let plain = "import { len, print, to_str } from builtins;\n\n\
                 fn main() !prints {\n    print(to_str([1, 2, 3].len()));\n}\n";
    assert_eq!(
        check_leaking("leak_gate_plain", plain, true),
        (
            "0 tests: 0 passed, 0 failed\n".to_string(),
            String::new(),
            Some(0)
        )
    );
}

/// A top-level `benchmark({ .. })` is a benchmark attached to no function: it runs with the
/// file's other benchmarks and is reported as `benchmark block N`, N counting the file's.
#[test]
fn a_top_level_benchmark_block_runs_and_is_reported_by_its_position() {
    let src = "import { equal as ==, measure } from builtins;\n\n\
               benchmark({\n    assert(measure(\"scalar\", 3, |n| n).allocations == 0);\n})\n\n\
               benchmark({\n    assert(measure(\"scalar\", 3, |n| n).instructions == 0);\n})\n";
    let (stdout, stderr, code) = check_here("benchmark_top_level", src);
    assert_eq!(
        stdout,
        "test failures:\ntest benchmark block 2 ... FAIL\n  \
         assert failed at ./benchmark_top_level.aipl:8 measure(\"scalar\", 3, |n| n).instructions == 0\n\
         2 tests: 1 passed, 1 failed\n"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn benchmark_and_ordinary_tests_share_one_summary_and_a_failure_of_either_fails() {
    let src = "import { equal as ==, measure } from builtins;\n\n\
               fn one() -> i64 { 1 }.test({\n    assert(one() == 1);\n}).benchmark({\n    \
               assert(measure(\"scalar\", 3, |n| n).allocations == 0);\n})\n\n\
               fn two() -> i64 { 2 }.test({\n    assert(two() == 3);\n})\n";
    let (stdout, stderr, code) = check_here("benchmark_summary", src);
    // Three tests — `one`, `one.benchmark`, `two` — and the one that fails is
    // the ordinary one. A benchmark block runs in another process, so it is
    // counted but has no row of its own.
    assert_eq!(
        stdout,
        "\
./benchmark_summary.aipl:
    one: 1 assertion passed
    two: 0 assertions passed, 1 failed
test failures:
test two ... FAIL
  assert failed at ./benchmark_summary.aipl:10 two() == 3
3 tests: 2 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn a_benchmark_failure_in_a_batch_names_its_file_and_does_not_stop_the_run() {
    let (stdout, stderr, code) = check_tree(
        "benchmark_batch",
        &[
            (
                "a_fails.aipl",
                &benchmark_block("assert(measure(\"scalar\", 3, |n| n).instructions == 0);"),
            ),
            (
                "b_holds.aipl",
                &benchmark_block("assert(measure(\"scalar\", 3, |n| n).allocations == 0);"),
            ),
        ],
        &[],
    );
    // The second file ran, and both blocks are in the one aggregate.
    assert_eq!(
        stdout,
        "\
test failures:
test ./a_fails.aipl::work.benchmark ... FAIL
  assert failed at ./a_fails.aipl:4 measure(\"scalar\", 3, |n| n).instructions == 0
2 files, 2 tests: 1 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn measure_outside_a_benchmark_block_is_refused() {
    // Not in a `.test` block either: a benchmark block is the only place that
    // may count anything.
    let in_test = "import { equal as ==, measure } from builtins;\n\n\
                   fn one() -> i64 { 1 }.test({\n    assert(measure(\"x\", 1, |n| n).allocations == 0);\n})\n";
    let (stdout, stderr, code) = check_here("measure_in_test", in_test);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
error: fn \"measure\" can only be called from a `.benchmark` block
 --> ./measure_in_test.aipl:4:12
  |
4 |     assert(measure(\"x\", 1, |n| n).allocations == 0);
  |            ^^^^^^^^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);

    let in_fn = "import { measure } from builtins;\n\n\
                 fn main() {\n    measure(\"x\", 1, |n| n);\n}\n";
    let (stdout, stderr, code) = check_here("measure_in_main", in_fn);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
error: fn \"measure\" can only be called from a `.benchmark` block
 --> ./measure_in_main.aipl:4:5
  |
4 |     measure(\"x\", 1, |n| n);
  |     ^^^^^^^^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_benchmark_block_may_not_use_an_effect_directly_or_through_what_it_measures() {
    let src = "\
import { measure, print } from builtins;

fn noisy(n: i64) !prints -> i64 {
    print(\"hi\");
    n
}

fn work() {}.benchmark({
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
    let (stdout, stderr, code) = check_here("benchmark_effects", src);
    // Each of the three uses is refused, and each says why: printing in the block
    // itself, handing it a function that prints, and a closure that prints.
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
error: effects are not supported in a `.benchmark` block yet: fn \"print\" has effect \"!prints\"
 --> ./benchmark_effects.aipl:9:5
  |
9 |     print(\"direct\");
  |     ^^^^^^^^^^^^^^^

error: effects are not supported in a `.benchmark` block yet: fn \"noisy\" has effect \"!prints\"
  --> ./benchmark_effects.aipl:10:25
   |
10 |     measure(\"named\", 1, noisy);
   |                         ^^^^^

error: effects are not supported in a `.benchmark` block yet: fn \"print\" has effect \"!prints\"
  --> ./benchmark_effects.aipl:15:13
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
               fn work() {}.benchmark({\n    measure(\"same\", 1, |n| n);\n    measure(\"same\", 2, |n| n);\n})\n";
    let (stdout, stderr, code) = check_here("benchmark_duplicate_label", src);
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
error: duplicate measurement label \"same\" in this `.benchmark` block — a report could not tell the two apart
 --> ./benchmark_duplicate_label.aipl:5:13
  |
5 |     measure(\"same\", 2, |n| n);
  |             ^^^^^^
"
    );
    assert_eq!(code, 1);

    // The same label in two blocks is two different reports, and fine.
    let two_blocks = "import { measure } from builtins;\n\n\
                      fn a() {}.benchmark({\n    measure(\"same\", 1, |n| n);\n})\n\n\
                      fn b() {}.benchmark({\n    measure(\"same\", 1, |n| n);\n})\n";
    let (stdout, stderr, code) = check_here("benchmark_label_per_block", two_blocks);
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        ("2 tests: 2 passed, 0 failed\n", "", 0)
    );
}

#[test]
fn a_type_error_in_a_benchmark_block_is_reported_with_its_source() {
    let (stdout, stderr, code) = check_here(
        "benchmark_type_error",
        &benchmark_block("assert(measure(\"x\", 1, |n| n).allocations == true);"),
    );
    assert_eq!(stderr, "");
    assert_eq!(
        stdout,
        "\
error: \"==\" between u64 and bool: both sides must be the same type
 --> ./benchmark_type_error.aipl:4:12
  |
4 |     assert(measure(\"x\", 1, |n| n).allocations == true);
  |            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_benchmark_block_that_leaks_fails_the_file_like_a_failing_test() {
    // The instrumented runtime has a test switch (`AIPL_INJECT_LEAK`) that stops
    // counting frees — nothing in the language can leak on purpose — so the block
    // below, which builds an array and drops it, reads as having leaked it. The
    // leak is named as a test of its own, and the summary counts it as one.
    let src = "import { measure } from builtins;\n\n\
               fn work() {}.benchmark({\n    measure(\"array\", 3, |n| [n, n, n]);\n})\n";
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check_here");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(dir.join("benchmark_leak.aipl"), src).expect("write temp source");
    let run = |env: &[(&str, &str)]| {
        let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
            .args(["check", "benchmark_leak.aipl"])
            .envs(env.iter().copied())
            .current_dir(&dir)
            .output()
            .expect("run aipl check");
        (
            String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code(),
        )
    };
    assert_eq!(
        run(&[("AIPL_INJECT_LEAK", "1")]),
        (
            "\
test failures:
test benchmark blocks ... FAIL
  leak: 1 of 1 allocations were never freed (0 deallocations)
2 tests: 1 passed, 1 failed
"
            .to_string(),
            String::new(),
            Some(1)
        )
    );
    // Without the switch it is the same block, passing.
    assert_eq!(
        run(&[]),
        (
            "1 tests: 1 passed, 0 failed\n".to_string(),
            String::new(),
            Some(0)
        )
    );
}

#[test]
fn a_benchmark_block_is_left_out_of_what_run_and_build_make() {
    // Same program with and without a block that calls `measure`: `run` behaves
    // identically. (The IR and binary being byte-identical is checked by hand
    // when the mechanism changes; this is the behavioural half.)
    let src = "import { measure, print } from builtins;\n\n\
               fn work() {}.benchmark({\n    measure(\"x\", 1, |n| n);\n})\n\n\
               fn main() !prints {\n    print(\"hi\");\n}\n";
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("check_here");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(dir.join("main.aipl"), src).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("run")
        .current_dir(&dir)
        .output()
        .expect("run aipl run");
    assert_eq!(
        (
            String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code()
        ),
        ("hi\n".to_string(), String::new(), Some(0))
    );
}

// ---------------------------------------------------------------------------
// `--fix`: format each file, then rewrite the recorded sections that no longer say
// what the file does. Written as examples: each states the whole of what it printed
// and what the file became.
// ---------------------------------------------------------------------------

/// What a tree file reads as after a run — `check_tree` leaves its files on disk.
fn tree_file(name: &str, rel: &str) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("check_tree")
            .join(name)
            .join(rel),
    )
    .expect("read a fixed file")
}

#[test]
fn fix_formats_the_file_and_refreshes_the_sections_that_went_stale() {
    // Unformatted, with a `stdout` and an `exit code` that no longer match.
    let src = "import { print } from builtins;\nfn main() !prints { print( \"hi\" ); }\n\
               --- stdout ---\nbye\n--- exit code ---\n7\n";
    let (stdout, stderr, code) = check_tree("fix_stale", &[("a.aipl", src)], &["--fix", "a.aipl"]);
    assert_eq!(
        stdout,
        format!("formatted ./a.aipl\nfixed ./a.aipl: stdout, exit code\n{NO_TESTS}")
    );
    assert_eq!((stderr.as_str(), code), ("", 0));
    assert_eq!(
        tree_file("fix_stale", "a.aipl"),
        "\
import { print } from builtins;

fn main() !prints {
    print(\"hi\");
}
--- stdout ---
hi
--- exit code ---
0
"
    );
    // And a plain `check` of the result has nothing to say.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("check_tree")
        .join("fix_stale");
    let again = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(["check", "a.aipl"])
        .current_dir(&dir)
        .output()
        .expect("run aipl check");
    assert_eq!(
        (
            String::from_utf8_lossy(&again.stdout).into_owned(),
            String::from_utf8_lossy(&again.stderr).into_owned(),
            again.status.code()
        ),
        (NO_TESTS.to_string(), String::new(), Some(0))
    );
}

#[test]
fn fix_refreshes_an_errors_section_after_formatting_has_moved_its_line_numbers() {
    // The diagnostic records a line and column, and formatting moves both — so the
    // formatter has to have run first, or the section would record a position the
    // next format changes.
    let src = "fn   bad()  ->  i64   {   true   }\n--- errors ---\nstale\n";
    let (stdout, stderr, code) =
        check_tree("fix_errors", &[("bad.aipl", src)], &["--fix", "bad.aipl"]);
    assert_eq!(stdout, "formatted ./bad.aipl\nfixed ./bad.aipl: errors\n");
    assert_eq!((stderr.as_str(), code), ("", 0));
    assert_eq!(
        tree_file("fix_errors", "bad.aipl"),
        "\
fn bad() -> i64 { true }
--- errors ---
error: fn \"bad\": body returns bool, but the declared return type is i64
 --> ./bad.aipl:1:19
  |
1 | fn bad() -> i64 { true }
  |                   ^^^^
"
    );
}

#[test]
fn fix_refreshes_a_report_section() {
    let src = "fn main() {}\n--- report ---\nstale\n";
    let (stdout, stderr, code) = check_tree("fix_report", &[("r.aipl", src)], &["--fix", "r.aipl"]);
    assert_eq!(stdout, format!("fixed ./r.aipl: report\n{NO_TESTS}"));
    assert_eq!((stderr.as_str(), code), ("", 0));
    assert_eq!(
        tree_file("fix_report", "r.aipl"),
        format!("fn main() {{}}\n--- report ---\n{REPORT_OF_EMPTY_MAIN}\n")
    );
}

#[test]
fn fix_does_not_hide_a_failing_test() {
    // The stale `stdout` is repaired; the failing assertion is a real failure, and
    // stays one.
    let src = "import { equal as ==, print } from builtins;\n\n\
               fn f() -> i64 { 1 }.test({\n    assert(f() == 2);\n})\n\n\
               fn main() !prints {\n    print(\"hi\");\n}\n--- stdout ---\nbye\n";
    let (stdout, stderr, code) =
        check_tree("fix_failing", &[("t.aipl", src)], &["--fix", "t.aipl"]);
    assert_eq!(
        stdout,
        "\
./t.aipl:
    f: 0 assertions passed, 1 failed
fixed ./t.aipl: stdout
test failures:
test f ... FAIL
  assert failed at ./t.aipl:4 f() == 2
1 tests: 0 passed, 1 failed
"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));
    assert!(tree_file("fix_failing", "t.aipl").ends_with("--- stdout ---\nhi\n"));
}

#[test]
fn fix_leaves_what_a_rewrite_cannot_repair() {
    // Each of these is a problem with the file, not a stale section: `main` that
    // is not there, an expected file the run never wrote, a section the file's
    // other sections forbid.
    let no_main = "fn f() -> i64 { 1 }\n--- stdout ---\nx\n";
    let (stdout, stderr, code) =
        check_tree("fix_no_main", &[("n.aipl", no_main)], &["--fix", "n.aipl"]);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "./n.aipl: a file with no `main` runs only its `.test` blocks — it can't have a stdout/stderr/exit/cli/expect-file section\n";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
    // And the file is as it was.
    assert_eq!(tree_file("fix_no_main", "n.aipl"), no_main);

    let missing = "fn main() {}\n--- expect file: never.txt ---\nx\n";
    let (stdout, stderr, code) =
        check_tree("fix_missing", &[("m.aipl", missing)], &["--fix", "m.aipl"]);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "./m.aipl: expected output file \"never.txt\" was not written: No such file or directory (os error 2)\n";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
    assert_eq!(tree_file("fix_missing", "m.aipl"), missing);
}

#[test]
fn fix_in_a_batch_repairs_every_file_and_counts_only_the_rest() {
    let (stdout, stderr, code) = check_tree(
        "fix_batch",
        &[
            ("a_stale.aipl", &format!("{GREETS}--- stdout ---\nbye\n")),
            ("b_ok.aipl", &format!("{GREETS}--- stdout ---\nhi\n")),
            ("c_stale.aipl", &format!("{GREETS}--- stdout ---\nbye\n")),
        ],
        &["--fix"],
    );
    assert_eq!(
        stdout,
        "fixed ./a_stale.aipl: stdout\nfixed ./c_stale.aipl: stdout\n3 files, 0 tests: 0 passed, 0 failed\n"
    );
    assert_eq!((stderr.as_str(), code), ("", 0));
    assert!(tree_file("fix_batch", "a_stale.aipl").ends_with("--- stdout ---\nhi\n"));
}

#[test]
fn fix_with_ignore_sections_only_formats() {
    let src = format!("import {{ print }} from builtins;\nfn main() !prints {{ print( \"hi\" ); }}\n--- stdout ---\nbye\n");
    let (stdout, stderr, code) = check_tree(
        "fix_ignore",
        &[("i.aipl", &src)],
        &["--fix", "--ignore-sections", "i.aipl"],
    );
    assert_eq!(stdout, format!("formatted ./i.aipl\n{NO_TESTS}"));
    assert_eq!((stderr.as_str(), code), ("", 0));
    // The section is still wrong; this run was not asked about it.
    assert!(tree_file("fix_ignore", "i.aipl").ends_with("--- stdout ---\nbye\n"));
}

#[test]
fn fix_leaves_a_file_that_does_not_parse_alone() {
    // The formatter cannot parse it, so there is nothing to rewrite; loading it
    // reports the syntax error, which is the diagnostic that helps.
    let src = "fn main( {\n";
    let (stdout, stderr, code) =
        check_tree("fix_unparsed", &[("p.aipl", src)], &["--fix", "p.aipl"]);
    assert_eq!(stderr, "");
    assert!(stdout.starts_with("error:"), "got:\n{stdout}");
    assert_eq!(code, 1);
    assert_eq!(tree_file("fix_unparsed", "p.aipl"), src);
}

// ---------------------------------------------------------------------------
// `--baseline`: a file's run is held to its row in a baseline table
// ---------------------------------------------------------------------------

/// Write `<name>.aipl` (an empty `main`) and `table.md` to a scratch directory and run
/// `aipl check --baseline table.md` on the file from there.
fn check_against(name: &str, table: &str) -> (String, String, i32) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("check_baseline")
        .join(name);
    fs::create_dir_all(&dir).expect("create temp dir");
    let file = format!("{name}.aipl");
    fs::write(dir.join(&file), "fn main() {}\n").expect("write temp source");
    fs::write(dir.join("table.md"), table).expect("write temp table");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(["check", "--baseline", "table.md", &file])
        .current_dir(&dir)
        .output()
        .expect("run aipl check");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

const TABLE_HEAD: &str = "| Test | allocations | bytes allocated | peak live bytes | instructions \
                          | code bytes | wall-clock (µs) | build (ms) | peak RSS (KB) |\n\
                          | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n";

#[test]
fn a_run_that_reproduces_its_baseline_row_passes() {
    let table = format!("{TABLE_HEAD}| baseline_ok | 0 | 0 | 0 | 3 | 8 | 1.00 | 1.0 | 1 |\n");
    let (stdout, stderr, code) = check_against("baseline_ok", &table);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), (NO_TESTS, "", 0));
}

#[test]
fn a_run_that_moved_from_its_baseline_row_names_each_column_and_asks_for_a_refresh() {
    let table = format!("{TABLE_HEAD}| baseline_moved | 0 | 0 | 0 | 1 | 8 | 1.00 | 1.0 | 1 |\n");
    let (stdout, stderr, code) = check_against("baseline_moved", &table);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./baseline_moved.aipl: performance baseline moved (table.md):
  instructions: 1 -> 3 (+200.0%)
handoff-fill: baseline-moved\tbaseline_moved.aipl\tinstructions: 1 -> 3 (+200.0%)
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

#[test]
fn a_file_with_no_baseline_row_fails_and_asks_for_one() {
    let (stdout, stderr, code) = check_against("baseline_missing", TABLE_HEAD);
    // What the file reports, then the summary: one stream, in that order.
    let findings = "\
./baseline_missing.aipl: performance baseline missing: table.md has no row for `baseline_missing`
handoff-fill: baseline-missing\tbaseline_missing.aipl
";
    assert_eq!(stdout, format!("{findings}{NO_TESTS}"));
    assert_eq!(stderr, "");
    assert_eq!(code, 1);
}

// ---------------------------------------------------------------------------
// `--format`, `--compile`, `--tests`, `--benchmarks`: the parts of a check
// ---------------------------------------------------------------------------

/// A file with something for every part to find: its import is not canonical, its test fails and
/// so does its benchmark.
const EVERY_PART_FAILS: &str = "import { equal as ==, measure} from builtins;\n\n\
     fn one() -> i64 { 1 }.test({\n    assert(one() == 2);\n}).benchmark({\n    \
     assert(measure(\"m\", 1, |n| n).instructions == 0);\n})\n";
const NEEDS_FORMATTING: &str = "./f.aipl: needs formatting (run `aipl check --fix ./f.aipl`)\n";
const TEST_FAILS: &str = "test one ... FAIL\n  assert failed at ./f.aipl:4 one() == 2\n";

#[test]
fn with_no_part_named_every_part_runs() {
    let (stdout, stderr, code) =
        check_tree("parts_all", &[("f.aipl", EVERY_PART_FAILS)], &["f.aipl"]);
    assert_eq!(stderr, "");
    assert!(stdout.starts_with(NEEDS_FORMATTING), "got:\n{stdout}");
    assert!(stdout.contains(TEST_FAILS), "got:\n{stdout}");
    assert!(stdout.contains("instructions == 0"), "got:\n{stdout}");
    assert_eq!(code, 1);
}

#[test]
fn format_checks_only_the_format() {
    let (stdout, stderr, code) = check_tree(
        "parts_format",
        &[("f.aipl", EVERY_PART_FAILS)],
        &["--format", "f.aipl"],
    );
    // Nothing ran, so there is no summary either.
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), code),
        (NEEDS_FORMATTING, "", 1)
    );

    // A canonical file passes however its tests fare.
    let (stdout, stderr, code) = check_tree(
        "parts_format_ok",
        &[("f.aipl", FAILS)],
        &["--format", "f.aipl"],
    );
    assert_eq!((stdout.as_str(), stderr.as_str(), code), ("", "", 0));
}

#[test]
fn compile_loads_and_compiles_without_running_anything() {
    let (stdout, stderr, code) = check_tree(
        "parts_compile",
        &[("f.aipl", EVERY_PART_FAILS)],
        &["--compile", "f.aipl"],
    );
    assert_eq!((stdout.as_str(), stderr.as_str(), code), ("", "", 0));

    // What a compile earns is reported, with the exit status to match.
    let (stdout, stderr, code) = check_tree(
        "parts_compile_broken",
        &[("f.aipl", BROKEN)],
        &["--compile", "f.aipl"],
    );
    assert!(
        stdout.contains("declared return type is i64"),
        "got:\n{stdout}"
    );
    assert_eq!((stderr.as_str(), code), ("", 1));

    // A `trace` is refused without the program having run: no trace value is printed.
    let (stdout, stderr, code) = check_tree(
        "parts_compile_trace",
        &[("f.aipl", TRACED)],
        &["--compile", "f.aipl"],
    );
    assert!(
        stdout.contains("`trace(..)` prints for debugging"),
        "got:\n{stdout}"
    );
    assert!(!stdout.contains("= 7"), "the file ran:\n{stdout}");
    assert_eq!((stderr.as_str(), code), ("", 1));
}

#[test]
fn tests_run_the_test_blocks_and_neither_format_nor_benchmarks() {
    let (stdout, stderr, code) = check_tree(
        "parts_tests",
        &[("f.aipl", EVERY_PART_FAILS)],
        &["--tests", "f.aipl"],
    );
    assert_eq!(stderr, "");
    assert!(!stdout.contains("needs formatting"), "got:\n{stdout}");
    assert!(stdout.contains(TEST_FAILS), "got:\n{stdout}");
    assert!(
        !stdout.contains("instructions == 0"),
        "the benchmark ran:\n{stdout}"
    );
    assert!(
        stdout.ends_with("1 tests: 0 passed, 1 failed\n"),
        "got:\n{stdout}"
    );
    assert_eq!(code, 1);

    // A file that cannot compile is reported the same way under either run part.
    for part in ["--tests", "--benchmarks"] {
        let (stdout, stderr, code) = check_tree(
            "parts_tests_broken",
            &[("f.aipl", BROKEN)],
            &[part, "f.aipl"],
        );
        assert!(
            stdout.contains("declared return type is i64"),
            "{part}:\n{stdout}"
        );
        assert_eq!((stderr.as_str(), code), ("", 1));
    }
}

#[test]
fn benchmarks_run_the_benchmark_blocks_and_not_the_tests() {
    let (stdout, stderr, code) = check_tree(
        "parts_benchmarks",
        &[("f.aipl", EVERY_PART_FAILS)],
        &["--benchmarks", "f.aipl"],
    );
    assert_eq!(stderr, "");
    assert!(!stdout.contains("needs formatting"), "got:\n{stdout}");
    assert!(!stdout.contains(TEST_FAILS), "the test ran:\n{stdout}");
    assert!(stdout.contains("instructions == 0"), "got:\n{stdout}");
    assert_eq!(code, 1);
}

#[test]
fn named_parts_combine_and_the_status_follows_the_parts_chosen() {
    let run = |name: &str, args: &[&str]| {
        let mut all = args.to_vec();
        all.push("f.aipl");
        check_tree(name, &[("f.aipl", EVERY_PART_FAILS)], &all)
    };
    let (stdout, _, code) = run("parts_format_tests", &["--format", "--tests"]);
    assert!(stdout.starts_with(NEEDS_FORMATTING), "got:\n{stdout}");
    assert!(
        stdout.contains(TEST_FAILS) && !stdout.contains("instructions == 0"),
        "got:\n{stdout}"
    );
    assert_eq!(code, 1);

    let (stdout, _, code) = run("parts_tests_benchmarks", &["--tests", "--benchmarks"]);
    assert!(
        stdout.contains(TEST_FAILS) && stdout.contains("instructions == 0"),
        "got:\n{stdout}"
    );
    assert!(!stdout.contains("needs formatting"), "got:\n{stdout}");
    assert_eq!(code, 1);

    // The failing parts left out, nothing is left to fail.
    let (stdout, stderr, code) = run("parts_compile_only_ok", &["--compile"]);
    assert_eq!((stdout.as_str(), stderr.as_str(), code), ("", "", 0));
}

#[test]
fn naming_a_part_leaves_the_files_recorded_sections_out() {
    let src = format!("{GREETS}--- stdout ---\nbye\n");
    let (stdout, _, code) = check_tree("parts_sections", &[("f.aipl", &src)], &["f.aipl"]);
    assert!(stdout.contains("`stdout` mismatch"), "got:\n{stdout}");
    assert_eq!(code, 1);
    for part in ["--format", "--compile", "--tests", "--benchmarks"] {
        let (stdout, stderr, code) =
            check_tree("parts_sections", &[("f.aipl", &src)], &[part, "f.aipl"]);
        assert!(!stdout.contains("mismatch"), "{part}:\n{stdout}");
        assert_eq!((stderr.as_str(), code), ("", 0), "{part}");
    }
}

#[test]
fn fix_with_a_part_named_formats_only_when_format_is_named() {
    let (stdout, _, code) = check_tree(
        "parts_fix_format",
        &[("f.aipl", UNFORMATTED)],
        &["--format", "--fix", "f.aipl"],
    );
    assert_eq!((stdout.as_str(), code), ("formatted ./f.aipl\n", 0));
    assert!(
        tree_file("parts_fix_format", "f.aipl").contains("import { equal as == } from builtins;")
    );

    let (stdout, _, code) = check_tree(
        "parts_fix_tests",
        &[("f.aipl", UNFORMATTED)],
        &["--tests", "--fix", "f.aipl"],
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(!stdout.contains("formatted "), "got:\n{stdout}");
    assert_eq!(tree_file("parts_fix_tests", "f.aipl"), UNFORMATTED);
}
