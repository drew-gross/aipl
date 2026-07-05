//! "Examples as tests": each file under `tests/cases/**/*.aipl` contains
//! an AIPL program followed by zero or more `--- section ---` blocks
//! describing the expected outcome. The harness builds + links + runs
//! the program (via the same pipeline as `aipl build`) and compares.
//!
//! Section format (all lines like `--- name ---` on their own line):
//!   `--- stdout ---`     — expected stdout (default: empty). A body of `?`
//!                          fills in the actual output (see the `fill_expected`
//!                          helper below).
//!   `--- stderr ---`     — expected stderr (default: empty)
//!   `--- exit code ---`  — expected exit code (default: 0)
//!   `--- cli ---`        — CLI arguments for the built binary, one per
//!                          line (default: none). The program receives them
//!                          as `main`'s `str[]` parameter.
//!   `--- errors ---`     — expected compiler error output. Presence of
//!                          this section means the program is expected
//!                          to fail to compile; mutually exclusive with
//!                          stdout/stderr/exit code.
//!   `--- performance ---`— expected accounting for a successful run, six
//!                          required lines: `allocations: N`,
//!                          `deallocations: M`, `reallocations: K`,
//!                          `bytes allocated: B`, `instructions executed: I`
//!                          (CLIF instructions executed), and `binary size: S`
//!                          (bytes of the compiler-emitted object code). The
//!                          harness builds a separate instrumented object, links
//!                          it against the instrumented runtime, runs it, and
//!                          checks the tallies (binary size is measured from the
//!                          non-instrumented object). A body of `?` fills in the
//!                          measured values (see the `fill_expected` helper below).
//!                          Mutually exclusive with `errors`. REQUIRED on every
//!                          running case under `tests/cases/` (the user-facing
//!                          `examples/` are exempt); a success case without one
//!                          fails. For a *library* case (no `main`), it measures
//!                          the `.test` run instead — see below.
//!
//! A case need not define `main`. A *library* case has `.test` blocks but no
//! `main`: it's exercised only through `aipl check`, and its `--- performance ---`
//! measures that test run (the harness AOT-builds the synthesized `.test` driver).
//! Such a case can't have a `stdout`/`stderr`/`exit code`/`cli`/`expect file`
//! section, since those observe a `main`-driven run.
//!   `--- check ---`      — expected stdout of `aipl check` (the in-language
//!                          `.test` runner) for this case, byte-for-byte. Lets a
//!                          *failing* test be a documented fixture. A body of `?`
//!                          fills in the actual report (see the `fill_expected` helper).
//!                          When absent, a case with `.test` blocks must instead
//!                          pass cleanly (the harness requires `check` to exit 0).
//!                          Mutually exclusive with `errors`.
//!   `--- monomorphizations ---` — the mangled names of the function instances
//!                          monomorphization emits into the final binary, one per
//!                          line, sorted (each generic specialization / owned form
//!                          is its own instance). Lets a change in what gets
//!                          specialized show up as a diff. A `?` body fills in the
//!                          actual list. REQUIRED on every running case under
//!                          `tests/cases/` (same gate as `performance` — the
//!                          user-facing `examples/` are exempt). Mutually
//!                          exclusive with `errors`.
//!   `--- file: rel/path.aipl ---` — additional source files, staged
//!                          alongside the entry source so `import`s
//!                          resolve as written.
//!   `--- expect file: rel/path ---` — a file the program is expected to
//!                          have *written* during its run (e.g. via
//!                          `write_string_to_file`). After the run, the harness
//!                          reads it from the staging dir and compares to the
//!                          section body (trailing newlines stripped, like
//!                          stdout). A body of `?` fills in the file's actual
//!                          contents (see the `fill_expected` helper). Success cases
//!                          only (mutually exclusive with `errors`).
//!
//! The runner walks `tests/cases/` recursively so cases can be grouped
//! into subdirectories.
//!
//! For fast iteration, set `AIPL_CASE` to a path substring to run just the
//! matching case(s) and skip the rest:
//!   `AIPL_CASE=some_value cargo test --test cases`
//!   `AIPL_CASE=options/ cargo test --test cases`
//! A filtered run *always fails on purpose* — cargo hides passing tests'
//! output, and failing also ensures a forgotten filter isn't mistaken for a
//! green full suite. The output (and any real failures) is printed regardless.
//!
//! Two author-helper "refresh" modes are `#[ignore]`d tests (a normal `cargo
//! test` skips them; opt in by name):
//!   - `cargo test --test cases -- --ignored fill_expected` — overwrite every
//!     `stdout`/`performance`/`monomorphizations`/`check`/`errors`/`expect file`
//!     section with actual output. Combine with `AIPL_CASE` to target a
//!     subset.
//!   - `cargo test --test cases -- --ignored refresh_perfmon` — rewrite the
//!     non-deterministic `tests/performance_metrics.md` table.
//!
//! Each diverges (fails) when done so its summary is visible. The relevant
//! failure messages (e.g. a `performance` mismatch) name the command to run.
//!
//! Trailing newlines on each section are stripped — a section that
//! contains a single line of text is just that text, not text+`\n`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use aipl::ast::Program;
use aipl::binary;
use aipl::codegen::{Compilation, ObjectCompilation};
use aipl::loader;
use aipl::DebugOptions;

/// Author "fill" mode, set by the ignored [`fill_expected`] test and read by
/// [`fill_mode`].
static FILL_MODE: AtomicBool = AtomicBool::new(false);

