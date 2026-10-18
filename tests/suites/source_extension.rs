//! Every command that takes a source file refuses one whose name does not end in
//! `.aipl`, before it reads it. (A CLI surface the `.aipl` cases framework, whose
//! files are all `.aipl`, can't exercise; an *imported* file's name is a case,
//! `cases/imports/err_import_non_aipl_file.aipl`.)

use std::path::PathBuf;
use std::process::Command;

/// A temp file named `name` holding a program that would run fine — the name is
/// all that is wrong with it. `tag` keeps tests in one process apart.
fn source_named(tag: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aipl_ext_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(name);
    std::fs::write(&path, "fn main() {}\n").expect("write temp source");
    path
}

/// What `aipl <args>` prints to stderr, and that it failed.
fn refused(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(args)
        .output()
        .expect("spawn aipl");
    assert!(!out.status.success(), "`aipl {args:?}` should have failed");
    String::from_utf8(out.stderr).expect("utf8 stderr")
}

fn message(path: &std::path::Path) -> String {
    format!(
        "{}: not an AIPL source file, whose name must end in `.aipl`",
        path.display()
    )
}

#[test]
fn ir_build_and_report_refuse_a_file_without_the_extension() {
    let path = source_named("compile", "program.txt");
    let file = path.to_str().expect("utf8 path");
    let out = path.with_file_name("out");
    let out = out.to_str().expect("utf8 path");
    for args in [
        vec!["ir", file],
        vec!["build", file, "-o", out],
        vec!["report", file],
    ] {
        assert_eq!(
            refused(&args),
            format!("error: {}\n", message(&path)),
            "{args:?}"
        );
    }
}

#[test]
fn doc_docs_and_check_refuse_a_file_without_the_extension() {
    let path = source_named("inspect", "program.txt");
    let file = path.to_str().expect("utf8 path");
    for args in [vec!["doc", file], vec!["docs", file], vec!["check", file]] {
        assert_eq!(refused(&args), format!("{}\n", message(&path)), "{args:?}");
    }
}

/// The extension is matched exactly: a name that merely contains `aipl`, has no
/// extension, or spells it in capitals is not one.
#[test]
fn only_the_exact_extension_counts() {
    for name in [
        "program",
        "program.aipl.bak",
        "program.AIPL",
        "program.aip",
        "aipl",
    ] {
        let path = source_named("exact", name);
        let file = path.to_str().expect("utf8 path");
        assert_eq!(
            refused(&["ir", file]),
            format!("error: {}\n", message(&path)),
            "{name}"
        );
    }
}

/// With the extension the same program runs, so the refusal above is about the
/// name and nothing else.
#[test]
fn the_same_program_runs_with_the_extension() {
    let path = source_named("accepted", "main.aipl");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("run")
        .current_dir(path.parent().expect("temp dir"))
        .output()
        .expect("spawn aipl");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
