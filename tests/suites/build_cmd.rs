//! The `aipl build` command's refusal of `trace(..)`.
//!
//! A CLI surface the `.aipl` cases framework can't exercise: the harness links a
//! case's binary through the library (`ObjectCompilation`), not through this
//! command, which is deliberate — a case like `debug/trace.aipl` *is* a traced
//! program whose output is the thing under test, so the refusal lives in the
//! command rather than in the compile.

use std::path::PathBuf;
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
    let run_dir = dir.join("run_traced");
    std::fs::create_dir_all(&run_dir).expect("create temp dir");
    std::fs::write(run_dir.join("main.aipl"), TRACED).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("run")
        .current_dir(&run_dir)
        .output()
        .expect("spawn aipl run");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    assert!(
        stdout.contains("main.aipl:3 v + 2 = 7"),
        "expected the trace's output, got:\n{stdout}"
    );
    assert_eq!(out.status.code().unwrap_or(-1), 0);
    assert!(
        !run_dir.join("main").exists(),
        "`run` should not produce an executable"
    );
}

#[test]
fn run_passes_what_follows_the_separator_to_main() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_cmd_run_args");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(
        dir.join("main.aipl"),
        "import { print } from builtins;\n\n\
         fn main(args: str[]) !prints -> i64 {\n    match (args[0]) {\n        none => {\n            print(\"usage\");\n            1\n        },\n        some(a) => {\n            print(a);\n            0\n        },\n    }\n}\n",
    )
    .expect("write temp source");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
            .arg("run")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("spawn aipl run");
        (
            String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
            out.status.success(),
        )
    };
    assert_eq!(run(&["--", "hello"]), ("hello\n".to_string(), true));
    // Without `--`, an argument is `run`'s own, and `run` has none yet.
    assert!(!run(&["hello"]).1);
}

/// Echoes each line of standard input in brackets, then the count.
const ECHO_LINES: &str = "import { print, read_line, wrapping_add as + } from builtins;\n\n\
     fn echo(count: i64) !prints !read_stdin -> i64 {\n\
     \x20   match (read_line()) {\n\
     \x20       none => count, #[allow]\n\
     \x20       some(line) => {\n\
     \x20           print(`[{line}]`);\n\
     \x20           echo(count + 1)\n\
     \x20       },\n\
     \x20   }\n\
     }\n\n\
     fn main() !prints !read_stdin {\n\
     \x20   print(`{echo(0)} lines`);\n\
     }\n";

/// What `ECHO_LINES` prints for `input` as piped to `command`'s standard input.
fn echo_lines(mut command: Command, input: &str) -> String {
    use std::io::Write;
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the program");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for the program");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

/// Line endings come off, an empty line is a line, and a last line with no
/// newline is still one.
const LINES_IN: &str = "one\r\ntwo\n\nthree";
const LINES_OUT: &str = "[one]\n[two]\n[]\n[three]\n4 lines\n";

#[test]
fn read_line_reads_piped_lines_under_run() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_cmd_read_line_run");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(dir.join("main.aipl"), ECHO_LINES).expect("write temp source");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aipl"));
    command.arg("run").current_dir(&dir);
    assert_eq!(echo_lines(command, LINES_IN), LINES_OUT);
}

#[test]
fn read_line_reads_piped_lines_in_a_built_binary() {
    let (stderr, code, exe) = build("read_line", ECHO_LINES);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(echo_lines(Command::new(exe), LINES_IN), LINES_OUT);
}

#[test]
fn read_line_returns_long_lines_whole() {
    let (stderr, code, exe) = build("read_line_long", ECHO_LINES);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    let long = "x".repeat(10_000);
    assert_eq!(
        echo_lines(Command::new(exe), &format!("{long}\n")),
        format!("[{long}]\n1 lines\n")
    );
}