/// The command that runs the ignored section-refresh helper ([`fill_expected`]).
/// Surfaced in failure messages so a stale `--- performance ---`/`errors`/`check`
/// section tells you exactly how to re-record it.
const FILL_CMD: &str = "cargo test --test cases -- --ignored fill_expected";
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
    /// A body of `?` triggers fill-in mode like `errors`.
    expect_files: Vec<(String, String)>,
    stdout: Option<String>,
    stderr: Option<String>,
    exit_code: Option<i32>,
    errors: Option<String>,
    /// Expected allocation accounting, as the raw `--- performance ---` body
    /// (e.g. `allocations: 3\ndeallocations: 3`). A body of `?` triggers
    /// fill-in mode like `errors`. `None` means the case is not perf-checked.
    performance: Option<String>,
    /// CLI arguments passed to the built binary, one per line of the
    /// `--- cli ---` section. Reaches the program as `main`'s `str[]`.
    cli: Vec<String>,
    /// Expected stdout of `aipl check` (the in-language `.test` runner) for this
    /// case, byte-for-byte. A body of `?` triggers fill-in mode like `errors`.
    /// When present, the harness compares `check`'s output against it (so a
    /// *failing* test can be a documented fixture); when absent, a case with
    /// `.test` blocks must instead pass cleanly (`check` exits 0).
    check: Option<String>,
    /// Expected list of monomorphized function instances emitted into the final
    /// binary (the `--- monomorphizations ---` body): the mangled instance names,
    /// one per line, sorted. A body of `?` triggers fill-in mode like the others.
    /// `None` means the case doesn't pin its monomorphizations.
    monomorphizations: Option<String>,
}

