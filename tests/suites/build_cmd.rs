//! The `aipl build` command's refusal of `trace(..)`.
//!
//! A CLI surface the `.aipl` cases framework can't exercise: the harness links a
//! case's binary through the library (`ObjectCompilation`), not through this
//! command, which is deliberate — a case like `debug/trace.aipl` *is* a traced
//! program whose output is the thing under test, so the refusal lives in the
//! command rather than in the compile.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `trace`'s value is that it needs no import and declares no effect, so a
/// function carrying one has an ordinary signature.
const TRACED: &str = "import { wrapping_add as + } from builtins;\n\n\
     fn bump(v: i64) -> i64 { trace(v + 2) }\n\n\
     fn main() {\n    bump(5);\n}\n";
/// The same program with the trace taken out — the control, so a passing refusal
/// test can't be a build that was broken for some other reason.
const PLAIN: &str = "import { wrapping_add as + } from builtins;\n\n\
     fn bump(v: i64) -> i64 { v + 2 }\n\n\
     fn main() {\n    bump(5);\n}\n";

/// Build `src` in a directory of its own, naming the executable explicitly so the
/// test can ask whether one was produced. Returns `(stderr, exit_code,
/// output_path)`.
fn build(name: &str, src: &str) -> (String, i32, PathBuf) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("build_cmd")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("program.aipl");
    std::fs::write(&path, src).expect("write temp source");
    let out_path = dir.join(aipl::binary::default_exe_name("program"));
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("build")
        .arg(&path)
        .arg("-o")
        .arg(&out_path)
        .output()
        .expect("spawn aipl build");
    (
        String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        out.status.code().unwrap_or(-1),
        out_path,
    )
}

#[test]
fn a_trace_is_refused_and_no_executable_is_written() {
    let (stderr, code, out_path) = build("traced", TRACED);
    assert!(
        stderr.contains("`trace(..)` prints for debugging"),
        "expected the trace diagnostic, got:\n{stderr}"
    );
    // The diagnostic names the command that is refusing, not a fixed one.
    assert!(
        stderr.contains("so `build` refuses"),
        "expected `build` to name itself, got:\n{stderr}"
    );
    // The caret underlines the traced expression, not the whole `trace(..)` call.
    assert!(stderr.contains("trace(v + 2)"), "got:\n{stderr}");
    assert_eq!(code, 1);
    // The whole point: a refused build leaves nothing behind to run. A trace that
    // reached a linked binary would be a print with no effect declared anywhere.
    assert!(
        !out_path.exists(),
        "a refused build wrote {}",
        out_path.display()
    );
}

#[test]
fn the_same_program_without_the_trace_builds() {
    let (stderr, code, out_path) = build("plain", PLAIN);
    assert_eq!(code, 0, "expected a clean build, stderr:\n{stderr}");
    assert!(
        out_path.exists(),
        "expected an executable at {}",
        out_path.display()
    );
}

#[test]
fn a_compile_error_is_reported_instead_of_the_trace() {
    // The refusal sits *after* the compile, so a program that is broken for a real
    // reason hears about that reason — a trace is trivially removed, and reporting
    // it first would bury the error that actually needs thought.
    let broken = "import { wrapping_add as + } from builtins;\n\n\
         fn bump(v: i64) -> str { trace(v + 2) }\n\n\
         fn main() {\n    bump(5);\n}\n";
    let (stderr, code, _) = build("broken", broken);
    assert!(
        stderr.contains("declared return type is str"),
        "expected the type error, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("`trace(..)` prints for debugging"),
        "the type error should be the only diagnostic, got:\n{stderr}"
    );
    assert_eq!(code, 1);
}

#[test]
fn run_does_not_refuse_a_trace() {
    // The deliberate asymmetry, pinned here because it is what makes `build`'s
    // refusal affordable: running a program to watch what it does is the case
    // `trace` exists for, so `run` prints the trace and exits 0.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_cmd");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("run_traced.aipl");
    std::fs::write(&path, TRACED).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("run")
        .arg(&path)
        .arg("main")
        .output()
        .expect("spawn aipl run");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert!(
        stdout.contains("run_traced.aipl:3 v + 2 = 7"),
        "expected the trace's output, got:\n{stdout}"
    );
    assert_eq!(out.status.code().unwrap_or(-1), 0);
    assert!(
        !Path::new(&dir.join("run_traced")).exists(),
        "`run` should not produce an executable"
    );
}
