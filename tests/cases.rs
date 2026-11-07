//! "Examples as tests": each file under `tests/cases/**/*.aipl` contains
//! an AIPL program followed by zero or more `--- section ---` blocks
//! describing the expected outcome.
//!
//! **`aipl check` validates the file; the harness runs it, once per case.** Every section a
//! case records (`stdout`, `stderr`, `exit code`, `errors`, `report`, `check <flags>`,
//! `expect file:`) is compared by `check`'s own code (`src/sections.rs`), so a case is held to
//! exactly what a user's project is. A failed run is the case's failure, carrying the
//! `handoff-fill:` lines `check` printed, said under the case's display path.
//! `fill_expected` is `check --fix` over the same cases.
//!
//! What the harness still holds itself, because `check` does not yet: **every running case is
//! measured** (an instrumented build, run once), refused if it leaks (allocations !=
//! deallocations), and held to its row in `tests/performance_baseline.md`.
//!
//! Section format (all lines like `--- name ---` on their own line):
//!   `--- stdout ---`, `--- stderr ---`, `--- exit code ---` — what a run of `main` prints and
//!                          returns (defaults: empty, empty, 0).
//!   `--- cli ---`        — CLI arguments for the built binary, one per
//!                          line (default: none). The program receives them
//!                          as `main`'s `str[]` parameter.
//!   `--- errors ---`     — expected compiler error output (`check --compile`). Presence of
//!                          this section means the program is expected
//!                          to fail to compile; mutually exclusive with the sections that
//!                          need a program.
//!   `--- report ---`     — expected `aipl report` for the case, as that command prints
//!                          it. Optional, and for the properties the function
//!                          list shows — an instance that was not minted, a call
//!                          inlined away. Mutually exclusive with `errors`.
//!                          (A case's counts are its row in `tests/performance_baseline.md`,
//!                          and the properties an optimization is *for* are stated
//!                          by a `.benchmark` block or a `--- report ---`.)
//!   `--- check --tests ---` and the other `check <flags>` sections — what `aipl check` prints
//!                          for those parts, byte-for-byte; a *failing* test can be a
//!                          documented fixture. `--- check ---` and `--- check exit code ---`
//!                          are the old spellings of the tests part and its status.
//!
//! A case need not define `main`. A *library* case has `.test` blocks but no
//! `main`: its baseline row measures the synthesized `.test` driver.
//! Such a case can't have a `stdout`/`stderr`/`exit code`/`cli`/`expect file`
//! section, since those observe a `main`-driven run.
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
//!   - `cargo test --test cases -- --ignored fill_expected` — `aipl check --fix`
//!     over the cases: every section that differs is overwritten with the
//!     actual output. Set `AIPL_CASE` to a path substring — or a comma-separated
//!     list of them — to target a subset; this is the one mode that still reads
//!     it, since it is a single test and libtest's filter can't reach inside it.
//!     Every entry must match at least one case, so a mistyped path in a batch
//!     fails rather than silently refreshing nothing.
//!   - `cargo test --test cases -- --ignored refresh_perfmon` — rewrite the
//!     `tests/performance_baseline.md` table: the deterministic columns every
//!     case's run is held to (below), and the non-deterministic ones that only
//!     track trends. `AIPL_CASE` scopes it the same way, to the matching rows.
//!   - `cargo test --test cases -- --ignored fill_case_tests` — regenerate the
//!     per-case `#[test]` list after adding or removing a case file.
//!
//! **Every running case has a row in `tests/performance_baseline.md`**, and its run
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
use aipl::codegen::ObjectCompilation;
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
/// The command that refreshes the trends table ([`refresh_perfmon`]).
const PERFMON_CMD: &str = "cargo test --test cases -- --ignored refresh_perfmon";
/// The command that refreshes the baseline table ([`refresh_baseline`]).
const BASELINE_CMD: &str = "cargo test --test cases -- --ignored refresh_baseline";

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
    errors: Option<String>,
    /// Whether the case pins an `aipl report`, which `check` validates.
    report: Option<String>,
    /// CLI arguments passed to the built binary, one per line of the
    /// `--- cli ---` section. Reaches the program as `main`'s `str[]`.
    cli: Vec<String>,
}

