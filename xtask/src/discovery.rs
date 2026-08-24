//! Reading a discovery run's output: which failures a refill can fix, which it
//! can't, and exactly which cases to refill.
//!
//! Every pattern here is matched against nextest's own output format, so they
//! are the coupling between this gate and the harness. The one that bites is the
//! optional `<module>::` prefix on a test name: the suites under `tests/suites/`
//! are `mod`s of the merged `compiler`/`dogfood` targets, so nextest reports
//! `dogfood_ir::checked_in_ir_is_current`, not the bare name. Matching only the
//! bare name silently reclassifies a regenerable staleness as a hard failure,
//! and the run then stops at a step it was supposed to fix itself.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static pattern compiles"))
}

/// `grep -n`-style excerpt: the first `limit` matching lines, each prefixed with
/// its 1-based line number.
fn grep_n(out: &str, pattern: &Regex, limit: usize) -> Vec<String> {
    out.lines()
        .enumerate()
        .filter(|(_, l)| pattern.is_match(l))
        .take(limit)
        .map(|(i, l)| format!("{}:{l}", i + 1))
        .collect()
}

/// Test names from nextest's per-failure summary lines:
///
/// ```text
///     FAIL [   0.007s] (1/1) aipl::dogfood_ir validate_staged_ir
/// ```
///
/// The status column also carries abnormal exits (SIGSEGV/ABORT/TIMEOUT), which
/// [`unfillable`] keys on separately. Deduped because nextest prints each failure
/// twice — once inline as it happens, once in the closing summary.
pub fn failed_tests(out: &str) -> BTreeSet<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = re(&RE, r"^ *(FAIL|SIGSEGV|ABORT|TIMEOUT|LEAK-FAIL) \[");
    out.lines()
        .filter(|l| re.is_match(l))
        .filter_map(|l| l.split_whitespace().next_back())
        .map(str::to_string)
        .collect()
}