fn parse_spec(contents: &str) -> Spec {
    let mut spec = Spec::default();
    let mut current: Option<String> = None;
    let mut buf = String::new();
    for line in contents.lines() {
        if let Some(name) = aipl::parse_test_section_header(line) {
            finalize(&mut spec, current.as_deref(), std::mem::take(&mut buf));
            current = Some(name);
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    finalize(&mut spec, current.as_deref(), buf);
    spec
}

fn finalize(spec: &mut Spec, current: Option<&str>, buf: String) {
    let trimmed = strip_trailing_newlines(buf);
    match current {
        None => spec.source = trimmed,
        Some("stdout") => spec.stdout = Some(trimmed),
        Some("stderr") => spec.stderr = Some(trimmed),
        Some("exit code") => {
            spec.exit_code = Some(
                trimmed
                    .trim()
                    .parse()
                    .expect("exit code section must contain a single integer"),
            );
        }
        Some("errors") => spec.errors = Some(trimmed),
        Some("performance") => spec.performance = Some(trimmed),
        Some("check") => spec.check = Some(trimmed),
        Some("monomorphizations") => spec.monomorphizations = Some(trimmed),
        // One CLI argument per line; an empty section means no arguments.
        Some("cli") => spec.cli = trimmed.lines().map(str::to_string).collect(),
        Some(name) if name.starts_with("expect file:") => {
            let rel = name["expect file:".len()..].trim();
            assert!(
                !rel.is_empty(),
                "`expect file:` section needs a path: `--- expect file: path/to/it ---`"
            );
            assert!(
                !rel.contains('\\'),
                "use `/` (not `\\`) in `expect file:` section paths: {rel:?}"
            );
            spec.expect_files.push((rel.to_string(), trimmed));
        }
        Some(name) if name.starts_with("file:") => {
            let rel = name["file:".len()..].trim();
            assert!(
                !rel.is_empty(),
                "`file:` section needs a path: `--- file: path/to/it.aipl ---`"
            );
            // `\` is a section path separator nuisance on Windows
            // markup; force forward-slash from the source side and let
            // PathBuf canonicalize on disk.
            assert!(
                !rel.contains('\\'),
                "use `/` (not `\\`) in `file:` section paths: {rel:?}"
            );
            spec.extra_files.push((rel.to_string(), trimmed));
        }
        Some(other) => panic!("unknown section name {other:?}"),
    }
}

fn strip_trailing_newlines(mut s: String) -> String {
    while matches!(s.chars().last(), Some('\n') | Some('\r')) {
        s.pop();
    }
    s
}

/// The result of running a single case. `Skip` covers cases that the
/// harness intentionally didn't check (e.g. fill mode — the `fill_expected`
/// helper — or a `?` error placeholder), so they're neither failures nor passes.
enum Outcome {
    Pass,
    Skip,
    Fail(String),
}

// The full case suite is the slowest part of the dev loop, so it's split into
// `NUM_SHARDS` independent `#[test]` functions that libtest runs in parallel
// (one per worker thread). Each shard handles the cases whose index is
// `≡ shard (mod NUM_SHARDS)` — a round-robin partition that balances well
// regardless of per-directory size. A filtered (`AIPL_CASE`) or fill-mode
// run is consolidated into shard 0 alone, preserving the
// single-shot dev-iteration semantics (one summary, one intentional failure).
macro_rules! case_shards {
    ($($name:ident = $idx:literal),+ $(,)?) => {
        const NUM_SHARDS: usize = [$($idx),+].len();
        $(
            #[test]
            fn $name() {
                on_big_stack(|| run_shard($idx));
            }
        )+
    };
}

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

case_shards! {
    cases_shard_00 = 0,
    cases_shard_01 = 1,
    cases_shard_02 = 2,
    cases_shard_03 = 3,
    cases_shard_04 = 4,
    cases_shard_05 = 5,
    cases_shard_06 = 6,
    cases_shard_07 = 7,
    cases_shard_08 = 8,
    cases_shard_09 = 9,
    cases_shard_10 = 10,
    cases_shard_11 = 11,
    cases_shard_12 = 12,
    cases_shard_13 = 13,
    cases_shard_14 = 14,
    cases_shard_15 = 15,
}

// The two author-helper "refresh" modes are `#[ignore]`d tests rather than env
// vars: a normal `cargo test` skips them, and you opt in explicitly by name with
// `-- --ignored <name>`. Each diverges (fails) when done so its summary is
// visible (cargo hides passing tests' output) and it's never mistaken for a
// normal green run. The `#[ignore]` reason repeats the command for `cargo test
// -- --list`/`--ignored` output.

/// Author helper: refresh every `--- stdout ---` section whose actual output
/// differs, and every `?`-bodied `--- performance ---` / `--- errors ---` /
/// `--- check ---` / `--- expect file ---` section. Set `AIPL_CASE` to target
/// a subset. Run with:
///   cargo test --test cases -- --ignored fill_expected
#[test]
#[ignore = "author helper — run: cargo test --test cases -- --ignored fill_expected"]
fn fill_expected() {
    FILL_MODE.store(true, Ordering::Relaxed);
    // Reuses the normal run on shard 0 (fill mode consolidates onto one shard);
    // `fill_mode()` now reads `FILL_MODE` instead of an env var.
    on_big_stack(|| run_shard(0));
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
/// collect every case across `tests/cases/`, `examples/`, and `crates/`.
fn setup_cases() -> (Vec<CaseFile>, PathBuf) {
    // Raw-string cases de-dent through the dogfooded AIPL `dedent` (FFI), which
    // the parser reaches via a hook with no native fallback — install it before
    // any case is compiled in-process.
    aipl::install_parser_hooks();

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("aipl-cases");
    fs::create_dir_all(&out_root).expect("mkdir cases output");

    let cases = collect_all_cases(
        &root.join("tests").join("cases"),
        &root.join("examples"),
        &root.join("crates"),
    );
    assert!(!cases.is_empty(), "no .aipl test cases found");
    (cases, out_root)
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
///     their `.test` blocks are verified and any `--- performance ---` section is
///     asserted.
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

/// Author-helper "fill" mode: overwrite `?`-bodied sections and stdout/errors
/// with actual output. Toggled by the ignored [`fill_expected`] test.
fn fill_mode() -> bool {
    FILL_MODE.load(Ordering::Relaxed)
}

fn run_shard(shard: usize) {
    let (cases, out_root) = setup_cases();

    // Optional substring filter for fast iteration on one (or a few) cases:
    //   AIPL_CASE=some_value cargo test --test cases
    // matches against the case's display path (e.g. `cases/options/some_value`),
    // with `/` separators regardless of platform.
    let filter = std::env::var("AIPL_CASE").ok().filter(|s| !s.is_empty());
    // A filtered or fill run is a focused dev iteration: run the whole suite on
    // shard 0 (single summary / single intentional failure) and skip the rest.
    // (Only shard 0 ever fills — the ignored `fill_expected` test invokes
    // `run_shard(0)`; the parallel shards never set `FILL_MODE`.)
    let whole = filter.is_some() || fill_mode();
    if whole && shard != 0 {
        return;
    }

    // Run every case this shard owns, collecting failures rather than stopping
    // at the first, so one run surfaces all broken cases at once.
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut matched = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (i, (path, root, prefix, stage)) in cases.iter().enumerate() {
        // Round-robin partition (skipped when a focused run owns everything).
        if !whole && i % NUM_SHARDS != shard {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_with_prefix = Path::new(prefix).join(rel);
        if let Some(f) = &filter {
            let name = rel_with_prefix.to_string_lossy().replace('\\', "/");
            if !name.contains(f.as_str()) {
                continue;
            }
        }
        matched += 1;
        match run_case(path, &rel_with_prefix, &out_root, *stage) {
            Outcome::Pass => passed += 1,
            Outcome::Skip => skipped += 1,
            Outcome::Fail(msg) => failures.push(msg),
        }
    }

    // A filter that matches nothing is almost always a typo — fail loudly
    // rather than silently "passing" with zero cases run.
    if let Some(f) = &filter {
        assert!(
            matched > 0,
            "AIPL_CASE={f:?} matched no test cases (of {} total)",
            cases.len()
        );
    }

    // Summary, always printed (use `--nocapture` to see it on success).
    match &filter {
        Some(f) => eprintln!(
            "\n=== test cases [filter {f:?}]: {passed} passed, {} failed, {skipped} skipped ({matched} of {} matched) ===",
            failures.len(),
            cases.len(),
        ),
        None => eprintln!(
            "\n=== test cases [shard {shard}/{NUM_SHARDS}]: {passed} passed, {} failed, {skipped} skipped ({matched} of {} total) ===",
            failures.len(),
            cases.len(),
        ),
    }
    if !failures.is_empty() {
        eprintln!("\n=== {} FAILING CASE(S) ===", failures.len());
        for failure in &failures {
            eprintln!("\n{failure}");
        }
    }

    // A filtered run always fails on purpose. Two reasons: cargo captures the
    // output of *passing* tests, so failing is the only way to see what ran;
    // and it guards against a forgotten `AIPL_CASE` making a partial run look
    // like a green full suite.
    if let Some(f) = &filter {
        panic!(
            "AIPL_CASE filter {f:?} active: ran {matched} of {} case(s) \
             ({passed} passed, {} failed). Failing intentionally so the output \
             is visible and a forgotten filter isn't mistaken for a full pass — \
             unset AIPL_CASE to run the whole suite.",
            cases.len(),
            failures.len(),
        );
    }
    if !failures.is_empty() {
        panic!(
            "shard {shard}: {} of {matched} run test case(s) failed (see summary above)",
            failures.len(),
        );
    }
    // Fill mode diverges when clean too: cargo hides passing tests' output, so
    // failing is the only way to show what was refreshed, and it ensures a fill
    // run is never mistaken for a normal green suite.
    if fill_mode() {
        panic!(
            "section refresh complete: {skipped} refreshed section(s), {passed} \
             already-current, {matched} case(s) seen. Failing intentionally so the \
             summary above is visible — this is not a normal test run (`{FILL_CMD}`).",
        );
    }
}

fn collect_cases(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read tests/cases dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_cases(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("aipl") {
            out.push(path);
        }
    }
}

fn run_case(path: &Path, rel: &Path, out_root: &Path, stage_to_temp: bool) -> Outcome {
    let contents = fs::read_to_string(path).expect("read test case");
    let spec = parse_spec(&contents);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap();
    let ctx = format!("[{}]", rel.display());

    // Authoring helper: if the case has an errors section, re-compile and
    // write the rendered error back into the file in fill mode.
    if fill_mode() && spec.errors.is_some() {
        try_fill_expected(path, &contents, &spec);
        return Outcome::Skip;
    }

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
        for (rel_path, contents) in &spec.extra_files {
            let p = dir.join(rel_path);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir companion parent");
            }
            fs::write(&p, contents).expect("write companion source");
        }
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

    if spec.errors.is_some() {
        if spec.stdout.is_some() || spec.stderr.is_some() || spec.exit_code.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `errors` section is mutually exclusive with stdout/stderr/exit code"
            ));
        }
        if spec.performance.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `performance` section requires a running program, so it cannot coexist with `errors`"
            ));
        }
        if spec.check.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `check` section requires a compiling program, so it cannot coexist with `errors`"
            ));
        }
        if spec.monomorphizations.is_some() {
            return Outcome::Fail(format!(
                "{ctx}: `monomorphizations` section requires a compiling program, so it cannot coexist with `errors`"
            ));
        }
        if !spec.expect_files.is_empty() {
            return Outcome::Fail(format!(
                "{ctx}: `expect file:` section requires a running program, so it cannot coexist with `errors`"
            ));
        }
        run_error_case(&ctx, &src_path, &spec)
    } else {
        // A `--- performance ---` section is mandatory for every running test
        // case (but not for the user-facing `examples/`, which aren't staged).
        // Author a new case with a `?` body and run the fill helper to capture
        // the measured allocation counts.
        if stage_to_temp && spec.performance.is_none() {
            return Outcome::Fail(format!(
                "{ctx}: missing required `--- performance ---` section. Add one with a \
                 `?` body and run `{FILL_CMD}` to fill in the measured \
                 allocation/deallocation counts."
            ));
        }
        // A `--- monomorphizations ---` section is likewise mandatory for every
        // running test case (same gate as performance — examples are exempt).
        if stage_to_temp && spec.monomorphizations.is_none() {
            return Outcome::Fail(format!(
                "{ctx}: missing required `--- monomorphizations ---` section. Add one with a \
                 `?` body and run `{FILL_CMD}` to fill in the emitted instances."
            ));
        }
        run_success_case(&ctx, path, &src_path, stem, &spec, &case_dir)
    }
}

