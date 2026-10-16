use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aipl::binary;
use aipl::codegen::{Compilation, ObjectCompilation};
use aipl::loader;
use aipl::{sections, DebugOptions, Error, SpecFields};

/// Render the errors a compile reported — every independent finding a pass
/// collected, not just the first — each with a source caret when possible.
/// Spans are relative to `file`'s own source (correct for a single-file
/// program; for an imported-file error only the caret line may be off — the
/// message is still right), falling back to the plain messages if the file
/// can't be read.
fn render_err(file: &str, errors: Vec<Error>) -> String {
    render_err_at(Path::new(file), file, errors)
}

/// [`render_err`] for a file whose source lives at `path` but should be *named*
/// `label` in the output — the two differ once `check` resolves inputs to
/// absolute paths (so they load from any working directory) while still
/// reporting them as the user wrote them.
fn render_err_at(path: &Path, label: &str, errors: Vec<Error>) -> String {
    match std::fs::read_to_string(path) {
        Ok(src) => Error::render_all(&errors, aipl::strip_test_sections(&src), label),
        Err(_) => Error::display_all(&errors),
    }
}

fn main() -> ExitCode {
    // Run every subcommand on a large-stack worker thread: debug codegen recurses
    // per AST node, deep enough to overflow the default ~1 MB main-thread stack
    // on Windows for moderately-sized programs (more so with narrow-int
    // conversions, whose expression trees are deeper).
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(cli)
        .expect("spawn worker thread")
        .join()
        .expect("worker thread panicked")
}

fn cli() -> ExitCode {
    // The raw-string de-denter calls the dogfooded AIPL `dedent` via the FFI;
    // install that hook before any source is parsed.
    aipl::install_parser_hooks();

    let args: Vec<String> = env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "aipl".into());

    let result = match args.get(1).map(String::as_str) {
        Some("run") => run_cmd(&args[2..]),
        Some("ir") => ir_cmd(&args[2..]),
        Some("doc") => doc_cmd(&args[2..]),
        Some("docs") => docs_cmd(&args[2..]),
        Some("build") => build_cmd(&args[2..]),
        // `check` owns its exit code (0 = all tests passed, 1 = a failure) and
        // prints its own report, so it returns an `ExitCode` directly.
        Some("check") => return check_cmd(&args[2..]),
        // `report` owns its exit code too: 1 for a program that leaks.
        Some("report") => return report_cmd(&args[2..]),
        Some("lsp") => lsp_cmd(&args[2..]),
        Some("--help") | Some("-h") | Some("help") | None => {
            println!("{}", usage(&prog));
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(format!("unknown command {other:?}\n\n{}", usage(&prog))),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(rendered) => {
            eprintln!("{rendered}");
            ExitCode::FAILURE
        }
    }
}

