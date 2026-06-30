use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aipl::binary;
use aipl::codegen::{Compilation, ObjectCompilation};
use aipl::loader;
use aipl::{DebugOptions, Error};

/// Render a compiler error with a source caret when possible. Spans are
/// relative to `file`'s own source (correct for a single-file program; for an
/// imported-file error only the caret line may be off — the message is still
/// right), falling back to the plain message if the file can't be read.
fn render_err(file: &str, e: Error) -> String {
    match std::fs::read_to_string(file) {
        Ok(src) => e.render(aipl::strip_test_sections(&src), file),
        Err(_) => e.to_string(),
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
  {prog} check <file.aipl>                  run every fn's `.test({{ .. }})` block

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

/// `check <file>` — JIT-run every function's `.test({ .. })` block and report.
/// Returns exit code 0 if all tests pass, 1 if any assertion fails (or on a
/// load/compile error). The pass/fail report is printed by the test runtime.
fn check_cmd(args: &[String]) -> ExitCode {
    let (args, dbg) = take_debug_flag(args);
    let file = match args.first() {
        Some(f) => f,
        None => {
            eprintln!("missing source file");
            return ExitCode::FAILURE;
        }
    };
    let program = match loader::load_program(Path::new(file), dbg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", render_err(file, e));
            return ExitCode::FAILURE;
        }
    };
    let test_program = aipl::codegen::build_test_program(&program);
    // `__test_main` runs each test and returns the exit code (0 ok, 1 failures),
    // printing the report itself. (Runs on `main`'s large-stack worker thread,
    // which gives codegen room for deep `.test` driver/expression trees.)
    let outcome = (|| {
        let comp = Compilation::new(&test_program, dbg).map_err(|e| render_err(file, e))?;
        comp.run_0("__test_main").map_err(|e| e.to_string())
    })();
    match outcome {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
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