fn run_error_case(ctx: &str, src_path: &Path, spec: &Spec) -> Outcome {
    let result = loader::load_program(src_path, debug_opts())
        .and_then(|prog| Compilation::new(&prog, debug_opts()).map(|_| ()));
    let err = match result {
        Err(e) => e,
        Ok(()) => {
            return Outcome::Fail(format!(
                "{ctx}: expected an error, but compilation succeeded"
            ))
        }
    };
    let actual = err.render(&spec.source, "input");
    let expected = spec.errors.as_deref().unwrap_or("");
    // Special-case: a `--- errors ---` section whose body is literally
    // `?` prints the actual error and skips the check. Use this when
    // authoring a new error test to capture the expected output.
    if expected.trim() == "?" {
        eprintln!("=== ACTUAL ERROR for {ctx} ===\n{actual}\n===");
        return Outcome::Skip;
    }
    if actual != expected {
        return Outcome::Fail(format!(
            "{ctx}: error mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}\n",
        ));
    }
    Outcome::Pass
}

// ---------- Perf-monitor refresh (non-deterministic metrics) ----------

/// One row of the metrics table.
struct PerfRow {
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
    let dir = out_root.join(rel.with_extension(""));
    if stage_to_temp {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir case staging");
        let src = dir.join(format!("{stem}.aipl"));
        fs::write(&src, &spec.source).expect("write staged source");
        for (rel_path, contents) in &spec.extra_files {
            let p = dir.join(rel_path);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir companion parent");
            }
            fs::write(&p, contents).expect("write companion source");
        }
        (src, dir)
    } else {
        fs::create_dir_all(&dir).expect("mkdir case output");
        (path.to_path_buf(), dir)
    }
}

/// The program whose binary we build/time: the case's own program when it has a
/// `main`; otherwise the synthesized `.test` driver, with its `__test_main`
/// renamed to `main` so it's the AOT entry. So a library case (no `main`, only
/// `.test` blocks) is built and measured as the same test run `aipl check`
/// verifies — both its asserted `--- performance ---` and its non-deterministic
/// perfmon-table row time that test run, not an absent `main`.
fn measured_program(program: &Program) -> Program {
    let has_main = program
        .items
        .iter()
        .any(|it| matches!(it, aipl::ast::Item::Fn(f) if f.name == "main"));
    if has_main {
        program.clone()
    } else {
        let mut tp = aipl::codegen::build_test_program(program);
        for it in &mut tp.items {
            if let aipl::ast::Item::Fn(f) = it {
                if f.name == "__test_main" {
                    f.name = "main".to_string();
                }
            }
        }
        tp
    }
}