/// Parse a case file into its [`Spec`]: only what the harness itself still reads. Every
/// expectation in the file is `aipl check`'s to validate.
///
/// Dogfooded: the sectioning and body trimming live in the AIPL `parse_spec`
/// (`crates/aipl-codegen/src/parse_spec.aipl`), reached through the FFI.
///
/// A malformed case (unknown section, non-integer `exit code`, unusable
/// companion path) panics: a case whose markup is wrong should stop the run,
/// not silently lose an expectation.
fn parse_spec(contents: &str) -> Spec {
    let f = aipl::parse_spec(contents).unwrap_or_else(|e| panic!("{e}"));
    Spec {
        source: f.source,
        extra_files: f.extra_files,
        errors: f.errors,
        report: f.report,
        cli: f.cli,
    }
}

/// The result of running a single case.
///
/// `Filled` mirrors `Fail`: it carries a message and represents a non-`Pass`
/// outcome, but instead of a validation failure it means fill mode (the
/// `fill_expected` helper) had `aipl check --fix` rewrite the case.
enum Outcome {
    Pass,
    Filled(String),
    Fail(String),
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
/// `tests/performance_trends.md`, printing an improvement/regression summary.
/// Nothing asserts on that file and nothing but this refresh writes it, so it
/// records the metrics at the moments someone chose to look. Run with:
///   cargo test --test cases -- --ignored refresh_perfmon
#[test]
#[ignore = "perfmon refresh — run: cargo test --test cases -- --ignored refresh_perfmon"]
fn refresh_perfmon() {
    on_big_stack(run_perfmon);
}

/// Baseline refresh: measure the deterministic metrics (allocations, bytes, peak
/// live bytes, instructions, code bytes) of every runnable case and rewrite
/// `tests/performance_baseline.md`, the table every case's run is held to.
/// `cargo handoff` runs it, scoped to the cases whose row moved or is missing.
/// Run with:
///   cargo test --test cases -- --ignored refresh_baseline
#[test]
#[ignore = "baseline refresh — run: cargo test --test cases -- --ignored refresh_baseline"]
fn refresh_baseline() {
    on_big_stack(run_baseline);
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

fn run_baseline() {
    let (cases, out_root) = setup_cases();
    run_baseline_refresh(&cases, &out_root);
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

/// What a failed `aipl check` run for a case says, as the case's failure.
///
/// `check` names the file by its path from the project root and asks for a refill with a
/// `handoff-fill:` line carrying it; the harness's own name for the case (`cases/..`) is what
/// `cargo handoff` and `AIPL_CASE` know it by, so each such line is said again under that name.
fn check_failure(ctx: &str, stdout: &str, stderr: &str) -> Outcome {
    let (asked, _) = Fill::scan(stdout);
    let report: String = stdout
        .lines()
        .filter(|line| Fill::parse(line).is_none())
        .map(|line| format!("{line}\n"))
        .collect();
    let mut markers = String::new();
    for fill in asked {
        if let Fill::Section { section, .. } = fill {
            markers.push_str(&format!(
                "{}\n",
                Fill::Section {
                    case: case_of(ctx),
                    section,
                }
                .marker()
            ));
        }
    }
    let how = if markers.is_empty() {
        String::new()
    } else {
        format!(
            "If this change is intended, run `{}`.\n",
            scoped_fill_cmd(ctx)
        )
    };
    Outcome::Fail(format!(
        "{ctx}: `aipl check` failed:\n{report}{stderr}\nRun just this case with `aipl check {}`,\n\
         or through this harness with `{}`.\n{how}{markers}",
        case_of(ctx),
        scoped_run_cmd(ctx),
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

/// Whether a failure's only complaint is a baseline row (see [`check_baseline`]): it prints
/// a [`Fill`] for each baseline problem and nothing else asks for a refill.
fn asks_only_for_baseline(failure: &str) -> bool {
    let fills = Fill::scan(failure).0;
    !fills.is_empty()
        && fills
            .iter()
            .all(|f| matches!(f, Fill::BaselineMoved { .. } | Fill::BaselineMissing { .. }))
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
            // A missing or moved baseline row is not stale text for this helper to
            // rewrite, and not a broken case either: `cargo handoff` settles it
            // with `refresh_baseline` once everything else has. A new case reaches
            // this check when it has nothing to fill, and must not be reported as a
            // failure no refill can fix.
            Outcome::Fail(msg) if asks_only_for_baseline(&msg) => passed += 1,
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

/// Run one case: `aipl check` validates the file, and the harness holds what `check` does not
/// yet — the case's row in the baseline table, and the leak gate on every running case.
///
/// `check` is run the way a person at the project root runs it, so everything a case records
/// (`errors`, `check`, `stdout`, `stderr`, `exit code`, `report`, `expect file:`) is validated by
/// the one implementation users have. In fill mode the same run is `check --fix`, which rewrites
/// what a rewrite repairs.
fn run_case(path: &Path, rel: &Path, out_root: &Path, stage_to_temp: bool, fill: bool) -> Outcome {
    let contents = fs::read_to_string(path).expect("read test case");
    let spec = parse_spec(&contents);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap();
    let ctx = format!("[{}]", rel.display());

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut check = Command::new(env!("CARGO_BIN_EXE_aipl"));
    // From the project root, the case named relative to it: `check` prints the file's name, and a
    // section that pinned this machine's path would fail on every other checkout.
    check.current_dir(root).arg("check");
    if fill {
        check.arg("--fix");
    }
    let output = match check.arg(path.strip_prefix(root).unwrap_or(path)).output() {
        Ok(o) => o,
        Err(e) => return Outcome::Fail(format!("{ctx}: `aipl check` spawn failed: {e}")),
    };
    let stdout = normalize_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = normalize_output(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return check_failure(&ctx, &stdout, &stderr);
    }
    if fill {
        let repaired: Vec<&str> = stdout
            .lines()
            .filter(|line| line.starts_with("fixed ") || line.starts_with("formatted "))
            .collect();
        if !repaired.is_empty() {
            return Outcome::Filled(format!("[{}]: {}", path.display(), repaired.join("; ")));
        }
    }
    // A case that must not compile has no program to measure.
    if spec.errors.is_some() {
        return Outcome::Pass;
    }

    // Two modes for staging:
    //   - tests/cases/, or any file with `file:` sections (as `check` does): stage
    //     source + companions into a per-case temp dir, so the case is self-contained.
    //   - the rest (examples/, crates/): load in place, so user-facing examples can
    //     `import` real companion files (e.g. examples/math/geometry.aipl).
    let (src_path, case_dir) = if stage_to_temp || !spec.extra_files.is_empty() {
        stage_case(path, rel, out_root, stem, &spec)
    } else {
        // Output executable still lands in a per-case temp dir so we
        // don't litter the source tree.
        let dir = out_root.join(rel.with_extension(""));
        fs::create_dir_all(&dir).expect("mkdir case output");
        (path.to_path_buf(), dir)
    };
    measure_case(&ctx, path, &src_path, stem, &spec, &case_dir)
}

/// The directory a staged case's project root is: the staging directory holds the case at
/// the path it has in the repository, under this name.
const STAGED_ROOT: &str = "case-root";

/// Stage a `tests/cases/` case: its section-stripped source and its `--- file: ---`
/// companions, in a directory of its own that mirrors the case's place in the repository
/// (`<out>/<case>/case-root/tests/cases/<dir>/`). The mirroring is what lets a staged case
/// be loaded with the repository's layout — see [`load_case`]. Returns the staged source and
/// the directory it sits in, where the program runs.
fn stage_case(
    path: &Path,
    rel: &Path,
    out_root: &Path,
    stem: &str,
    spec: &Spec,
) -> (PathBuf, PathBuf) {
    let case_out = out_root.join(rel.with_extension(""));
    let _ = fs::remove_dir_all(&case_out);
    let in_repo = path
        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .expect("a case lies under the repository");
    let dir = case_out
        .join(STAGED_ROOT)
        .join(in_repo.parent().unwrap_or(Path::new("")));
    fs::create_dir_all(&dir).expect("mkdir case staging");
    let src = dir.join(format!("{stem}.aipl"));
    fs::write(&src, &spec.source).expect("write staged source");
    aipl::stage_companions(&dir, &spec.extra_files).expect("stage companions");
    (src, dir)
}

/// Load a case, rooted at the repository.
///
/// The repository is the project a case belongs to (DESIGN_PRINCIPLES.md §7), so a case's
/// file is named `./tests/cases/<dir>/<case>.aipl` — what `aipl check` calls it, and what a
/// `trace(..)` or a diagnostic in it says. The baked label is in the program's data section,
/// so it has to be the same wherever the staging directory sits (`CARGO_TARGET_TMPDIR` moves
/// with `CARGO_TARGET_DIR`): a staged case is rooted at the directory [`stage_case`] built
/// the repository's layout under, and a case loaded in place (`examples/`) at the repository.
fn load_case(src_path: &Path) -> Result<Program, Vec<Error>> {
    // The staging directory is itself under the repository's `target/`, so look for
    // the staged root before concluding the case is in place.
    let root = src_path
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == STAGED_ROOT))
        .unwrap_or(Path::new(env!("CARGO_MANIFEST_DIR")));
    loader::load_program_rooted(src_path, root, debug_opts())
}

// ---------- Perf-monitor refresh (non-deterministic metrics) ----------

/// One row of the trends table (`tests/performance_trends.md`): the metrics that differ
/// from run to run, kept only to look at how they move. Nothing asserts on them.
#[derive(Clone, PartialEq)]
struct TrendRow {
    test: String,
    wall_us: f64,
    build_ms: f64,
    peak_kb: u64,
}

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
    if stage_to_temp || !spec.extra_files.is_empty() {
        stage_case(path, rel, out_root, stem, spec)
    } else {
        let dir = out_root.join(rel.with_extension(""));
        fs::create_dir_all(&dir).expect("mkdir case output");
        (path.to_path_buf(), dir)
    }
}

/// Build a case's production (non-instrumented) binary, the one the wall-clock and
/// memory columns run. Returns its path and how long it took to build (load +
/// compile + link).
fn build_case_binary(
    src_path: &Path,
    stem: &str,
    case_dir: &Path,
) -> Result<(PathBuf, f64), String> {
    let start = Instant::now();
    let program = load_case(src_path).map_err(|e| Error::display_all(&e))?;
    let measured = perf::measured_program(&program);
    let comp = ObjectCompilation::new(&measured, stem, debug_opts(), false)
        .map_err(|e| Error::display_all(&e))?;
    let obj = comp.emit().map_err(|e| e.to_string())?;
    let exe = case_dir.join(binary::default_exe_name(stem));
    binary::link(&obj, &exe).map_err(|e| e.to_string())?;
    Ok((exe, start.elapsed().as_secs_f64() * 1000.0))
}

/// Measure a case with the instrumented build, for the baseline's columns: the same
/// measurement the case's own run takes, since `cwd` is the directory the harness
/// would have staged it in.
fn measure_case_stats(
    src_path: &Path,
    stem: &str,
    case_dir: &Path,
    cli: &[String],
) -> Result<PerfStats, String> {
    let program = load_case(src_path).map_err(|e| Error::display_all(&e))?;
    let measured = perf::measured_program(&program);
    let comp = ObjectCompilation::new(&measured, stem, debug_opts(), false)
        .map_err(|e| Error::display_all(&e))?;
    let symbol_names = comp.code_symbol_names();
    let obj = comp.emit().map_err(|e| e.to_string())?;
    perf::measure(
        &measured,
        stem,
        &obj,
        &symbol_names,
        cli,
        case_dir,
        debug_opts(),
    )
    .map_err(|e| e.render("", ""))
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

/// Where the baseline table lives, relative to the repo root: the deterministic metrics every
/// case's run is held to, rewritten whenever a change moves them.
const METRICS_FILE: &str = "tests/performance_baseline.md";
/// Where the trends table lives: the metrics that differ from run to run, rewritten only by
/// [`refresh_perfmon`].
const TRENDS_FILE: &str = "tests/performance_trends.md";

fn metrics_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(METRICS_FILE)
}

fn trends_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(TRENDS_FILE)
}

/// The key a case's table row is filed under: its path from the repository root,
/// without the `.aipl` (`tests/cases/arrays/slice`), which is the name `aipl check`
/// gives the file. A case's display path (`cases/arrays/slice`, what its `[..]`
/// context brackets) lacks the `tests/` its directory sits under.
fn row_key(display_with_extension: &str) -> String {
    let key = display_with_extension
        .trim_end_matches(".aipl")
        .replace('\\', "/");
    if key.starts_with("cases/") {
        format!("tests/{key}")
    } else {
        key
    }
}

/// What a refresh of one table produced: the rows it started from, the rows to write
/// (sorted by case), and how many cases it measured and skipped.
struct Refreshed<R> {
    old: HashMap<String, R>,
    rows: Vec<(String, R)>,
    measured: usize,
    skipped: usize,
}

/// Measure the cases `AIPL_CASE` selects (all of them when unset) with `measure`, which is
/// given a case's staged source, its name, its output directory and its spec and answers
/// `None` for a case it cannot measure. Every other row of `old` is carried over, and a
/// row whose case no longer exists goes, whatever the scope: nothing else would ever
/// remove it.
///
/// `workers` cases are measured at once. A measurement that reads the clock must pass 1,
/// because a neighbour running beside it perturbs the number; one that counts (the
/// baseline's) is the same however many run together.
fn refresh_rows<R: Clone + Send>(
    cases: &[CaseFile],
    out_root: &Path,
    old: HashMap<String, R>,
    workers: usize,
    measure: impl Fn(&Path, &str, &Path, &Spec) -> Option<R> + Sync,
) -> Refreshed<R> {
    let mut filter = CaseFilter::from_env();
    let mut kept = old.clone();
    let mut existing: HashSet<String> = HashSet::new();
    let mut skipped = 0usize;
    // The cases to measure, staged here one after another (it only copies files), so that
    // the workers share nothing but the list.
    let mut jobs: Vec<(String, PathBuf, String, PathBuf, Spec)> = Vec::new();
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
        // Error cases never build a binary, so there's nothing to measure.
        if spec.errors.is_some() {
            skipped += 1;
            continue;
        }
        let stem = case_path.file_stem().and_then(|s| s.to_str()).unwrap();
        let (src_path, case_dir) =
            stage_case_for_build(case_path, &rel_with_prefix, out_root, stem, &spec, *stage);
        jobs.push((display, src_path, stem.to_string(), case_dir, spec));
    }
    filter.assert_all_matched(cases.len());

    // Each worker takes the next unmeasured job until none is left, on the big stack the
    // case tests use (compiling recurses deeply).
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<Option<R>>> =
        std::sync::Mutex::new((0..jobs.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.clamp(1, jobs.len().max(1)))
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(256 * 1024 * 1024)
                    .spawn_scoped(scope, || loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((_, src_path, stem, case_dir, spec)) = jobs.get(i) else {
                            break;
                        };
                        let row = measure(src_path, stem, case_dir, spec);
                        results.lock().expect("results")[i] = row;
                        let finished = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if finished.is_multiple_of(25) {
                            eprintln!("measured {finished} case(s)...");
                        }
                    })
                    .expect("spawn worker")
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker panicked");
        }
    });
    let mut measured: Vec<(String, R)> = Vec::new();
    for ((display, ..), row) in jobs.into_iter().zip(results.into_inner().expect("results")) {
        match row {
            Some(row) => measured.push((display, row)),
            None => skipped += 1,
        }
    }

    kept.retain(|test, _| existing.contains(test));
    let count = measured.len();
    let mut rows: Vec<(String, R)> = kept.into_iter().chain(measured).collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    Refreshed {
        old,
        rows,
        measured: count,
        skipped,
    }
}

