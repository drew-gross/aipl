//! What checking one file found, and how that is written down.
//!
//! `aipl check` produces a [`CheckResult`] per file — plain data: the formatting verdict, the tests'
//! rows, the compile failure if there was one, the traces, the section findings and what `--fix`
//! wrote — and the command prints it. Producing and printing are separate so that something other
//! than the terminal (a section that pins part of this output) can read the same result, and so each
//! piece has one renderer, tested apart from the command.
//!
//! Nothing here runs a file. A file's tests are handed over by the runtime as a [`TestRun`] (rows,
//! counts, failure text) and a batch's totals are the sum of its files' runs, so no count lives
//! anywhere but in a result.

use crate::codegen::{TestRecord, TestRun};
use crate::sections::Finding;

/// Which parts of `check` a run performs. With none selected on the command line, all of them.
///
/// - `format`: the file against the canonical format (and, with `--fix`, rewriting it).
/// - `compile`: loading and compiling, so every diagnostic a file earns: syntax and type errors,
///   lints, the `.benchmark` refusals and the `trace(..)` refusal. Nothing runs.
/// - `tests`: also running the `.test` blocks (and holding that run to freeing what it allocates).
/// - `benchmarks`: also running the `.benchmark` blocks.
///
/// Running a part needs the file to compile, so `tests` and `benchmarks` report compile failures
/// too; the selectors differ in what runs, not in what is reported about a file that cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parts {
    pub format: bool,
    pub compile: bool,
    pub tests: bool,
    pub benchmarks: bool,
}

impl Parts {
    pub const ALL: Parts = Parts {
        format: true,
        compile: true,
        tests: true,
        benchmarks: true,
    };

    /// The selectors named in `args`, with the arguments that are not selectors; `None` when no
    /// selector is named, which means every part.
    pub fn take_from(args: Vec<String>) -> (Vec<String>, Option<Parts>) {
        let none = Parts {
            format: false,
            compile: false,
            tests: false,
            benchmarks: false,
        };
        let mut parts = none;
        let mut rest = Vec::new();
        for arg in args {
            match arg.as_str() {
                "--format" => parts.format = true,
                "--compile" => parts.compile = true,
                "--tests" => parts.tests = true,
                "--benchmarks" => parts.benchmarks = true,
                _ => rest.push(arg),
            }
        }
        (rest, (parts != none).then_some(parts))
    }

    /// These parts, less the ones in `pinned`. The compile part is never left out: it is what a
    /// pinned part's own run reports a broken file by.
    pub fn without(self, pinned: Parts) -> Parts {
        Parts {
            format: self.format && !pinned.format,
            compile: self.compile,
            tests: self.tests && !pinned.tests,
            benchmarks: self.benchmarks && !pinned.benchmarks,
        }
    }

    /// Whether any part loads the file.
    pub fn loads(&self) -> bool {
        self.compile || self.tests || self.benchmarks
    }
}

/// Whether a file is in canonical format, and what `--fix` did about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Formatting {
    /// Already canonical, or a source the formatter cannot parse (loading it reports the syntax
    /// error, which is the diagnostic that helps).
    Clean,
    /// Not canonical, and not asked to rewrite it.
    Needs,
    /// `--fix` rewrote it.
    Rewritten,
    /// `--fix` could not write the rewrite; the message says why.
    RewriteFailed(String),
}

impl Formatting {
    /// Whether the file is still not in canonical format after this step.
    pub fn is_unformatted(&self) -> bool {
        matches!(self, Formatting::Needs | Formatting::RewriteFailed(_))
    }

    /// The line this step reports for `label`, if any.
    pub fn line(&self, label: &str) -> Option<String> {
        match self {
            Formatting::Clean => None,
            Formatting::Needs => Some(format!(
                "{label}: needs formatting (run `aipl check --fix {label}`)"
            )),
            Formatting::Rewritten => Some(format!("formatted {label}")),
            Formatting::RewriteFailed(why) => Some(format!("{label}: could not rewrite: {why}")),
        }
    }
}

