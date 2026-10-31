//! "Examples as tests": each file under `tests/cases/**/*.aipl` contains
//! an AIPL program followed by zero or more `--- section ---` blocks
//! describing the expected outcome. The harness builds + links + runs
//! the program (via the same pipeline as `aipl build`) and compares.
//!
//! Every present section is *validated* on a normal `cargo test` run: if the
//! actual output differs from the section body, the case fails (there is no `?`
//! or placeholder escape — a stale or `?` body is just a mismatch). The
//! `fill_expected` helper (below) refreshes every section from the actual output
//! in a single pass.
//!
//! Section format (all lines like `--- name ---` on their own line):
//!   `--- stdout ---`     — expected stdout (default: empty)
//!   `--- stderr ---`     — expected stderr (default: empty)
//!   `--- exit code ---`  — expected exit code (default: 0)
//!   `--- cli ---`        — CLI arguments for the built binary, one per
//!                          line (default: none). The program receives them
//!                          as `main`'s `str[]` parameter.
//!   `--- errors ---`     — expected compiler error output. Presence of
//!                          this section means the program is expected
//!                          to fail to compile; mutually exclusive with
//!                          stdout/stderr/exit code.
//!   `--- report ---`     — expected `aipl report` for the case, as that command prints
//!                          it: aligned counts, the peak of live heap, a table of
//!                          functions. Optional, and for the properties the function
//!                          list shows — an instance that was not minted, a call
//!                          inlined away. Refreshed by `fill_expected` where it is
//!                          present, never added. Mutually exclusive with `errors`.
//!                          (There is no `--- performance ---` any more: a case's
//!                          counts are its row in `tests/performance_metrics.md`,
//!                          and the properties an optimization is *for* are stated
//!                          by a `.performance` block or a `--- report ---`.)
//!
//! Every running case is also *measured*, whatever sections it has, and refused if
//! it leaks (allocations != deallocations): the leak gate is not a side effect of
//! comparing `report`, and applies to the exempt `examples/` too.
//!
//! A case need not define `main`. A *library* case has `.test` blocks but no
//! `main`: it's exercised only through `aipl check`, and its baseline row and
//! `report` measure that test run (the harness AOT-builds the synthesized `.test`
//! driver).
//! Such a case can't have a `stdout`/`stderr`/`exit code`/`cli`/`expect file`
//! section, since those observe a `main`-driven run.
//!   `--- check ---`      — expected stdout of `aipl check` (the in-language
//!                          `.test` runner) for this case, byte-for-byte. Lets a
//!                          *failing* test be a documented fixture.
//!                          Mutually exclusive with `errors`.
//!   `--- check exit code ---` — the status that same `aipl check` run exits
//!                          with. The other half of `check`: a `--- check ---`
//!                          section pins only stdout, so a case whose point is
//!                          that `check` *fails* while its tests pass needs this
//!                          to say so.
//!
//! Both read one `aipl check` run, so pinning either, both or neither costs the
//! same single subprocess (`check_plan`). With *neither*, a case that has `.test`
//! blocks must instead pass cleanly — the harness requires `check` to exit 0.
//!   `--- file: rel/path.aipl ---` — additional source files, staged
//!                          alongside the entry source so `import`s
//!                          resolve as written.
//!   `--- expect file: rel/path ---` — a file the program is expected to
//!                          have *written* during its run (e.g. via
//!                          `write_string_to_file`). After the run, the harness
//!                          reads it from the staging dir and compares to the
//!                          section body (trailing newlines stripped, like
//!                          stdout). Success cases
//!                          only (mutually exclusive with `errors`).
//!
//! The runner walks `tests/cases/` recursively so cases can be grouped
//! into subdirectories.
//!
//! Every case is its own `#[test]`, named after its display path with the
//! separators flattened (`cases/options/some_value` -> `cases_options_some_value`)
//! — so libtest runs them in parallel, reports each by name, and its own filter
//! is the fast-iteration tool:
//!   `cargo test --test cases -- cases_options_some_value`
//!   `cargo test --test cases -- cases_options`
//! The test functions are checked in ([`CASE_TESTS_FILE`]) rather than discovered
//! at run time, since `#[test]` is a compile-time thing; [`every_case_has_a_test`]
//! fails if that list and the case files on disk have drifted apart, and names the
//! helper that regenerates it.
//!
//! Two author-helper "refresh" modes are `#[ignore]`d tests (a normal `cargo
//! test` skips them; opt in by name):
//!   - `cargo test --test cases -- --ignored fill_expected` — in a single pass,
//!     overwrite every section that differs (`stdout`/`stderr`/`exit code`/
//!     `report`/`check`/`errors`/`expect file`) with the
//!     actual output. Set `AIPL_CASE` to a path substring — or a comma-separated
//!     list of them — to target a subset; this is the one mode that still reads
//!     it, since it is a single test and libtest's filter can't reach inside it.
//!     Every entry must match at least one case, so a mistyped path in a batch
//!     fails rather than silently refreshing nothing.
//!   - `cargo test --test cases -- --ignored refresh_perfmon` — rewrite the
//!     `tests/performance_metrics.md` table: the deterministic columns every
//!     case's run is held to (below), and the non-deterministic ones that only
//!     track trends. `AIPL_CASE` scopes it the same way, to the matching rows.
//!   - `cargo test --test cases -- --ignored fill_case_tests` — regenerate the
//!     per-case `#[test]` list after adding or removing a case file.
//!
//! **Every running case has a row in `tests/performance_metrics.md`**, and its run
//! must reproduce the row's deterministic columns (allocations, bytes allocated,
//! peak live bytes, instructions, code bytes) exactly: see [`check_baseline`].
//! That is the one comparison here a refill cannot make — `fill_expected` leaves
//! it alone — because a moved number is something to look at, not stale text.
//! `cargo handoff` runs everything else to the end and then fails with the list.
//!
//! Each diverges (fails) when done so its summary is visible. The relevant
//! failure messages (e.g. a `report` mismatch) name the command to run.
//!
//! Trailing newlines on each section are stripped — a section that
//! contains a single line of text is just that text, not text+`\n`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use aipl::ast::Program;
use aipl::binary;
use aipl::codegen::{Compilation, ObjectCompilation};
use aipl::loader;
use aipl::perf::{self, PerfStats};
use aipl::{DebugOptions, Error};
use handoff::protocol::Fill;

/// The command that runs the ignored section-refresh helper ([`fill_expected`]).
/// Surfaced in failure messages so a stale `--- report ---`/`errors`/`check`
/// section tells you exactly how to re-record it.
const FILL_CMD: &str = "cargo test --test cases -- --ignored fill_expected";

/// Build a **scoped** fill command for one case's failure message: refresh just
/// the offending case via `AIPL_CASE` instead of re-running the whole (serial)
/// fill over every case. `ctx` is the bracketed display path
/// (`[cases/foo/bar.aipl]`); stripping the brackets recovers the exact substring
/// the `AIPL_CASE` filter matches on. Preferred over bare [`FILL_CMD`] in any
/// per-case mismatch message — the whole-suite form is slow and rarely what you
/// want when a single case is stale.
fn scoped_fill_cmd(ctx: &str) -> String {
    let path = ctx.trim_start_matches('[').trim_end_matches(']');
    format!("AIPL_CASE='{path}' {FILL_CMD}")
}

/// Build a **scoped** *run* command for one case's failure message: re-run just
/// the offending case (as opposed to [`scoped_fill_cmd`], which refreshes its
/// sections). Each case is its own `#[test]`, so this is libtest's own name
/// filter. `ctx` is the bracketed display path (`[cases/foo/bar]`).
fn scoped_run_cmd(ctx: &str) -> String {
    let path = ctx.trim_start_matches('[').trim_end_matches(']');
    format!(
        "cargo test --test cases -- {}",
        test_name_for(path.trim_end_matches(".aipl"))
    )
}

/// The command that regenerates the per-case `#[test]` list ([`fill_case_tests`]).
const CASE_TESTS_CMD: &str = "cargo test --test cases -- --ignored fill_case_tests";
/// The command that runs the ignored perfmon-table refresh ([`refresh_perfmon`]).
const PERFMON_CMD: &str = "cargo test --test cases -- --ignored refresh_perfmon";

/// Tracing for the cases harness, enabled by setting the `AIPL_DEBUG` env var.
/// Lets you re-run a hanging case (`AIPL_DEBUG=1 cargo test ...`) and read the
/// last pass it reached on stderr.
fn debug_opts() -> DebugOptions {
    DebugOptions::new(std::env::var_os("AIPL_DEBUG").is_some())
}

#[derive(Default)]
struct Spec {
    /// The entry-point AIPL source. Anything before the first section
    /// header.
    source: String,
    /// (relative path, contents) for any `--- file: ... ---` companion
    /// sources. Resolved relative to the staging directory.
    extra_files: Vec<(String, String)>,
    /// (relative path, expected contents) for any `--- expect file: ... ---`
    /// section: a file the program is expected to have *written* (e.g. via
    /// `write_string_to_file`), checked against the staging dir after the run.
    expect_files: Vec<(String, String)>,
    stdout: Option<String>,
    stderr: Option<String>,
    exit_code: Option<i32>,
    errors: Option<String>,
    /// Expected `aipl report` for the case — the readable form of the same run,
    /// compared as written and refilled only when the section is already there.
    report: Option<String>,
    /// CLI arguments passed to the built binary, one per line of the
    /// `--- cli ---` section. Reaches the program as `main`'s `str[]`.
    cli: Vec<String>,
    /// Expected stdout of `aipl check` (the in-language `.test` runner) for this
    /// case, byte-for-byte.
    /// When present, the harness compares `check`'s output against it (so a
    /// *failing* test can be a documented fixture); when absent, a case with
    /// `.test` blocks must instead pass cleanly (`check` exits 0).
    check: Option<String>,
    /// Expected exit status of `aipl check` for this case. The other half of
    /// `check`: a `--- check ---` section alone says nothing about the status, so
    /// a case whose point is that `check` *fails* while its tests pass — the
    /// `trace` fixtures — needs this to say so. Compared against the same single
    /// run; see [`check_plan`].
    check_exit_code: Option<i32>,
}