/// Rewrite the trends table: wall-clock, build time and peak memory of the production
/// build of every selected case.
fn run_perfmon_refresh(cases: &[CaseFile], out_root: &Path) {
    let path = trends_path();
    let old = read_trends(&path);
    let refreshed = refresh_rows(cases, out_root, old, 1, |src_path, stem, case_dir, spec| {
        let (exe, build_ms) = build_case_binary(src_path, stem, case_dir).ok()?;
        let stats_path = case_dir.join("perfmon_stats.txt");
        let (mut wall_ns, mut peak_bytes) = (u64::MAX, u64::MAX);
        for _ in 0..PERFMON_RUNS {
            let (w, p) = run_once_perfmon(&exe, &spec.cli, case_dir, &stats_path)?;
            wall_ns = wall_ns.min(w);
            peak_bytes = peak_bytes.min(p);
        }
        Some(TrendRow {
            test: String::new(),
            wall_us: wall_ns as f64 / 1000.0,
            build_ms,
            peak_kb: peak_bytes.div_ceil(1024),
        })
    });
    let rows: Vec<TrendRow> = refreshed
        .rows
        .into_iter()
        .map(|(test, row)| TrendRow { test, ..row })
        .collect();
    fs::write(&path, render_trends(&rows)).expect("write trends file");
    eprintln!("{}", compare_trends(&refreshed.old, &rows));

    // Diverge so the summary is visible (cargo hides passing tests' output) and a
    // refresh is never mistaken for a normal green run.
    panic!(
        "perfmon refresh complete: wrote {} ({} measured, {} skipped, {} rows). Failing \
         intentionally so the summary above is visible — this is not a normal test \
         run (`{PERFMON_CMD}`).",
        path.display(),
        refreshed.measured,
        refreshed.skipped,
        rows.len(),
    );
}