/// How checking a file that loaded and compiled came out.
pub struct Checked {
    /// The tests' driver exit code: 0 when every test passed.
    pub exit_code: i64,
    /// The rendered `trace(..)` diagnostics, when the program still holds any.
    pub traces: Option<String>,
    /// How the file's recorded sections disagree with what it does, after `--fix` has repaired what
    /// it can. Empty when they all match, or when sections were not validated.
    pub sections: Vec<Finding>,
    /// The sections `--fix` rewrote, in the order first written.
    pub fixed: Vec<String>,
    /// Why `--fix` could not write its rewrite, when it could not.
    pub fix_error: Option<String>,
    /// Whether the test driver ran at all: an `errors` file never compiles, so it has no tests and no
    /// summary to print.
    pub ran_tests: bool,
}

/// Everything checking one file found.
pub struct CheckResult {
    /// The file's path as the user typed it or as discovery found it.
    pub label: String,
    pub formatting: Formatting,
    /// What the file's tests did, `.benchmark` blocks and leak checks included; all zero when it
    /// has none or never compiled.
    pub run: TestRun,
    /// `Err` is a rendered load/compile failure.
    pub outcome: Result<Checked, String>,
}

impl CheckResult {
    /// Whether the file compiled and every test passed.
    pub fn tests_passed(&self) -> bool {
        self.outcome
            .as_ref()
            .is_ok_and(|c| c.exit_code == 0 && self.run.failed == 0)
    }

    pub fn failed_to_compile(&self) -> bool {
        self.outcome.is_err()
    }

    pub fn has_traces(&self) -> bool {
        self.outcome.as_ref().is_ok_and(|c| c.traces.is_some())
    }

    pub fn has_section_findings(&self) -> bool {
        self.outcome.as_ref().is_ok_and(|c| !c.sections.is_empty())
    }

    /// Whether the file passed everything: formatted, compiled, tests green, no trace, sections
    /// matching.
    pub fn passed(&self) -> bool {
        !self.formatting.is_unformatted()
            && self.tests_passed()
            && !self.has_traces()
            && !self.has_section_findings()
    }

    /// The tests as rows under the file's name; see [`render_test_rows`].
    pub fn render_rows(&self) -> String {
        render_test_rows(&self.label, &self.run.records)
    }

    /// The line saying which sections `--fix` wrote, or why it could not.
    pub fn render_fixed(&self) -> Option<String> {
        let checked = self.outcome.as_ref().ok()?;
        if let Some(why) = &checked.fix_error {
            return Some(format!(
                "{}: could not write the fixed sections: {why}",
                self.label
            ));
        }
        (!checked.fixed.is_empty())
            .then(|| format!("fixed {}: {}", self.label, checked.fixed.join(", ")))
    }

    /// Everything the file reports after its formatting line, in the one order every mode
    /// uses: the tests' rows, then either the compile failure or (what `--fix` wrote, the
    /// `trace(..)` refusal, the section findings). All of it is for stdout; stderr is for
    /// usage and I/O errors. Every line ends in a newline.
    ///
    /// The formatting line is separate because it is reported before the file runs, so it
    /// precedes anything the run prints. The failing tests' text and the pass/fail counts
    /// follow the last file, as an aggregate.
    pub fn render(&self) -> String {
        let mut out = self.render_rows();
        match &self.outcome {
            Err(msg) => out.push_str(&format!("{msg}\n")),
            Ok(checked) => {
                if let Some(line) = self.render_fixed() {
                    out.push_str(&format!("{line}\n"));
                }
                if let Some(report) = &checked.traces {
                    out.push_str(&format!("{report}\n"));
                }
                out.push_str(&self.render_findings());
            }
        }
        out
    }