/// Parse a case file into its [`Spec`].
///
/// Dogfooded: the sectioning, body trimming and per-section validation all live
/// in the AIPL `parse_spec` (`crates/aipl-codegen/src/parse_spec.aipl`), reached
/// through the FFI. This is only the marshaling shim that widens the flat
/// `SpecFields` into the harness's own struct — there is deliberately no second
/// implementation of the rules here to drift from it.
///
/// A malformed case (unknown section, non-integer `exit code`, unusable
/// companion path) panics, as it did when the rules lived here: a case whose
/// markup is wrong should stop the run, not silently lose an expectation.
fn parse_spec(contents: &str) -> Spec {
    let f = aipl::parse_spec(contents).unwrap_or_else(|e| panic!("{e}"));
    Spec {
        source: f.source,
        extra_files: f.extra_files,
        expect_files: f.expect_files,
        stdout: f.stdout,
        stderr: f.stderr,
        exit_code: f.exit_code,
        errors: f.errors,
        report: f.report,
        cli: f.cli,
        check: f.check,
        check_exit_code: f.check_exit_code,
    }
}

/// What one `aipl check` run for a case is compared against.
///
/// There is exactly **one** of these, or none — never two — which is how "check
/// runs at most once per case" is stated in the types rather than left to the
/// shape of an `if`/`else`. Both sections are served by the same run's output, so
/// pinning the report, the exit code, both, or neither all cost the one
/// subprocess. See [`check_plan`].
struct CheckPlan<'a> {
    /// The `--- check ---` body: `check`'s stdout, byte-for-byte.
    report: Option<&'a str>,
    /// The `--- check exit code ---` body: the status `check` exits with.
    exit_code: Option<i32>,
    /// Nothing is pinned, so the only expectation left is that the case's
    /// in-language tests pass. False as soon as either section says what to
    /// expect — a fixture documenting a *failing* `check` must not also be
    /// required to succeed.
    require_success: bool,
}

/// Whether `aipl check` has to run for this case, and what its one run is
/// compared against.
///
/// `None` is the block-less majority of the corpus, which pays nothing. Otherwise
/// the run happens because the source carries `.test` or `.performance` blocks —
/// `check` is the only thing that runs either, so a case whose claims live in a
/// `.performance` block is held to them only by this run — or because a section
/// pins something about `check`: a case may pin a report with no tests of its own
/// (a compile failure's report, say), which is why the sections count on their own.
fn check_plan(spec: &Spec) -> Option<CheckPlan<'_>> {
    let report = spec.check.as_deref();
    let exit_code = spec.check_exit_code;
    let pinned = report.is_some() || exit_code.is_some();
    if !pinned && !spec.source.contains(".test") && !spec.source.contains(".performance") {
        return None;
    }
    Some(CheckPlan {
        report,
        exit_code,
        require_success: !pinned,
    })
}

/// The result of running a single case.
///
/// `Filled` mirrors `Fail`: it carries a message and represents a non-`Pass`
/// outcome, but instead of a validation failure it means fill mode (the
/// `fill_expected` helper) rewrote the case's stale sections from the actual
/// output. Only [`Fills::flush`] builds one — a section check itself only
/// `Pass`es or `Fail`s, because fill mode *records* the new body and the whole
/// batch is written back once, when the case is done.
enum Outcome {
    Pass,
    Filled(String),
    Fail(String),
}

/// Propagate a section check's `Outcome` like `?`: a validation `Fail` returns
/// from the enclosing case, so a later section can never mask an earlier
/// failure. Anything else falls through to the next section.
macro_rules! check_section {
    ($outcome:expr) => {
        if let fail @ Outcome::Fail(_) = $outcome {
            return fail;
        }
    };
}

/// The section rewrites a fill run has decided on for one case, held until every
/// section has been checked and then written back in a single pass.
///
/// `enabled` is what makes a run a *fill* run: with it off a differing section
/// is a failure, with it on it is recorded here instead. Batching is the point
/// — [`flush`](Fills::flush) costs one read, one walk of the file and one write
/// for the whole case, where rewriting per section paid for all three once per
/// stale section (and a case whose `stdout`, `exit code` and `report` all
/// moved is the common shape after a codegen change).
struct Fills {
    enabled: bool,
    /// `(section name, new body)`, in the order the sections were checked. The
    /// file's own order is what the rewrite follows, so this one only decides
    /// where *appended* (absent) sections land.
    pending: Vec<(String, String)>,
}

impl Fills {
    /// A collector for a run in fill mode (`enabled`) or a normal validating one.
    fn new(enabled: bool) -> Self {
        Fills {
            enabled,
            pending: Vec::new(),
        }
    }