/// Rewrite the baseline table: the deterministic metrics of the instrumented build of every
/// selected case. No wall-clock runs, so it is the cheap half of the old refresh.
fn run_baseline_refresh(cases: &[CaseFile], out_root: &Path) {
    let path = metrics_path();
    let old = perf::read_baseline(&path).unwrap_or_default();
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let refreshed = refresh_rows(
        cases,
        out_root,
        old,
        workers,
        |src_path, stem, case_dir, spec| {
            let stats = measure_case_stats(src_path, stem, case_dir, &spec.cli).ok()?;
            Some(perf::baseline_values(&stats))
        },
    );
    fs::write(&path, render_baseline(&refreshed.rows)).expect("write baseline file");
    eprintln!("{}", compare_baseline(&refreshed.old, &refreshed.rows));
    panic!(
        "baseline refresh complete: wrote {} ({} measured, {} skipped, {} rows). Failing \
         intentionally so the summary above is visible — this is not a normal test \
         run (`{BASELINE_CMD}`).",
        path.display(),
        refreshed.measured,
        refreshed.skipped,
        refreshed.rows.len(),
    );
}

/// Render the baseline table (Markdown, rows already sorted by test path).
fn render_baseline(rows: &[(String, [u64; 5])]) -> String {
    let mut s = String::new();
    s.push_str("# Performance baseline\n\n");
    s.push_str(
        "One row per case that runs: allocations, bytes allocated, peak live bytes,\n\
         instructions and code bytes. These are deterministic, measured by the\n\
         instrumented build, and every case's run must reproduce its row exactly: a case\n\
         whose numbers moved fails with `performance baseline moved`, and a case with no\n\
         row fails too. `cargo handoff` rewrites the rows that moved, and lists them for\n\
         review; the failure names the scoped refresh that accepts a change by hand.\n\n\
         The metrics that differ from run to run are in `performance_trends.md`.\n\n\
         `AIPL_CASE` (a path substring, or a comma-separated list) rewrites only the\n\
         matching rows; rows whose case no longer exists are always dropped.\n\n\
         ```\n\
         cargo test --test cases -- --ignored refresh_baseline\n\
         ```\n\n",
    );
    s.push_str(&format!(
        "| Test | {} |\n",
        perf::BASELINE_METRICS.join(" | ")
    ));
    s.push_str("| --- | ---: | ---: | ---: | ---: | ---: |\n");
    for (test, v) in rows {
        s.push_str(&format!(
            "| {test} | {} | {} | {} | {} | {} |\n",
            v[0], v[1], v[2], v[3], v[4]
        ));
    }
    s
}