/// Build the production (non-instrumented) binary for a case, returning its path.
fn build_case_binary(src_path: &Path, stem: &str, case_dir: &Path) -> Result<PathBuf, String> {
    let program = loader::load_program(src_path, debug_opts()).map_err(|e| e.to_string())?;
    let measured = measured_program(&program);
    let comp =
        ObjectCompilation::new(&measured, stem, debug_opts(), false).map_err(|e| e.to_string())?;
    let obj = comp.emit().map_err(|e| e.to_string())?;
    let exe = case_dir.join(binary::default_exe_name(stem));
    binary::link(&obj, &exe).map_err(|e| e.to_string())?;
    Ok(exe)
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

fn run_perfmon_refresh(cases: &[CaseFile], out_root: &Path) {
    let mut rows: Vec<PerfRow> = Vec::new();
    let mut skipped = 0usize;
    for (path, root, prefix, stage) in cases {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_with_prefix = Path::new(prefix).join(rel);
        let display = rel_with_prefix
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let contents = fs::read_to_string(path).expect("read case");
        let spec = parse_spec(&contents);
        // Error cases never build a binary, so there's nothing to time.
        if spec.errors.is_some() {
            skipped += 1;
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap();
        let (src_path, case_dir) =
            stage_case_for_build(path, &rel_with_prefix, out_root, stem, &spec, *stage);

        let start = Instant::now();
        let exe = match build_case_binary(&src_path, stem, &case_dir) {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let build_ms = start.elapsed().as_secs_f64() * 1000.0;

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
        rows.push(PerfRow {
            test: display,
            wall_us: wall_ns as f64 / 1000.0,
            build_ms,
            peak_kb: peak_bytes.div_ceil(1024),
        });
        if rows.len().is_multiple_of(25) {
            eprintln!("measured {} case(s)...", rows.len());
        }
    }
    rows.sort_by(|a, b| a.test.cmp(&b.test));

    let metrics_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("performance_metrics.md");
    let old = read_metrics(&metrics_path);
    fs::write(&metrics_path, render_metrics(&rows)).expect("write metrics file");
    eprintln!("{}", compare_metrics(&old, &rows));

    // Diverge so the summary is visible (cargo hides passing tests' output) and a
    // refresh is never mistaken for a normal green run.
    panic!(
        "perfmon refresh complete: wrote {} ({} measured, {} skipped). Failing \
         intentionally so the summary above is visible — this is not a normal test \
         run (`{PERFMON_CMD}`).",
        metrics_path.display(),
        rows.len(),
        skipped,
    );
}

/// Render the metrics table (Markdown, rows already sorted by test path).
fn render_metrics(rows: &[PerfRow]) -> String {
    let mut s = String::new();
    s.push_str("# Performance metrics (non-deterministic)\n\n");
    s.push_str(
        "Per-test wall-clock (measured in-process, so process spawn/teardown is\n\
         excluded), build time (load + compile + link), and peak resident memory.\n\
         These drift run-to-run and are **not asserted** anywhere — they're checked in\n\
         only to track trends. Regenerate and review the printed regression/improvement\n\
         summary with:\n\n\
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

/// Parse a previously-written metrics file into `test -> (wall_us, build_ms,
/// peak_kb)`. Returns empty on a missing/unreadable file (first run).
fn read_metrics(path: &Path) -> HashMap<String, (f64, f64, f64)> {
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
        if cols.len() != 4 || cols[0] == "Test" || cols[0].starts_with("---") {
            continue;
        }
        if let (Ok(w), Ok(b), Ok(p)) = (
            cols[1].parse::<f64>(),
            cols[2].parse::<f64>(),
            cols[3].parse::<f64>(),
        ) {
            map.insert(cols[0].to_string(), (w, b, p));
        }
    }
    map
}

/// One metric's per-test outlier rules: only flag a change that is both
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
}

const METRICS: [Metric; 3] = [
    Metric {
        name: "wall-clock",
        unit: "µs",
        decimals: 2,
        threshold: 10.0,
        floor: 2.0,
    },
    Metric {
        name: "build",
        unit: "ms",
        decimals: 2,
        threshold: 10.0,
        floor: 20.0,
    },
    Metric {
        name: "peak RSS",
        unit: "KB",
        decimals: 0,
        threshold: 10.0,
        floor: 128.0,
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
        if d > m.threshold {
            regs.push((t, *o, *n, d));
        } else if d < -m.threshold {
            imps.push((t, *o, *n, d));
        }
    }
    regs.sort_by(|a, b| b.3.total_cmp(&a.3)); // biggest regression first
    imps.sort_by(|a, b| a.3.total_cmp(&b.3)); // biggest improvement first
    let (name, unit, prec, thr, floor) = (m.name, m.unit, m.decimals, m.threshold, m.floor);
    let mut out = String::new();
    let mut list = |label: &str, items: &[(&str, f64, f64, f64)]| {
        out.push_str(&format!(
            "{name} {label} (>{thr:.0}% and >{floor:.0} {unit}): {}\n",
            items.len()
        ));
        for (t, o, n, d) in items.iter().take(20) {
            out.push_str(&format!(
                "  {t}: {o:.prec$} -> {n:.prec$} {unit} ({d:+.1}%)\n"
            ));
        }
    };
    list("regressions", &regs);
    list("improvements", &imps);
    out
}

/// Build the improvement/regression summary comparing the previous metrics to the
/// freshly measured `rows`: an aggregate "overall change" per metric (summed over
/// shared tests), per-test outliers for **every** metric (wall-clock, build, peak
/// RSS — see [`METRICS`]), and added/removed tests.
fn compare_metrics(old: &HashMap<String, (f64, f64, f64)>, rows: &[PerfRow]) -> String {
    let pct = |o: f64, n: f64| if o > 0.0 { (n - o) / o * 100.0 } else { 0.0 };

    let (mut ow, mut nw, mut ob, mut nb, mut op, mut np) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let mut shared = 0usize;
    // Per-metric `(test, old, new)` deltas over the shared tests, indexed like
    // `METRICS` (wall-clock, build, peak RSS).
    let mut deltas: [Vec<(String, f64, f64)>; 3] = Default::default();
    let mut added: Vec<&str> = Vec::new();
    let mut present: HashSet<&str> = HashSet::new();

    for r in rows {
        present.insert(r.test.as_str());
        match old.get(&r.test) {
            Some(&(w, b, p)) => {
                shared += 1;
                ow += w;
                nw += r.wall_us;
                ob += b;
                nb += r.build_ms;
                op += p;
                np += r.peak_kb as f64;
                deltas[0].push((r.test.clone(), w, r.wall_us));
                deltas[1].push((r.test.clone(), b, r.build_ms));
                deltas[2].push((r.test.clone(), p, r.peak_kb as f64));
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
        out.push_str(&format!(
            "  wall-clock: {ow:.1} -> {nw:.1} µs ({:+.1}%)\n",
            pct(ow, nw)
        ));
        out.push_str(&format!(
            "  build:      {ob:.1} -> {nb:.1} ms ({:+.1}%)\n",
            pct(ob, nb)
        ));
        out.push_str(&format!(
            "  peak RSS:   {op:.0} -> {np:.0} KB ({:+.1}%)\n",
            pct(op, np)
        ));

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

fn run_success_case(
    ctx: &str,
    orig_path: &Path,
    src_path: &Path,
    stem: &str,
    spec: &Spec,
    case_dir: &Path,
) -> Outcome {
    let program = match loader::load_program(src_path, debug_opts()) {
        Ok(p) => p,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: load failed:\n{}",
                e.render(&spec.source, "input")
            ))
        }
    };
    // A *library* case has `.test` blocks but no `main`: it's exercised only
    // through `aipl check` (below), and its `--- performance ---` measures that
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
    let measured = measured_program(&program);
    // The object used for behavior checks (and the basis for `aipl build`) is
    // *not* instrumented — production runs carry no instruction-counter overhead.
    // The `--- performance ---` check builds its own instrumented object below.
    let obj_comp = match ObjectCompilation::new(&measured, stem, debug_opts(), false) {
        Ok(c) => c,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: compile failed:\n{}",
                e.render(&spec.source, "input")
            ))
        }
    };
    // The monomorphized instances emitted into this binary, pinned by an optional
    // `--- monomorphizations ---` section. Read before `emit` consumes `obj_comp`.
    if let Some(expected) = &spec.monomorphizations {
        let actual = obj_comp.monomorphized_fns().join("\n");
        if fill_mode() && actual != *expected {
            fill_or_add_section(orig_path, "monomorphizations", &actual);
            eprintln!("[{}]: filled monomorphizations list", orig_path.display());
            return Outcome::Skip;
            // Already correct — fall through so subsequent fill targets (e.g. performance) get reached.
        }
        if actual != *expected {
            return Outcome::Fail(format!(
                "{ctx}: monomorphizations mismatch\n--- expected ---\n{expected}\n\
                 --- actual ---\n{actual}\nIf this change is intended, run `{FILL_CMD}`."
            ));
        }
    }
    let obj_bytes = match obj_comp.emit() {
        Ok(b) => b,
        Err(e) => {
            return Outcome::Fail(format!(
                "{ctx}: emit failed:\n{}",
                e.render(&spec.source, "input")
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
                e.render(&spec.source, "input")
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

        let stdout_placeholder = spec.stdout.as_deref() == Some("?");
        if fill_mode() && stdout != exp_stdout {
            fill_or_add_section(orig_path, "stdout", &stdout);
            eprintln!("[{}]: filled stdout", orig_path.display());
            return Outcome::Skip;
        }
        if stdout_placeholder {
            eprintln!("=== ACTUAL STDOUT for {ctx} ===\n{stdout}\n===");
            return Outcome::Skip;
        }
        if stdout != exp_stdout {
            return Outcome::Fail(format!(
            "{ctx}: stdout mismatch\n--- expected ---\n{exp_stdout}\n--- actual ---\n{stdout}\n",
        ));
        }
        if stderr != exp_stderr {
            return Outcome::Fail(format!(
            "{ctx}: stderr mismatch\n--- expected ---\n{exp_stderr}\n--- actual ---\n{stderr}\n",
        ));
        }
        if exit != exp_exit {
            return Outcome::Fail(format!(
                "{ctx}: exit code mismatch (expected {exp_exit}, got {exit})"
            ));
        }

        // Validate any files the program was expected to *write* (e.g. via
        // `write_string_to_file`). Checked after stdout/exit so a file mismatch
        // never masks a behavioral regression. The behavior run above wrote them
        // into `case_dir`; a `?` body captures the actual contents in fill mode.
        let mut filled_file = false;
        for (rel, expected) in &spec.expect_files {
            let actual = match fs::read_to_string(case_dir.join(rel)) {
                Ok(c) => normalize_output(&c),
                Err(e) => {
                    return Outcome::Fail(format!(
                        "{ctx}: expected output file {rel:?} was not written: {e}"
                    ))
                }
            };
            if fill_mode() {
                fill_or_add_section(orig_path, &format!("expect file: {rel}"), &actual);
                eprintln!("[{}]: filled expect file {rel}", orig_path.display());
                filled_file = true;
                continue;
            }
            if &actual != expected {
                return Outcome::Fail(format!(
                "{ctx}: output file {rel:?} mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}\n",
            ));
            }
        }
        if filled_file {
            return Outcome::Skip;
        }
    } // end `if has_main` (behavior run)

    // Run the case's in-language `.test({ .. })` blocks via the real `aipl
    // check` binary. A subprocess (not in-process) because the test runner keeps
    // process-global pass/fail counters — a fresh process gives each case its
    // own clean state and avoids races between parallel shards. Run when the
    // source has `.test` blocks, or when a `--- check ---` section pins the
    // expected report (so the testless majority of cases pay nothing).
    if spec.check.is_some() || spec.source.contains(".test") {
        let output = match Command::new(env!("CARGO_BIN_EXE_aipl"))
            .arg("check")
            .arg(src_path)
            .current_dir(case_dir)
            .output()
        {
            Ok(o) => o,
            Err(e) => return Outcome::Fail(format!("{ctx}: `aipl check` spawn failed: {e}")),
        };
        let report = normalize_output(&String::from_utf8_lossy(&output.stdout));
        match &spec.check {
            // A `--- check ---` section pins the expected report exactly (this is
            // how a *failing* test is documented).
            Some(expected) => {
                if fill_mode() {
                    fill_or_add_section(orig_path, "check", &report);
                    eprintln!("[{}]: filled check report", orig_path.display());
                    return Outcome::Skip;
                }
                if &report != expected {
                    return Outcome::Fail(format!(
                        "{ctx}: `check` output mismatch\n--- expected ---\n{expected}\n--- actual ---\n{report}\n",
                    ));
                }
            }
            // No pinned report: the in-language tests must simply pass.
            None => {
                if !output.status.success() {
                    let errs = normalize_output(&String::from_utf8_lossy(&output.stderr));
                    return Outcome::Fail(format!(
                        "{ctx}: `aipl check` (in-language tests) failed:\n{report}{errs}"
                    ));
                }
            }
        }
    }

    // Allocation accounting, if requested. Correctness (above) is checked
    // first so a perf mismatch never masks a behavioral regression.
    if let Some(perf) = &spec.performance {
        // `obj_bytes` is the non-instrumented (production) object; its length is
        // the `binary size` metric — the machine code emitted for the program.
        return run_performance_check(
            ctx,
            orig_path,
            &measured,
            stem,
            spec,
            case_dir,
            perf,
            obj_bytes.len() as u64,
        );
    }
    Outcome::Pass
}

/// Build the *instrumented* object (executed-instruction counter enabled), link
/// it against the instrumented runtime, run it, and verify (or fill in) the
/// expected counts from a `--- performance ---` section. This object is separate
/// from the behavior-check one so production/JIT runs carry no counter overhead.
fn run_performance_check(
    ctx: &str,
    orig_path: &Path,
    program: &Program,
    stem: &str,
    spec: &Spec,
    case_dir: &Path,
    expected_body: &str,
    prod_obj_size: u64,
) -> Outcome {
    let obj_bytes =
        match ObjectCompilation::new(program, stem, debug_opts(), true).and_then(|c| c.emit()) {
            Ok(b) => b,
            Err(e) => {
                return Outcome::Fail(format!(
                    "{ctx}: instrumented compile failed:\n{}",
                    e.render(&spec.source, "input")
                ))
            }
        };
    let actual = match measure_perf_stats(ctx, &obj_bytes, stem, spec, case_dir, prod_obj_size) {
        Ok(v) => v,
        Err(msg) => return Outcome::Fail(msg),
    };

    // Memory-leak gate: every heap allocation must be paired with a free.
    // Catch this before fill_mode so `fill_expected` cannot bake in a leak.
    if actual.allocations != actual.deallocations {
        return Outcome::Fail(format!(
            "{ctx}: memory leak — allocations ({}) != deallocations ({})\n\
             Fix the leak, then re-run `{FILL_CMD}` to refresh the expected counts.",
            actual.allocations, actual.deallocations,
        ));
    }

    let expected = parse_perf_stats(expected_body);

    let placeholder = expected_body.trim() == "?";
    // In fill mode always overwrite — no `?` required.
    if fill_mode() {
        fill_or_add_section(orig_path, "performance", &actual.render());
        eprintln!("[{}]: filled performance counts", orig_path.display());
        return Outcome::Skip;
    }
    if placeholder {
        eprintln!(
            "=== ACTUAL PERFORMANCE for {ctx} ===\n{}\n===",
            actual.render()
        );
        return Outcome::Skip;
    }

    let expected = match expected {
        Some(v) => v,
        None => {
            return Outcome::Fail(format!(
                "{ctx}: malformed `performance` section; expected `allocations: N`, \
                 `deallocations: M`, `reallocations: K`, `bytes allocated: B`, \
                 `instructions executed: I`, and `binary size: S` lines, got:\n{expected_body}"
            ))
        }
    };

    if actual != expected {
        return Outcome::Fail(format!(
            "{ctx}: performance mismatch\n--- expected ---\n{}\n--- actual ---\n{}\n\
             If this change is intended, run `{FILL_CMD}`.",
            expected.render(),
            actual.render(),
        ));
    }
    Outcome::Pass
}

/// Build the instrumented variant of the case, run it with `AIPL_ALLOC_STATS`
/// pointed at a temp file, and read back the `PerfStats`. The runtime reports
/// the five execution counters; `binary size` is the harness-measured
/// `prod_obj_size` (the production object's byte length), folded in here so the
/// whole `PerfStats` parses through one path.
fn measure_perf_stats(
    ctx: &str,
    obj_bytes: &[u8],
    stem: &str,
    spec: &Spec,
    case_dir: &Path,
    prod_obj_size: u64,
) -> Result<PerfStats, String> {
    let exe = case_dir.join(binary::default_exe_name(&format!("{stem}_instr")));
    if let Err(e) = binary::link_instrumented(obj_bytes, &exe) {
        return Err(format!(
            "{ctx}: instrumented link failed:\n{}",
            e.render(&spec.source, "input")
        ));
    }

    let stats_path = case_dir.join("alloc_stats.txt");
    let _ = fs::remove_file(&stats_path);
    let output = Command::new(&exe)
        .args(&spec.cli)
        .current_dir(case_dir)
        .env("AIPL_ALLOC_STATS", &stats_path)
        .output()
        .map_err(|e| format!("{ctx}: instrumented spawn failed: {e}"))?;
    if !output.status.success() && spec.exit_code.unwrap_or(0) == 0 {
        // A nonzero exit the case didn't expect means the instrumented run
        // crashed before reporting — surface it rather than a missing-file error.
        return Err(format!(
            "{ctx}: instrumented run exited with {:?} before reporting alloc stats",
            output.status.code()
        ));
    }

    let contents = fs::read_to_string(&stats_path).map_err(|e| {
        format!(
            "{ctx}: could not read alloc stats at {}: {e}",
            stats_path.display()
        )
    })?;
    // The runtime reports the execution counters; append the (harness-measured)
    // binary size so the combined text parses into a full `PerfStats`.
    let contents = format!("{contents}\nbinary size: {prod_obj_size}");
    parse_perf_stats(&contents)
        .ok_or_else(|| format!("{ctx}: malformed perf stats from runtime:\n{contents}"))
}

/// Measured performance statistics for a case run. All six fields are required
/// in a `--- performance ---` body. `instructions` is the CLIF instructions
/// executed; `binary_size` is the byte length of the compiler-emitted (non-
/// instrumented) object — the machine code produced for the program, excluding
/// the separately-linked runtime. All are deterministic for a fixed toolchain
/// (codegen emits from ordered work-lists, not HashMap iteration), though
/// `binary_size` is target-specific (machine code + object format), unlike the
/// others.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PerfStats {
    allocations: u64,
    deallocations: u64,
    reallocations: u64,
    bytes_allocated: u64,
    instructions: u64,
    binary_size: u64,
}

impl PerfStats {
    /// The `--- performance ---` body these stats represent.
    fn render(&self) -> String {
        format!(
            "allocations: {}\ndeallocations: {}\nreallocations: {}\nbytes allocated: {}\n\
             instructions executed: {}\nbinary size: {}",
            self.allocations,
            self.deallocations,
            self.reallocations,
            self.bytes_allocated,
            self.instructions,
            self.binary_size,
        )
    }
}

/// Parse the `allocations:` / `deallocations:` / `reallocations:` / `bytes
/// allocated:` / `instructions executed:` / `binary size:` lines (order-
/// independent, surrounding whitespace ignored). All six are required; a
/// missing line yields `None`.
fn parse_perf_stats(s: &str) -> Option<PerfStats> {
    let mut allocations = None;
    let mut deallocations = None;
    let mut reallocations = None;
    let mut bytes_allocated = None;
    let mut instructions = None;
    let mut binary_size = None;
    for line in s.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("bytes allocated:") {
            bytes_allocated = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("instructions executed:") {
            instructions = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("binary size:") {
            binary_size = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("allocations:") {
            allocations = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("deallocations:") {
            deallocations = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("reallocations:") {
            reallocations = v.trim().parse().ok();
        }
    }
    Some(PerfStats {
        allocations: allocations?,
        deallocations: deallocations?,
        reallocations: reallocations?,
        bytes_allocated: bytes_allocated?,
        instructions: instructions?,
        binary_size: binary_size?,
    })
}

/// Replace the body of the `--- <section> ---` block in the case file with
/// `body` (dropping the old body up to the next header or EOF), or append
/// `--- <section> ---\n<body>\n` when the section doesn't already exist. Every
/// call site here targets a section that's already required to exist except
/// `stdout` (which many cases omit, relying on the default-empty behavior), so
/// one function covers both. The transform itself is the dogfooded AIPL
/// fill_or_add_section, run from checked-in IR (see
/// [`aipl::codegen::fill_or_add_section`]).
fn fill_or_add_section(path: &Path, section: &str, body: &str) {
    let contents = fs::read_to_string(path).expect("read case for fill");
    let out = aipl::codegen::fill_or_add_section(&contents, section, body);
    fs::write(path, out).expect("rewrite case file adding section");
}

fn normalize_output(s: &str) -> String {
    let lf = s.replace("\r\n", "\n");
    let trimmed = lf.trim_end_matches(['\n', '\r']);
    trimmed.to_string()
}

fn try_fill_expected(path: &Path, contents: &str, spec: &Spec) {
    // Re-run the load/compile path against the in-memory source so we
    // can render an error to splice in. If compilation succeeds the
    // author has a bigger problem than missing expected output, so the
    // re-run will fail when the placeholder gets dropped.
    //
    // Stage to a per-PID subdir using the case's stem as the filename,
    // so any errors that mention the source file's name (e.g. the
    // loader's "duplicate top-level item" error) reproduce exactly what
    // the real test will see. Also stage any `file:` companions.
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("utf-8 case stem");
    let dir = std::env::temp_dir().join(format!("aipl-fill-{}-{stem}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir fill staging");
    let tmp = dir.join(format!("{stem}.aipl"));
    fs::write(&tmp, &spec.source).expect("write tmp source");
    for (rel_path, contents) in &spec.extra_files {
        let p = dir.join(rel_path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir companion parent");
        }
        fs::write(&p, contents).expect("write companion source");
    }
    let result = loader::load_program(&tmp, debug_opts())
        .and_then(|prog| Compilation::new(&prog, debug_opts()).map(|_| ()));
    let _ = fs::remove_dir_all(&dir);
    let err = match result {
        Err(e) => e,
        Ok(()) => {
            eprintln!(
                "[{}]: fill_expected: program compiled — nothing to splice in.",
                path.display()
            );
            return;
        }
    };
    let rendered = err.render(&spec.source, "input");
    // Replace the `?` placeholder line with the rendered error.
    let header_marker = "--- errors ---";
    let header_idx = contents
        .find(header_marker)
        .expect("`--- errors ---` header in source");
    let after_header = header_idx + header_marker.len();
    let new_contents = format!(
        "{}\n{}\n",
        &contents[..after_header].trim_end(),
        rendered.trim_end()
    );
    fs::write(path, new_contents).expect("rewrite case file");
    eprintln!("[{}]: filled expected error", path.display());
}