    /// Write every recorded section back to `path` in one pass, reporting what
    /// was refreshed (one line per section, so a case's diff is legible in the
    /// fill summary). `Pass` when nothing was recorded — including every normal
    /// run, where nothing ever is.
    fn flush(self, path: &Path) -> Outcome {
        if self.pending.is_empty() {
            return Outcome::Pass;
        }
        let note = self
            .pending
            .iter()
            .map(|(section, _)| format!("[{}]: refreshed `{section}`", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        let p = path.to_str().expect("utf-8 case path");
        aipl::codegen::fill_or_add_sections_file(p, &self.pending)
            .unwrap_or_else(|e| panic!("fill_or_add_sections_file({p:?}): {e}"));
        Outcome::Filled(note)
    }
}

// One `#[test]` per case: libtest runs them in parallel on its own thread pool,
// reports each by name, and filters them by name — so a failure points at the
// case directly and a focused re-run needs no bespoke filter env var. The
// invocation lists every case's test name alongside its display path, and
// doubles as the declared set `every_case_has_a_test` checks the tree against.
macro_rules! case_tests {
    ($($(#[$attr:meta])* $name:ident = $display:literal),+ $(,)?) => {
        /// Every case with a `#[test]`, as `(test name, display path)`. Kept in
        /// the same order as the generated list, which is sorted by display path.
        /// Burn-down cases are declared here like any other — they are `#[ignore]`d,
        /// not omitted, so `every_case_has_a_test` still accounts for them.
        const DECLARED_CASES: &[(&str, &str)] = &[$((stringify!($name), $display)),+];
        $(
            #[test]
            $(#[$attr])*
            fn $name() {
                on_big_stack(|| run_one($display));
            }
        )+
    };
}

/// The checked-in [`case_tests!`] invocation — one `#[test]` per case. It lives
/// in its own file (and outside `tests/`'s top level, so cargo doesn't compile
/// it as a test target of its own) to keep this harness readable at 500+ cases.
/// Regenerate it with the `fill_case_tests` helper.
const CASE_TESTS_FILE: &str = "tests/support/case_tests.rs";
include!("support/case_tests.rs");

/// Run `f` on a 256 MB-stack worker, like the `aipl` CLI: compiling a library
/// case's synthesized `.test` driver (and debug codegen generally) recurses
/// deeply enough to overflow the default test thread's stack. Propagates the
/// worker's panic, so an intentional "filter active"/"fill complete"/failure
/// divergence still fails the test.
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(f)
        .expect("spawn worker")
        .join()
        .expect("worker panicked");
}

/// The `#[test]` name for the case at display path `display`: every run of
/// non-alphanumerics collapses to one `_` (`cases/strings/raw/raw_string` ->
/// `cases_strings_raw_raw_string`). Shared by the generator and by
/// [`every_case_has_a_test`], so the two can never disagree on the mapping.
fn test_name_for(display: &str) -> String {
    let mut out = String::with_capacity(display.len());
    for c in display.chars() {
        match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => out.push(c),
            _ if out.ends_with('_') => {}
            _ => out.push('_'),
        }
    }
    out.trim_matches('_').to_string()
}

/// Run the single case at display path `display`, panicking with the harness's
/// own message on a failure. A `display` with no matching file on disk means the
/// checked-in list is stale — [`every_case_has_a_test`] reports that properly, so
/// here it's just a pointed panic.
fn run_one(display: &str) {
    let (cases, out_root) = setup_cases();
    let (path, root, prefix, stage) = cases
        .iter()
        .find(|c| case_display(c) == display)
        .unwrap_or_else(|| {
            panic!(
                "no case file for {display:?} — the {CASE_TESTS_FILE} list is stale; \
                 regenerate it with `{CASE_TESTS_CMD}`\n{}",
                // Only the list is stale: a deleted case has no file left to fill.
                Fill::CaseTests { added: Vec::new() }.marker()
            )
        });
    let rel_with_prefix = Path::new(prefix).join(path.strip_prefix(root).unwrap_or(path));
    match run_case(path, &rel_with_prefix, out_root, *stage, false) {
        Outcome::Pass => {}
        Outcome::Filled(msg) => panic!("unexpected fill outside fill mode: {msg}"),
        Outcome::Fail(msg) => panic!("{msg}"),
    }
}

/// The display path of a discovered case (`cases/arithmetic/division`) — the
/// name its `#[test]` is derived from, with `/` separators on every platform.
fn case_display((path, root, prefix, _): &CaseFile) -> String {
    Path::new(prefix)
        .join(path.strip_prefix(root).unwrap_or(path))
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Guard for the checked-in per-case `#[test]` list: since `#[test]` functions
/// are compile-time and case files are discovered at run time, the two can drift
/// — a new `.aipl` case would otherwise be silently *never run*, which is the
/// failure mode this whole arrangement has to rule out. Fails when a case has no
/// test (or a test names a case that no longer exists), and when two cases
/// collide on one test name.
/// The leak gate runs for a case that pins nothing.
///
/// `examples/arrays` has no `--- report ---` section, and the gate must hold it to
/// freeing what it allocates all the same: it used to run only as a side effect of
/// comparing a section, which is no longer a thing a case needs to have.
///
/// Nothing in the language can leak on purpose, so the instrumented runtime has a
/// test switch that stops counting frees (`AIPL_INJECT_LEAK`). It is set on a
/// *child* run of this very test binary, filtered to that one case: setting it here
/// would reach every case running in parallel in this process.
#[test]
fn the_leak_gate_runs_for_a_case_with_no_sections() {
    let spec = parse_spec(
        &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/arrays.aipl"))
            .expect("read the example"),
    );
    assert!(
        spec.report.is_none(),
        "this test needs a case that pins nothing"
    );

    // What a child run of that one case reports: whether it ran at all, and the
    // harness's leak message if it gave one. Whether the case as a whole passed
    // is not asked: that includes its row in the baseline table, which is stale
    // between a codegen change and the refresh that settles it, and a leak gate
    // that only held while a metrics table was current would make that refresh
    // wait on itself. The leak gate is judged on the message alone.
    let run = |leak: bool| {
        // A place of its own to link in: the same case is being run, by nextest,
        // in another process, and a shared executable path is a race between them.
        let out_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("aipl-leak-gate-{}-{leak}", std::process::id()));
        let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
        cmd.args(["--exact", "examples_arrays", "--test-threads=1"]);
        cmd.env("AIPL_CASES_OUT", &out_root);
        if leak {
            cmd.env("AIPL_INJECT_LEAK", "1");
        }
        let out = cmd.output().expect("re-run the harness on one case");
        let _ = fs::remove_dir_all(&out_root);
        let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        // libtest's own lines vary; the harness's failure message is the point.
        let message: Vec<String> = stdout
            .lines()
            .filter(|l| l.contains("memory leak") || *l == "Fix the leak.")
            .map(str::to_string)
            .collect();
        (stdout.contains("running 1 test"), message)
    };

    assert_eq!(run(false), (true, vec![]));
    assert_eq!(
        run(true),
        (
            true,
            vec![
                "[examples/arrays.aipl]: memory leak — allocations (3) != deallocations (0)"
                    .to_string(),
                "Fix the leak.".to_string(),
            ]
        )
    );
}

#[test]
fn every_case_has_a_test() {
    let (cases, _) = setup_cases();
    let on_disk: Vec<(String, String)> = cases
        .iter()
        .map(|c| {
            let display = case_display(c);
            (test_name_for(&display), display)
        })
        .collect();

    // A collision would make one case shadow another in the generated list, so
    // it has to be an error rather than something the regenerate helper papers
    // over by dropping a case.
    let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, display) in &on_disk {
        by_name.entry(name).or_default().push(display);
    }
    let collisions: Vec<String> = by_name
        .iter()
        .filter(|(_, ds)| ds.len() > 1)
        .map(|(name, ds)| format!("  {name} <- {}", ds.join(", ")))
        .collect();
    assert!(
        collisions.is_empty(),
        "{} case(s) map to the same `#[test]` name — rename one of each pair:\n{}",
        collisions.len(),
        collisions.join("\n"),
    );

    let declared: HashSet<&str> = DECLARED_CASES.iter().map(|(_, d)| *d).collect();
    let found: HashSet<&str> = on_disk.iter().map(|(_, d)| d.as_str()).collect();
    let mut missing: Vec<&&str> = found.difference(&declared).collect();
    let mut stale: Vec<&&str> = declared.difference(&found).collect();
    missing.sort();
    stale.sort();
    if missing.is_empty() && stale.is_empty() {
        return;
    }
    let list = |label: &str, items: &[&&str]| {
        if items.is_empty() {
            String::new()
        } else {
            format!(
                "\n{} {label}:\n{}",
                items.len(),
                items
                    .iter()
                    .map(|d| format!("  {d} ({})", test_name_for(d)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    };
    panic!(
        "{CASE_TESTS_FILE} is out of date — it declares a `#[test]` per case, and \
         a case with no test never runs.{}{}\n\nRegenerate with `{CASE_TESTS_CMD}`.\n{}",
        list("case(s) with no `#[test]`", &missing),
        list("declared `#[test]`(s) with no case file", &stale),
        // The cases with no test have never run, so none of their sections has
        // been compared: they need filling as well as listing.
        Fill::CaseTests {
            added: missing.iter().map(|d| (**d).to_string()).collect(),
        }
        .marker(),
    );
}

// The two author-helper "refresh" modes are `#[ignore]`d tests rather than env
// vars: a normal `cargo test` skips them, and you opt in explicitly by name with
// `-- --ignored <name>`. Each diverges (fails) when done so its summary is
// visible (cargo hides passing tests' output) and it's never mistaken for a
// normal green run. The `#[ignore]` reason repeats the command for `cargo test
// -- --list`/`--ignored` output.

/// Author helper: in one pass, rewrite every section whose body differs from the
/// actual output — `stdout` / `stderr` / `exit code` / `report` /
/// `check` / `errors` / `expect file`. No `?` placeholder
/// or repeated runs are needed; a single pass refreshes all sections of every
/// case. Set `AIPL_CASE` to target a subset. Run with:
///   cargo test --test cases -- --ignored fill_expected
#[test]
#[ignore = "author helper — run: cargo test --test cases -- --ignored fill_expected"]
fn fill_expected() {
    on_big_stack(run_fill);
}

/// Author helper: rewrite [`CASE_TESTS_FILE`] from the cases on disk, so a newly
/// added (or deleted) `.aipl` case gets (or loses) its `#[test]`. Run with:
///   cargo test --test cases -- --ignored fill_case_tests
#[test]
#[ignore = "author helper — run: cargo test --test cases -- --ignored fill_case_tests"]
fn fill_case_tests() {
    let (cases, _) = setup_cases();
    let mut body = String::from(
        "// @generated by `cargo test --test cases -- --ignored fill_case_tests`.\n\
         // One `#[test]` per case file, sorted by display path. Do not edit by hand:\n\
         // add or remove a `.aipl` case and regenerate. `every_case_has_a_test`\n\
         // fails when this list and the tree disagree.\n\
         case_tests! {\n",
    );
    let mut rows: Vec<(String, String)> = cases
        .iter()
        .map(|c| {
            let display = case_display(c);
            (test_name_for(&display), display)
        })
        .collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    for (name, display) in &rows {
        body.push_str(&format!("    {name} = {display:?},\n"));
    }
    body.push_str("}\n");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CASE_TESTS_FILE);
    fs::create_dir_all(path.parent().expect("case-tests dir")).expect("mkdir case-tests dir");
    fs::write(&path, body).expect("write case tests");
    // Diverges like the other author helpers: cargo hides passing tests' output,
    // and the next `cargo test` has to recompile against the new list anyway.
    panic!(
        "wrote {} `#[test]`(s) to {}. Failing intentionally so this is visible — \
         re-run `cargo test` to compile and run the regenerated list.",
        rows.len(),
        path.display(),
    );
}

/// Perf-monitor refresh: measure non-deterministic metrics (wall-clock, build
/// time, peak memory) for every runnable case and rewrite
/// `tests/performance_metrics.md`, printing an improvement/regression summary.
/// Run with:
///   cargo test --test cases -- --ignored refresh_perfmon
#[test]
#[ignore = "perfmon refresh — run: cargo test --test cases -- --ignored refresh_perfmon"]
fn refresh_perfmon() {
    on_big_stack(run_perfmon);
}

/// One discovered case: its file `path`, the `root` its display path is relative
/// to, the display `prefix` (`cases`/`examples`/`crates`), and whether it's
/// staged to a temp dir (`tests/cases/**`) or loaded in place (`examples/`,
/// `crates/`, whose cross-file `import`s resolve against the real directory).
type CaseFile = (PathBuf, PathBuf, &'static str, bool);

/// Shared harness setup: install parser hooks, resolve the output dir, and
/// collect every case across `tests/cases/`, `examples/`, and `crates/`. Done
/// once per process and shared: with one `#[test]` per case this is called 500+
/// times, and the directory walk (and the hook install) is the same every time.
fn setup_cases() -> &'static (Vec<CaseFile>, PathBuf) {
    static SETUP: std::sync::OnceLock<(Vec<CaseFile>, PathBuf)> = std::sync::OnceLock::new();
    SETUP.get_or_init(|| {
        // Raw-string cases de-dent through the dogfooded AIPL `dedent` (FFI), which
        // the parser reaches via a hook with no native fallback — install it before
        // any case is compiled in-process.
        aipl::install_parser_hooks();

        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        // Where each case stages and links. One fixed place per case, so two
        // processes running the same case at once would link the same
        // executable; `AIPL_CASES_OUT` gives a process a place of its own (the
        // leak-gate test re-runs a case in a child while nextest runs it in
        // another process).
        let out_root = std::env::var_os("AIPL_CASES_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("aipl-cases"));
        fs::create_dir_all(&out_root).expect("mkdir cases output");

        let cases = collect_all_cases(
            &root.join("tests").join("cases"),
            &root.join("examples"),
            &root.join("crates"),
        );
        assert!(!cases.is_empty(), "no .aipl test cases found");
        (cases, out_root)
    })
}

/// Entry point for the ignored `refresh_perfmon` test.
fn run_perfmon() {
    let (cases, out_root) = setup_cases();
    run_perfmon_refresh(&cases, &out_root);
}

/// Collect every `.aipl` case (sorted within each group for run-to-run-stable
/// shard assignment). Three groups, each tagged with how it's run:
///   - `tests/cases/**` — self-contained fixtures, staged to a temp dir.
///   - `examples/` (top level) — user programs, loaded in place so their
///     `import`s resolve.
///   - `crates/**/*.aipl` — the compiler-dogfooded helpers (`add`, `count_while`,
///     `dedent`, `process_raw_string`, …), loaded in place. Run as library cases:
///     their `.test` blocks are verified and their baseline row is asserted.
fn collect_all_cases(cases_root: &Path, examples_root: &Path, crates_root: &Path) -> Vec<CaseFile> {
    let mut cases = Vec::new();
    collect_cases(cases_root, &mut cases);
    cases.sort();
    // examples/ is walked non-recursively — subdirectories like examples/lib
    // and examples/math hold imported library files with no `main` of their own.
    let mut examples = Vec::new();
    for entry in fs::read_dir(examples_root).expect("read examples dir") {
        let path = entry.expect("dir entry").path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("aipl") {
            examples.push(path);
        }
    }
    examples.sort();
    let mut crate_files = Vec::new();
    collect_cases(crates_root, &mut crate_files);
    crate_files.sort();

    let mut out: Vec<CaseFile> = Vec::new();
    out.extend(
        cases
            .into_iter()
            .map(|p| (p, cases_root.to_path_buf(), "cases", true)),
    );
    out.extend(
        examples
            .into_iter()
            .map(|p| (p, examples_root.to_path_buf(), "examples", false)),
    );
    out.extend(
        crate_files
            .into_iter()
            .map(|p| (p, crates_root.to_path_buf(), "crates", false)),
    );
    out
}

/// Validate one expected section, or record it for refresh in fill mode.
///
/// - Normal run (`fills.enabled == false`): `Outcome::Pass` if `actual ==
///   expected`, otherwise `Outcome::Fail`. There is no `?`/placeholder escape
///   hatch, so a stale (or `?`) section fails like any other mismatch.
/// - Fill mode: a matching section is still `Pass`; a differing one is queued on
///   `fills` — also `Pass`, so the caller keeps checking the *remaining*
///   sections. [`Fills::flush`] writes the whole batch back at the end of the
///   case, which is why one fill pass refreshes every section of every case.
fn check_or_fill(
    fills: &mut Fills,
    ctx: &str,
    section: &str,
    actual: &str,
    expected: &str,
) -> Outcome {
    if actual == expected {
        return Outcome::Pass;
    }
    if fills.enabled {
        fills
            .pending
            .push((section.to_string(), actual.to_string()));
        return Outcome::Pass;
    }
    Outcome::Fail(format!(
        "{ctx}: `{section}` mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}\n\
         If this change is intended, run `{}`.\n{}",
        scoped_fill_cmd(ctx),
        Fill::Section {
            case: case_of(ctx),
            section: section.to_string(),
        }
        .marker()
    ))
}

/// The display path a failure's `[bracketed]` context names — what `AIPL_CASE`
/// matches and a [`Fill`] marker carries.
fn case_of(ctx: &str) -> String {
    ctx.trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

/// The `AIPL_CASE` selection shared by the two author helpers that read it,
/// [`fill_expected`] and [`refresh_perfmon`]: a path substring, or a
/// comma-separated list of them (a case path never contains a comma), matched
/// against a case's display path *with* its `.aipl` extension — the exact text a
/// per-case failure brackets, so the `AIPL_CASE='<path>'` it suggests can be
/// pasted back verbatim. That is also how `cargo handoff` names every stale case
/// in a single run: each `nextest` invocation pays a fresh-binary startup that
/// dwarfs the work itself, so one invocation for N cases beats N for one.
///
/// Unset (or empty) selects every case.
struct CaseFilter {
    /// `AIPL_CASE` as given, for summaries.
    raw: Option<String>,
    entries: Vec<String>,
    /// Which entries have selected something, so a typo in a batch is named
    /// rather than hidden behind its neighbours matching.
    hit: Vec<bool>,
}

impl CaseFilter {
    fn from_env() -> Self {
        let raw = std::env::var("AIPL_CASE").ok().filter(|s| !s.is_empty());
        let entries: Vec<String> = raw
            .iter()
            .flat_map(|s| s.split(','))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        CaseFilter {
            hit: vec![false; entries.len()],
            raw,
            entries,
        }
    }

    /// Whether the case at `display` (with `/` separators and its extension) is
    /// selected, recording which entries selected it.
    fn matches(&mut self, display: &str) -> bool {
        if self.entries.is_empty() {
            return true;
        }
        let mut any = false;
        for (i, entry) in self.entries.iter().enumerate() {
            if display.contains(entry.as_str()) {
                self.hit[i] = true;
                any = true;
            }
        }
        any
    }

    /// A filter that matches nothing is almost always a typo — fail loudly rather
    /// than silently doing nothing. Reported per entry: in a batch the others will
    /// have matched, so "something matched" alone would let a mistyped path
    /// through as a silent no-op.
    fn assert_all_matched(&self, total: usize) {
        let missed: Vec<&str> = self
            .entries
            .iter()
            .zip(&self.hit)
            .filter(|(_, hit)| !**hit)
            .map(|(entry, _)| entry.as_str())
            .collect();
        assert!(
            missed.is_empty(),
            "AIPL_CASE matched no test cases (of {total} total) for: {}",
            missed.join(", ")
        );
    }
}

/// Entry point for the ignored [`fill_expected`] helper: one serial pass over
/// every case, refreshing each section whose body differs from the actual output.
/// Unlike a normal run this is a single test, so it collects results rather than
/// panicking per case — and `AIPL_CASE` is the only way to scope it, since
/// libtest's own filter can't reach inside one test.
fn run_fill() {
    let (cases, out_root) = setup_cases();

    let mut filter = CaseFilter::from_env();

    let mut passed = 0usize;
    let mut matched = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut refreshed: Vec<String> = Vec::new();
    for (path, root, prefix, stage) in cases {
        let rel_with_prefix = Path::new(prefix).join(path.strip_prefix(root).unwrap_or(path));
        if !filter.matches(&rel_with_prefix.to_string_lossy().replace('\\', "/")) {
            continue;
        }
        matched += 1;
        match run_case(path, &rel_with_prefix, out_root, *stage, true) {
            Outcome::Pass => passed += 1,
            Outcome::Filled(msg) => refreshed.push(msg),
            Outcome::Fail(msg) => failures.push(msg),
        }
    }
    filter.assert_all_matched(cases.len());

    // Summary. The filtered wording is a parsed contract — `cargo handoff`
    // parses it to confirm a scoped refill ran clean — so keep it as it reads.
    match &filter.raw {
        Some(f) => eprintln!(
            "\n=== test cases [filter {f:?}]: {passed} passed, {} failed, {} refreshed ({matched} of {} matched) ===",
            failures.len(),
            refreshed.len(),
            cases.len(),
        ),
        None => eprintln!(
            "\n=== test cases [fill_expected]: {passed} passed, {} failed, {} refreshed ({matched} of {} total) ===",
            failures.len(),
            refreshed.len(),
            cases.len(),
        ),
    }
    if !refreshed.is_empty() {
        eprintln!("\n=== {} REFRESHED SECTION(S) ===", refreshed.len());
        for note in &refreshed {
            eprintln!("{note}");
        }
    }
    if !failures.is_empty() {
        eprintln!("\n=== {} FAILING CASE(S) ===", failures.len());
        for failure in &failures {
            eprintln!("\n{failure}");
        }
        panic!(
            "{} of {matched} case(s) failed in a way no refill can fix (see summary above)",
            failures.len(),
        );
    }
    // Diverges when clean too: cargo hides passing tests' output, so failing is
    // the only way to show what was refreshed, and it ensures a fill run is never
    // mistaken for a normal green suite.
    panic!(
        "section refresh complete: {} case(s) refreshed, {passed} \
         already-current, {matched} case(s) seen. Failing intentionally so the \
         summary above is visible — this is not a normal test run (`{FILL_CMD}`).",
        refreshed.len(),
    );
}

/// Every `.aipl` case under `dir`, recursively — walked by the dogfooded AIPL
/// `find_files` (see [`aipl::codegen::find_files`]), not `std::fs`. Discovery
/// finding nothing would silently run no cases at all, so an unwalkable
/// directory panics rather than yielding an empty list.
fn collect_cases(dir: &Path, out: &mut Vec<PathBuf>) {
    let dir = dir.to_str().expect("utf-8 directory path");
    let found = aipl::codegen::find_files(dir, ".aipl")
        .unwrap_or_else(|e| panic!("find_files({dir:?}): {e}"));
    out.extend(found.into_iter().map(PathBuf::from));
}

/// Run one case: parse its sections, stage its sources, and check (or, in fill
/// mode, refresh) every expectation.
///
/// The case file is read and parsed **once**, here. Fill mode collects the
/// sections that moved in a [`Fills`] and flushes them back in one rewrite when
/// the case is done, so a case with three stale sections costs one read/walk/
/// write, not three — and the flush is the only thing that touches the file.
fn run_case(path: &Path, rel: &Path, out_root: &Path, stage_to_temp: bool, fill: bool) -> Outcome {
    let contents = fs::read_to_string(path).expect("read test case");
    let spec = parse_spec(&contents);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap();
    let ctx = format!("[{}]", rel.display());
    let mut fills = Fills::new(fill);

    // Two modes for staging:
    //   - tests/cases/: stage source + companion `file:` sections into
    //     a per-case temp dir, so the case file is self-contained.
    //   - examples/: load in place, so user-facing examples can `import`
    //     real companion files (e.g. examples/math/geometry.aipl).
    let (src_path, case_dir) = if stage_to_temp {
        let dir = out_root.join(rel.with_extension(""));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir case staging");
        let src = dir.join(format!("{stem}.aipl"));
        fs::write(&src, &spec.source).expect("write staged source");
        aipl::stage_companions(&dir, &spec.extra_files).expect("stage companions");
        (src, dir)
    } else {
        if !spec.extra_files.is_empty() {
            return Outcome::Fail(format!(
                "{ctx}: `file:` sections are not supported for in-place sources"
            ));
        }
        // Output executable still lands in a per-case temp dir so we
        // don't litter the source tree.
        let dir = out_root.join(rel.with_extension(""));
        fs::create_dir_all(&dir).expect("mkdir case output");
        (path.to_path_buf(), dir)
    };

    let outcome = if spec.errors.is_some() {
        if spec.stdout.is_some() || spec.stderr.is_some() || spec.exit_code.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `errors` section is mutually exclusive with stdout/stderr/exit code"
            ));
        }
        if spec.report.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `report` section requires a running program, so it cannot coexist with `errors`"
            ));
        }
        if spec.check.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `check` section requires a compiling program, so it cannot coexist with `errors`"
            ));
        }
        if !spec.expect_files.is_empty() {
            return Outcome::Fail(format!(
                "{ctx}: `expect file:` section requires a running program, so it cannot coexist with `errors`"
            ));
        }
        run_error_case(&ctx, path, &src_path, &spec, &mut fills)
    } else {
        run_success_case(&ctx, path, &src_path, stem, &spec, &case_dir, &mut fills)
    };
    // Write back whatever fill mode collected, whether or not a later section
    // failed: the sections checked before the failure were measured on this run
    // and refusing to record them would only mean measuring them again on the
    // next. A failure still decides the case's outcome.
    let filled = fills.flush(path);
    if matches!(outcome, Outcome::Fail(_)) {
        outcome
    } else {
        filled
    }
}