fn usage(prog: &str) -> String {
    format!(
        "usage:
  {prog} run   <file.aipl> [fn] [args...]   compile and JIT-execute a function (default: main)
  {prog} ir    <file.aipl>                  print cranelift IR for a source file
  {prog} doc   <file.aipl>                  print the file's and each declaration's `# ..` docs
  {prog} docs  [path...] [-o <dir>]         write an HTML documentation site
  {prog} build <file.aipl> [-o <output>]    link a native binary executable
  {prog} check [path...] [--fix] [--ignore-sections]  run the tests, and check formatting and each file's sections
  {prog} report <file.aipl> [--machine] [-- args...]  count what a run of the file costs
  {prog} lsp                                serve the Language Server Protocol on stdin/stdout

`docs` is `doc`'s whole-project counterpart: where `doc` prints one file's
documentation to stdout, `docs` walks a tree and writes a static HTML site —
no server and no JavaScript, so it opens straight off the filesystem. With no
path it documents the working directory, into `./docs` unless `-o` says
otherwise.

`check` with no path checks every `.aipl` file under the working directory (one
process, so the engine links once); pass a file to check just that one, or a
directory to check the tree under it. It also reports any file that isn't in
canonical format, runs a function's `.performance({{ .. }})` block too (in an
instrumented build, which needs the linker `build` does), and validates the
`--- section ---` blocks a file may end with: `errors` (the file must fail to
compile with exactly those diagnostics, named relative to where `check` was
started), `stdout`, `stderr`, `exit code`, `cli` and `expect file: p` (one run of
its `main`, made only when one of them is present), and `report` (what `aipl
report` prints for it). `--- file: p ---` sections are staged beside the file so
its imports and fixtures resolve. `--ignore-sections` checks the tests alone. It
reports every failure rather than stopping at the first, and exits 0 only if all
of them were formatted, compiled, passed, and matched.

`--fix` repairs what a rewrite can: it formats each file in place, then rewrites
each recorded section that no longer says what the file does (printing `formatted
<file>` and `fixed <file>: <sections>`). Formatting goes first because it moves
line numbers that `errors` and `report` record. It does not hide a real problem:
a failing test, a leak, a `trace`, a file that does not compile, a section for a
file that was never written, or sections that contradict each other are still
failures. A section a file does not have is added only for output the file's own
run produced, since an absent `stdout` already means `prints nothing`.

`report` builds the file's `main` (or, for a file with only `.test` blocks, the run
of those tests) in the instrumented build and runs it once: heap allocations,
bytes asked for, the most heap live at once, CLIF instructions executed, the
object's size split into code/data/metadata, and every function's call count and
code size, runtime builtins included. The counts are exact and repeatable. It
needs the linker `build` does. Arguments after `--` go to the program (default:
the file's `--- cli ---` section); `--machine` prints the same counts as `key:
value` lines for a tool to read. A program that frees fewer blocks than it
allocated leaks, which `report` says and exits 1 for.

`lsp` is for an editor to spawn, not for a person to run: it speaks the
Language Server Protocol over stdin/stdout and answers go-to-definition, hover,
the document outline, formatting, and diagnostics. An editor should spawn the
`aipl` sitting in the project (see DESIGN_PRINCIPLES.md §4) so that what it
reports is what `aipl check` reports.

args to `run` are parsed as i64. Functions of arity 0, 1, or 2 are supported.
`build` requires `clang` on PATH (used as linker driver).

pass `--debug` to any command to trace each compiler pass (loader,
monomorphization, codegen) to stderr — the last line before a hang points at
where the compiler got stuck."
    )
}

/// Pull a `--debug` flag out of `args` (it may appear anywhere), returning the
/// remaining positional args and the resulting [`DebugOptions`].
fn take_debug_flag(args: &[String]) -> (Vec<String>, DebugOptions) {
    let mut rest = Vec::with_capacity(args.len());
    let mut enabled = false;
    for a in args {
        if a == "--debug" {
            enabled = true;
        } else {
            rest.push(a.clone());
        }
    }
    (rest, DebugOptions::new(enabled))
}

fn run_cmd(args: &[String]) -> Result<(), String> {
    let (args, dbg) = take_debug_flag(args);
    let (file, rest) = args.split_first().ok_or("missing source file")?;
    let fn_name = rest.first().map(String::as_str).unwrap_or("main");
    let trailing = &rest[rest.len().min(1)..];

    let program = loader::load_program(Path::new(file), dbg).map_err(|e| render_err(file, e))?;
    let comp = Compilation::new(&program, dbg).map_err(|e| render_err(file, e))?;

    // A `str[]`-taking function (e.g. `fn main(args: str[])`) receives the
    // trailing tokens as CLI arguments; otherwise they're parsed as i64.
    let result = if comp.takes_cli_args(fn_name) {
        comp.run_cli(fn_name, trailing).map_err(|e| e.to_string())?
    } else {
        let fn_args: Vec<i64> = trailing
            .iter()
            .map(|s| s.parse::<i64>().map_err(|e| format!("bad arg {s:?}: {e}")))
            .collect::<Result<_, _>>()?;
        match fn_args.as_slice() {
            [] => comp.run_0(fn_name).map_err(|e| e.to_string())?,
            [a] => comp.run_1(fn_name, *a).map_err(|e| e.to_string())?,
            [a, b] => comp.run_2(fn_name, *a, *b).map_err(|e| e.to_string())?,
            _ => {
                return Err(format!(
                    "too many args ({}); only 0-2 supported for now",
                    fn_args.len()
                ));
            }
        }
    };
    println!("{result}");
    Ok(())
}

/// Collect every `*.aipl` file under `dir`, recursively, into `out`.
///
/// Skips hidden entries (`.git`, editor state) and `target/` build output —
/// neither holds project sources, and descending into them is pure cost. A
/// directory that can't be read is reported and skipped rather than aborting the
/// walk, so one unreadable corner doesn't cost you the whole check.
fn collect_aipl(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: cannot read {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_aipl(&path, out);
        } else if path.extension() == Some(OsStr::new("aipl")) {
            out.push(path);
        }
    }
}

/// What checking one file found: the `__test_main` driver's exit code (0 = every
/// test passed), the rendered `trace(..)` diagnostics if the program still holds
/// any, and how its recorded `--- section ---`s disagree with what it does (empty
/// when they all match, or when sections were not validated).
struct Checked {
    exit_code: i64,
    traces: Option<String>,
    sections: Vec<sections::Finding>,
    /// Whether the test driver ran at all: an `errors` file never compiles, so it
    /// has no tests and no summary to print.
    ran_tests: bool,
}

/// Where one file is loaded from: the `entry` source, and the `root` its
/// `trace(..)` labels and imported-file names are relative to.
///
/// Normally that is the file itself and the directory `check` was started in. A
/// file carrying `--- file:` companions is a self-contained case instead: its
/// imports are the companions, which exist only beside a staged copy of its
/// source, so it is loaded from there and rooted at the scratch directory.
struct Staged {
    entry: PathBuf,
    root: PathBuf,
}

/// Load, compile, and run one file's `__test_main` driver, then validate its
/// recorded sections. `Err` is a rendered load/compile failure.
///
/// `staged.entry` is absolute (so loading and its relative imports resolve no
/// matter what the working directory is); `label` is the path as the user typed
/// or as discovery found it, and is what diagnostics show. See [`in_staged_dir`]
/// for why the tests run from a different directory than this was started in.
///
/// `section_label` is `Some` when the file's sections are validated, and is the
/// path an `errors` section names in its diagnostics; `None` checks the tests
/// alone (`--ignore-sections`).
///
/// A file with an `errors` section is *expected* not to compile, so it is
/// compiled for its diagnostics and nothing else runs. Any other file runs its
/// tests, and also its `main` when it has a section that observes a run (see
/// [`sections::runs_main`]).
///
/// The `trace` report is returned rather than printed: a trace is only a failure
/// once the tests have had their chance to hit it and print, so the caller
/// reports it after this returns.
///
/// The pass/fail tallies live in the test runtime and accumulate across calls,
/// which is what lets a batch run report one aggregate at the end.
fn check_file(
    staged: &Staged,
    label: &str,
    spec: &SpecFields,
    section_label: Option<&str>,
    dbg: DebugOptions,
) -> Result<Checked, String> {
    let render = |e| render_err_at(&staged.entry, label, e);
    if let (Some(section_label), Some(_)) = (section_label, &spec.errors) {
        // Loaded from the text the section was recorded against (`spec.source`),
        // not whatever the loader would cut from the file, so an end-of-file
        // diagnostic sits where it did when the section was written.
        let compiled = loader::load_program_overlay(&staged.entry, &spec.source, dbg)
            .and_then(|program| Compilation::new(&program, dbg).map(|_| ()));
        let mut found = sections::conflicts(spec);
        found.extend(sections::errors(spec, compiled, section_label));
        return Ok(Checked {
            exit_code: 0,
            traces: None,
            sections: found,
            ran_tests: false,
        });
    }
    let program = loader::load_program_rooted(&staged.entry, &staged.root, dbg).map_err(render)?;
    // A `.performance` block can be wrong in a way its types and effects do not
    // say; that is reported up front, like a type error, and nothing runs.
    let faults = aipl::performance_diagnostics(&program);
    if !faults.is_empty() {
        return Err(render(faults));
    }
    let traces = aipl::trace_diagnostics(&program, "check");
    let test_program = aipl::codegen::build_test_program(&program);
    // `__test_main` runs each test and returns the exit code (0 ok, 1 failures),
    // printing failures itself. (Runs on `main`'s large-stack worker thread,
    // which gives codegen room for deep `.test` driver/expression trees.)
    let comp = Compilation::new(&test_program, dbg).map_err(render)?;
    let mut exit_code = comp.run_0("__test_main").map_err(|e| e.to_string())?;

    // The `.performance` blocks, run apart from the tests above: they need an
    // instrumented build, which the JIT is not. Their failures print as the tests'
    // do and their tallies join the same aggregate, so a failing block fails the
    // file exactly as a failing test does — and, like one, stops nothing else.
    if let Some(driver) = aipl::codegen::build_performance_program(&program) {
        let stem = staged
            .entry
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("a.out");
        let batch_label = aipl::codegen::test_file().unwrap_or_default();
        match aipl::perf::run_performance_tests(&driver, stem, &batch_label, dbg) {
            Ok(run) => {
                print!("{}", run.report);
                let mut failed = run.failed;
                let mut total = run.total;
                // A leak fails the file as a test of its own would, named like one:
                // every instrumented run is held to freeing what it allocates.
                if let Some((allocated, freed)) = run.leaked {
                    let at = if batch_label.is_empty() {
                        String::new()
                    } else {
                        format!("{batch_label}::")
                    };
                    println!(
                        "test {at}performance blocks ... FAIL\n  leak: {} of {allocated} allocations were never freed ({freed} deallocations)",
                        allocated.abs_diff(freed)
                    );
                    total += 1;
                    failed += 1;
                }
                aipl::codegen::add_test_totals(total, failed);
                if failed > 0 {
                    exit_code = 1;
                }
            }
            Err(aipl::perf::MeasureError::Compile(errors)) => return Err(render(errors)),
            Err(aipl::perf::MeasureError::Link(e)) => return Err(e.to_string()),
            Err(aipl::perf::MeasureError::Run(msg)) => return Err(msg),
        }
    }

    let mut found = Vec::new();
    if section_label.is_some() && sections::runs_main(spec) {
        let stem = staged
            .entry
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("a.out");
        // A run that could not be produced is a finding about the file like a
        // mismatch is, not a reason to forget that its tests already ran.
        found = sections::behavior(&program, spec, stem, label, dbg)
            .unwrap_or_else(|msg| vec![sections::Finding::unfixable(msg)]);
    }
    // A pinned `aipl report`: built both ways and run once, like the command.
    if section_label.is_some() && spec.report.is_some() {
        let stem = staged
            .entry
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("a.out");
        found.extend(
            sections::report(&program, spec, stem, label, dbg)
                .unwrap_or_else(|msg| vec![sections::Finding::unfixable(msg)]),
        );
    }
    Ok(Checked {
        exit_code,
        traces: (!traces.is_empty()).then(|| render(traces)),
        sections: found,
        ran_tests: true,
    })
}

/// A file's trailing sections, parsed. An unreadable file has none: loading it
/// reports that with the diagnostic worth reading.
///
/// A malformed section (an unknown name, a non-integer `exit code`, a companion
/// path that cannot be staged) is the file's error to report. Skipping it would
/// drop an expectation, and the file would pass for the wrong reason.
///
/// `label` is the path as the user gave it (or discovery found it), which is what
/// the error names.
fn read_spec(file: &Path, label: &str) -> Result<SpecFields, String> {
    match std::fs::read_to_string(file) {
        Ok(src) => aipl::parse_spec(&src).map_err(|e| format!("{label}: {e}")),
        Err(_) => Ok(SpecFields::default()),
    }
}

/// The path an `errors` section names `file` by: relative to the directory
/// `check` was started in, `./`-prefixed — the same spelling however `check` was
/// pointed at it (`a.aipl`, `./a.aipl`, an absolute path, or found by walking a
/// directory), which is what lets a recorded diagnostic be compared at all.
fn section_label(file: &Path, project_root: &Path) -> String {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    match file.strip_prefix(&root) {
        Ok(rel) => format!("./{}", rel.to_string_lossy().replace('\\', "/")),
        Err(_) => file.display().to_string(),
    }
}

/// Run `f` with the process working directory pointed at a fresh scratch
/// directory holding `file`'s `--- file:` companions.
///
/// A `.test` block is ordinary code: it can write files, and it resolves
/// relative paths against the working directory. Run in place, `aipl check`
/// would scatter whatever its tests write across the tree it was invoked from —
/// so tests get a scratch directory instead, staged with the sibling files a
/// case expects to find (fixtures its tests read, modules they import).
///
/// A file without companions is still loaded from where it is, so its imports
/// resolve against the real tree rather than an empty scratch copy. One *with*
/// companions is loaded from a copy of its source staged beside them, since that
/// is the only place its imports exist (see [`Staged`]).
fn in_staged_dir<T>(
    file: &Path,
    spec: &SpecFields,
    project_root: &Path,
    f: impl FnOnce(&Staged) -> T,
) -> Result<T, String> {
    let dir = binary::scratch_dir("check").map_err(|e| e.to_string())?;
    let staged = (|| {
        aipl::stage_companions(&dir, &spec.extra_files)
            .map_err(|e| format!("stage companions in {}: {e}", dir.display()))?;
        if spec.extra_files.is_empty() {
            return Ok(Staged {
                entry: file.to_path_buf(),
                root: project_root.to_path_buf(),
            });
        }
        let entry = dir.join(file.file_name().unwrap_or(OsStr::new("a.aipl")));
        std::fs::write(&entry, &spec.source)
            .map_err(|e| format!("stage {}: {e}", entry.display()))?;
        Ok(Staged {
            entry,
            root: dir.clone(),
        })
    })();
    let staged = match staged {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let prev = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    env::set_current_dir(&dir).map_err(|e| format!("enter {}: {e}", dir.display()))?;
    let out = f(&staged);
    // Restore first, so a failure to clean up can't leave the process parked in
    // a directory it is about to delete.
    let restored = env::set_current_dir(&prev);
    let _ = std::fs::remove_dir_all(&dir);
    restored.map_err(|e| format!("return to {}: {e}", prev.display()))?;
    Ok(out)
}

/// Check one resolved file: parse its sections, stage it, and run
/// [`check_file`]. `sections` says whether its recorded sections are validated.
fn check_resolved(
    path: &Path,
    label: &str,
    project_root: &Path,
    sections: bool,
    dbg: DebugOptions,
) -> Result<Checked, String> {
    let spec = read_spec(path, label)?;
    let section_label = sections.then(|| section_label(path, project_root));
    in_staged_dir(path, &spec, project_root, |staged| {
        check_file(staged, label, &spec, section_label.as_deref(), dbg)
    })?
}

/// Print each way a file's sections disagree with what it does, naming the file.
fn report_sections(label: &str, found: &[sections::Finding]) {
    for f in found {
        eprintln!("{label}: {}", f.message);
    }
}

/// `--fix` for the sections: write each finding that has a fix back into `path`,
/// through the same dogfooded function `fill_expected` refreshes the corpus with,
/// and return the findings that remain — the ones no rewrite repairs.
///
/// A finding's fix is the section saying what the file now does, so applying it
/// makes the finding true; what is left (a failing test, a leak, an expected file
/// never written, sections that contradict each other) is a real problem that a
/// rewritten section would only hide.
fn apply_fixes(path: &Path, label: &str, found: Vec<sections::Finding>) -> Vec<sections::Finding> {
    let (fixable, mut rest): (Vec<_>, Vec<_>) = found.into_iter().partition(|f| f.fix.is_some());
    if fixable.is_empty() {
        return rest;
    }
    let fills: Vec<(String, String)> = fixable.iter().filter_map(|f| f.fix.clone()).collect();
    match aipl::codegen::fill_or_add_sections_file(&path.to_string_lossy(), &fills) {
        Ok(()) => {
            let mut names: Vec<&str> = Vec::new();
            for (section, _) in &fills {
                if !names.contains(&section.as_str()) {
                    names.push(section);
                }
            }
            println!("fixed {label}: {}", names.join(", "));
        }
        Err(e) => {
            eprintln!("{label}: could not write the fixed sections: {e}");
            rest.extend(fixable);
        }
    }
    rest
}

/// `--fix` for formatting: rewrite `file` in canonical form. Returns whether it is
/// still unformatted afterwards (only if the rewrite could not be written).
///
/// Done before anything else is checked, because formatting moves line numbers and
/// an `errors` or `report` section records them: fixing those first would record
/// positions the next format changes. A source the formatter cannot parse is left
/// alone — loading it reports the syntax error, which is the diagnostic that helps.
fn fix_formatting(file: &Path, label: &str) -> bool {
    let Ok(src) = std::fs::read_to_string(file) else {
        return false;
    };
    let Ok(formatted) = aipl::fmt::format_source(&src, &aipl::fmt::FmtOptions::default()) else {
        return false;
    };
    if formatted == src {
        return false;
    }
    match std::fs::write(file, &formatted) {
        Ok(()) => {
            println!("formatted {label}");
            false
        }
        Err(e) => {
            eprintln!("{label}: could not rewrite: {e}");
            true
        }
    }
}

/// Report `file` to stderr if it isn't in canonical format, returning whether it
/// needs formatting.
///
/// Unformatted source is a `check` failure, but not a fatal one: it is reported
/// and the file's tests still run, so one invocation surfaces every problem —
/// the same reason a file that fails to compile doesn't stop the run.
///
/// A source the formatter can't parse yields no complaint. Loading it below
/// reports the syntax error, which is the diagnostic that actually helps;
/// "needs formatting" stacked on top of it is noise.
fn check_formatting(file: &Path, label: &str) -> bool {
    let Ok(src) = std::fs::read_to_string(file) else {
        return false;
    };
    let Ok(formatted) = aipl::fmt::format_source(&src, &aipl::fmt::FmtOptions::default()) else {
        return false;
    };
    if formatted == src {
        return false;
    }
    eprintln!("{label}: needs formatting (run `aipl check --fix {label}`)");
    true
}

/// `check [path...]` — JIT-run every function's `.test({ .. })` block, run its
/// `.performance({ .. })` block in an instrumented build, and report.
///
/// With no path, checks every `.aipl` file under the working directory: `aipl
/// check` on its own is meant to be a project's whole handoff step. A path may
/// be a file (check just that one, for targeted development) or a directory
/// (check the tree under it), and several may be given.
///
/// Checking many files in one process is the point: each file still becomes its
/// own program — a file's tests are its own, since the loader drops imported
/// functions' `.test` bodies — but the ~0.2s dogfood-engine link is a lazy
/// per-thread `thread_local!`, so a batch pays it once instead of once per file.
///
/// Each file is also checked against the canonical format, since a project running
/// `aipl check` as its handoff gate wants one command to say whether the tree is
/// ready — not two. `--fix` is the same question answered by repairing: it formats
/// the file, then refreshes the sections that no longer match (see [`apply_fixes`]),
/// and leaves the failures a rewrite cannot repair — so it is also how a tree is
/// made ready.
///
/// A file's trailing `--- section ---` blocks are expectations about it, and are
/// validated too (see [`sections`]): `errors` for a file that must not compile,
/// `stdout`/`stderr`/`exit code`/`cli`/`expect file:` for what its `main` does.
/// `--ignore-sections` checks the tests alone, for a caller that validates the
/// sections itself.
///
/// A file that fails to compile, or fails formatting, is reported and does *not*
/// stop the run; you get every problem in the codebase from one invocation
/// rather than the first one. Exit code 0 only if every file was formatted,
/// compiled, had every test pass, and matched its sections.
fn check_cmd(args: &[String]) -> ExitCode {
    let (args, dbg) = take_debug_flag(args);
    let validate_sections = !args.iter().any(|a| a == "--ignore-sections");
    let fix = args.iter().any(|a| a == "--fix");
    let args: Vec<String> = args
        .into_iter()
        .filter(|a| a != "--ignore-sections" && a != "--fix")
        .collect();

    // One explicit file keeps the single-file report: bare `test <name>` headers
    // and the runtime's own summary line. Anything else — no argument, a
    // directory, or several paths — is a batch, which prefixes failures with
    // their file and prints one aggregate.
    let single = args.len() == 1 && Path::new(&args[0]).is_file();

    let mut files = Vec::new();
    if args.is_empty() {
        collect_aipl(Path::new("."), &mut files);
    }
    for arg in &args {
        let path = Path::new(arg);
        if path.is_dir() {
            collect_aipl(path, &mut files);
        } else if path.exists() {
            files.push(path.to_path_buf());
        } else {
            eprintln!("no such file or directory: {arg}");
            return ExitCode::FAILURE;
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        // Checking nothing and reporting success would quietly disarm a handoff
        // that runs a bare `aipl check`, so an empty tree is a failure.
        let where_ = if args.is_empty() {
            ".".to_string()
        } else {
            args.join(", ")
        };
        eprintln!("no .aipl files found under {where_}");
        return ExitCode::FAILURE;
    }

    // Captured before anything stages a scratch directory: this is the root the
    // discovered labels are relative to, and so what a `trace(..)` names its
    // file against (see `check_file`).
    let project_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Resolve up front: the tests below run from a scratch directory, so a
    // relative path would no longer find its file. Diagnostics keep naming the
    // path as discovery found it.
    let resolved: Vec<(PathBuf, String)> = files
        .iter()
        .map(|f| {
            let abs = std::fs::canonicalize(f).unwrap_or_else(|_| f.clone());
            (abs, f.display().to_string())
        })
        .collect();

    if single {
        let (path, label) = &resolved[0];
        // Reported before the tests run, and either way: the point is to end the
        // invocation knowing about every problem, not just the first kind.
        let unformatted = if fix {
            fix_formatting(path, label)
        } else {
            check_formatting(path, label)
        };
        // The summary is printed here, once both kinds of test have run, rather
        // than by the JIT runner when its own tests finish.
        aipl::codegen::set_quiet_summary(true);
        let outcome = check_resolved(path, label, &project_root, validate_sections, dbg);
        let (tests_ok, traced, mismatched) = match outcome {
            // Reported after the run, so a trace the tests hit has already
            // printed its value by the time the diagnostic explains itself.
            Ok(checked) => {
                // Fixed before the summary is printed, so the summary is the last
                // thing said about the file.
                let remaining = if fix {
                    apply_fixes(path, label, checked.sections)
                } else {
                    checked.sections
                };
                if checked.ran_tests {
                    let (total, passed, failed) = aipl::codegen::test_totals();
                    println!("{total} tests: {passed} passed, {failed} failed");
                }
                if let Some(report) = &checked.traces {
                    eprintln!("{report}");
                }
                report_sections(label, &remaining);
                (
                    checked.exit_code == 0,
                    checked.traces.is_some(),
                    !remaining.is_empty(),
                )
            }
            Err(msg) => {
                eprintln!("{msg}");
                (false, false, false)
            }
        };
        return if tests_ok && !unformatted && !traced && !mismatched {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    aipl::codegen::set_quiet_summary(true);
    let mut broken = 0usize;
    let mut unformatted = 0usize;
    let mut traced = 0usize;
    let mut mismatched = 0usize;
    for (path, label) in &resolved {
        let still_unformatted = if fix {
            fix_formatting(path, label)
        } else {
            check_formatting(path, label)
        };
        if still_unformatted {
            unformatted += 1;
        }
        aipl::codegen::set_test_file(Some(label));
        let outcome = check_resolved(path, label, &project_root, validate_sections, dbg);
        match outcome {
            Ok(checked) => {
                if let Some(report) = &checked.traces {
                    eprintln!("{report}");
                    traced += 1;
                }
                let remaining = if fix {
                    apply_fixes(path, label, checked.sections)
                } else {
                    checked.sections
                };
                if !remaining.is_empty() {
                    report_sections(label, &remaining);
                    mismatched += 1;
                }
            }
            Err(msg) => {
                eprintln!("{msg}");
                broken += 1;
            }
        }
    }
    aipl::codegen::set_test_file(None);

    let (total, passed, failed) = aipl::codegen::test_totals();
    let n = files.len();
    let files_word = if n == 1 { "file" } else { "files" };
    println!("{n} {files_word}, {total} tests: {passed} passed, {failed} failed");
    if broken > 0 {
        // Called out separately: a file that never compiled contributes no test
        // counts, so "0 failed" above must not read as "everything is fine".
        let word = if broken == 1 { "file" } else { "files" };
        println!("{broken} {word} failed to compile");
    }
    if unformatted > 0 {
        // Likewise: an unformatted file's tests may all have passed, so the
        // tallies above give no hint that the run is about to exit non-zero.
        let word = if unformatted == 1 {
            "file needs"
        } else {
            "files need"
        };
        println!("{unformatted} {word} formatting");
    }
    if traced > 0 {
        // And likewise: `trace(..)` is a debugging aid, so its files' tests
        // pass — the run still fails, and this is what says why.
        let phrase = if traced == 1 {
            "file still calls"
        } else {
            "files still call"
        };
        println!("{traced} {phrase} trace()");
    }
    if mismatched > 0 {
        // Likewise: a file whose sections disagree with what it does may have
        // passed every test, so "0 failed" says nothing about it.
        let (word, verb) = if mismatched == 1 {
            ("file", "has")
        } else {
            ("files", "have")
        };
        println!("{mismatched} {word} {verb} mismatched sections");
    }
    if failed > 0 || broken > 0 || unformatted > 0 || traced > 0 || mismatched > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `report <file.aipl> [--machine] [-- args...]` — run the file once in the
/// instrumented build and print what it cost.
///
/// What it runs is what the cases harness measures for the baseline table: the
/// file's `main`, or for a library with only `.test` blocks the synthesized test
/// driver (so the report is of the same run `aipl check` makes, minus
/// `.performance` blocks). It is built and run the way `check` stages a file —
/// `--- file: ---` companions beside it, in a scratch directory the program runs
/// in — so a program that reads fixtures or writes files neither fails nor
/// litters.
///
/// Exit 0, or 1 when the file does not compile, cannot be run, or leaks.
fn report_cmd(args: &[String]) -> ExitCode {
    let (args, dbg) = take_debug_flag(args);
    let mut machine = false;
    let mut file: Option<&str> = None;
    let mut program_args: Option<Vec<String>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                program_args = Some(args[i + 1..].to_vec());
                break;
            }
            "--machine" => machine = true,
            other if file.is_none() => file = Some(other),
            other => {
                eprintln!("unexpected arg {other:?}");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("missing source file");
        return ExitCode::FAILURE;
    };
    let Ok(path) = std::fs::canonicalize(file) else {
        eprintln!("no such file: {file}");
        return ExitCode::FAILURE;
    };
    let result = report_resolved(&path, file, program_args, dbg);
    let stats = match result {
        Ok(stats) => stats,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };
    if machine {
        println!("{}", stats.render_machine());
    } else {
        print!("{}", stats.render_report());
    }
    match stats.leaked() {
        Some(n) => {
            eprintln!(
                "leak: {n} of {} allocations were never freed ({} deallocations)",
                stats.allocations, stats.deallocations
            );
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}

/// [`report_cmd`] up to the printing: stage the file, build it, run it.
fn report_resolved(
    path: &Path,
    label: &str,
    program_args: Option<Vec<String>>,
    dbg: DebugOptions,
) -> Result<aipl::perf::PerfStats, String> {
    let spec = read_spec(path, label)?;
    // The file is a one-file project rooted at its own directory, not at wherever
    // `report` was run from. That root is what an `assert` or `trace` names its file
    // relative to, and the name is baked into the binary's data: rooted anywhere
    // else, `binary size` would depend on the working directory, and would stop
    // agreeing with the cases harness, which roots a case the same way.
    let project_root = path.parent().unwrap_or(path).to_path_buf();
    // Explicit arguments win; otherwise the file's own `--- cli ---`.
    let cli = program_args.unwrap_or_else(|| spec.cli.clone());
    in_staged_dir(path, &spec, &project_root, |staged| {
        report_file(staged, label, &cli, dbg)
    })?
}

fn report_file(
    staged: &Staged,
    label: &str,
    cli: &[String],
    dbg: DebugOptions,
) -> Result<aipl::perf::PerfStats, String> {
    let render = |e| render_err_at(&staged.entry, label, e);
    let program = loader::load_program_rooted(&staged.entry, &staged.root, dbg).map_err(render)?;
    let stem = staged
        .entry
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("a.out");
    // The staged scratch directory the program runs in (see `in_staged_dir`).
    let dir = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    aipl::perf::measure_program(&program, stem, cli, &dir, dbg).map_err(|e| match e {
        aipl::perf::MeasureError::Compile(errors) => render(errors),
        aipl::perf::MeasureError::Link(e) => e.to_string(),
        aipl::perf::MeasureError::Run(msg) => msg,
    })
}

/// Serve the Language Server Protocol on stdin/stdout.
///
/// Everything a server needs to know it is told by the client in `initialize`,
/// so the only argument accepted is `--stdio`, and it is accepted because
/// clients send it rather than because it decides anything: it is how a client
/// announces which transport it is using, and `vscode-languageclient` appends
/// it unprompted for `TransportKind.stdio`. Stdio is the only transport here,
/// so the flag is a no-op — and refusing it means refusing to start for a
/// client that did nothing wrong.
///
/// Anything else is reported. A stray argument is almost always a person
/// running this by hand expecting output.
fn lsp_cmd(args: &[String]) -> Result<(), String> {
    if let Some(arg) = args.iter().find(|arg| *arg != "--stdio") {
        return Err(format!(
            "`lsp` takes no arguments other than `--stdio` (got {arg:?}); it is spawned by an \
             editor and speaks the Language Server Protocol on stdin/stdout"
        ));
    }
    // An editor that goes away closes the pipe, which `serve_stdio` reports as
    // a broken pipe rather than as an error worth printing.
    match aipl::lsp::serve_stdio() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(format!("language server: {e}")),
    }
}

fn ir_cmd(args: &[String]) -> Result<(), String> {
    let (args, dbg) = take_debug_flag(args);
    let file = args.first().ok_or("missing source file")?;
    let program = loader::load_program(Path::new(file), dbg).map_err(|e| render_err(file, e))?;
    let comp = Compilation::new(&program, dbg).map_err(|e| render_err(file, e))?;
    print!("{}", comp.ir());
    Ok(())
}

/// `doc <file>` — print the file's own `# ..` documentation, then the block
/// above each declaration. Undocumented declarations are skipped. Parses just
/// this file (it doesn't resolve imports, compile, or run it), so docs come out
/// under the names written here — not the loader's cross-file-mangled forms —
/// and are available even for code that wouldn't otherwise build.
fn doc_cmd(args: &[String]) -> Result<(), String> {
    let (args, _dbg) = take_debug_flag(args);
    let file = args.first().ok_or("missing source file")?;
    let src = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    // Strip any trailing `--- .. ---` harness sections (some `.aipl` files carry
    // a `--- stdout ---` block) so the source parses on its own.
    let stripped = aipl::strip_test_sections(&src);
    let program = aipl::parse(stripped).map_err(|e| e.render(stripped, file))?;
    // The file's own documentation leads, under the file's name — it is about
    // no declaration, so it has no other name to print under.
    if let Some(doc) = &program.doc {
        print_doc(file, doc);
    }
    for item in &program.items {
        // Every kind of declaration a `# ..` block can sit above; an `import`
        // declares nothing, so it is the one item with no documentation slot.
        let (name, doc) = match item {
            aipl::ast::Item::Fn(f) => (&f.name, &f.doc),
            aipl::ast::Item::Struct(s) => (&s.name, &s.doc),
            aipl::ast::Item::Variant(v) => (&v.name, &v.doc),
            aipl::ast::Item::Refinement(r) => (&r.name, &r.doc),
            aipl::ast::Item::Const(c) => (&c.name, &c.doc),
            aipl::ast::Item::Import(_) => continue,
        };
        if let Some(doc) = doc {
            print_doc(name, doc);
        }
        // A variant's cases carry docs of their own, printed as `Variant.Case`
        // so a case reads as the alternative of its type that it is.
        if let aipl::ast::Item::Variant(v) = item {
            for case in &v.cases {
                if let Some(doc) = &case.doc {
                    print_doc(&format!("{}.{}", v.name, case.name), doc);
                }
            }
        }
    }
    Ok(())
}

/// One documented name: the name on its own line, its doc lines indented
/// beneath it, and a blank line after.
fn print_doc(name: &str, doc: &str) {
    println!("{name}");
    for line in doc.lines() {
        println!("    {line}");
    }
    println!();
}

/// `docs [path...] [-o <dir>] [--name <title>]` — write an HTML documentation
/// site for a project's AIPL source.
///
/// The whole-project counterpart of [`doc_cmd`]: that one prints one file's
/// documentation to stdout, this one indexes every `.aipl` under each
/// `path` and writes a browsable static site. Several paths document one site,
/// which is how a project keeps its library and its examples together
/// (`aipl docs crates examples`).
///
/// Paths in the site are shown relative to the working directory, whatever the
/// arguments were, so a page reads `crates/aipl-codegen/src/grammar.aipl`.
///
/// Files that do not parse are reported and skipped rather than failing the
/// run — a project usually has a few deliberate error fixtures, and documenting
/// the rest of it is still worth doing.
fn docs_cmd(args: &[String]) -> Result<(), String> {
    let (args, _dbg) = take_debug_flag(args);
    let mut paths: Vec<&str> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" => {
                let dir = args.get(i + 1).ok_or("-o needs a directory")?;
                out = Some(PathBuf::from(dir));
                i += 2;
            }
            "--name" => {
                name = Some(args.get(i + 1).ok_or("--name needs a title")?.clone());
                i += 2;
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unexpected argument {flag:?}"));
            }
            path => {
                paths.push(path);
                i += 1;
            }
        }
    }
    if paths.is_empty() {
        paths.push(".");
    }
    let out = out.unwrap_or_else(|| PathBuf::from("docs"));

    aipl::install_parser_hooks();
    // A file argument documents just that file; a directory is walked (the same
    // walk `check` uses, so the two agree on what counts as project source).
    let mut files = Vec::new();
    for path in &paths {
        let path = Path::new(path);
        if path.is_file() {
            files.push(path.to_path_buf());
        } else {
            collect_aipl(path, &mut files);
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err(format!("no .aipl files under {}", paths.join(", ")));
    }

    let mut index = aipl::index::Index::new();
    let mut skipped = 0usize;
    for path in &files {
        let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // A file that does not parse says nothing about the rest of the tree.
        if index.add(path.clone(), &src).is_err() {
            skipped += 1;
        }
    }

    // Paths are displayed relative to the working directory rather than to any
    // one argument, so `aipl docs crates examples` shows both trees under the
    // names they are known by.
    let here = Path::new(".");
    let project = name.unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "AIPL".to_string())
    });
    aipl::docs::write_site(&index, here, &project, &out).map_err(|e| e.to_string())?;

    let documented = files.len() - skipped;
    println!(
        "wrote {} to {}",
        plural(documented, "page"),
        out.join("index.html").display()
    );
    if skipped > 0 {
        println!("skipped {} that did not parse", plural(skipped, "file"));
    }
    Ok(())
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("{n} {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn build_cmd(args: &[String]) -> Result<(), String> {
    let mut file: Option<&str> = None;
    let mut output: Option<PathBuf> = None;
    let mut dbg = DebugOptions::OFF;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                let v = args.get(i + 1).ok_or("`-o` requires a path")?;
                output = Some(PathBuf::from(v));
                i += 2;
            }
            "--debug" => {
                dbg = DebugOptions::new(true);
                i += 1;
            }
            other => {
                if file.is_some() {
                    return Err(format!("unexpected arg {other:?}"));
                }
                file = Some(other);
                i += 1;
            }
        }
    }
    let file = file.ok_or("missing source file")?;
    let src_path = Path::new(file);
    let stem = src_path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("a.out");
    let output = output.unwrap_or_else(|| PathBuf::from(binary::default_exe_name(stem)));

    let program = loader::load_program(src_path, dbg).map_err(|e| render_err(file, e))?;
    let comp =
        ObjectCompilation::new(&program, stem, dbg, false).map_err(|e| render_err(file, e))?;
    // After the compile, so a genuine error is what a broken program hears about,
    // and before anything is written, so a refused build leaves no executable
    // behind. `trace` is for watching a program run (`aipl run`), and a binary
    // that outlives that is exactly what it must not reach — which is what lets it
    // print without declaring an effect. See `trace_diagnostics`.
    let traces = aipl::trace_diagnostics(&program, "build");
    if !traces.is_empty() {
        return Err(render_err(file, traces));
    }
    let obj_bytes = comp.emit().map_err(|e| e.to_string())?;
    binary::link(&obj_bytes, &output).map_err(|e| e.to_string())?;
    println!("wrote {}", output.display());
    Ok(())
}
