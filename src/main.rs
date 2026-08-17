use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aipl::binary;
use aipl::codegen::{Compilation, ObjectCompilation};
use aipl::loader;
use aipl::{DebugOptions, Error};

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
        Some("build") => build_cmd(&args[2..]),
        // `fmt` owns its exit code (`--check` reports needs-formatting as 1).
        Some("fmt") => return fmt_cmd(&args[2..]),
        // `check` owns its exit code (0 = all tests passed, 1 = a failure) and
        // prints its own report, so it returns an `ExitCode` directly.
        Some("check") => return check_cmd(&args[2..]),
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
  {prog} doc   <file.aipl>                  print each fn's `.doc(\"..\")` documentation
  {prog} build <file.aipl> [-o <output>]    link a native binary executable
  {prog} fmt   <file.aipl> [--check]        rewrite the file in canonical format
  {prog} check [path...]                    run every fn's `.test({{ .. }})` block

`check` with no path checks every `.aipl` file under the working directory (one
process, so the engine links once); pass a file to check just that one, or a
directory to check the tree under it. It reports every failure rather than
stopping at the first, and exits 0 only if all of them compiled and passed.

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

/// `fmt <file> [--check]` — rewrite the file in canonical format (in place).
/// With `--check`, write nothing: exit 0 if already formatted, 1 if not (or on
/// any error). Trailing `--- section ---` blocks are preserved byte-for-byte.
fn fmt_cmd(args: &[String]) -> ExitCode {
    let mut file: Option<&str> = None;
    let mut check = false;
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            other => {
                if file.is_some() {
                    eprintln!("unexpected arg {other:?}");
                    return ExitCode::FAILURE;
                }
                file = Some(other);
            }
        }
    }
    let Some(file) = file else {
        eprintln!("missing source file");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{file}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let formatted = match aipl::fmt::format_source(&src, &aipl::fmt::FmtOptions::default()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}", e.render(aipl::strip_test_sections(&src), file));
            return ExitCode::FAILURE;
        }
    };
    if formatted == src {
        return ExitCode::SUCCESS;
    }
    if check {
        eprintln!("{file}: needs formatting");
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(file, &formatted) {
        eprintln!("{file}: {e}");
        return ExitCode::FAILURE;
    }
    println!("formatted {file}");
    ExitCode::SUCCESS
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

/// Load, compile, and run one file's `__test_main` driver, returning its exit
/// code (0 = every test passed). `Err` is a rendered load/compile failure.
///
/// `file` is absolute (so loading and its relative imports resolve no matter
/// what the working directory is); `label` is the path as the user typed or as
/// discovery found it, and is what diagnostics show.
///
/// The pass/fail tallies live in the test runtime and accumulate across calls,
/// which is what lets a batch run report one aggregate at the end.
fn check_file(file: &Path, label: &str, dbg: DebugOptions) -> Result<i64, String> {
    let render = |e| render_err_at(file, label, e);
    let program = loader::load_program(file, dbg).map_err(render)?;
    let test_program = aipl::codegen::build_test_program(&program);
    // `__test_main` runs each test and returns the exit code (0 ok, 1 failures),
    // printing failures itself. (Runs on `main`'s large-stack worker thread,
    // which gives codegen room for deep `.test` driver/expression trees.)
    let comp = Compilation::new(&test_program, dbg).map_err(render)?;
    comp.run_0("__test_main").map_err(|e| e.to_string())
}

/// Run `f` with the process working directory pointed at a fresh scratch
/// directory holding `file`'s `--- file: ---` companions.
///
/// A `.test` block is ordinary code: it can write files, and it resolves
/// relative paths against the working directory. Run in place, `aipl check`
/// would scatter whatever its tests write across the tree it was invoked from —
/// so tests get a scratch directory instead, staged with the sibling files a
/// case expects to find (fixtures its tests read, modules they import).
///
/// Loading still happens from the original absolute path, so imports resolve
/// against the real tree rather than the scratch copy.
fn in_staged_dir<T>(file: &Path, f: impl FnOnce() -> T) -> Result<T, String> {
    let companions = match std::fs::read_to_string(file) {
        // A `file:` marker naming an unstageable path is the file's error to
        // report, not something to quietly stage somewhere else.
        Ok(src) => aipl::companion_files(&src).map_err(|e| format!("{}: {e}", file.display()))?,
        // Unreadable is not fatal here — loading the file below reports it with
        // the diagnostic the user actually wants.
        Err(_) => Vec::new(),
    };
    let dir = binary::scratch_dir("check").map_err(|e| e.to_string())?;
    aipl::stage_companions(&dir, &companions)
        .map_err(|e| format!("stage companions in {}: {e}", dir.display()))?;
    let prev = env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    env::set_current_dir(&dir).map_err(|e| format!("enter {}: {e}", dir.display()))?;
    let out = f();
    // Restore first, so a failure to clean up can't leave the process parked in
    // a directory it is about to delete.
    let restored = env::set_current_dir(&prev);
    let _ = std::fs::remove_dir_all(&dir);
    restored.map_err(|e| format!("return to {}: {e}", prev.display()))?;
    Ok(out)
}

/// `check [path...]` — JIT-run every function's `.test({ .. })` block and report.
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
/// A file that fails to compile is reported and does *not* stop the run; you get
/// every problem in the codebase from one invocation rather than the first one.
/// Exit code 0 only if every file compiled and every test passed.
fn check_cmd(args: &[String]) -> ExitCode {
    let (args, dbg) = take_debug_flag(args);

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
        let outcome = match in_staged_dir(path, || check_file(path, label, dbg)) {
            Ok(o) => o,
            Err(msg) => Err(msg),
        };
        return match outcome {
            Ok(0) => ExitCode::SUCCESS,
            Ok(_) => ExitCode::FAILURE,
            Err(msg) => {
                eprintln!("{msg}");
                ExitCode::FAILURE
            }
        };
    }

    aipl::codegen::set_quiet_summary(true);
    let mut broken = 0usize;
    for (path, label) in &resolved {
        aipl::codegen::set_test_file(Some(label));
        let outcome = match in_staged_dir(path, || check_file(path, label, dbg)) {
            Ok(o) => o,
            Err(msg) => Err(msg),
        };
        if let Err(msg) = outcome {
            eprintln!("{msg}");
            broken += 1;
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
    if failed > 0 || broken > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
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

/// `doc <file>` — print each function's attached `.doc("..")` documentation.
/// Functions without a `.doc` are skipped. Parses just this file (it doesn't
/// resolve imports, compile, or run it), so docs come out under the names
/// written here — not the loader's cross-file-mangled forms — and are available
/// even for code that wouldn't otherwise build.
fn doc_cmd(args: &[String]) -> Result<(), String> {
    let (args, _dbg) = take_debug_flag(args);
    let file = args.first().ok_or("missing source file")?;
    let src = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    // Strip any trailing `--- .. ---` harness sections (some `.aipl` files carry
    // a `--- performance ---` block) so the source parses on its own.
    let stripped = aipl::strip_test_sections(&src);
    let program = aipl::parse(stripped).map_err(|e| e.render(stripped, file))?;
    for item in &program.items {
        let aipl::ast::Item::Fn(f) = item else {
            continue;
        };
        let Some(doc) = &f.doc else { continue };
        println!("{}", f.name);
        for line in doc.lines() {
            println!("    {line}");
        }
        println!();
    }
    Ok(())
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
    let obj_bytes = comp.emit().map_err(|e| e.to_string())?;
    binary::link(&obj_bytes, &output).map_err(|e| e.to_string())?;
    println!("wrote {}", output.display());
    Ok(())
}