/// Render the trends table (Markdown, rows already sorted by test path).
fn render_trends(rows: &[TrendRow]) -> String {
    let mut s = String::new();
    s.push_str("# Performance trends\n\n");
    s.push_str(
        "One row per case that runs: wall-clock (measured in-process, so process\n\
         spawn/teardown is excluded), build time (load + compile + link) and peak resident\n\
         memory. These drift run-to-run, so nothing asserts on them, and nothing but the\n\
         refresh below writes this file: it holds the numbers as of the last time someone\n\
         chose to look. Run it before and after a change to see its effect, or now and\n\
         then to see the trend, and read the printed summary.\n\n\
         The deterministic metrics every case is held to are in `performance_baseline.md`.\n\n\
         `AIPL_CASE` (a path substring, or a comma-separated list) rewrites only the\n\
         matching rows; rows whose case no longer exists are always dropped.\n\n\
         ```\n\
         cargo test --test cases -- --ignored refresh_perfmon\n\
         ```\n\n",
    );
    s.push_str("| Test | wall-clock (µs) | build (ms) | peak RSS (KB) |\n");
    s.push_str("| --- | ---: | ---: | ---: |\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {:.2} | {:.2} | {} |\n",
            r.test, r.wall_us, r.build_ms, r.peak_kb
        ));
    }
    s
}

