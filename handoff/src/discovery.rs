//! Reading a nextest run: which tests failed, which failures a refill can fix,
//! which it can't, and exactly which cases to refill.
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

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::runner::Output;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static pattern compiles"))
}

/// One failed test: its name and everything it printed.
pub struct Failure {
    /// The bare test name, as a filter would spell it — `cases_arrays_slice`,
    /// or `dogfood_ir::checked_in_ir_is_current` for a suite that is a `mod` of
    /// a merged target.
    pub name: String,
    /// The test's captured output. libtest-json folds a test's stdout and stderr
    /// together into one field, so a panic message (stderr) is here too.
    pub output: String,
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
            failures.push(Failure {
                name: bare_name(name),
                output: v["stdout"].as_str().unwrap_or_default().to_string(),
            });
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

    /// Names of every failed test, deduped.
    pub fn failed_names(&self) -> BTreeSet<String> {
        self.failures.iter().map(|f| f.name.clone()).collect()
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

    /// Failures a refill can't fix — there is no section to record actual output
    /// into. A case that won't build/link/spawn, a crash, or a failed in-language
    /// `.test`. Returns the offending lines, or empty when there are none.
    ///
    /// Checked before anything else: burning a full-corpus refill on one of these
    /// costs a corpus run and fixes nothing.
    pub fn unfillable(&self) -> Vec<String> {
        static RE: OnceLock<Regex> = OnceLock::new();
        // Assembled with `concat!` rather than one string with `\` continuations:
        // these are *raw* strings, where a trailing backslash is a literal
        // character and would silently turn the alternative it ends into
        // "…followed by a newline" — which never matches a single line.
        let re = re(
            &RE,
            concat!(
                r"(load|compile|emit|link|spawn|instrumented compile) failed:",
                r"|\(in-language tests\) failed:|Abort trap|SIGSEGV|SIGABRT",
            ),
        );
        // The crash lines first — they are the reason to stop, and a segfault may
        // print nothing else at all.
        let mut hits = self.abnormal.clone();
        for f in &self.failures {
            for line in f.output.lines().filter(|l| re.is_match(l)) {
                hits.push(format!("{}: {line}", f.name));
            }
        }
        hits.truncate(20);
        hits
    }

    /// Failed tests that are *not* a per-case test, the case-list gate, or a
    /// known IR-staleness gate — i.e. real test failures.
    ///
    /// Each case is its own `#[test]`, named `<prefix>_<display path>` for the
    /// three case roots; their fillable mismatches are handled by [`Run::plan`].
    /// [`Run::unfillable`] has already stopped on a case that genuinely broke, so
    /// what reaches here is a stale section.
    ///
    /// The optional `<module>::` prefix matters: the suites under `tests/suites/`
    /// are `mod`s of the merged `compiler`/`dogfood` targets, so a name arrives
    /// as `dogfood_ir::checked_in_ir_is_current`, not bare. Matching only the
    /// bare name silently reclassifies a regenerable staleness as a hard failure,
    /// and the run then stops at a step it was supposed to fix itself.
    pub fn hard_failures(&self) -> Vec<String> {
        static RE: OnceLock<Regex> = OnceLock::new();
        // `concat!`, not a `\` continuation — see the note in [`Run::unfillable`].
        let re = re(
            &RE,
            concat!(
                r"^([A-Za-z0-9_]+::)?((cases|examples|crates)_.*",
                r"|every_case_has_a_test|checked_in_ir_is_current|no_staged_ir_pending)$",
            ),
        );
        self.failed_names()
            .into_iter()
            .filter(|t| !re.is_match(t))
            .collect()
    }

    /// Classify the run into the remediation work it implies.
    pub fn plan(&self) -> Plan {
        static MISMATCH: OnceLock<Regex> = OnceLock::new();
        static GATES: OnceLock<Regex> = OnceLock::new();
        static CASE_TESTS: OnceLock<Regex> = OnceLock::new();
        static SECTION: OnceLock<Regex> = OnceLock::new();
        static BEHAVIORAL: OnceLock<Regex> = OnceLock::new();
        static CASE_PATH: OnceLock<Regex> = OnceLock::new();

        let out = self.failure_output();
        let mut plan = Plan {
            need_fill: re(
                &MISMATCH,
                r"`[a-z ]+` mismatch|error mismatch|missing required `--- [a-z ]+ ---` section",
            )
            .is_match(&out),
            ..Plan::default()
        };

        let failed = self.failed_names();
        let gates = re(
            &GATES,
            r"^([A-Za-z0-9_]+::)?(checked_in_ir_is_current|no_staged_ir_pending)$",
        );
        plan.need_ir = failed.iter().any(|t| gates.is_match(t));
        let case_tests = re(&CASE_TESTS, r"^([A-Za-z0-9_]+::)?every_case_has_a_test$");
        plan.need_case_tests = failed.iter().any(|t| case_tests.is_match(t));

        // Distinct sections that changed, and whether any are behavioral.
        plan.changed_sections = re(&SECTION, r"`([a-z ]+)` mismatch")
            .captures_iter(&out)
            .map(|c| c[1].to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        plan.behavioral_changed = re(
            &BEHAVIORAL,
            r"`(stdout|stderr|exit code|errors|check)` mismatch|error mismatch",
        )
        .is_match(&out);

        // The exact cases that need refilling. Each mismatch prints
        // ``[<display-path>]: `<section>` mismatch`` (or `error mismatch`), and
        // that bracketed display path is precisely what `AIPL_CASE` filters on —
        // so we refill just the failing cases instead of paying for a
        // whole-corpus fill. Deduped: one case may report several mismatched
        // sections, and one scoped fill refreshes all of that case's sections.
        let case_path = re(
            &CASE_PATH,
            r"\[([^\]]+)\]: ((`[a-z ]+`|error) mismatch|missing required)",
        );
        let mut cases: BTreeSet<String> = case_path
            .captures_iter(&out)
            .map(|c| c[1].to_string())
            .collect();

        // Cases added since the checked-in `#[test]` list was generated. These
        // are the reason a brand-new case used to cost a second handoff: with no
        // `#[test]` yet, the discovery run never *executed* them, so their
        // unrecorded sections went unseen and `need_fill` stayed false — the
        // final run (after the list is regenerated) was the first thing to
        // notice. `every_case_has_a_test` already names them, so fold them into
        // the fill set and finish them in this pass.
        let undeclared = undeclared_cases(&out);
        if !undeclared.is_empty() {
            plan.need_fill = true;
            cases.extend(undeclared);
        }
        plan.fail_cases = cases.into_iter().collect();
        plan
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
#[derive(Default)]
pub struct Plan {
    /// A section mismatch, or a *missing* required section (what a brand-new
    /// case reports — `fill_expected` creates one that isn't there, so a new
    /// case finishes in the same pass).
    pub need_fill: bool,
    /// One of the two IR gates tripped.
    pub need_ir: bool,
    /// The checked-in per-case `#[test]` list has drifted from the tree.
    pub need_case_tests: bool,
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
        !self.need_fill && !self.need_ir && !self.need_case_tests
    }
}

/// The cases `every_case_has_a_test` reported as having no `#[test]` yet.
///
/// Only that half of its message: the other half lists *deleted* cases, which
/// have no file left to fill.
fn undeclared_cases(out: &str) -> Vec<String> {
    static ENTRY: OnceLock<Regex> = OnceLock::new();
    let entry = re(&ENTRY, r"^\s*(\S+) \([A-Za-z0-9_]+\)$");
    let mut found = Vec::new();
    let mut grabbing = false;
    for line in out.lines() {
        if line.contains("case(s) with no `#[test]`:") {
            grabbing = true;
            continue;
        }
        if line.contains("declared `#[test]`(s) with no case file:") || line.trim().is_empty() {
            grabbing = false;
        }
        if grabbing {
            // A well-formed entry is `<display path> (<test name>)`; anything
            // else is passed through as-is.
            match entry.captures(line) {
                Some(c) => found.push(c[1].to_string()),
                None => found.push(line.to_string()),
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Run` from `(test name, that test's output)` pairs, as the JSON would
    /// have yielded.
    fn run(failures: &[(&str, &str)]) -> Run {
        Run {
            failures: failures
                .iter()
                .map(|(n, o)| Failure {
                    name: (*n).to_string(),
                    output: (*o).to_string(),
                })
                .collect(),
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
            r#""stdout":"[cases/x/y]: `stdout` mismatch\n"}"#,
            "\n",
            r#"{"type":"suite","event":"failed","passed":1,"failed":1}"#,
        );
        let run = parse(stdout, "");
        assert!(run.saw_events());
        assert_eq!(run.failed_names().len(), 1, "only the failed test counts");
        assert!(run.failed_names().contains("cases_x_y"));
        assert!(run.failure_output().contains("`stdout` mismatch"));
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
            !run.unfillable().is_empty(),
            "a crash must stop the gate, not trigger a refill"
        );
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

    #[test]
    fn module_qualified_gates_are_not_hard_failures() {
        let run = run(&[("dogfood_ir::checked_in_ir_is_current", "")]);
        assert!(run.hard_failures().is_empty());
        assert!(run.plan().need_ir);
    }

    #[test]
    fn every_gate_name_is_excluded_from_hard_failures() {
        for name in [
            "cases_strings_slice",
            "examples_hello",
            "crates_aipl_mono_src_builtin_all",
            "every_case_has_a_test",
            "checked_in_ir_is_current",
            "no_staged_ir_pending",
            "dogfood_ir::checked_in_ir_is_current",
        ] {
            assert!(
                run(&[(name, "")]).hard_failures().is_empty(),
                "{name} should be excluded"
            );
        }
    }

    #[test]
    fn a_real_failure_is_reported() {
        let run = run(&[("mono::specializes_generics", "assertion failed")]);
        assert_eq!(run.hard_failures(), vec!["mono::specializes_generics"]);
    }

    #[test]
    fn mismatches_name_their_cases_and_sections() {
        let run = run(&[
            (
                "cases_strings_slice",
                "[cases/strings/slice]: `performance` mismatch\n\
                 [cases/strings/slice]: `stdout` mismatch",
            ),
            (
                "cases_arrays_take",
                "[cases/arrays/take]: missing required `--- performance ---` section",
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
        assert_eq!(plan.changed_sections, ["performance", "stdout"]);
        assert!(plan.behavioral_changed);
    }

    #[test]
    fn a_metrics_only_refill_is_not_behavioral() {
        let plan = run(&[(
            "cases_strings_slice",
            "[cases/strings/slice]: `performance` mismatch",
        )])
        .plan();
        assert!(plan.need_fill);
        assert!(!plan.behavioral_changed);
    }

    #[test]
    fn new_cases_are_folded_into_the_fill_set() {
        let run = run(&[(
            "every_case_has_a_test",
            "2 case(s) with no `#[test]`:\n\
             \x20   cases/optimizations/new_one (cases_optimizations_new_one)\n\
             \x20   cases/optimizations/new_two (cases_optimizations_new_two)\n\
             \n\
             1 declared `#[test]`(s) with no case file:\n\
             \x20   cases/gone/old (cases_gone_old)",
        )]);
        let plan = run.plan();
        assert!(plan.need_fill, "a new case still needs its sections filled");
        assert!(plan.need_case_tests);
        assert_eq!(
            plan.fail_cases,
            ["cases/optimizations/new_one", "cases/optimizations/new_two"],
            "deleted cases have no file left to fill"
        );
    }

    #[test]
    fn crashes_and_build_breaks_are_unfillable() {
        for output in [
            "[cases/x/y]: compile failed: no such function",
            "[cases/x/y]: (in-language tests) failed: 1 assertion",
        ] {
            assert!(
                !run(&[("cases_x_y", output)]).unfillable().is_empty(),
                "should be unfillable: {output}"
            );
        }
        assert!(run(&[("cases_x_y", "[cases/x/y]: `stdout` mismatch")])
            .unfillable()
            .is_empty());
    }

    #[test]
    fn unfillable_lines_name_the_test_they_came_from() {
        // The attribution the flat-text version could not do.
        let hits = run(&[("cases_x_y", "[cases/x/y]: compile failed: boom")]).unfillable();
        assert_eq!(hits, vec!["cases_x_y: [cases/x/y]: compile failed: boom"]);
    }

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
}
