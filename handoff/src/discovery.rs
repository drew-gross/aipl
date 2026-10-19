//! Reading a nextest run: which tests failed, which failures a refill can fix,
//! and exactly what to regenerate.
//!
//! # Nothing is fillable unless the test says so
//!
//! [`Run::fillable`] is the one classifier, and it is a whitelist the failing
//! tests write themselves: a failure is fixable only if its output carries a
//! [`Fill`] marker (see [`handoff::protocol`]). Every other failure — a case that
//! will not compile, a crash, a leak, an in-language `.test`, a Rust assertion, a
//! test nobody taught the protocol — is stuck, and the gate stops and names it.
//! The inverse design recognised the failures it could *not* fix by pattern, and
//! so had to be complete to be safe; this one only has to be honest.
//!
//! # Where the facts come from
//!
//! A step run under [`crate::runner::Cmd::json`] produces two streams, and this
//! module needs **both**:
//!
//! - **stdout** is nextest's `libtest-json-plus` event stream, one JSON object
//!   per line. A failing test's record carries its own captured output, so a
//!   mismatch message arrives already attributed to the test that produced it —
//!   no grepping a flat blob and hoping the nearby lines belong together.
//! - **stderr** is the ordinary human output, and it is the *only* place an
//!   abnormal exit is reported. The JSON is a **libtest** schema, and libtest
//!   has no concept of a signal: a segfaulting test is
//!   `{"event":"failed","stdout":""}` and nothing more, while stderr says
//!   `SIGSEGV [ 0.008s] (2/2) …`. Since "this crashed" is precisely the
//!   condition the gate must stop on rather than refill, that one fact is still
//!   read from the human stream. (Verified against nextest 0.9.143 by making a
//!   test abort and another segfault.)
//!
//! # Why every message the gate wants is on a *failed* test
//!
//! Passing tests carry no output in the JSON — `--success-output` does not
//! populate an `ok` record. That would be a problem except that every author
//! helper the gate drives (`format_corpus`, `fill_case_tests`, `fill_expected`,
//! `fill_staged_ir`, `promote_staged_ir`) *diverges on purpose* so its summary
//! is visible, exactly as `tests/cases.rs` puts it: "Diverges like the other
//! author helpers". So their confirmations land in a `failed` record. If one
//! ever stops diverging, [`Run::helper_output`] says so in as many words instead
//! of reporting a mysteriously absent message.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use handoff::protocol::Fill;
use regex::Regex;

use crate::runner::Output;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static pattern compiles"))
}

/// One failed test: its name, everything it printed, and what it asked for.
pub struct Failure {
    /// The bare test name, as a filter would spell it — `cases_arrays_slice`,
    /// or `dogfood_ir::checked_in_ir_is_current` for a suite that is a `mod` of
    /// a merged target.
    pub name: String,
    /// The test's captured output. libtest-json folds a test's stdout and stderr
    /// together into one field, so a panic message (stderr) is here too.
    pub output: String,
    /// The remedies the test asked for, from the marker lines in `output`.
    fills: Vec<Fill>,
    /// Marker lines that did not parse. A test that tried to ask for something
    /// and got it wrong is stuck, not silent.
    malformed: Vec<String>,
}

impl Failure {
    pub fn new(name: impl Into<String>, output: impl Into<String>) -> Failure {
        let output = output.into();
        let (fills, malformed) = Fill::scan(&output);
        Failure {
            name: name.into(),
            output,
            fills,
            malformed,
        }
    }

    /// Whether a regeneration can fix this: it asked for one, and asked
    /// correctly.
    fn is_fillable(&self) -> bool {
        !self.fills.is_empty() && self.malformed.is_empty()
    }
}

/// A parsed nextest run.
pub struct Run {
    failures: Vec<Failure>,
    /// Abnormal-exit lines lifted from the human stream — the one thing the JSON
    /// cannot say (see the module docs).
    abnormal: Vec<String>,
    /// Whether the JSON stream contained any well-formed event at all.
    saw_events: bool,
}

