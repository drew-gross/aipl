//! Integration tests for `aipl report` — what a run of a file costs.
//!
//! These are meant to be read as examples: where a test is about what `report`
//! prints, it states the whole output. (The human form lives in the corpus, as the
//! `--- report ---` sections of `tests/cases/report/`.)
//!
//! The central claim is that `report` and the cases harness are one
//! implementation (`aipl::perf`): so for any corpus case, the counts `report`
//! prints are exactly the ones the case's `--- performance ---` section records.
//! That is checked here on cases chosen for the ways a run can differ — a
//! `--- cli ---` section, a library with only `.test` blocks, a program that
//! writes a file, companions staged beside the source. The rest pins `report`'s
//! own behaviour: its arguments, its two output forms, and what it refuses.
//!
//! The binary sizes written out below are this platform's, as the corpus's own
//! `--- performance ---` sections are, and move with codegen the same way.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use aipl::perf::PerfStats;

fn normalized(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// Run `aipl report <args>` from `dir`, returning `(stdout, stderr, exit code)`.
fn report_in(dir: &str, args: &[&str]) -> (String, String, i32) {
    report_with_env(dir, args, &[])
}

fn report_with_env(dir: &str, args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("report")
        .args(args)
        .envs(env.iter().copied())
        .current_dir(dir)
        .output()
        .expect("run aipl report");
    (
        normalized(&out.stdout),
        normalized(&out.stderr),
        out.status.code().unwrap_or(-1),
    )
}

/// `aipl report <args>` from the repository root, where the corpus paths resolve.
fn report(args: &[&str]) -> (String, String, i32) {
    report_in(env!("CARGO_MANIFEST_DIR"), args)
}

/// Write `src` to `<name>.aipl` in a scratch directory and report on it *from that
/// directory*, by its bare name — so a message that names the file says `name.aipl`
/// and not a path that depends on where the target directory is.
fn report_source(name: &str, src: &str, args: &[&str]) -> (String, String, i32) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("report");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(dir.join(format!("{name}.aipl")), src).expect("write temp source");
    let file = format!("{name}.aipl");
    let mut all = vec![file.as_str()];
    all.extend_from_slice(args);
    report_in(&dir.to_string_lossy(), &all)
}

/// The stats a corpus case records in its `--- performance ---` section.
fn recorded(case: &str) -> PerfStats {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(case);
    let src = fs::read_to_string(&path).expect("read case");
    let spec = aipl::parse_spec(&src).expect("parse case sections");
    let body = spec.performance.expect("case has a performance section");
    PerfStats::parse(&body).expect("parse the recorded stats")
}

fn assert_report_matches_the_case(case: &str) {
    let (stdout, stderr, code) = report(&["--machine", case]);
    assert_eq!(code, 0, "{case}:\n{stdout}{stderr}");
    let reported =
        PerfStats::parse(&stdout).unwrap_or_else(|| panic!("{case}: unparsable:\n{stdout}"));
    // The recorded section is the `--- performance ---` form, which has no peak.
    assert_eq!(reported.render(), recorded(case).render(), "{case}");
}

// ---- parity with the harness --------------------------------------------------

#[test]
fn report_agrees_with_the_recorded_section_of_a_program_given_cli_arguments() {
    assert_report_matches_the_case("tests/cases/cli/args.aipl");
}

#[test]
fn report_agrees_with_the_recorded_section_of_a_library_run_by_its_tests() {
    assert_report_matches_the_case("tests/cases/lambdas/fold.aipl");
}

#[test]
fn report_agrees_with_the_recorded_section_of_a_program_that_writes_a_file() {
    assert_report_matches_the_case("tests/cases/files/write_file.aipl");
}

#[test]
fn report_agrees_with_the_recorded_section_of_a_case_with_companions() {
    assert_report_matches_the_case("tests/cases/lambdas/operator_as_value_custom.aipl");
}

#[test]
fn report_does_not_depend_on_where_it_is_run_from() {
    // The file's label is baked into the binary, so rooting it at the working
    // directory would change `binary size` with the directory.
    let case = format!(
        "{}/tests/cases/lambdas/fold.aipl",
        env!("CARGO_MANIFEST_DIR")
    );
    let from_root = report(&["--machine", &case]).0;
    let from_elsewhere = report_in(env!("CARGO_TARGET_TMPDIR"), &["--machine", &case]).0;
    assert_eq!(from_root, from_elsewhere);
}

// ---- what a report looks like -------------------------------------------------
//
// The human form is not pinned here. It is written out as the `--- report ---`
// section of `tests/cases/report/` — the smallest program, a program that calls
// into the runtime, and a library's test run — where `check` holds each to its
// program and handoff refreshes it when codegen moves, which a Rust test could not
// be. What stays in this file is what has no section: the machine form.

#[test]
fn the_machine_form_is_the_performance_section_plus_the_peak() {
    let (stdout, stderr, code) = report_source("machine", "fn main() {}\n", &["--machine"]);
    assert_eq!(stderr, "");
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        "\
allocations: 0
deallocations: 0
reallocations: 0
bytes allocated: 0
peak live bytes: 0
instructions executed: 3
binary size: 488
  code: 8
  data: 1
  metadata: 479
functions:
  main:
    calls: 1
    bytes: 8
"
    );
    // And it reads back to what it printed.
    let stats = PerfStats::parse(&stdout).expect("machine output parses back");
    assert_eq!(stats.render_machine() + "\n", stdout);
}

// ---- what a report counts -----------------------------------------------------