/// Parse a previously-written trends file into `test -> row`. Returns empty on a
/// missing/unreadable file (first run).
fn read_trends(path: &Path) -> HashMap<String, TrendRow> {
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
        if cols.len() != 4 {
            continue;
        }
        if let (Ok(wall_us), Ok(build_ms), Ok(peak_kb)) = (
            cols[1].parse::<f64>(),
            cols[2].parse::<f64>(),
            cols[3].parse::<u64>(),
        ) {
            map.insert(
                cols[0].to_string(),
                TrendRow {
                    test: cols[0].to_string(),
                    wall_us,
                    build_ms,
                    peak_kb,
                },
            );
        }
    }
    map
}

/// The checked-in baseline table, read once.
fn baselines() -> &'static HashMap<String, [u64; 5]> {
    static BASELINES: std::sync::OnceLock<HashMap<String, [u64; 5]>> = std::sync::OnceLock::new();
    BASELINES.get_or_init(|| perf::read_baseline(&metrics_path()).unwrap_or_default())
}

/// Hold a case's measurement to its row in the baseline table: every column must
/// match exactly. (Exact for now; thresholds are undecided.)
///
/// The two failures are told apart by the [`Fill`] each prints, a contract with
/// `cargo handoff`: it runs everything else to the end and then settles them with
/// one `refresh_baseline`, rather than stopping at the first.
fn check_baseline(ctx: &str, stats: &PerfStats) -> Outcome {
    let key = row_key(ctx.trim_start_matches('[').trim_end_matches(']'));
    let accept = format!("AIPL_CASE='{key}.aipl' {BASELINE_CMD}");
    let Some(row) = baselines().get(&key) else {
        return Outcome::Fail(format!(
            "{ctx}: performance baseline missing: {METRICS_FILE} has no row for this case.\n\
             Add it with `{accept}`.\n{}",
            Fill::BaselineMissing { case: case_of(ctx) }.marker()
        ));
    };
    let moved = perf::baseline_moves(*row, stats);
    if moved.is_empty() {
        return Outcome::Pass;
    }
    Outcome::Fail(format!(
        "{ctx}: performance baseline moved ({METRICS_FILE}):\n{}\n\
         If this change is intended, accept it with `{accept}`.\n{}",
        moved
            .iter()
            .map(|m| format!("  {m}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Fill::BaselineMoved {
            case: case_of(ctx),
            moves: moved.clone(),
        }
        .marker()
    ))
}

/// One trend metric's per-test outlier rules: only flag a change that is both
/// relatively large (`> threshold` percent) and absolutely non-trivial (`> floor`
/// in the metric's unit), so normal run-to-run jitter doesn't swamp the lists.
/// These programs run in (sub-)microseconds and link in tens of milliseconds,
/// where tiny absolute jitter is a huge *percentage*; the floors filter that out.
/// The overall aggregate (below) still reflects every shared test regardless.
struct Metric {
    name: &'static str,
    unit: &'static str,
    /// Decimal places when printing values.
    decimals: usize,
    threshold: f64,
    floor: f64,
    read: fn(&TrendRow) -> f64,
}

/// Every column of the trends table, in its order.
const METRICS: [Metric; 3] = [
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

/// Build the improvement/regression summary comparing the previous trends to the
/// freshly measured `rows`: an aggregate "overall change" per metric (summed over
/// shared tests), per-test outliers for **every** metric (see [`METRICS`]), and
/// added/removed tests.
fn compare_trends(old: &HashMap<String, TrendRow>, rows: &[TrendRow]) -> String {
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

/// The summary of a baseline refresh: each column's total over the shared cases before and
/// after, then every case whose row moved (the first twenty) and the cases added or removed.
fn compare_baseline(old: &HashMap<String, [u64; 5]>, rows: &[(String, [u64; 5])]) -> String {
    let pct = |o: u64, n: u64| {
        if o > 0 {
            (n as f64 - o as f64) / o as f64 * 100.0
        } else {
            0.0
        }
    };
    let mut totals = [(0u64, 0u64); 5];
    let mut moved: Vec<String> = Vec::new();
    let mut added: Vec<&str> = Vec::new();
    for (test, now) in rows {
        let Some(was) = old.get(test) else {
            added.push(test);
            continue;
        };
        for (total, (o, n)) in totals.iter_mut().zip(was.iter().zip(now)) {
            total.0 += o;
            total.1 += n;
        }
        if was != now {
            let columns: Vec<String> = perf::BASELINE_METRICS
                .iter()
                .zip(was.iter().zip(now))
                .filter(|(_, (o, n))| o != n)
                .map(|(name, (o, n))| format!("{name}: {o} -> {n} ({:+.1}%)", pct(*o, *n)))
                .collect();
            moved.push(format!("  {test}\n      {}", columns.join("\n      ")));
        }
    }
    let present: HashSet<&str> = rows.iter().map(|(t, _)| t.as_str()).collect();
    let removed: Vec<&String> = old
        .keys()
        .filter(|k| !present.contains(k.as_str()))
        .collect();

    let mut out = String::from("\n=== baseline refresh summary ===\n");
    out.push_str("overall change across the shared case(s):\n");
    for (name, (o, n)) in perf::BASELINE_METRICS.iter().zip(totals) {
        out.push_str(&format!(
            "  {:<16}{o} -> {n} ({:+.1}%)\n",
            format!("{name}:"),
            pct(o, n)
        ));
    }
    out.push_str(&format!("\nmoved case(s): {}\n", moved.len()));
    for m in moved.iter().take(20) {
        out.push_str(m);
        out.push('\n');
    }
    if !added.is_empty() {
        out.push_str(&format!("\nnew case(s): {}\n", added.len()));
        for t in &added {
            out.push_str(&format!("  + {t}\n"));
        }
    }
    if !removed.is_empty() {
        out.push_str(&format!("removed case(s): {}\n", removed.len()));
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

/// Measure a case that runs (see [`perf::measure`]: an instrumented build, linked against the
/// instrumented runtime and run once), refuse it if it leaks, and hold the counts to the case's
/// row in the metrics table.
///
/// **Every running case is measured, whether or not it pins anything**: the run is what the leak
/// gate needs, and the gate is the one check here that has found real bugs, so it must not depend
/// on a case having a section to compare. For a case with a `main` the measured program is that
/// `main`; for a library it is the synthesized `.test` driver, so its row measures the test run.
fn measure_case(
    ctx: &str,
    orig_path: &Path,
    src_path: &Path,
    stem: &str,
    spec: &Spec,
    case_dir: &Path,
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
    let measured = perf::measured_program(&program);
    // The production object: the baseline's code size is read off it, and the measurement builds
    // its own instrumented one.
    let obj_comp = match ObjectCompilation::new(&measured, stem, debug_opts(), false) {
        Ok(c) => c,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: compile failed:\n{}",
                Error::render_all(&e, &spec.source, &render_path(orig_path))
            ))
        }
    };
    // What the object's symbols are called in AIPL, for the per-function `code` breakdown. Read
    // before `emit` consumes `obj_comp`.
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
    let actual = match perf::measure(
        &measured,
        stem,
        &obj_bytes,
        &symbol_names,
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

    // Memory-leak gate: every heap allocation must be paired with a free.
    if actual.allocations != actual.deallocations {
        return Outcome::Fail(format!(
            "{ctx}: memory leak — allocations ({}) != deallocations ({})\nFix the leak.",
            actual.allocations, actual.deallocations,
        ));
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

// ---- The `cargo handoff` contract -------------------------------------------
//
// `cargo handoff` regenerates only what a failing test asks it to (see
// `handoff::protocol`), so a stale section that stopped asking would stop the
// gate rather than get refilled — a failure mode that reads as handoff being
// broken, not as a missing line here.

#[test]
fn a_stale_section_asks_for_a_refill_of_its_case() {
    // What `check` prints for a stale section: its own name for the file, and the line that asks.
    let stdout = format!(
        "./tests/cases/x/y.aipl: `stdout` mismatch\n{}\n",
        Fill::Section {
            case: "tests/cases/x/y.aipl".to_string(),
            section: "stdout".to_string(),
        }
        .marker()
    );
    let Outcome::Fail(message) = check_failure("[cases/x/y.aipl]", &stdout, "") else {
        panic!("a failed check must fail the case");
    };
    let (asked, unreadable) = Fill::scan(&message);
    assert!(unreadable.is_empty(), "{unreadable:?}");
    // The case is named the way `cargo handoff` and `AIPL_CASE` know it.
    assert_eq!(
        asked,
        vec![Fill::Section {
            case: "cases/x/y.aipl".to_string(),
            section: "stdout".to_string(),
        }]
    );
}

#[test]
fn a_failed_check_with_nothing_to_refill_asks_for_nothing() {
    let Outcome::Fail(message) =
        check_failure("[cases/x/y.aipl]", "./x.aipl: the tests leaked\n", "")
    else {
        panic!("a failed check must fail the case");
    };
    assert_eq!(Fill::scan(&message).0, vec![]);
    assert!(message.contains("the tests leaked"), "{message}");
}

#[test]
fn a_baseline_failure_alone_is_not_a_failed_fill() {
    let missing = Fill::BaselineMissing {
        case: "cases/x/y.aipl".to_string(),
    };
    assert!(asks_only_for_baseline(&format!(
        "[cases/x/y.aipl]: performance baseline missing\n{}",
        missing.marker()
    )));
    // A failure that asks for nothing, or for more than a row, is still a failure.
    assert!(!asks_only_for_baseline(
        "[cases/x/y.aipl]: the tests leaked"
    ));
    let section = Fill::Section {
        case: "cases/x/y.aipl".to_string(),
        section: "stdout".to_string(),
    };
    assert!(!asks_only_for_baseline(&format!(
        "{}\n{}",
        missing.marker(),
        section.marker()
    )));
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