    /// The `N tests: P passed, F failed` line of a single-file report, when tests ran.
    pub fn render_summary(&self) -> Option<String> {
        let ran = self.outcome.as_ref().is_ok_and(|c| c.ran_tests);
        ran.then(|| {
            let run = &self.run;
            format!(
                "{} tests: {} passed, {} failed",
                run.total, run.passed, run.failed
            )
        })
    }

    /// The lines for each way the file's sections disagree with what it does, naming the file.
    pub fn render_findings(&self) -> String {
        match &self.outcome {
            Ok(checked) => render_findings(&self.label, &checked.sections),
            Err(_) => String::new(),
        }
    }
}

/// The totals of a batch: its files' runs added up, and how many files failed in each way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchTotals {
    pub files: usize,
    pub total: i64,
    pub passed: i64,
    pub failed: i64,
    /// Files that did not compile.
    pub broken: usize,
    /// Files not in canonical format.
    pub unformatted: usize,
    /// Files that still call `trace(..)`.
    pub traced: usize,
    /// Files whose sections disagree with what they do.
    pub mismatched: usize,
    /// What the failing tests said, in file order.
    pub failure_report: String,
}

impl BatchTotals {
    /// Count one more checked file in.
    pub fn add(&mut self, result: &CheckResult) {
        self.files += 1;
        self.total += result.run.total;
        self.passed += result.run.passed;
        self.failed += result.run.failed;
        self.failure_report.push_str(&result.run.failure_report);
        self.broken += usize::from(result.failed_to_compile());
        self.unformatted += usize::from(result.formatting.is_unformatted());
        self.traced += usize::from(result.has_traces());
        self.mismatched += usize::from(result.has_section_findings());
    }

    /// Whether every file passed everything.
    pub fn passed_all(&self) -> bool {
        self.failed == 0
            && self.broken == 0
            && self.unformatted == 0
            && self.traced == 0
            && self.mismatched == 0
    }

    /// The aggregate report: the failing tests' text under its heading (when there is any), then
    /// the `N files, T tests` line, then one line for each way a file can fail that the test counts
    /// give no hint of (a file that never compiled contributes no counts; an unformatted file's
    /// tests may all have passed). Every line ends in a newline.
    pub fn render(&self) -> String {
        let mut out = render_failures(&self.failure_report);
        let files_word = if self.files == 1 { "file" } else { "files" };
        out.push_str(&format!(
            "{} {files_word}, {} tests: {} passed, {} failed\n",
            self.files, self.total, self.passed, self.failed
        ));
        let plural =
            |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
        if self.broken > 0 {
            out.push_str(&format!(
                "{} {} failed to compile\n",
                self.broken,
                plural(self.broken, "file", "files")
            ));
        }
        if self.unformatted > 0 {
            out.push_str(&format!(
                "{} {} formatting\n",
                self.unformatted,
                plural(self.unformatted, "file needs", "files need")
            ));
        }
        if self.traced > 0 {
            out.push_str(&format!(
                "{} {} trace()\n",
                self.traced,
                plural(self.traced, "file still calls", "files still call")
            ));
        }
        if self.mismatched > 0 {
            out.push_str(&format!(
                "{} {} mismatched sections\n",
                self.mismatched,
                plural(self.mismatched, "file has", "files have")
            ));
        }
        out
    }
}

/// What the failing tests said, under one heading after every line about the tests that passed.
/// Nothing when none failed.
pub fn render_failures(report: &str) -> String {
    if report.is_empty() {
        String::new()
    } else {
        format!("test failures:\n{report}")
    }
}

/// "3 assertions passed", "1 assertion passed".
fn assertions_passed(n: u64) -> String {
    format!("{n} assertion{} passed", if n == 1 { "" } else { "s" })
}

/// What a test (or the standalone tests together) came to: the assertions that held, and — only
/// when there are some — the ones that did not. A test that failed with no assertion failing, by a
/// `?` that propagated an error, says so.
fn tally(passed: u64, failed: u64, test_failed: bool) -> String {
    let mut out = assertions_passed(passed);
    if failed > 0 {
        out.push_str(&format!(", {failed} failed"));
    } else if test_failed {
        out.push_str(", test failed");
    }
    out
}

