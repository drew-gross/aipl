//! Repo automation. Today that is one command:
//!
//! ```text
//! cargo handoff
//! ```
//!
//! the one-command pre-handoff gate for this repo. It runs the finish-a-task
//! sequence in dependency order, paying for the *expensive* regeneration steps
//! (perf/section refill, dogfood-IR regen — each a full-corpus run) only when a
//! test run proves they're needed, and stopping with a pointed message on any
//! failure a refill can't fix. One command instead of hand-driving six, so it's
//! the whole sequence in a fraction of the tokens.
//!
//! # Order & why
//!
//! 1. `cargo fmt` (Rust), then a build, then the `format_corpus` helper (every
//!    checked-in `.aipl`), then a compile check. Formatting is cheap and
//!    *span-shifting*, so doing it ahead of the test runs means a
//!    formatting-only fix never costs a second full-corpus run.
//!
//!    The build is its own step for attribution: every step here is a `cargo`
//!    invocation, so whichever runs first silently absorbs any pending rebuild —
//!    which is how `aipl fmt` came to report ~850s for ~38s of formatting. It
//!    goes after `cargo fmt` (which can invalidate what it built) and before
//!    `format_corpus` (which, since the dogfooded `.aipl` sources became
//!    run-time reads, no longer can).
//! 2. Discovery run. Three outcomes:
//!    - green → done.
//!    - only fillable staleness (a section mismatch, a drifted per-case
//!      `#[test]` list, or IR staleness) → remediate (steps 3-5), then
//!      re-confirm. Refreshing a section is always recoverable
//!      (`git reset --hard HEAD`), so even behavioral sections (stdout / exit
//!      code / errors / check) are refilled and the git diff is the review
//!      surface, flagged at the end.
//!    - a failure a refill can't fix (a compile/link error, a crash, a failed
//!      in-language `.test`, or any non-`cases` test) → STOP. No section to
//!      record from, so a refill would just burn a full corpus run without
//!      fixing it.
//! 3. `fill_case_tests` regenerates the checked-in per-case `#[test]` list when a
//!    case file was added or removed. First, so a new case can reach step 4.
//! 4. `fill_expected`, scoped with `AIPL_CASE` to each mismatched case, refreshes
//!    just those cases' sections from actual output (not the whole corpus).
//! 5. Staged dogfood-IR regen: fill → validate → corpus run against the staged
//!    artifact → auto-promote when that run is green. Then (step "5b") a
//!    rebuild, if step 3 regenerated the per-case `#[test]` list — the one thing
//!    the remediation steps rewrite that is compiled in rather than read at run
//!    time. Its own labelled step purely so the timing report attributes that
//!    compile to the thing that caused it instead of to the final run.
//! 6. Final run confirms green against the live (promoted) artifacts.
//!
//! # Test runner
//!
//! `cargo nextest`, which runs each test in its own process (so one crashing
//! case can't take the binary down with it). Every step that actually *runs*
//! tests asks for the machine-readable event stream ([`Cmd::json`]), so
//! [`discovery`] reads structured records — each failure carrying its own
//! output — rather than grepping a presentation format. The `--no-run` builds
//! stay plain: nothing runs, so there are no events, and cargo's compile errors
//! are the point.
//!
//! One nextest difference this has to account for: it *defaults to fail-fast*,
//! the opposite of `cargo test`, so every whole-suite run below passes
//! `--no-fail-fast`. (It also skips doctests, of which the repo has none left —
//! the documented examples are ordinary tests in `tests/ffi.rs`.) `--color
//! never` keeps the human half of the output readable.
//!
//! # Exit status
//!
//! 0 = handoff green; 1 = stopped (the message says at which step and why).

mod discovery;
mod machine;
mod runner;

use std::path::{Path, PathBuf};

use runner::{Cmd, Output, Runner, BOLD, DIM, GREEN, OFF};