impl Run {
    /// Parse a step's output: `stdout` is the JSON stream, `stderr` the human
    /// one.
    pub fn parse(out: &Output) -> Run {
        static ABNORMAL: OnceLock<Regex> = OnceLock::new();
        let mut failures = Vec::new();
        let mut saw_events = false;

        for line in out.stdout.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue; // not an event line; nextest may print others
            };
            if v.get("type").is_none() {
                continue;
            }
            saw_events = true;
            if v["type"] != "test" || v["event"] != "failed" {
                continue;
            }
            let Some(name) = v["name"].as_str() else {
                continue;
            };
            failures.push(Failure::new(
                bare_name(name),
                v["stdout"].as_str().unwrap_or_default(),
            ));
        }

        // nextest's status column, which carries what libtest's schema can't.
        let abnormal_re = re(&ABNORMAL, r"^ *(SIGSEGV|ABORT|TIMEOUT|LEAK-FAIL) \[");
        let abnormal = out
            .stderr
            .lines()
            .filter(|l| abnormal_re.is_match(l))
            .map(str::to_string)
            .collect();

        Run {
            failures,
            abnormal,
            saw_events,
        }
    }

    /// Whether the JSON stream produced anything at all.
    ///
    /// A run that executed tests and emitted no events means the machine-readable
    /// interface moved out from under the gate (it is still experimental in
    /// nextest) — better to say so than to conclude "nothing failed" from a
    /// stream that was never parsed.
    pub fn saw_events(&self) -> bool {
        self.saw_events
    }

    /// Every failed test, in the order nextest reported them.
    pub fn failures(&self) -> &[Failure] {
        &self.failures
    }

    /// Everything the failed tests printed, concatenated — the haystack for the
    /// harness's own messages.
    pub fn failure_output(&self) -> String {
        self.failures
            .iter()
            .map(|f| f.output.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The output of the single author helper this step ran, or an explanation
    /// of why there isn't any.
    ///
    /// Every helper diverges on purpose (see the module docs), so "the step
    /// passed" means the confirmation the caller is about to look for was never
    /// printed — a far more useful thing to report than its absence.
    pub fn helper_output(&self) -> Result<String, String> {
        // Told apart on purpose. "No events at all" means the step was not run
        // under `Cmd::json` (or the experimental interface moved) — a wiring
        // bug, and one that otherwise reads as the helper having passed.
        if !self.saw_events {
            return Err(
                "the step produced no JSON events on stdout, so there is nothing to read. Either \
                 it was not built with `Cmd::json`, or nextest's experimental libtest-json \
                 interface changed — check MESSAGE_FORMAT_VERSION in handoff/src/runner.rs."
                    .to_string(),
            );
        }
        if self.failures.is_empty() {
            return Err(
                "the helper exited 0. Every author helper diverges on purpose so its summary is \
                 visible, so this means it never ran (a filter that matched nothing?) or it no \
                 longer diverges — either way there is no summary to confirm."
                    .to_string(),
            );
        }
        Ok(self.failure_output())
    }

    /// What this run's failures ask a regeneration to fix, and which failures
    /// ask for nothing.
    ///
    /// The gate proceeds only if [`Fillable::stuck`] is empty. The check comes
    /// before any regeneration: burning a corpus run on a refill that cannot
    /// help costs minutes and fixes nothing.
    ///
    /// A test is stuck when it printed no [`Fill`] marker, printed one that does
    /// not parse, or died abnormally (a signal or a timeout, which the JSON
    /// cannot see and which no refill answers). A test that both asks for a
    /// refill and fails some other way is the writer's to untangle: the final
    /// run, which re-runs it after the refill, is what catches a refill that did
    /// not help.
    pub fn fillable(&self) -> Fillable {
        let mut fills: BTreeSet<Fill> = BTreeSet::new();
        // The crash lines first — they are the reason to stop, and a segfault may
        // print nothing else at all.
        let mut stuck = self.abnormal.clone();
        for f in &self.failures {
            if f.is_fillable() {
                fills.extend(f.fills.iter().cloned());
                continue;
            }
            if f.malformed.is_empty() {
                stuck.push(format!("{}: {}", f.name, first_line(&f.output)));
            } else {
                for why in &f.malformed {
                    stuck.push(format!("{}: unreadable fill marker: {why}", f.name));
                }
            }
        }
        stuck.truncate(20);
        Fillable {
            fills: fills.into_iter().collect(),
            stuck,
        }
    }

    /// The baseline failures in this run (see [`Baseline`]), from the
    /// [`Fill::BaselineMoved`] and [`Fill::BaselineMissing`] markers
    /// `check_baseline` in `tests/cases.rs` prints.
    pub fn baseline(&self) -> Baseline {
        let mut found = Baseline::default();
        for fill in self.failures.iter().flat_map(|f| &f.fills) {
            match fill {
                Fill::BaselineMoved { case, moves } => {
                    found.moved.insert(case.clone(), moves.clone());
                }
                Fill::BaselineMissing { case } => {
                    found.missing.insert(case.clone());
                }
                _ => {}
            }
        }
        found
    }

    /// Whether the run failed for nothing but baseline moves: at least one failed
    /// test, none abnormal, and every one of them asking only for a baseline row.
    ///
    /// A run like that is complete — every other test passed — so the gate can
    /// refresh the rows and report the moves at the end instead of stopping.
    pub fn only_baseline(&self) -> bool {
        !self.failures.is_empty()
            && self.abnormal.is_empty()
            && self.failures.iter().all(|f| {
                f.is_fillable()
                    && f.fills.iter().all(|x| {
                        matches!(x, Fill::BaselineMoved { .. } | Fill::BaselineMissing { .. })
                    })
            })
    }

    /// Classify the run into the remediation work it implies.
    ///
    /// Reads only what the failing tests asked for, so a stuck failure
    /// contributes nothing here — [`Run::fillable`] is what stops the gate on it.
    pub fn plan(&self) -> Plan {
        let mut plan = Plan::default();
        let mut cases: BTreeSet<String> = BTreeSet::new();
        let mut sections: BTreeSet<String> = BTreeSet::new();
        for fill in self.failures.iter().flat_map(|f| &f.fills) {
            match fill {
                // One scoped fill refreshes every section of a case, so the case
                // list is deduped even when a case mismatched in several.
                Fill::Section { case, section } => {
                    plan.need_fill = true;
                    cases.insert(case.clone());
                    sections.insert(section.clone());
                }
                // A case with no `#[test]` never ran, so its sections were never
                // compared: fold it into the fill set and finish it in this pass
                // rather than in a second handoff.
                Fill::CaseTests { added } => {
                    plan.need_case_tests = true;
                    if !added.is_empty() {
                        plan.need_fill = true;
                        cases.extend(added.iter().cloned());
                    }
                }
                Fill::Ir => plan.need_ir = true,
                Fill::Docs => plan.need_docs = true,
                Fill::Compilers => plan.need_compilers = true,
                // Settled by `refresh_perfmon` at the end, not a remediation step
                // of their own.
                Fill::BaselineMoved { .. } | Fill::BaselineMissing { .. } => {}
            }
        }
        // Only a `report` is numbers the compiler produces; every other section
        // records what a program does, so a change to one deserves a read of the
        // diff.
        plan.behavioral_changed = sections.iter().any(|s| s != "report");
        plan.changed_sections = sections.into_iter().collect();
        plan.fail_cases = cases.into_iter().collect();
        plan
    }
}

/// What a run's failures ask for, and what they leave unexplained. See
/// [`Run::fillable`].
#[derive(Debug, PartialEq, Eq)]
pub struct Fillable {
    /// Every distinct remedy the failing tests asked for.
    pub fills: Vec<Fill>,
    /// Failures that asked for none, as `name: first line of output` (or the
    /// abnormal-exit line). Anything here stops the gate.
    pub stuck: Vec<String>,
}

/// The first non-blank line of a failed test's output, trimmed, for naming it.
fn first_line(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

/// Cases whose row in `tests/performance_metrics.md` disagreed with their run.
///
/// Neither kind is stale *text* a section refill can rewrite, and neither is a
/// broken program: a `moved` row is a number somebody should look at (the change
/// may be the regression the table exists to catch), and a `missing` row is
/// bookkeeping for a case that has none yet. Both are rewritten by one scoped
/// `refresh_perfmon` at the end of the gate, and a move is reported for review
/// in the diff — see [`Run::baseline`].
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Baseline {
    /// Display path (`cases/x/y.aipl`, the text `AIPL_CASE` matches) to the
    /// lines saying what moved, as the harness printed them.
    pub moved: BTreeMap<String, Vec<String>>,
    /// Display paths with no row at all.
    pub missing: BTreeSet<String>,
}

impl Baseline {
    /// Fold `other` in: a case reported by either run is reported.
    pub fn merge(&mut self, other: Baseline) {
        self.moved.extend(other.moved);
        self.missing.extend(other.missing);
    }
}

/// The bare test name from a JSON record's `name`.
///
/// nextest spells it `<crate>::<binary>$<test>` — `aipl::cases$cases_x_y`,
/// `aipl::compiler$fmt::format_corpus`. Everything downstream matches on the
/// part after `$`, which is what a `-E 'test(=…)'` filter and the harness's own
/// messages use.
fn bare_name(name: &str) -> String {
    name.rsplit('$').next().unwrap_or(name).to_string()
}

/// What remediation a discovery run calls for.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Plan {
    /// A section mismatch, or a *missing* required section (what a brand-new
    /// case reports — `fill_expected` creates one that isn't there, so a new
    /// case finishes in the same pass).
    pub need_fill: bool,
    /// One of the two IR gates tripped.
    pub need_ir: bool,
    /// The checked-in per-case `#[test]` list has drifted from the tree.
    pub need_case_tests: bool,
    /// The checked-in documentation site under `docs/` no longer matches the
    /// source it documents.
    pub need_docs: bool,
    /// A project under `projects/` is carrying a compiler that is not the one
    /// this repository now builds.
    pub need_compilers: bool,
    /// The exact cases to refill — `AIPL_CASE` filter strings.
    pub fail_cases: Vec<String>,
    /// Distinct section names that changed, for the closing review note.
    pub changed_sections: Vec<String>,
    /// Any of the changed sections is behavioral, so the git diff is worth a
    /// closer look than a metrics-only refill.
    pub behavioral_changed: bool,
}

impl Plan {
    /// Nothing here is remediable — the caller should stop rather than guess.
    pub fn is_empty(&self) -> bool {
        !self.need_fill
            && !self.need_ir
            && !self.need_case_tests
            && !self.need_docs
            && !self.need_compilers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Run` from `(test name, that test's output)` pairs, as the JSON would
    /// have yielded.
    fn run(failures: &[(&str, &str)]) -> Run {
        Run {
            failures: failures.iter().map(|(n, o)| Failure::new(*n, *o)).collect(),
            abnormal: Vec::new(),
            saw_events: true,
        }
    }

    /// A `Run` parsed from real JSON, to keep the schema coupling tested.
    fn parse(stdout: &str, stderr: &str) -> Run {
        Run::parse(&Output {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            merged: format!("{stdout}\n{stderr}"),
        })
    }

    /// The output of a test that asks for `fills`, with the prose around the
    /// markers a real panic message has.
    fn asking(fills: &[Fill]) -> String {
        let markers: Vec<String> = fills.iter().map(Fill::marker).collect();
        format!(
            "thread 'x' panicked:\nsomething is stale\n{}",
            markers.join("\n")
        )
    }

    fn section(case: &str, section: &str) -> Fill {
        Fill::Section {
            case: case.to_string(),
            section: section.to_string(),
        }
    }

    #[test]
    fn parses_the_json_event_stream() {
        // Real shapes from nextest 0.9.143.
        let stdout = concat!(
            r#"{"type":"suite","event":"started","test_count":2}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"aipl::cases$cases_x_y"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"aipl::cases$cases_fine","exec_time":0.1}"#,
            "\n",
            r#"{"type":"test","event":"failed","name":"aipl::cases$cases_x_y","exec_time":0.2,"#,
            r#""stdout":"[cases/x/y]: `stdout` mismatch\nhandoff-fill: section\tcases/x/y\tstdout\n"}"#,
            "\n",
            r#"{"type":"suite","event":"failed","passed":1,"failed":1}"#,
        );
        let run = parse(stdout, "");
        assert!(run.saw_events());
        assert_eq!(run.failures().len(), 1, "only the failed test counts");
        assert_eq!(run.failures()[0].name, "cases_x_y");
        assert!(run.failure_output().contains("`stdout` mismatch"));
        // The marker survives the JSON round trip, tab and all.
        assert_eq!(
            run.fillable(),
            Fillable {
                fills: vec![section("cases/x/y", "stdout")],
                stuck: vec![],
            }
        );
    }

    #[test]
    fn strips_the_binary_prefix_from_a_name() {
        // `<crate>::<binary>$<test>`; the module path after `$` is kept, because
        // that is what filters and the gate's own matchers use.
        assert_eq!(bare_name("aipl::cases$cases_x_y"), "cases_x_y");
        assert_eq!(
            bare_name("aipl::compiler$fmt::format_corpus"),
            "fmt::format_corpus"
        );
        assert_eq!(bare_name("no_dollar_sign"), "no_dollar_sign");
    }

    #[test]
    fn an_abnormal_exit_is_read_from_the_human_stream() {
        // libtest's schema has no way to say "signal", so a segfault is an
        // ordinary `failed` with empty output. Only stderr knows.
        let stdout =
            r#"{"type":"test","event":"failed","name":"aipl::cases$cases_x_y","stdout":""}"#;
        let stderr = "     SIGSEGV [   0.008s] (2/2) aipl::cases cases_x_y\n\
                      (test aborted with signal 11: SIGSEGV)";
        let run = parse(stdout, stderr);
        assert!(
            !run.fillable().stuck.is_empty(),
            "a crash must stop the gate, not trigger a refill"
        );
    }

    /// A crash that happens in a test which had already asked for a refill is
    /// still a crash: the abnormal line is on its own account.
    #[test]
    fn a_crash_is_stuck_even_beside_a_request() {
        let mut run = run(&[("cases_x_y", &asking(&[section("cases/x/y", "stdout")]))]);
        run.abnormal.push("SIGSEGV [ 0.008s] cases_x_y".to_string());
        assert_eq!(run.fillable().stuck, ["SIGSEGV [ 0.008s] cases_x_y"]);
        assert!(!run.only_baseline());
    }

    #[test]
    fn an_empty_json_stream_is_detectable() {
        // The experimental interface moving would look exactly like this.
        let run = parse(
            "",
            "     Summary [ 1.0s] 800 tests run: 799 passed, 1 failed",
        );
        assert!(!run.saw_events());
    }

    // ---- The polarity: nothing is fillable unless the test says so.

    /// The whole point. Every one of these used to need a pattern to be told
    /// apart from staleness, or was waved through as staleness for want of one.
    #[test]
    fn a_failure_that_asks_for_nothing_is_stuck() {
        for (name, output) in [
            ("cases_x_y", "[cases/x/y]: compile failed: no such function"),
            (
                "cases_x_y",
                "[cases/x/y]: (in-language tests) failed: 1 assertion",
            ),
            ("mono::specializes_generics", "assertion failed"),
            ("cases_x_y", ""),
            // The report test that started this: a stale pinned report reads as
            // staleness to anyone, but only the test knows it is one.
            (
                "report::report_agrees_with_the_pinned_report",
                "left: \"allocations 2\"\n right: \"allocations 3\"",
            ),
        ] {
            let stuck = run(&[(name, output)]).fillable().stuck;
            assert_eq!(stuck.len(), 1, "{name}: {output}");
            assert!(stuck[0].starts_with(&format!("{name}: ")), "{stuck:?}");
        }
    }

    /// Verbatim from `tests/cases.rs`, second line included: it points at
    /// `fill_expected`, which reads as fillable. The harness runs the leak gate in
    /// fill mode too, so a refill cannot fix it — and it asks for nothing, so it
    /// is not treated as if it could.
    #[test]
    fn a_leak_that_names_fill_expected_is_still_stuck() {
        let output = "[cases/structs/x]: memory leak — allocations (3) != deallocations (2)\n\
                      Fix the leak, then re-run `AIPL_CASE='cases/structs/x' cargo test \
                      --test cases -- --ignored fill_expected` to refresh the expected counts.";
        let run = run(&[("cases_structs_x", output)]);
        assert_eq!(
            run.fillable().stuck,
            vec![
                "cases_structs_x: [cases/structs/x]: memory leak — allocations (3) \
                 != deallocations (2)"
            ]
        );
        assert!(run.plan().is_empty());
    }

    #[test]
    fn stuck_lines_name_the_test_they_came_from() {
        let stuck = run(&[("cases_x_y", "\n  \n[cases/x/y]: compile failed: boom")])
            .fillable()
            .stuck;
        assert_eq!(stuck, vec!["cases_x_y: [cases/x/y]: compile failed: boom"]);
        assert_eq!(
            run(&[("cases_x_y", "")]).fillable().stuck,
            vec!["cases_x_y: (no output)"]
        );
    }

    /// A marker the gate cannot read is the writer and the reader disagreeing —
    /// the one case that must not read as either "nothing asked" or "asked".
    #[test]
    fn an_unreadable_marker_is_stuck_and_says_why() {
        let stuck = run(&[("cases_x_y", "handoff-fill: nonsense\ntext")])
            .fillable()
            .stuck;
        assert_eq!(stuck.len(), 1);
        assert!(stuck[0].contains("unreadable fill marker"), "{stuck:?}");
        assert!(stuck[0].contains("unknown kind"), "{stuck:?}");
    }

    #[test]
    fn a_request_beside_a_stuck_failure_does_not_unstick_it() {
        let fillable = run(&[
            ("cases_a_b", &asking(&[section("cases/a/b", "stdout")])),
            ("mono::broken", "assertion failed"),
        ])
        .fillable();
        assert_eq!(fillable.fills, vec![section("cases/a/b", "stdout")]);
        assert_eq!(fillable.stuck, vec!["mono::broken: assertion failed"]);
    }

    #[test]
    fn requests_are_deduped_across_failures() {
        let fillable = run(&[
            ("a", &asking(&[Fill::Docs])),
            ("b", &asking(&[Fill::Docs, Fill::Ir])),
        ])
        .fillable();
        assert_eq!(fillable.fills, vec![Fill::Ir, Fill::Docs]);
        assert!(fillable.stuck.is_empty());
    }

    // ---- The plan, from what was asked for.

    #[test]
    fn each_artifact_request_asks_for_its_regeneration() {
        let plan = |fill: Fill| run(&[("t", &asking(&[fill]))]).plan();
        assert!(plan(Fill::Ir).need_ir);
        assert!(plan(Fill::Docs).need_docs);
        assert!(plan(Fill::Compilers).need_compilers);
        for fill in [Fill::Ir, Fill::Docs, Fill::Compilers] {
            assert!(!plan(fill).is_empty());
        }
    }

    #[test]
    fn sections_name_their_cases() {
        let run = run(&[
            (
                "cases_strings_slice",
                &asking(&[
                    section("cases/strings/slice", "report"),
                    section("cases/strings/slice", "stdout"),
                ]),
            ),
            (
                "cases_arrays_take",
                &asking(&[section("cases/arrays/take", "errors")]),
            ),
        ]);
        let plan = run.plan();
        assert!(plan.need_fill);
        // One scoped fill refreshes every section of a case, so the case list is
        // deduped even though `slice` mismatched twice.
        assert_eq!(
            plan.fail_cases,
            ["cases/arrays/take", "cases/strings/slice"]
        );
        assert_eq!(plan.changed_sections, ["errors", "report", "stdout"]);
        assert!(plan.behavioral_changed);
    }

    /// The sections the old pattern (`[a-z ]+`) could not even spell: a case
    /// whose only stale section was an `expect file:` was never planned, and
    /// stopped the final run instead.
    #[test]
    fn a_section_with_punctuation_in_its_name_is_planned() {
        let plan = run(&[(
            "cases_files_write_file",
            &asking(&[section("cases/files/write_file", "expect file: out.txt")]),
        )])
        .plan();
        assert_eq!(plan.fail_cases, ["cases/files/write_file"]);
    }

    #[test]
    fn a_metrics_only_refill_is_not_behavioral() {
        let plan = run(&[(
            "report::report_agrees_with_the_pinned_report",
            &asking(&[section("cases/report/runtime_calls", "report")]),
        )])
        .plan();
        assert!(plan.need_fill);
        assert!(!plan.behavioral_changed);
    }

    #[test]
    fn new_cases_are_folded_into_the_fill_set() {
        let plan = run(&[(
            "every_case_has_a_test",
            &asking(&[Fill::CaseTests {
                added: vec![
                    "cases/optimizations/new_one".into(),
                    "cases/optimizations/new_two".into(),
                ],
            }]),
        )])
        .plan();
        assert!(plan.need_fill, "a new case still needs its sections filled");
        assert!(plan.need_case_tests);
        assert_eq!(
            plan.fail_cases,
            ["cases/optimizations/new_one", "cases/optimizations/new_two"],
        );
    }

    /// A list that only has *stale* entries has no file left to fill.
    #[test]
    fn deleted_cases_only_regenerate_the_list() {
        let plan = run(&[(
            "every_case_has_a_test",
            &asking(&[Fill::CaseTests { added: vec![] }]),
        )])
        .plan();
        assert!(plan.need_case_tests);
        assert!(!plan.need_fill);
        assert!(plan.fail_cases.is_empty());
    }

    /// A stuck failure contributes nothing to the plan: it asked for nothing, and
    /// reading a request into its prose is the mistake this design removes.
    #[test]
    fn prose_that_looks_like_a_mismatch_plans_nothing() {
        let plan = run(&[(
            "cases_x_y",
            "[cases/x/y]: `stdout` mismatch\nIf this change is intended, run `fill_expected`.",
        )])
        .plan();
        assert!(plan.is_empty());
    }

    // ---- Helper output.

    #[test]
    fn a_helper_that_did_not_diverge_says_so() {
        let err = run(&[]).helper_output().unwrap_err();
        assert!(err.contains("diverges on purpose"), "{err}");
    }

    #[test]
    fn a_step_with_no_json_is_named_as_such() {
        // The failure mode of forgetting `Cmd::json` on a step: the helper did
        // fail, but there is no structured output to see it in. Reporting that
        // as "the helper exited 0" sends the reader after the wrong thing.
        let err = parse("", "        FAIL [ 0.6s] (1/1) aipl::cases fill_expected")
            .helper_output()
            .unwrap_err();
        assert!(err.contains("no JSON events"), "{err}");
        assert!(!err.contains("exited 0"), "{err}");
    }

    // ---- Baseline rows.

    fn moved() -> String {
        asking(&[Fill::BaselineMoved {
            case: "cases/x/y.aipl".into(),
            moves: vec![
                "allocations: 3 -> 4 (+33.3%)".into(),
                "instructions: 120 -> 130 (+8.3%)".into(),
            ],
        }])
    }

    fn missing() -> String {
        asking(&[Fill::BaselineMissing {
            case: "cases/new/one.aipl".into(),
        }])
    }

    #[test]
    fn reads_moved_and_missing_baselines() {
        let r = run(&[("cases_x_y", &moved()), ("cases_new_one", &missing())]);
        let b = r.baseline();
        assert_eq!(
            b.moved.into_iter().collect::<Vec<_>>(),
            [(
                "cases/x/y.aipl".to_string(),
                vec![
                    "allocations: 3 -> 4 (+33.3%)".to_string(),
                    "instructions: 120 -> 130 (+8.3%)".to_string()
                ]
            )]
        );
        assert_eq!(
            b.missing.into_iter().collect::<Vec<_>>(),
            ["cases/new/one.aipl"]
        );
    }

    #[test]
    fn a_run_of_only_baseline_failures_is_complete() {
        assert!(run(&[("cases_x_y", &moved()), ("cases_new_one", &missing())]).only_baseline());
        // One failure of any other kind makes it an ordinary failing run.
        assert!(!run(&[
            ("cases_x_y", &moved()),
            ("cases_a_b", &asking(&[section("cases/a/b.aipl", "stdout")]))
        ])
        .only_baseline());
        assert!(!run(&[
            ("cases_x_y", &moved()),
            ("mono::broken", "assertion failed")
        ])
        .only_baseline());
        // A green run is not "only baseline failures" either.
        assert!(!run(&[]).only_baseline());
    }

    #[test]
    fn a_baseline_failure_is_not_a_stale_section_or_a_broken_case() {
        let r = run(&[("cases_x_y", &moved()), ("cases_new_one", &missing())]);
        // Fixable, not stuck; and no remediation step of its own.
        assert!(r.fillable().stuck.is_empty());
        assert_eq!(r.plan(), Plan::default());
    }
}