/// Reads two bytes, the rest of the line, one more byte, then past the end: the
/// bytes and the line come from one stream, in order.
const READ_BYTES: &str = "import { print, read_byte, read_line, to_str } from builtins;\n\n\
     fn main() !prints !read_stdin !read_stdin_raw {\n\
     \x20   print(to_str(read_byte()));\n\
     \x20   print(to_str(read_byte()));\n\
     \x20   print(to_str(read_line()));\n\
     \x20   print(to_str(read_byte()));\n\
     \x20   print(to_str(read_byte()));\n\
     }\n";

/// An arrow key's bytes, a newline, then a letter with nothing after it.
const BYTES_IN: &str = "\x1b[A\nz";
const BYTES_OUT: &str = "some(27)\nsome(91)\nsome(\"A\")\nsome(122)\nnone\n";

#[test]
fn read_byte_reads_piped_bytes_under_run() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_cmd_read_byte_run");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(dir.join("main.aipl"), READ_BYTES).expect("write temp source");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aipl"));
    command.arg("run").current_dir(&dir);
    assert_eq!(echo_lines(command, BYTES_IN), BYTES_OUT);
}

#[test]
fn read_byte_reads_piped_bytes_in_a_built_binary() {
    let (stderr, code, exe) = build("read_byte", READ_BYTES);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(echo_lines(Command::new(exe), BYTES_IN), BYTES_OUT);
}

/// Prints the name of each key press until input ends.
const READ_KEYS: &str =
    "import { Down, Key, Left, Other, Right, Up, print, read_key } from builtins;\n\n\
     fn name(self: Key) -> str {\n\
     \x20   match (self) {\n\
     \x20       Up => \"up\",\n\
     \x20       Down => \"down\",\n\
     \x20       Left => \"left\",\n\
     \x20       Right => \"right\",\n\
     \x20       Other(byte) => `other {byte}`,\n\
     \x20   }\n\
     }\n\n\
     fn show() !prints !read_stdin_raw {\n\
     \x20   match (read_key()) {\n\
     \x20       none => {},\n\
     \x20       some(key) => {\n\
     \x20           print(key.name());\n\
     \x20           show();\n\
     \x20       },\n\
     \x20   };\n\
     }\n\n\
     fn main() !prints !read_stdin_raw {\n\
     \x20   show();\n\
     }\n";

/// The four arrows, a plain key, and an escape that is not an arrow.
const KEYS_IN: &str = "\x1b[A\x1b[B\x1b[C\x1b[Dx\x1bz";
const KEYS_OUT: &str = "up\ndown\nright\nleft\nother 120\nother 122\n";

#[test]
fn read_key_decodes_piped_arrow_keys_under_run() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_cmd_read_key_run");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(dir.join("main.aipl"), READ_KEYS).expect("write temp source");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aipl"));
    command.arg("run").current_dir(&dir);
    assert_eq!(echo_lines(command, KEYS_IN), KEYS_OUT);
}

#[test]
fn read_key_decodes_piped_arrow_keys_in_a_built_binary() {
    let (stderr, code, exe) = build("read_key", READ_KEYS);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(echo_lines(Command::new(exe), KEYS_IN), KEYS_OUT);
}

/// Clears the terminal, then puts the cursor at the third row, fifth column.
const TERMINAL: &str = "import { clear_terminal, move_to } from builtins;\n\n\
     fn main() !terminal {\n\
     \x20   clear_terminal();\n\
     \x20   move_to(2, 4);\n\
     }\n";

/// Both control sequences, with nothing after them: no newline, unlike `print`.
const TERMINAL_BYTES: &[u8] = b"\x1b[2J\x1b[H\x1b[3;5H";

#[test]
fn terminal_operations_send_their_control_sequences() {
    let (stderr, code, exe) = build("terminal", TERMINAL);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    let out = Command::new(exe).output().expect("run the program");
    assert!(out.status.success());
    assert_eq!(out.stdout, TERMINAL_BYTES);
}

#[test]
fn terminal_operations_send_their_control_sequences_under_run() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_cmd_terminal_run");
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    std::fs::write(dir.join("main.aipl"), TERMINAL).expect("write the source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("run")
        .current_dir(&dir)
        .output()
        .expect("run the program");
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, TERMINAL_BYTES);
}