fn main() {
    let mut args = std::env::args().skip(1);
    let task = args.next();
    let extra: Vec<String> = args.collect();
    match task.as_deref() {
        Some("handoff") if extra.is_empty() => handoff(),
        // Refused rather than ignored: the gate is a several-minute run that
        // formats and rewrites files, and `cargo handoff --help` silently
        // *starting* one is a nasty way to find out it takes no arguments.
        Some("handoff") => {
            eprintln!(
                "xtask: handoff takes no arguments (got {})\n\n{USAGE}",
                extra.join(" ")
            );
            std::process::exit(2);
        }
        Some(other) => {
            eprintln!("xtask: unknown task {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
        None => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

const USAGE: &str = "\
usage: cargo xtask <task>

tasks:
  handoff   the pre-handoff gate — format, test, regenerate what a run proved
            stale, and confirm green (also available as `cargo handoff`)";

/// The whole suite. `--no-fail-fast` because nextest cancels on the first
/// failure by default and this gate needs to see *all* remediable staleness in
/// one pass (see the discovery step).
fn nextest() -> Cmd {
    Cmd::new("cargo")
        .args(["nextest", "run", "--color", "never", "--no-fail-fast"])
        .json()
}

/// Build every test target without running anything.
///
/// The only nextest step that is *not* [`Cmd::json`]: nothing runs, so there are
/// no test events, and what this step reports on — cargo's compile errors — is
/// build output rather than test results.
fn nextest_build() -> Cmd {
    Cmd::new("cargo").args(["nextest", "run", "--no-run", "--color", "never"])
}

/// One `#[ignore]`d author helper, by exact name. nextest selects ignored tests
/// with `--run-ignored only` rather than libtest's `-- --ignored <name>`.
fn helper(name: &str) -> Cmd {
    Cmd::new("cargo")
        .args([
            "nextest",
            "run",
            "--color",
            "never",
            "--run-ignored",
            "only",
            "-E",
            &format!("test(={name})"),
        ])
        .json()
}

/// A helper step's summary, or a stop explaining why there wasn't one.
///
/// Every author helper diverges on purpose, so its message is in a failed test's
/// record — see [`discovery`].
fn helper_output(r: &mut Runner, label: &str) -> String {
    match discovery::Run::parse(&r.out).helper_output() {
        Ok(out) => out,
        Err(why) => {
            r.save_out();
            r.fail(label, &why);
        }
    }
}

/// First `limit` lines of `out` matching `pred`, prefixed with their line
/// numbers — the `grep -n .. | head -N` the failure messages quote.
fn excerpt(out: &str, limit: usize, pred: impl Fn(&str) -> bool) -> String {
    out.lines()
        .enumerate()
        .filter(|(_, l)| pred(l))
        .take(limit)
        .map(|(i, l)| format!("{}:{l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Last `n` lines of `out` — the fallback excerpt when there's no better signal
/// than "here is how it ended".
fn tail(out: &str, n: usize) -> String {
    let lines: Vec<&str> = out.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn handoff() -> ! {
    let repo = repo_root();
    std::env::set_current_dir(&repo).expect("repo root is a directory");

    // The dogfood-IR corpus run spawns the compiler as a subprocess whose CWD
    // isn't the repo root, so these paths must be absolute. Two artifacts are
    // staged and promoted together: the parser-hook engine and the formatter
    // engine (see `FMT_SOURCE_FILES` in aipl-codegen). Either one pending means
    // an interrupted workflow.
    let staged = repo.join("crates/aipl-codegen/src/dogfood.clif.staged");
    let staged_fmt = repo.join("crates/aipl-codegen/src/fmt.clif.staged");

    let mut r = Runner::new();

    // --- 0. Startup sweeps -----------------------------------------------

    let orphans = machine::orphaned_listers();
    if !orphans.is_empty() {
        machine::signal(&orphans, "-KILL");
        eprintln!(
            "{DIM}(cleared {} orphaned nextest listing process(es) from an interrupted run){OFF}",
            orphans.len()
        );
    }

    // A leftover staged artifact means a previous IR workflow was interrupted; a
    // plain suite run fails on `no_staged_ir_pending` until it's resolved. Don't
    // guess.
    if staged.exists() || staged_fmt.exists() {
        r.set_saved(staged.clone());
        r.fail(
            "startup",
            &format!(
                "A staged IR artifact already exists:
    {} / {}
Resolve the interrupted workflow first — promote it
    cargo nextest run --run-ignored only -E 'test(=dogfood_ir::promote_staged_ir)'
or discard it
    rm -f '{}' '{}'",
                staged.display(),
                staged_fmt.display(),
                staged.display(),
                staged_fmt.display(),
            ),
        );
    }

    // --- 1. Format + build (cheap, up front, span-shifting) ---------------

    if !r.step("cargo fmt (Rust)", Cmd::new("cargo").args(["fmt"])) {
        r.save_out();
        let detail = r.out.merged.clone();
        r.fail("cargo fmt", &detail);
    }

    // Build everything up front, as its own step, purely so the timing report
    // attributes the compile to the compile. Every step below is a `cargo`
    // invocation that would otherwise absorb whatever rebuild is pending — and
    // it was always the *first* one that paid, which made `aipl fmt` read as
    // ~850s when the formatting itself is ~38s. Right after `cargo fmt`, so a
    // reformat of a `.rs` file can't invalidate what this just built.
    //
    // A Rust compile error now also surfaces here, first and on its own, instead
    // of inside the formatting step.
    if !r.step("nextest --no-run (build)", nextest_build()) {
        r.save_out();
        let detail = excerpt(&r.out.merged, 30, |l| l.starts_with("error"));
        r.fail("build", &detail);
    }

    // `format_corpus` rewrites any mis-formatted `.aipl` in place, then fails on
    // purpose to show its summary — so its non-zero exit is expected; a genuine
    // formatter error prints "format failed:".
    //
    // Formatting stays ahead of the *test* runs because it is span-shifting:
    // doing it up front means a formatting-only fix never costs a second
    // full-corpus run.
    //
    // It no longer has to run ahead of the build to avoid invalidating it. The
    // `crates/aipl-codegen/src/*.aipl` this reformats used to be `include_str!`d,
    // so rewriting one after a build cost a ~730s rebuild; they are read at run
    // time now. What remains embedded is `crates/aipl-mono/src/*.aipl` (the
    // AIPL-defined builtins), so reformatting one of *those* does still
    // invalidate aipl-mono — which the compile check below absorbs and reports
    // as its own step.
    r.step("aipl fmt (format_corpus)", helper("fmt::format_corpus"));
    let fmt_out = helper_output(&mut r, "aipl fmt");
    if fmt_out.contains("format failed:") {
        r.save_out();
        let detail = excerpt(&fmt_out, usize::MAX, |l| l.contains("format failed:"));
        r.fail("aipl fmt", &detail);
    }
    if let Some(n) = reformatted_count(&fmt_out) {
        if n > 0 {
            eprintln!("{DIM}    (reformatted {n} .aipl file(s)){OFF}");
        }
    }

    // Re-compile after the formatters, so the discovery run starts from a fully
    // built tree whatever they just rewrote — a reformatted
    // `crates/aipl-mono/src/*.aipl` (still `include_str!`d) invalidates
    // aipl-mono, and that rebuild belongs to this step rather than to the run
    // below. A no-op second or two when they changed nothing, the common case.
    if !r.step("nextest --no-run (compile check)", nextest_build()) {
        r.save_out();
        let detail = excerpt(&r.out.merged, 30, |l| l.starts_with("error"));
        r.fail("compile", &detail);
    }

    // --- 2. Discovery run --------------------------------------------------

    // `--no-fail-fast` so this executes *every* test and reports all remediable
    // staleness in one pass. Stopping at the first failure would be actively
    // wrong here: a change that makes both the per-case `--- performance ---`
    // sections and `dogfood.clif` stale would cancel inside `cases`, `need_ir`
    // would stay false, the staged-IR regen (step 5) would be skipped, and the
    // final run would then fail on `checked_in_ir_is_current` with the IR never
    // regenerated. Running everything surfaces both together.
    if r.step("nextest (discovery)", nextest()) {
        eprintln!("\n{GREEN}{BOLD}HANDOFF OK{OFF} (green with no regeneration needed)");
        r.timing_report();
        std::process::exit(0);
    }
    r.save_out(); // keep the discovery output for any failure message below
    let run = discovery::Run::parse(&r.out);

    // The run failed, so it ran tests, so the JSON stream must have carried
    // events. None means the machine-readable interface moved — which would
    // otherwise read as "nothing failed" and send the gate on to remediate
    // staleness it never saw.
    if !run.saw_events() {
        r.fail(
            "nextest (no JSON events)",
            "The run failed but emitted no parseable events on stdout. nextest's libtest-json
output is still experimental, so this most likely means the interface changed:
check `cargo nextest run --help` for --message-format / --message-format-version
and update MESSAGE_FORMAT_VERSION in xtask/src/runner.rs.",
        );
    }

    let unfillable = run.unfillable();
    if !unfillable.is_empty() {
        r.fail(
            "nextest (failure a refill can't fix)",
            &unfillable.join("\n"),
        );
    }

    let hard = run.hard_failures();
    if !hard.is_empty() {
        r.fail("nextest (failing test)", &hard.join("\n"));
    }

    let plan = run.plan();
    if plan.is_empty() {
        let detail = tail(&run.failure_output(), 40);
        r.fail("nextest (unrecognized failure)", &detail);
    }

    // --- 3. Regenerate the per-case `#[test]` list -------------------------

    // Before the refills: a case added without its `#[test]` never ran in
    // discovery, so regenerating first gives the fill step (and the final run) a
    // chance to see it.
    if plan.need_case_tests {
        r.step(
            "fill_case_tests (per-case #[test] list)",
            helper("fill_case_tests"),
        );
        let out = helper_output(&mut r, "fill_case_tests");
        if !wrote_case_tests(&out) {
            r.save_out();
            let detail = tail(&out, 40);
            r.fail("fill_case_tests", &detail);
        }
    }

    // --- 4. Refill changed sections ----------------------------------------

    if plan.need_fill {
        // A scoped `AIPL_CASE` fill runs (and refreshes) only the matched case,
        // so we refill exactly the cases that mismatched instead of the whole
        // corpus. If we somehow couldn't pin down any case path (unexpected
        // message format), fall back to a whole-corpus fill so a mismatch is
        // never left unrefreshed.
        if plan.fail_cases.is_empty() {
            r.step(
                "fill_expected (section refill — full corpus, no case path found)",
                helper("fill_expected"),
            );
            let out = helper_output(&mut r, "fill_expected");
            if !out.contains("refresh complete") {
                r.save_out();
                let detail = tail(&out, 40);
                r.fail("fill_expected", &detail);
            }
        } else {
            for case in &plan.fail_cases {
                // A scoped run diverges with the "filter active" panic, not the
                // whole-corpus "section refresh complete" message — but the
                // refill still happens during the run. Success is that scoped
                // summary line reporting the case was seen (`matched > 0`, else
                // the harness asserts before printing) with `0 failed` (no
                // unfillable failure slipped in).
                // Built from `helper` rather than spelled out, so it cannot
                // drift from the other helper steps — writing the arguments
                // longhand here is exactly how this one ended up without
                // `Cmd::json` and with nothing to read.
                r.step(
                    &format!("fill_expected (section refill — {case})"),
                    helper("fill_expected").env("AIPL_CASE", case),
                );
                let label = format!("fill_expected ({case})");
                let out = helper_output(&mut r, &label);
                if !scoped_fill_succeeded(&out) {
                    r.save_out();
                    let detail = tail(&out, 40);
                    r.fail(&label, &detail);
                }
            }
        }
    }

    // --- 5. Regenerate + validate + promote dogfood IR ---------------------

    if plan.need_ir {
        r.step("fill_staged_ir", helper("dogfood_ir::fill_staged_ir"));
        let out = helper_output(&mut r, "fill_staged_ir");
        if !wrote_staged(&out) {
            r.save_out();
            let detail = tail(&out, 40);
            r.fail("fill_staged_ir", &detail);
        }

        if !r.step(
            "validate_staged_ir (entry-level pre-check)",
            helper("dogfood_ir::validate_staged_ir"),
        ) {
            r.save_out();
            let detail = tail(&r.out.merged, 40);
            r.fail("validate_staged_ir", &detail);
        }

        let ok = r.step(
            "staged-IR corpus run (AIPL_DOGFOOD_IR + AIPL_FMT_IR)",
            nextest()
                .env("AIPL_DOGFOOD_IR", &staged.to_string_lossy())
                .env("AIPL_FMT_IR", &staged_fmt.to_string_lossy()),
        );
        if !ok {
            r.save_out();
            let detail = failure_excerpt(&r.out);
            r.fail(
                "staged-IR corpus run (candidate IR is wrong — diff .staged vs live)",
                &detail,
            );
        }

        r.step("promote_staged_ir", helper("dogfood_ir::promote_staged_ir"));
        let out = helper_output(&mut r, "promote_staged_ir");
        if !out.contains("promoted") {
            r.save_out();
            let detail = tail(&out, 40);
            r.fail("promote_staged_ir", &detail);
        }
    }

    // --- 5b. Rebuild what the regeneration invalidated ---------------------

    // `fill_case_tests` regenerates tests/support/case_tests.rs, which
    // tests/cases.rs pulls in with `include!` — so it's a source change, and the
    // final run below can't start until the `cases` binary is rebuilt. Its own
    // labelled step so the timing report attributes that compile to the
    // regeneration that caused it instead of making the final run look
    // mysteriously slow.
    //
    // Nothing else in steps 3-5 invalidates the build. `fill_expected` rewrites
    // case `.aipl` files, read at run time. `promote_staged_ir` rewrites
    // dogfood.clif and fmt.clif, also read at run time (DOGFOOD_CLIF_PATH /
    // FMT_CLIF_PATH are compile-time *paths*, not `include_str!`d text) — that
    // used to cost ~730s here, all of it rebuilding the test binaries, which is
    // why it's a path now. Nor does the staged-IR corpus run above: the
    // `.staged` files are read through AIPL_DOGFOOD_IR/AIPL_FMT_IR.
    if plan.need_case_tests
        && !r.step(
            "nextest --no-run (rebuild after regeneration)",
            nextest_build(),
        )
    {
        r.save_out();
        let detail = excerpt(&r.out.merged, 30, |l| l.starts_with("error"));
        r.fail("rebuild after regeneration", &detail);
    }

    // --- 6. Final confirmation against the live artifacts ------------------

    if !r.step("nextest (final)", nextest()) {
        r.save_out();
        let detail = failure_excerpt(&r.out);
        r.fail("nextest (final — regeneration didn't settle)", &detail);
    }

    // --- Report ------------------------------------------------------------

    eprintln!("\n{GREEN}{BOLD}HANDOFF OK{OFF}");
    if plan.need_fill {
        if plan.changed_sections.is_empty() {
            // Only *missing* sections (a brand-new case) — nothing mismatched.
            eprintln!("  filled in missing section(s) for a new case");
        } else {
            eprintln!("  refilled sections: {}", plan.changed_sections.join(", "));
        }
    }
    if plan.need_ir {
        eprintln!("  regenerated + promoted dogfood IR");
    }
    if plan.need_case_tests {
        eprintln!("  regenerated the per-case #[test] list");
    }
    if plan.behavioral_changed {
        eprintln!(
            "{BOLD}  ! behavioral output changed — review the git diff before committing{OFF}"
        );
        eprintln!(
            "{DIM}    (git reset --hard HEAD undoes the refill if it recorded a regression){OFF}"
        );
    }
    r.timing_report();
    std::process::exit(0);
}

/// The failing tests of a corpus run, each with the first line it printed.
///
/// Replaces the original's `grep -nE 'mismatch|FAILED|Abort'` over a flat blob:
/// the JSON already knows which tests failed and what each one said, so the
/// excerpt names them instead of quoting whichever nearby lines matched.
fn failure_excerpt(out: &Output) -> String {
    let run = discovery::Run::parse(out);
    let mut lines: Vec<String> = run
        .failures()
        .iter()
        .take(20)
        .map(|f| match f.output.lines().find(|l| !l.trim().is_empty()) {
            Some(first) => format!("{}: {}", f.name, first.trim()),
            None => f.name.clone(),
        })
        .collect();
    if lines.is_empty() {
        // A failed run with no failed *test* — a build error, or nextest itself
        // giving up. The human stream is all there is.
        lines.push(tail(&out.stderr, 20));
    }
    lines.join("\n")
}

/// `reformatted <n> file(s)` from the formatter's summary.
fn reformatted_count(out: &str) -> Option<u32> {
    let rest = out.split("reformatted ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `wrote <n> `#[test]`` — `fill_case_tests` confirming it rewrote the list.
fn wrote_case_tests(out: &str) -> bool {
    out.lines().any(|l| {
        l.split("wrote ").nth(1).is_some_and(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            !digits.is_empty() && rest[digits.len()..].starts_with(" `#[test]`")
        })
    })
}

/// `wrote <path>.staged` — `fill_staged_ir` confirming it produced candidates.
fn wrote_staged(out: &str) -> bool {
    out.lines().any(|l| {
        l.split("wrote ")
            .nth(1)
            .is_some_and(|p| p.contains(".staged"))
    })
}

/// The scoped-fill summary line: `[filter "<case>"]: <n> passed, 0 failed,`.
fn scoped_fill_succeeded(out: &str) -> bool {
    out.lines().any(|l| {
        let Some(rest) = l.split("[filter \"").nth(1) else {
            return false;
        };
        let Some(rest) = rest.split("\"]: ").nth(1) else {
            return false;
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        !digits.is_empty() && rest[digits.len()..].starts_with(" passed, 0 failed,")
    })
}

/// The repo root: the nearest ancestor of the working directory holding a
/// workspace `Cargo.toml`.
///
/// Walked at run time rather than taken from `CARGO_MANIFEST_DIR`, which is
/// baked in at *compile* time — a checkout that moved without invalidating the
/// build would otherwise send the gate at the directory it was built in and
/// reformat someone else's tree. The compile-time path stays as the fallback,
/// for an invocation from outside any workspace.
fn repo_root() -> PathBuf {
    let is_workspace_root = |dir: &Path| {
        std::fs::read_to_string(dir.join("Cargo.toml"))
            .is_ok_and(|t| t.lines().any(|l| l.trim_start().starts_with("[workspace]")))
    };
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            cwd.ancestors()
                .find(|d| is_workspace_root(d))
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("xtask/ has a parent")
                .to_path_buf()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_formatter_summary() {
        assert_eq!(reformatted_count("reformatted 3 files"), Some(3));
        assert_eq!(reformatted_count("reformatted 0 files"), Some(0));
        assert_eq!(reformatted_count("nothing to do"), None);
    }

    #[test]
    fn recognizes_the_helper_confirmations() {
        assert!(wrote_case_tests("wrote 597 `#[test]` entries"));
        assert!(!wrote_case_tests("wrote 597 lines"));
        assert!(wrote_staged(
            "wrote crates/aipl-codegen/src/fmt.clif.staged"
        ));
        assert!(!wrote_staged("wrote crates/aipl-codegen/src/fmt.clif"));
        // The path has to follow "wrote " — a line merely mentioning a staged
        // artifact is not `fill_staged_ir` reporting it produced one.
        assert!(!wrote_staged("removed fmt.clif.staged, wrote nothing"));
    }

    #[test]
    fn scoped_fill_needs_zero_failures() {
        assert!(scoped_fill_succeeded(
            r#"=== test cases [filter "cases/x/y"]: 1 passed, 0 failed, 1 refreshed ==="#
        ));
        assert!(!scoped_fill_succeeded(
            r#"=== test cases [filter "cases/x/y"]: 0 passed, 1 failed, 0 refreshed ==="#
        ));
        assert!(!scoped_fill_succeeded("1 passed, 0 failed,"));
    }

    #[test]
    fn excerpts_carry_line_numbers() {
        let out = "fine\nerror: boom\nfine\nerror: again";
        assert_eq!(
            excerpt(out, 30, |l| l.starts_with("error")),
            "2:error: boom\n4:error: again"
        );
        assert_eq!(excerpt(out, 1, |l| l.starts_with("error")), "2:error: boom");
    }

    #[test]
    fn tail_is_bounded_by_the_input() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 40), "a");
    }
}