/// Load a case, rooted at its own directory.
///
/// A case is a one-file project (plus whatever its `--- file: ---` companions
/// stage beside it), so its directory *is* its project root — which is what a
/// `trace(..)` names its file relative to. Rooting it anywhere else would make
/// that name depend on where the staging directory happens to sit, and
/// `CARGO_TARGET_TMPDIR` moves with `CARGO_TARGET_DIR`: the baked label is in the
/// program's data section, so a case's `binary size` would stop being a property
/// of the case.
fn load_case(src_path: &Path) -> Result<Program, Vec<Error>> {
    let root = src_path.parent().unwrap_or(Path::new("."));
    loader::load_program_rooted(src_path, root, debug_opts())
}

/// Check (or refresh) the `--- errors ---` section of a case that is expected
/// not to compile. Fill mode records the rendered diagnostic through the same
/// [`check_or_fill`] every other section goes through, so there is no second
/// splicing path for this one section to drift from.
fn run_error_case(
    ctx: &str,
    orig_path: &Path,
    src_path: &Path,
    spec: &Spec,
    fills: &mut Fills,
) -> Outcome {
    let result =
        load_case(src_path).and_then(|prog| Compilation::new(&prog, debug_opts()).map(|_| ()));
    let errs = match result {
        Err(e) => e,
        Ok(()) => {
            // Nothing a refill can fix — the case claims an error the compiler
            // no longer reports, so the expectation itself is what's wrong.
            return Outcome::Fail(format!(
                "{ctx}: expected an error, but compilation succeeded"
            ));
        }
    };
    let actual = Error::render_all(&errs, &spec.source, &render_path(orig_path));
    check_or_fill(
        fills,
        ctx,
        "errors",
        &actual,
        spec.errors.as_deref().unwrap_or(""),
    )
}

