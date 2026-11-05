//! What checking one file found, and how that is written down.
//!
//! `aipl check` produces a [`CheckResult`] per file — plain data: the formatting verdict, the tests'
//! rows, the compile failure if there was one, the traces, the section findings and what `--fix`
//! wrote — and the command prints it. Producing and printing are separate so that something other
//! than the terminal (a section that pins part of this output) can read the same result, and so each
//! piece has one renderer, tested apart from the command.
//!
//! Nothing here runs a file. The runtime's pass/fail tallies and failure text stay in
//! `aipl::codegen` because they accumulate across a batch; a file's result carries only its own rows.

use crate::codegen::TestRecord;
use crate::sections::Finding;

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

    /// The line this step prints for `label`, and whether it goes to stdout (`false`: stderr).
    pub fn line(&self, label: &str) -> Option<(String, bool)> {
        match self {
            Formatting::Clean => None,
            Formatting::Needs => Some((
                format!("{label}: needs formatting (run `aipl check --fix {label}`)"),
                false,
            )),
            Formatting::Rewritten => Some((format!("formatted {label}"), true)),
            Formatting::RewriteFailed(why) => {
                Some((format!("{label}: could not rewrite: {why}"), false))
            }
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
    /// The file's tests, one record each; empty when it has none or never compiled.
    pub tests: Vec<TestRecord>,
    /// `Err` is a rendered load/compile failure.
    pub outcome: Result<Checked, String>,
}

impl CheckResult {
    /// Whether the file compiled and every test passed.
    pub fn tests_passed(&self) -> bool {
        self.outcome.as_ref().is_ok_and(|c| c.exit_code == 0)
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
        render_test_rows(&self.label, &self.tests)
    }

    /// The line saying which sections `--fix` wrote, or why it could not.
    pub fn render_fixed(&self) -> Option<(String, bool)> {
        let checked = self.outcome.as_ref().ok()?;
        if let Some(why) = &checked.fix_error {
            return Some((
                format!("{}: could not write the fixed sections: {why}", self.label),
                false,
            ));
        }
        (!checked.fixed.is_empty()).then(|| {
            (
                format!("fixed {}: {}", self.label, checked.fixed.join(", ")),
                true,
            )
        })
    }

    /// The `N tests: P passed, F failed` line of a single-file report, when tests ran.
    pub fn render_summary(&self, total: i64, passed: i64, failed: i64) -> Option<String> {
        let ran = self.outcome.as_ref().is_ok_and(|c| c.ran_tests);
        ran.then(|| format!("{total} tests: {passed} passed, {failed} failed"))
    }

    /// The lines for each way the file's sections disagree with what it does, naming the file.
    pub fn render_findings(&self) -> String {
        match &self.outcome {
            Ok(checked) => render_findings(&self.label, &checked.sections),
            Err(_) => String::new(),
        }
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

    #[test]
    fn formatting_lines_go_to_the_stream_check_uses() {
        assert_eq!(Formatting::Clean.line("a.aipl"), None);
        assert_eq!(
            Formatting::Needs.line("a.aipl"),
            Some((
                "a.aipl: needs formatting (run `aipl check --fix a.aipl`)".to_string(),
                false
            ))
        );
        assert_eq!(
            Formatting::Rewritten.line("a.aipl"),
            Some(("formatted a.aipl".to_string(), true))
        );
        assert!(Formatting::Needs.is_unformatted());
        assert!(!Formatting::Rewritten.is_unformatted());
    }
}