/// A file's tests as rows under its name: each tested function with its assertion tally, and one
/// row for the top-level `test({ .. })` blocks together. A file with no tests has no rows and no
/// heading. Every line ends in a newline.
pub fn render_test_rows(label: &str, records: &[TestRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }
    let mut out = format!("{label}:\n");
    for record in records.iter().filter(|r| !r.is_standalone()) {
        out.push_str(&format!(
            "    {}: {}\n",
            record.name,
            tally(
                record.assertions_passed,
                record.assertions_failed,
                record.failed
            )
        ));
    }
    let standalone: Vec<_> = records.iter().filter(|r| r.is_standalone()).collect();
    if !standalone.is_empty() {
        out.push_str(&format!(
            "    standalone tests: {}\n",
            tally(
                standalone.iter().map(|r| r.assertions_passed).sum(),
                standalone.iter().map(|r| r.assertions_failed).sum(),
                standalone.iter().any(|r| r.failed),
            )
        ));
    }
    out
}

/// Each way a file's sections disagree with what it does, naming the file. A finding a rewrite
/// repairs is followed by the line `cargo handoff` reads, naming the file by its path from the
/// project root. Every line ends in a newline.
pub fn render_findings(label: &str, found: &[Finding]) -> String {
    let mut out = String::new();
    for f in found {
        out.push_str(&format!("{label}: {}\n", f.message));
        if let Some((section, _)) = &f.fix {
            out.push_str(&format!(
                "{}\n",
                handoff::protocol::Fill::Section {
                    case: label.trim_start_matches("./").to_string(),
                    section: section.clone(),
                }
                .marker()
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, passed: u64, failed: u64, test_failed: bool) -> TestRecord {
        TestRecord {
            file: None,
            name: name.to_string(),
            assertions_passed: passed,
            assertions_failed: failed,
            failed: test_failed,
        }
    }

    #[test]
    fn rows_list_each_function_and_total_the_standalone_blocks() {
        let rows = render_test_rows(
            "./a.aipl",
            &[
                record("add", 3, 0, false),
                record("sub", 1, 1, true),
                record("block 1", 2, 0, false),
                record("block 2", 1, 0, false),
            ],
        );
        assert_eq!(
            rows,
            "./a.aipl:\n    add: 3 assertions passed\n    sub: 1 assertion passed, 1 failed\n    standalone tests: 3 assertions passed\n"
        );
    }

    #[test]
    fn a_test_that_failed_without_a_failing_assertion_says_so() {
        let rows = render_test_rows("./a.aipl", &[record("f", 1, 0, true)]);
        assert_eq!(rows, "./a.aipl:\n    f: 1 assertion passed, test failed\n");
    }

    #[test]
    fn a_file_with_no_tests_has_no_rows() {
        assert_eq!(render_test_rows("./a.aipl", &[]), "");
    }

    #[test]
    fn a_finding_with_a_fix_is_followed_by_the_line_handoff_reads() {
        let found = [
            Finding::unfixable("the tests leaked"),
            Finding {
                message: "stdout is stale".to_string(),
                fix: Some(("stdout".to_string(), "hi\n".to_string())),
            },
        ];
        assert_eq!(
            render_findings("./sub/a.aipl", &found),
            "./sub/a.aipl: the tests leaked\n./sub/a.aipl: stdout is stale\nhandoff-fill: section\tsub/a.aipl\tstdout\n"
        );
    }

    fn result(label: &str, total: i64, failed: i64) -> CheckResult {
        CheckResult {
            label: label.to_string(),
            formatting: Formatting::Clean,
            run: TestRun {
                total,
                passed: total - failed,
                failed,
                ..TestRun::default()
            },
            outcome: Ok(Checked {
                exit_code: i64::from(failed > 0),
                traces: None,
                sections: Vec::new(),
                fixed: Vec::new(),
                fix_error: None,
                ran_tests: true,
            }),
        }
    }

    #[test]
    fn a_batch_adds_its_files_up() {
        let mut totals = BatchTotals::default();
        totals.add(&result("./a.aipl", 2, 0));
        let mut b = result("./b.aipl", 3, 1);
        b.run.failure_report = "test ./b.aipl::f ... FAIL\n".to_string();
        totals.add(&b);
        assert!(!totals.passed_all());
        assert_eq!(
            totals.render(),
            "test failures:\ntest ./b.aipl::f ... FAIL\n2 files, 5 tests: 4 passed, 1 failed\n"
        );
    }

    #[test]
    fn a_batch_names_each_way_a_file_can_fail_that_the_counts_hide() {
        let mut totals = BatchTotals::default();
        let mut broken = result("./a.aipl", 0, 0);
        broken.outcome = Err("error: no".to_string());
        totals.add(&broken);
        let mut unformatted = result("./b.aipl", 1, 0);
        unformatted.formatting = Formatting::Needs;
        totals.add(&unformatted);
        assert_eq!(
            totals.render(),
            "2 files, 1 tests: 1 passed, 0 failed\n1 file failed to compile\n1 file needs formatting\n"
        );
    }

    #[test]
    fn a_clean_single_file_batch_is_one_line() {
        let mut totals = BatchTotals::default();
        totals.add(&result("./a.aipl", 4, 0));
        assert!(totals.passed_all());
        assert_eq!(totals.render(), "1 file, 4 tests: 4 passed, 0 failed\n");
    }

    #[test]
    fn selectors_are_taken_out_of_the_arguments_and_none_means_every_part() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Parts::take_from(args(&["a.aipl", "--fix"])),
            (args(&["a.aipl", "--fix"]), None)
        );
        let (rest, parts) = Parts::take_from(args(&["--tests", "a.aipl", "--format"]));
        assert_eq!(rest, args(&["a.aipl"]));
        assert_eq!(
            parts,
            Some(Parts {
                format: true,
                compile: false,
                tests: true,
                benchmarks: false
            })
        );
    }

    #[test]
    fn formatting_lines_name_the_file() {
        assert_eq!(Formatting::Clean.line("a.aipl"), None);
        assert_eq!(
            Formatting::Needs.line("a.aipl"),
            Some("a.aipl: needs formatting (run `aipl check --fix a.aipl`)".to_string())
        );
        assert_eq!(
            Formatting::Rewritten.line("a.aipl"),
            Some("formatted a.aipl".to_string())
        );
        assert!(Formatting::Needs.is_unformatted());
        assert!(!Formatting::Rewritten.is_unformatted());
    }

    #[test]
    fn a_file_reports_rows_then_what_fix_wrote_then_traces_then_findings() {
        let mut checked = result("./a.aipl", 1, 0);
        checked.run.records = vec![record("f", 1, 0, false)];
        let Ok(c) = &mut checked.outcome else {
            unreachable!()
        };
        c.fixed = vec!["stdout".to_string()];
        c.traces = Some("error: trace".to_string());
        c.sections = vec![Finding::unfixable("exit code is stale")];
        assert_eq!(
            checked.render(),
            "./a.aipl:\n    f: 1 assertion passed\nfixed ./a.aipl: stdout\nerror: trace\n./a.aipl: exit code is stale\n"
        );
    }

    #[test]
    fn a_compile_failure_follows_the_rows_of_the_tests_that_ran_first() {
        let mut broken = result("./a.aipl", 1, 0);
        broken.run.records = vec![record("f", 1, 0, false)];
        broken.outcome = Err("error: bad".to_string());
        assert_eq!(
            broken.render(),
            "./a.aipl:\n    f: 1 assertion passed\nerror: bad\n"
        );
    }
}