// ---------- Perf-monitor refresh (non-deterministic metrics) ----------

/// One row of the metrics table.
///
/// The first five fields are **deterministic** and asserted: every case's run
/// must reproduce them exactly (see [`check_baseline`]). The last three are the
/// non-deterministic ones, kept only to track trends.
#[derive(Clone, PartialEq)]
struct PerfRow {
    test: String,
    allocations: u64,
    bytes_allocated: u64,
    peak_live_bytes: u64,
    instructions: u64,
    code_bytes: u64,
    wall_us: f64,
    build_ms: f64,
    peak_kb: u64,
}

/// A deterministic metric of the table: its column heading, and how to read it off
/// a row. One list, so the table, the comparison and the failure message cannot
/// disagree about which columns are asserted or what they are called.
const BASELINE_METRICS: [(&str, fn(&PerfRow) -> u64); 5] = [
    ("allocations", |r| r.allocations),
    ("bytes allocated", |r| r.bytes_allocated),
    ("peak live bytes", |r| r.peak_live_bytes),
    ("instructions", |r| r.instructions),
    ("code bytes", |r| r.code_bytes),
];

/// Number of times each binary is run; we keep the minimum wall-clock/peak (the
/// least noise-perturbed sample).
const PERFMON_RUNS: usize = 5;

/// Stage a case for building (mirrors `run_case`'s staging): for `tests/cases/`
/// write the section-stripped source + companion `file:`s into a per-case temp
/// dir; for `examples/` build in place. Returns `(src_path, case_dir)`.
fn stage_case_for_build(
    path: &Path,
    rel: &Path,
    out_root: &Path,
    stem: &str,
    spec: &Spec,
    stage_to_temp: bool,
) -> (PathBuf, PathBuf) {
    let dir = out_root.join(rel.with_extension(""));
    if stage_to_temp {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir case staging");
        let src = dir.join(format!("{stem}.aipl"));
        fs::write(&src, &spec.source).expect("write staged source");
        aipl::stage_companions(&dir, &spec.extra_files).expect("stage companions");
        (src, dir)
    } else {
        fs::create_dir_all(&dir).expect("mkdir case output");
        (path.to_path_buf(), dir)
    }
}

/// Build a case both ways: the production (non-instrumented) binary, whose build
/// is timed and which the wall-clock and memory columns run, and the instrumented
/// one, which `perf::measure` runs once for the deterministic columns.
///
/// Returns the production binary's path, how long it took to build (load +
/// compile + link, the instrumented build excluded), and the measurement — the
/// same one the case's own run takes, since `cwd` is the directory the harness
/// would have staged it in.
fn build_case_binary(
    src_path: &Path,
    stem: &str,
    case_dir: &Path,
    cli: &[String],
) -> Result<(PathBuf, f64, PerfStats), String> {
    let start = Instant::now();
    let program = load_case(src_path).map_err(|e| Error::display_all(&e))?;
    let measured = perf::measured_program(&program);
    let comp = ObjectCompilation::new(&measured, stem, debug_opts(), false)
        .map_err(|e| Error::display_all(&e))?;
    let symbol_names = comp.code_symbol_names();
    let obj = comp.emit().map_err(|e| e.to_string())?;
    let exe = case_dir.join(binary::default_exe_name(stem));
    binary::link(&obj, &exe).map_err(|e| e.to_string())?;
    let build_ms = start.elapsed().as_secs_f64() * 1000.0;

    let stats = perf::measure(
        &measured,
        stem,
        &obj,
        &symbol_names,
        cli,
        case_dir,
        debug_opts(),
    )
    .map_err(|e| e.render("", ""))?;
    Ok((exe, build_ms, stats))
}

/// Run `exe` once with `AIPL_PERFMON_STATS` set and read back the
/// `(wall_clock_ns, peak_rss_bytes)` the runtime reports. The exit code is
/// ignored (some cases exit nonzero by design); `None` if the stats weren't
/// produced (e.g. the program aborted before reporting).
fn run_once_perfmon(
    exe: &Path,
    cli: &[String],
    case_dir: &Path,
    stats_path: &Path,
) -> Option<(u64, u64)> {
    let _ = fs::remove_file(stats_path);
    Command::new(exe)
        .args(cli)
        .current_dir(case_dir)
        .env("AIPL_PERFMON_STATS", stats_path)
        .output()
        .ok()?;
    let contents = fs::read_to_string(stats_path).ok()?;
    let (mut wall, mut peak) = (None, None);
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("wall_clock_ns:") {
            wall = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("peak_rss_bytes:") {
            peak = v.trim().parse().ok();
        }
    }
    Some((wall?, peak?))
}

/// Where the metrics table lives, relative to the repo root.
const METRICS_FILE: &str = "tests/performance_metrics.md";

fn metrics_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(METRICS_FILE)
}

/// The display path a case's table row is keyed by: `cases/arrays/slice` — the
/// text a case's `[..]` context brackets, minus the `.aipl`.
fn row_key(display_with_extension: &str) -> String {
    display_with_extension
        .trim_end_matches(".aipl")
        .replace('\\', "/")
}