/// A program that builds a hundred-element array `rounds` times, each one dead
/// before the next begins.
fn builds_arrays(rounds: u32) -> String {
    format!(
        "import {{ not_equal as !=, push, wrapping_increment as ++ }} from builtins;\n\n\
         fn built(n: i64) -> i64[] {{\n    mut xs: i64[] = [];\n    mut i: i64 = 0;\n    \
         while (i != n) {{\n        set xs.push(i);\n        set i++;\n    }}\n    xs\n}}\n\n\
         fn main() {{\n    mut round: i64 = 0;\n    while (round != {rounds}) {{\n        \
         built(100);\n        set round++;\n    }}\n}}\n"
    )
}

#[test]
fn the_peak_is_the_most_live_at_once_not_the_total() {
    // Each round's array is dead before the next is built, so five rounds
    // allocate five times as much and peak exactly as high as one.
    let stats = |name: &str, rounds| {
        let (stdout, stderr, code) = report_source(name, &builds_arrays(rounds), &["--machine"]);
        assert_eq!((stderr.as_str(), code), ("", 0));
        PerfStats::parse(&stdout).expect("parses")
    };
    let once = stats("peak_once", 1);
    let five = stats("peak_five", 5);
    // One 64-byte allocation that grows in place to 1552 bytes by the end.
    assert_eq!(
        (once.allocations, once.bytes_allocated, once.peak_live_bytes),
        (1, 64, 1552)
    );
    assert_eq!(
        (five.allocations, five.bytes_allocated, five.peak_live_bytes),
        (5, 320, 1552)
    );
    assert_eq!(five.deallocations, five.allocations);
}

const ECHO: &str = "import { len, print, to_str } from builtins;\n\nfn main(args: str[]) !prints {\n    print(to_str(len(args)));\n}\n";

/// `bytes allocated` by `ECHO` run with `args` — which is what the arguments
/// cost, since the program's output is not shown and this is how their arrival
/// shows.
fn bytes_allocated(name: &str, src: &str, args: &[&str]) -> u64 {
    let mut all = vec!["--machine"];
    all.extend_from_slice(args);
    let (stdout, stderr, code) = report_source(name, src, &all);
    assert_eq!((stderr.as_str(), code), ("", 0));
    PerfStats::parse(&stdout).expect("parses").bytes_allocated
}

#[test]
fn arguments_after_the_double_dash_reach_the_program() {
    assert_eq!(bytes_allocated("args_none", ECHO, &[]), 49);
    assert_eq!(
        bytes_allocated("args_three", ECHO, &["--", "a", "b", "c"]),
        121
    );
}

#[test]
fn a_cli_section_is_the_default_arguments_and_the_double_dash_replaces_it() {
    let with_section = format!("{ECHO}--- cli ---\none\ntwo\nthree\nfour\n");
    // Four arguments from the section...
    assert_eq!(bytes_allocated("args_section", &with_section, &[]), 145);
    // ...and a bare `--` is "no arguments", the same as having no section.
    assert_eq!(bytes_allocated("args_override", &with_section, &["--"]), 49);
}

// ---- what it refuses ----------------------------------------------------------

#[test]
fn a_file_that_does_not_compile_is_reported_and_exits_one() {
    let (stdout, stderr, code) = report_source("broken", "fn main() -> i64 { true }\n", &[]);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "\
error: fn \"main\": body returns bool, but the declared return type is i64
 --> broken.aipl:1:20
  |
1 | fn main() -> i64 { true }
  |                    ^^^^
"
    );
    assert_eq!(code, 1);
}

#[test]
fn a_missing_file_and_missing_or_extra_arguments_are_refused() {
    assert_eq!(
        report(&["no_such_file.aipl"]),
        (
            String::new(),
            "no such file: no_such_file.aipl\n".to_string(),
            1
        )
    );
    assert_eq!(
        report(&[]),
        (String::new(), "missing source file\n".to_string(), 1)
    );
    assert_eq!(
        report(&["tests/cases/cli/args.aipl", "second.aipl"]),
        (
            String::new(),
            "unexpected arg \"second.aipl\"\n".to_string(),
            1
        )
    );
}

#[test]
fn a_leak_is_named_and_fails_but_the_counts_still_print() {
    // Nothing in the language can leak on purpose, so the instrumented runtime has a
    // test switch that stops counting frees (see `leak_injected` in the runtime):
    // the run then reads as one that never freed a block. It is the only way to
    // show that the gate still fires.
    let leaked = [("AIPL_INJECT_LEAK", "1")];
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("report");
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(dir.join("leaky.aipl"), ECHO).expect("write temp source");
    let (stdout, stderr, code) = report_with_env(&dir.to_string_lossy(), &["leaky.aipl"], &leaked);
    assert_eq!(
        stdout,
        "\
allocations       2
deallocations     0
reallocations     0
bytes allocated   49
peak live bytes   49
instructions      38
binary size       1240 bytes (code 336, data 1, metadata 903)

function            calls  code bytes
__to_str_u64            1         204
aipl_array_dec          1    external
aipl_array_new          1    external
aipl_u64_len            2    external
aipl_write_u64          1    external
main                    1         132
"
    );
    assert_eq!(
        stderr,
        "leak: 2 of 2 allocations were never freed (0 deallocations)\n"
    );
    assert_eq!(code, 1);

    // Without the switch the same file is clean.
    let (_stdout, stderr, code) = report_in(&dir.to_string_lossy(), &["leaky.aipl"]);
    assert_eq!((stderr.as_str(), code), ("", 0));
}

#[test]
fn only_a_disagreement_between_the_counts_is_a_leak() {
    let balanced = recorded("tests/cases/cli/args.aipl");
    assert_eq!(balanced.leaked(), None);
    let mut leaky = balanced.clone();
    leaky.deallocations -= 1;
    assert_eq!(leaky.leaked(), Some(1));
}