/// Failures a refill can't fix — there is no section to record actual output
/// into. A case that won't build/link/spawn, a crash, or a failed in-language
/// `.test`. Returns the matching lines, or empty when there are none.
///
/// Checked before anything else: burning a full-corpus refill on one of these
/// costs a corpus run and fixes nothing.
pub fn unfillable(out: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Assembled with `concat!` rather than one string with `\` continuations:
    // these are *raw* strings, where a trailing backslash is a literal character
    // and would silently turn the alternative it ends into "…followed by a
    // newline" — which never matches a single line.
    let re = re(
        &RE,
        concat!(
            r"(load|compile|emit|link|spawn|instrumented compile) failed:",
            r"|\(in-language tests\) failed:|Abort trap|SIGSEGV|SIGABRT",
            // nextest reports an abnormal exit in its status column rather than
            // as a signal name in the output, so key on those too.
            r"|^ *(SIGSEGV|ABORT|TIMEOUT) \[",
        ),
    );
    grep_n(out, re, 20)
}

/// Failed tests that are *not* a per-case test, the case-list gate, or a known
/// IR-staleness gate — i.e. real test failures.
///
/// Each case is its own `#[test]`, named `<prefix>_<display path>` for the three
/// case roots; their fillable mismatches are handled by [`plan`]. [`unfillable`]
/// has already stopped on a case that genuinely broke, so what reaches here is a
/// stale section.
pub fn hard_failures(out: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `concat!`, not a `\` continuation — see the note in [`unfillable`].
    let re = re(
        &RE,
        concat!(
            r"^([A-Za-z0-9_]+::)?((cases|examples|crates)_.*",
            r"|every_case_has_a_test|checked_in_ir_is_current|no_staged_ir_pending)$",
        ),
    );
    failed_tests(out)
        .into_iter()
        .filter(|t| !re.is_match(t))
        .collect()
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

/// Classify a discovery run's output into the work it implies.
pub fn plan(out: &str) -> Plan {
    static MISMATCH: OnceLock<Regex> = OnceLock::new();
    static GATES: OnceLock<Regex> = OnceLock::new();
    static CASE_TESTS: OnceLock<Regex> = OnceLock::new();
    static SECTION: OnceLock<Regex> = OnceLock::new();
    static BEHAVIORAL: OnceLock<Regex> = OnceLock::new();
    static CASE_PATH: OnceLock<Regex> = OnceLock::new();

    let mut plan = Plan {
        need_fill: re(
            &MISMATCH,
            r"`[a-z ]+` mismatch|error mismatch|missing required `--- [a-z ]+ ---` section",
        )
        .is_match(out),
        ..Plan::default()
    };

    let failed = failed_tests(out);
    let gates = re(
        &GATES,
        r"^([A-Za-z0-9_]+::)?(checked_in_ir_is_current|no_staged_ir_pending)$",
    );
    plan.need_ir = failed.iter().any(|t| gates.is_match(t));
    let case_tests = re(&CASE_TESTS, r"^([A-Za-z0-9_]+::)?every_case_has_a_test$");
    plan.need_case_tests = failed.iter().any(|t| case_tests.is_match(t));

    // Distinct sections that changed, and whether any are behavioral.
    let section = re(&SECTION, r"`([a-z ]+)` mismatch");
    plan.changed_sections = section
        .captures_iter(out)
        .map(|c| c[1].to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    plan.behavioral_changed = re(
        &BEHAVIORAL,
        r"`(stdout|stderr|exit code|errors|check)` mismatch|error mismatch",
    )
    .is_match(out);

    // The exact cases that need refilling. Each mismatch prints
    // ``[<display-path>]: `<section>` mismatch`` (or `error mismatch`), and that
    // bracketed display path is precisely what `AIPL_CASE` filters on — so we
    // refill just the failing cases instead of paying for a whole-corpus fill.
    // Deduped: one case may report several mismatched sections, and one scoped
    // fill refreshes all of that case's sections.
    let case_path = re(
        &CASE_PATH,
        r"\[([^\]]+)\]: ((`[a-z ]+`|error) mismatch|missing required)",
    );
    let mut cases: BTreeSet<String> = case_path
        .captures_iter(out)
        .map(|c| c[1].to_string())
        .collect();

    // Cases added since the checked-in `#[test]` list was generated. These are
    // the reason a brand-new case used to cost a second handoff: with no
    // `#[test]` yet, the discovery run never *executed* them, so their
    // unrecorded sections went unseen and `need_fill` stayed false — the final
    // run (after the list is regenerated) was the first thing to notice.
    // `every_case_has_a_test` already names them, so fold them into the fill set
    // and finish them in this pass.
    let undeclared = undeclared_cases(out);
    if !undeclared.is_empty() {
        plan.need_fill = true;
        cases.extend(undeclared);
    }
    plan.fail_cases = cases.into_iter().collect();
    plan
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
            // else is passed through as-is, matching the original's `sed`.
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

    #[test]
    fn failed_tests_takes_the_name_and_dedupes() {
        let out = "\
    FAIL [   0.007s] (1/1) aipl::dogfood_ir validate_staged_ir
        FAIL [   0.007s] (1/1) aipl::dogfood_ir validate_staged_ir
   SIGSEGV [   1.000s] (2/9) aipl::cases cases_strings_boom
        PASS [   0.001s] (3/9) aipl::cases cases_strings_fine";
        let got = failed_tests(out);
        assert_eq!(got.len(), 2);
        assert!(got.contains("validate_staged_ir"));
        assert!(got.contains("cases_strings_boom"));
    }

    #[test]
    fn module_qualified_gates_are_not_hard_failures() {
        // The suites are `mod`s of a merged target, so nextest qualifies the
        // name. Matching only the bare name would stop the gate at a step it is
        // supposed to fix itself.
        let out = "    FAIL [ 0.0s] (1/1) aipl::dogfood dogfood_ir::checked_in_ir_is_current";
        assert!(hard_failures(out).is_empty());
        assert!(plan(out).need_ir);
    }

    #[test]
    fn every_gate_name_is_excluded_from_hard_failures() {
        // One alternative per gate; a broken one here silently turns a
        // regenerable staleness into a hard stop at a step the gate should fix.
        for name in [
            "cases_strings_slice",
            "examples_hello",
            "crates_aipl_mono_src_builtin_all",
            "every_case_has_a_test",
            "checked_in_ir_is_current",
            "no_staged_ir_pending",
            "dogfood_ir::checked_in_ir_is_current",
        ] {
            let out = format!("    FAIL [ 0.0s] (1/1) aipl::x {name}");
            assert!(hard_failures(&out).is_empty(), "{name} should be excluded");
        }
    }

    #[test]
    fn a_real_failure_is_reported() {
        let out = "    FAIL [ 0.0s] (1/1) aipl::compiler mono::specializes_generics";
        assert_eq!(hard_failures(out), vec!["mono::specializes_generics"]);
    }

    #[test]
    fn mismatches_name_their_cases_and_sections() {
        let out = "\
[cases/strings/slice]: `performance` mismatch
[cases/strings/slice]: `stdout` mismatch
[cases/arrays/take]: missing required `--- performance ---` section";
        let plan = plan(out);
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
        let plan = plan("[cases/strings/slice]: `performance` mismatch");
        assert!(plan.need_fill);
        assert!(!plan.behavioral_changed);
    }

    #[test]
    fn new_cases_are_folded_into_the_fill_set() {
        let out = "\
2 case(s) with no `#[test]`:
    cases/optimizations/new_one (cases_optimizations_new_one)
    cases/optimizations/new_two (cases_optimizations_new_two)

1 declared `#[test]`(s) with no case file:
    cases/gone/old (cases_gone_old)";
        let plan = plan(out);
        assert!(plan.need_fill, "a new case still needs its sections filled");
        assert_eq!(
            plan.fail_cases,
            ["cases/optimizations/new_one", "cases/optimizations/new_two"],
            "deleted cases have no file left to fill"
        );
    }

    #[test]
    fn crashes_and_build_breaks_are_unfillable() {
        for line in [
            "[cases/x/y]: compile failed: no such function",
            "[cases/x/y]: (in-language tests) failed: 1 assertion",
            "   SIGSEGV [ 1.0s] (1/1) aipl::cases cases_x_y",
        ] {
            assert!(!unfillable(line).is_empty(), "should be unfillable: {line}");
        }
        assert!(unfillable("[cases/x/y]: `stdout` mismatch").is_empty());
    }
}