fn run_perfmon_refresh(cases: &[CaseFile], out_root: &Path) {
    // Scoped like `fill_expected`: `AIPL_CASE` names the cases whose rows are
    // rewritten and every other row is carried over as it is.
    let mut filter = CaseFilter::from_env();
    let path = metrics_path();
    let old = read_metrics(&path);
    let mut kept = old.clone();
    let mut existing: HashSet<String> = HashSet::new();
    let mut measured: Vec<PerfRow> = Vec::new();
    let mut skipped = 0usize;
    for (case_path, root, prefix, stage) in cases {
        let rel = case_path.strip_prefix(root).unwrap_or(case_path);
        let rel_with_prefix = Path::new(prefix).join(rel);
        let with_extension = rel_with_prefix.to_string_lossy().replace('\\', "/");
        let display = row_key(&with_extension);
        // Every case counts as existing, measured or not, so that a row is
        // pruned only when its case is really gone.
        existing.insert(display.clone());
        if !filter.matches(&with_extension) {
            continue;
        }
        // Replaced by whatever this run measures, or dropped if the case no
        // longer runs (it became an error case, or stopped building).
        kept.remove(&display);
        let contents = fs::read_to_string(case_path).expect("read case");
        let spec = parse_spec(&contents);
        // Error cases never build a binary, so there's nothing to time.
        if spec.errors.is_some() {
            skipped += 1;
            continue;
        }
        let stem = case_path.file_stem().and_then(|s| s.to_str()).unwrap();
        let (src_path, case_dir) =
            stage_case_for_build(case_path, &rel_with_prefix, out_root, stem, &spec, *stage);

        let (exe, build_ms, stats) = match build_case_binary(&src_path, stem, &case_dir, &spec.cli)
        {
            Ok(built) => built,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let stats_path = case_dir.join("perfmon_stats.txt");
        let (mut wall_ns, mut peak_bytes) = (u64::MAX, u64::MAX);
        let mut ok = true;
        for _ in 0..PERFMON_RUNS {
            match run_once_perfmon(&exe, &spec.cli, &case_dir, &stats_path) {
                Some((w, p)) => {
                    wall_ns = wall_ns.min(w);
                    peak_bytes = peak_bytes.min(p);
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            skipped += 1;
            continue;
        }
        measured.push(PerfRow {
            test: display,
            allocations: stats.allocations,
            bytes_allocated: stats.bytes_allocated,
            peak_live_bytes: stats.peak_live_bytes,
            instructions: stats.instructions,
            code_bytes: stats.binary_size.code,
            wall_us: wall_ns as f64 / 1000.0,
            build_ms,
            peak_kb: peak_bytes.div_ceil(1024),
        });
        if measured.len().is_multiple_of(25) {
            eprintln!("measured {} case(s)...", measured.len());
        }
    }
    filter.assert_all_matched(cases.len());

    // A row whose case no longer exists goes, whatever the scope: nothing else
    // would ever remove it.
    kept.retain(|test, _| existing.contains(test));
    let mut rows: Vec<PerfRow> = kept.into_values().chain(measured.iter().cloned()).collect();
    rows.sort_by(|a, b| a.test.cmp(&b.test));

    fs::write(&path, render_metrics(&rows)).expect("write metrics file");
    eprintln!("{}", compare_metrics(&old, &rows));

    // Diverge so the summary is visible (cargo hides passing tests' output) and a
    // refresh is never mistaken for a normal green run.
    panic!(
        "perfmon refresh complete: wrote {} ({} measured, {} skipped, {} rows). Failing \
         intentionally so the summary above is visible — this is not a normal test \
         run (`{PERFMON_CMD}`).",
        path.display(),
        measured.len(),
        skipped,
        rows.len(),
    );
}

/// Render the metrics table (Markdown, rows already sorted by test path).
fn render_metrics(rows: &[PerfRow]) -> String {
    let mut s = String::new();
    s.push_str("# Performance metrics\n\n");
    s.push_str(
        "One row per case that runs. The columns come in two groups.\n\n\
         **Asserted** — allocations, bytes allocated, peak live bytes, instructions and\n\
         code bytes. These are deterministic, measured by the instrumented build, and\n\
         every case's run must reproduce its row exactly: a case whose numbers moved\n\
         fails with `performance baseline moved`, and a case with no row fails too. The\n\
         failure names the scoped refresh that accepts the change.\n\n\
         **Not asserted** — wall-clock (measured in-process, so process spawn/teardown is\n\
         excluded), build time (load + compile + link) and peak resident memory. These\n\
         drift run-to-run, so they're checked in only to track trends.\n\n\
         Regenerate and review the printed summary with the command below. `AIPL_CASE`\n\
         (a path substring, or a comma-separated list) rewrites only the matching rows;\n\
         rows whose case no longer exists are always dropped.\n\n\
         ```\n\
         cargo test --test cases -- --ignored refresh_perfmon\n\
         ```\n\n",
    );
    s.push_str(
        "| Test | allocations | bytes allocated | peak live bytes | instructions | code bytes \
         | wall-clock (µs) | build (ms) | peak RSS (KB) |\n",
    );
    s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {:.2} | {:.2} | {} |\n",
            r.test,
            r.allocations,
            r.bytes_allocated,
            r.peak_live_bytes,
            r.instructions,
            r.code_bytes,
            r.wall_us,
            r.build_ms,
            r.peak_kb
        ));
    }
    s
}

/// Parse a previously-written metrics file into `test -> row`. Returns empty on a
/// missing/unreadable file (first run).
fn read_metrics(path: &Path) -> HashMap<String, PerfRow> {
    let mut map = HashMap::new();
    let Ok(contents) = fs::read_to_string(path) else {
        return map;
    };
    for line in contents.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cols: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cols.len() != 9 || cols[0] == "Test" || cols[0].starts_with("---") {
            continue;
        }
        let int = |i: usize| cols[i].parse::<u64>().ok();
        let float = |i: usize| cols[i].parse::<f64>().ok();
        if let (Some(a), Some(b), Some(p), Some(i), Some(c), Some(w), Some(bm), Some(kb)) = (
            int(1),
            int(2),
            int(3),
            int(4),
            int(5),
            float(6),
            float(7),
            int(8),
        ) {
            map.insert(
                cols[0].to_string(),
                PerfRow {
                    test: cols[0].to_string(),
                    allocations: a,
                    bytes_allocated: b,
                    peak_live_bytes: p,
                    instructions: i,
                    code_bytes: c,
                    wall_us: w,
                    build_ms: bm,
                    peak_kb: kb,
                },
            );
        }
    }
    map
}

/// The checked-in table, read once.
fn baselines() -> &'static HashMap<String, PerfRow> {
    static BASELINES: std::sync::OnceLock<HashMap<String, PerfRow>> = std::sync::OnceLock::new();
    BASELINES.get_or_init(|| read_metrics(&metrics_path()))
}

/// Hold a case's measurement to its row in the metrics table: every deterministic
/// column must match exactly. (Exact for now; thresholds are undecided.)
///
/// The two failures are told apart by the [`Fill`] each prints, a contract with
/// `cargo handoff`: it runs everything else to the end and then settles them with
/// one `refresh_perfmon`, rather than stopping at the first.
fn check_baseline(ctx: &str, stats: &PerfStats) -> Outcome {
    let key = row_key(ctx.trim_start_matches('[').trim_end_matches(']'));
    let accept = format!("AIPL_CASE='{key}.aipl' {PERFMON_CMD}");
    let Some(row) = baselines().get(&key) else {
        return Outcome::Fail(format!(
            "{ctx}: performance baseline missing: {METRICS_FILE} has no row for this case.\n\
             Add it with `{accept}`.\n{}",
            Fill::BaselineMissing { case: case_of(ctx) }.marker()
        ));
    };
    let actual = [
        stats.allocations,
        stats.bytes_allocated,
        stats.peak_live_bytes,
        stats.instructions,
        stats.binary_size.code,
    ];
    let moved: Vec<String> = BASELINE_METRICS
        .iter()
        .zip(actual)
        .filter_map(|((name, read), now)| {
            let was = read(row);
            (was != now).then(|| {
                let pct = if was > 0 {
                    format!(" ({:+.1}%)", (now as f64 - was as f64) / was as f64 * 100.0)
                } else {
                    String::new()
                };
                format!("  {name}: {was} -> {now}{pct}")
            })
        })
        .collect();
    if moved.is_empty() {
        return Outcome::Pass;
    }
    Outcome::Fail(format!(
        "{ctx}: performance baseline moved ({METRICS_FILE}):\n{}\n\
         If this change is intended, accept it with `{accept}`.\n{}",
        moved.join("\n"),
        Fill::BaselineMoved {
            case: case_of(ctx),
            moves: moved.iter().map(|m| m.trim().to_string()).collect(),
        }
        .marker()
    ))
}

/// One metric's per-test outlier rules: only flag a change that is both
/// relatively large (`> threshold` percent) and absolutely non-trivial (`> floor`
/// in the metric's unit), so normal run-to-run jitter doesn't swamp the lists.
/// These programs run in (sub-)microseconds and link in tens of milliseconds,
/// where tiny absolute jitter is a huge *percentage*; the floors filter that out.
/// The overall aggregate (below) still reflects every shared test regardless.
///
/// A deterministic metric has neither: any change at all is a move.
struct Metric {
    name: &'static str,
    unit: &'static str,
    /// Decimal places when printing values.
    decimals: usize,
    threshold: f64,
    floor: f64,
    read: fn(&PerfRow) -> f64,
}

impl Metric {
    /// A metric that is asserted exactly, so every change is reported.
    const fn exact(name: &'static str, unit: &'static str, read: fn(&PerfRow) -> f64) -> Self {
        Metric {
            name,
            unit,
            decimals: 0,
            threshold: 0.0,
            floor: 0.0,
            read,
        }
    }
}

/// Every column of the table, in its order.
const METRICS: [Metric; 8] = [
    Metric::exact("allocations", "", |r| r.allocations as f64),
    Metric::exact("bytes allocated", "B", |r| r.bytes_allocated as f64),
    Metric::exact("peak live bytes", "B", |r| r.peak_live_bytes as f64),
    Metric::exact("instructions", "", |r| r.instructions as f64),
    Metric::exact("code bytes", "B", |r| r.code_bytes as f64),
    Metric {
        name: "wall-clock",
        unit: "µs",
        decimals: 2,
        threshold: 10.0,
        floor: 2.0,
        read: |r| r.wall_us,
    },
    Metric {
        name: "build",
        unit: "ms",
        decimals: 2,
        threshold: 10.0,
        floor: 20.0,
        read: |r| r.build_ms,
    },
    Metric {
        name: "peak RSS",
        unit: "KB",
        decimals: 0,
        threshold: 10.0,
        floor: 128.0,
        read: |r| r.peak_kb as f64,
    },
];

/// Format one metric's per-test regressions and improvements from the shared
/// tests' `(test, old, new)` deltas, applying [`Metric`]'s threshold/floor.
fn metric_outliers(m: &Metric, deltas: &[(String, f64, f64)]) -> String {
    let pct = |o: f64, n: f64| if o > 0.0 { (n - o) / o * 100.0 } else { 0.0 };
    let mut regs: Vec<(&str, f64, f64, f64)> = Vec::new();
    let mut imps: Vec<(&str, f64, f64, f64)> = Vec::new();
    for (t, o, n) in deltas {
        if (n - o).abs() <= m.floor {
            continue;
        }
        let d = pct(*o, *n);
        // A rise from zero has no percentage; it is still a regression.
        if n > o && d >= m.threshold {
            regs.push((t, *o, *n, d));
        } else if n < o && d <= -m.threshold {
            imps.push((t, *o, *n, d));
        }
    }
    regs.sort_by(|a, b| b.3.total_cmp(&a.3)); // biggest regression first
    imps.sort_by(|a, b| a.3.total_cmp(&b.3)); // biggest improvement first
    let (name, prec) = (m.name, m.decimals);
    let unit = if m.unit.is_empty() {
        String::new()
    } else {
        format!(" {}", m.unit)
    };
    let bound = if m.threshold == 0.0 && m.floor == 0.0 {
        "any change".to_string()
    } else {
        format!(">{:.0}% and >{:.0}{unit}", m.threshold, m.floor)
    };
    let mut out = String::new();
    let mut list = |label: &str, items: &[(&str, f64, f64, f64)]| {
        out.push_str(&format!("{name} {label} ({bound}): {}\n", items.len()));
        for (t, o, n, d) in items.iter().take(20) {
            out.push_str(&format!(
                "  {t}: {o:.prec$} -> {n:.prec$}{unit} ({d:+.1}%)\n"
            ));
        }
    };
    list("regressions", &regs);
    list("improvements", &imps);
    out
}

/// Build the improvement/regression summary comparing the previous metrics to the
/// freshly measured `rows`: an aggregate "overall change" per metric (summed over
/// shared tests), per-test outliers for **every** metric (see [`METRICS`]), and
/// added/removed tests.
fn compare_metrics(old: &HashMap<String, PerfRow>, rows: &[PerfRow]) -> String {
    let pct = |o: f64, n: f64| if o > 0.0 { (n - o) / o * 100.0 } else { 0.0 };

    let mut totals = [(0.0f64, 0.0f64); METRICS.len()];
    let mut shared = 0usize;
    // Per-metric `(test, old, new)` deltas over the shared tests, indexed like
    // `METRICS`.
    let mut deltas: Vec<Vec<(String, f64, f64)>> = vec![Vec::new(); METRICS.len()];
    let mut added: Vec<&str> = Vec::new();
    let mut present: HashSet<&str> = HashSet::new();

    for r in rows {
        present.insert(r.test.as_str());
        match old.get(&r.test) {
            Some(was) => {
                shared += 1;
                for (i, m) in METRICS.iter().enumerate() {
                    let (o, n) = ((m.read)(was), (m.read)(r));
                    totals[i].0 += o;
                    totals[i].1 += n;
                    deltas[i].push((r.test.clone(), o, n));
                }
            }
            None => added.push(&r.test),
        }
    }
    let removed: Vec<&String> = old
        .keys()
        .filter(|k| !present.contains(k.as_str()))
        .collect();

    let mut out = String::from("\n=== perfmon refresh summary ===\n");
    if shared == 0 {
        out.push_str("no previous metrics to compare against (first run).\n");
    } else {
        out.push_str(&format!("overall change across {shared} shared test(s):\n"));
        for (m, (o, n)) in METRICS.iter().zip(totals) {
            let prec = m.decimals;
            let unit = if m.unit.is_empty() {
                String::new()
            } else {
                format!(" {}", m.unit)
            };
            out.push_str(&format!(
                "  {:<16}{o:.prec$} -> {n:.prec$}{unit} ({:+.1}%)\n",
                format!("{}:", m.name),
                pct(o, n)
            ));
        }

        // Per-test outliers for every metric.
        for (m, d) in METRICS.iter().zip(&deltas) {
            out.push('\n');
            out.push_str(&metric_outliers(m, d));
        }
    }

    if !added.is_empty() {
        out.push_str(&format!("\nnew test(s): {}\n", added.len()));
        for t in &added {
            out.push_str(&format!("  + {t}\n"));
        }
    }
    if !removed.is_empty() {
        out.push_str(&format!("removed test(s): {}\n", removed.len()));
        for t in &removed {
            out.push_str(&format!("  - {t}\n"));
        }
    }
    out
}

/// The path an `Error::render` filename should show for `path` (a case
/// file): relative to the repo root and `/`-separated regardless of host
/// platform, prefixed with `./` — host-independent (no absolute filesystem
/// path), so it's safe to bake into a checked-in `--- errors ---` fixture,
/// but still a real, useful path instead of the generic old `"input"`
/// placeholder.
fn render_path(path: &Path) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let rel = path.strip_prefix(root).unwrap_or(path);
    format!("./{}", rel.to_string_lossy().replace('\\', "/"))
}

fn run_success_case(
    ctx: &str,
    orig_path: &Path,
    src_path: &Path,
    stem: &str,
    spec: &Spec,
    case_dir: &Path,
    // Where a fill run collects the sections that moved; `fills.enabled` is what
    // distinguishes fill mode from a normal validating run.
    fills: &mut Fills,
) -> Outcome {
    let program = match load_case(src_path) {
        Ok(p) => p,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: load failed:\n{}",
                Error::render_all(&e, &spec.source, &render_path(orig_path))
            ))
        }
    };
    // A *library* case has `.test` blocks but no `main`: it's exercised only
    // through `aipl check` (below), and its baseline row and `report` measure that
    // test run. Behavior sections (which observe a run's stdout/exit/files) need a
    // `main` to drive them.
    let has_main = program
        .items
        .iter()
        .any(|it| matches!(it, aipl::ast::Item::Fn(f) if f.name == "main"));
    if !has_main
        && (spec.stdout.is_some()
            || spec.stderr.is_some()
            || spec.exit_code.is_some()
            || !spec.cli.is_empty()
            || !spec.expect_files.is_empty())
    {
        return Outcome::Fail(format!(
            "{ctx}: a case with no `main` runs only its `.test` blocks — it can't have a \
             stdout/stderr/exit/cli/expect-file section"
        ));
    }
    // The object whose `binary size` and perf are measured: the case's own program
    // when it has a `main`; otherwise the synthesized `.test` driver. So a
    // library's perf section measures the same test run `aipl check` verifies.
    let measured = perf::measured_program(&program);
    // The object used for behavior checks (and the basis for `aipl build`) is
    // *not* instrumented — production runs carry no instruction-counter overhead.
    // The measurement builds its own instrumented object below.
    let obj_comp = match ObjectCompilation::new(&measured, stem, debug_opts(), false) {
        Ok(c) => c,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: compile failed:\n{}",
                Error::render_all(&e, &spec.source, &render_path(orig_path))
            ))
        }
    };
    // What the object's symbols are called in AIPL, for the per-function `code`
    // breakdown in the `report`. Read before `emit` consumes `obj_comp`.
    let symbol_names = obj_comp.code_symbol_names();
    let obj_bytes = match obj_comp.emit() {
        Ok(b) => b,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: emit failed:\n{}",
                e.render(&spec.source, &render_path(orig_path))
            ))
        }
    };

    // Behavior (stdout/stderr/exit + written files) needs a real `main`. A library
    // skips straight to its `.test` blocks below.
    if has_main {
        let exe_path = case_dir.join(binary::default_exe_name(stem));
        if let Err(e) = binary::link(&obj_bytes, &exe_path) {
            return Outcome::Fail(format!(
                "{ctx}: link failed:\n{}",
                e.render(&spec.source, &render_path(orig_path))
            ));
        }

        // Run in the case's staging dir so relative file reads (e.g.
        // `read_file_to_string("data.txt")`) resolve against its `file:` companions.
        let output = match Command::new(&exe_path)
            .args(&spec.cli)
            .current_dir(case_dir)
            .output()
        {
            Ok(o) => o,
            Err(e) => return Outcome::Fail(format!("{ctx}: spawn failed: {e}")),
        };

        // Strip trailing whitespace/newlines so a single-line expected like
        // "hello world" matches a child that wrote "hello world\n". Also
        // collapse CRLF to LF — on Windows, libc text-mode stdout translates
        // `\n` from the runtime into `\r\n` at the pipe.
        let stdout = normalize_output(&String::from_utf8_lossy(&output.stdout));
        let stderr = normalize_output(&String::from_utf8_lossy(&output.stderr));
        let exit = output.status.code().unwrap_or(-1) & 0xff;

        let exp_stdout = spec.stdout.as_deref().unwrap_or("");
        let exp_stderr = spec.stderr.as_deref().unwrap_or("");
        let exp_exit = spec.exit_code.unwrap_or(0) & 0xff;

        check_section!(check_or_fill(fills, ctx, "stdout", &stdout, exp_stdout));
        check_section!(check_or_fill(fills, ctx, "stderr", &stderr, exp_stderr));
        check_section!(check_or_fill(
            fills,
            ctx,
            "exit code",
            &exit.to_string(),
            &exp_exit.to_string(),
        ));

        // Validate any files the program was expected to *write* (e.g. via
        // `write_string_to_file`). Checked after stdout/exit so a file mismatch
        // never masks a behavioral regression. The behavior run above wrote them
        // into `case_dir`.
        for (rel, expected) in &spec.expect_files {
            let actual = match fs::read_to_string(case_dir.join(rel)) {
                Ok(c) => normalize_output(&c),
                Err(e) => {
                    return Outcome::Fail(format!(
                        "{ctx}: expected output file {rel:?} was not written: {e}"
                    ))
                }
            };
            check_section!(check_or_fill(
                fills,
                ctx,
                &format!("expect file: {rel}"),
                &actual,
                expected,
            ));
        }
    } // end `if has_main` (behavior run)

    // Run the case's in-language `.test({ .. })` blocks via the real `aipl
    // check` binary — at most once, whichever of its two sections are in play
    // (see `check_plan`). A subprocess (not in-process) because the test runner
    // keeps process-global pass/fail counters — a fresh process gives each case
    // its own clean state and avoids races between parallel shards.
    //
    // Pointed at the *original* case file, not the staged copy: `check` stages
    // each file's `--- file: ---` companions into a scratch directory and runs
    // the tests there (so writes never land in the tree), and it can only read
    // those sections from the unstripped source. That keeps one staging
    // implementation — `check`'s — rather than a second one here.
    if let Some(plan) = check_plan(spec) {
        let output = match Command::new(env!("CARGO_BIN_EXE_aipl"))
            .arg("check")
            // The harness validates the sections itself (and, in fill mode,
            // against the file as it was before this run rewrites it), so this
            // run is for the `.test` blocks alone.
            .arg("--ignore-sections")
            .arg(orig_path)
            .output()
        {
            Ok(o) => o,
            Err(e) => return Outcome::Fail(format!("{ctx}: `aipl check` spawn failed: {e}")),
        };
        let report = normalize_output(&String::from_utf8_lossy(&output.stdout));
        let exit = output.status.code().unwrap_or(-1) & 0xff;
        // A `--- check ---` section pins the report byte-for-byte and a
        // `--- check exit code ---` section pins the status, each independently —
        // that a *failing* `check` can be a documented fixture is the point of
        // both.
        if let Some(expected) = plan.report {
            check_section!(check_or_fill(fills, ctx, "check", &report, expected));
        }
        if let Some(expected) = plan.exit_code {
            check_section!(check_or_fill(
                fills,
                ctx,
                "check exit code",
                &exit.to_string(),
                &expected.to_string(),
            ));
        }
        // Neither pinned: the in-language tests must simply pass.
        if plan.require_success && !output.status.success() {
            let errs = normalize_output(&String::from_utf8_lossy(&output.stderr));
            return Outcome::Fail(format!(
                "{ctx}: `aipl check` (in-language tests) failed:\n{report}{errs}\n\
                 Run just this case's tests with `aipl check {}`,\n\
                 or through this harness with `{}`.",
                render_path(orig_path),
                scoped_run_cmd(ctx),
            ));
        }
    }

    // Allocation accounting. Correctness (above) is checked first so a perf
    // mismatch never masks a behavioral regression.
    //
    // **Every running case is measured, whether or not it pins anything**: the run
    // is what the leak gate needs, and the gate is the one check here that has
    // found real bugs, so it must not depend on a case having a section to compare.
    // (That used to be true by accident — a `performance` section was required on
    // every case but the examples.)
    // What is *compared* is the case's row in the metrics table, and its `report`
    // where it pins one: one measurement serves both.
    check_section!(run_performance_check(
        ctx,
        orig_path,
        &measured,
        stem,
        spec,
        case_dir,
        spec.report.as_deref(),
        &obj_bytes,
        &symbol_names,
        fills,
    ));
    // Every section matched, or fill mode queued the ones that didn't; a normal
    // run would have short-circuited to `Fail` before here. The caller flushes.
    Outcome::Pass
}

/// Measure the case (see [`perf::measure`]: an instrumented build, linked against
/// the instrumented runtime and run once), refuse it if it leaks, and verify, or
/// fill in, the `--- report ---` section when the case has one (`Some`), and hold
/// the counts to the case's row in the metrics table.
///
/// The leak gate does not depend on either: it runs for every case, section or no
/// section. The instrumented object is separate from the behavior-check one so
/// production/JIT runs carry no counter overhead.
#[allow(clippy::too_many_arguments)]
fn run_performance_check(
    ctx: &str,
    orig_path: &Path,
    program: &Program,
    stem: &str,
    spec: &Spec,
    case_dir: &Path,
    report: Option<&str>,
    production: &[u8],
    symbol_names: &HashMap<String, String>,
    fills: &mut Fills,
) -> Outcome {
    let actual = match perf::measure(
        program,
        stem,
        production,
        symbol_names,
        &spec.cli,
        case_dir,
        debug_opts(),
    ) {
        Ok(v) => v,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: {}",
                e.render(&spec.source, &render_path(orig_path))
            ))
        }
    };

    // Memory-leak gate: every heap allocation must be paired with a free. Checked
    // even in fill mode so `fill_expected` cannot bake in a leak, and with no
    // section to refresh there is nothing to say about counts afterwards.
    if actual.allocations != actual.deallocations {
        let then = if report.is_some() {
            format!(
                "Fix the leak, then re-run `{}` to refresh the report.",
                scoped_fill_cmd(ctx)
            )
        } else {
            "Fix the leak.".to_string()
        };
        return Outcome::Fail(format!(
            "{ctx}: memory leak — allocations ({}) != deallocations ({})\n{then}",
            actual.allocations, actual.deallocations,
        ));
    }

    let report = match report {
        // A section body has its trailing newlines stripped, and a report ends in
        // one. Compared as written: it is text a reader is meant to read.
        Some(expected) => check_or_fill(
            fills,
            ctx,
            "report",
            actual.render_report().trim_end(),
            expected,
        ),
        None => Outcome::Pass,
    };
    // The case's row in the metrics table comes last, so a stale section — which
    // a refill fixes — is what a run says first. A fill run leaves it alone: the
    // table is refreshed by `refresh_perfmon`, not by `fill_expected`.
    if fills.enabled || !matches!(report, Outcome::Pass) {
        return report;
    }
    check_baseline(ctx, &actual)
}

/// Normalizes captured child output before comparing it against the expected
/// sections. Dogfooded: the AIPL `normalize_output` (collapse CRLF to LF, then
/// strip the trailing `\n`/`\r` run), run through the embedding FFI. No native
/// fallback — see `aipl::codegen::normalize_output`.
fn normalize_output(s: &str) -> String {
    aipl::codegen::normalize_output(s)
}

/// `aipl check` runs **at most once** per case, whichever of its two sections are
/// in play — the property [`CheckPlan`] exists to make statable.
///
/// Reading "pin the report" and "pin the exit code" as two independent
/// expectations is the obvious implementation and the wrong one: it spawns the
/// compiler twice for a case that uses both, and the corpus runs `check` hundreds
/// of times. `check_plan` returns one plan or none, so there is no shape that can
/// yield two runs, and this walks every combination to show the count never
/// changes — only what that one run is compared against.
#[test]
fn check_runs_at_most_once_whichever_sections_are_used() {
    const TESTS: &str = "fn f() -> i64 { 1 }.test({ assert(true); })\n";
    const REPORT: &str = "--- check ---\n1 tests: 1 passed, 0 failed\n";
    const EXIT: &str = "--- check exit code ---\n1\n";
    // Owned, because a `CheckPlan` borrows the `Spec` it was read from: what is
    // under test is the plan's content, not its lifetime.
    let plan = |src: &str| {
        let spec = parse_spec(src);
        check_plan(&spec).map(|p| (p.report.map(str::to_string), p.exit_code, p.require_success))
    };

    // Neither section and no `.test` or `.performance` blocks: nothing to ask `check`, so it never
    // runs — which is what keeps the testless majority of the corpus free.
    for src in ["fn main() {}\n", "fn main() {}\n--- stdout ---\nhi\n"] {
        assert!(plan(src).is_none(), "should not run check: {src:?}");
    }

    // A `.performance` block is run by `check` and by nothing else, so a file with
    // only that block still needs the run: its claims are otherwise never held.
    let performance = "fn f() -> i64 { 1 }.performance({ assert(true); })\nfn main() {}\n";
    assert_eq!(plan(performance), Some((None, None, true)));

    // Every other combination is exactly one run. Two of the three reasons to run
    // are the sections themselves, so a case may pin `check` with no tests of its
    // own.
    for src in [
        TESTS.to_string(),
        format!("fn main() {{}}\n{REPORT}"),
        format!("fn main() {{}}\n{EXIT}"),
        format!("fn main() {{}}\n{REPORT}{EXIT}"),
        format!("{TESTS}{REPORT}"),
        format!("{TESTS}{EXIT}"),
        format!("{TESTS}{REPORT}{EXIT}"),
    ] {
        assert!(plan(&src).is_some(), "should run check once: {src:?}");
    }

    // What that one run is compared against, per combination.
    let pinned_report = Some("1 tests: 1 passed, 0 failed".to_string());
    // Nothing pinned, so the one expectation left is that the tests pass.
    assert_eq!(plan(TESTS), Some((None, None, true)));
    // Each section pins its own half and leaves the other unasked. Either one
    // turns `require_success` off: a fixture whose point is that `check` *fails*
    // while its tests pass could not otherwise exist.
    assert_eq!(
        plan(&format!("{TESTS}{REPORT}")),
        Some((pinned_report.clone(), None, false))
    );
    assert_eq!(
        plan(&format!("{TESTS}{EXIT}")),
        Some((None, Some(1), false))
    );
    assert_eq!(
        plan(&format!("{TESTS}{REPORT}{EXIT}")),
        Some((pinned_report, Some(1), false))
    );
}

// ---- The `cargo handoff` contract -------------------------------------------
//
// `cargo handoff` regenerates only what a failing test asks it to (see
// `handoff::protocol`), so a stale section that stopped asking would stop the
// gate rather than get refilled — a failure mode that reads as handoff being
// broken, not as a missing line here.

#[test]
fn a_stale_section_asks_for_a_refill_of_its_case() {
    let mut fills = Fills::new(false);
    let Outcome::Fail(message) =
        check_or_fill(&mut fills, "[cases/x/y.aipl]", "stdout", "new\n", "old\n")
    else {
        panic!("a differing section must fail");
    };
    let (asked, unreadable) = Fill::scan(&message);
    assert!(unreadable.is_empty(), "{unreadable:?}");
    assert_eq!(
        asked,
        vec![Fill::Section {
            case: "cases/x/y.aipl".to_string(),
            section: "stdout".to_string(),
        }]
    );
}

#[test]
fn a_moved_or_missing_baseline_row_asks_for_a_refresh() {
    let stats = PerfStats {
        allocations: 0,
        deallocations: 0,
        reallocations: 0,
        bytes_allocated: 0,
        peak_live_bytes: 0,
        instructions: 0,
        binary_size: aipl::perf::BinSizes {
            total: 0,
            code: 0,
            data: 0,
            metadata: 0,
        },
        functions: Vec::new(),
    };
    // A case with no row at all is `missing`; the table has none named like this.
    let Outcome::Fail(message) = check_baseline("[cases/no/such/case.aipl]", &stats) else {
        panic!("a case with no row must fail");
    };
    assert_eq!(
        Fill::scan(&message).0,
        vec![Fill::BaselineMissing {
            case: "cases/no/such/case.aipl".to_string()
        }]
    );
}
