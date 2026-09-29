//! The `aipl doc` command: prints the file's own `# ..` documentation, then the
//! block above each declaration, skipping undocumented ones. (A CLI surface the
//! `.aipl` cases framework, which only `run`s/`check`s, can't exercise.)

use std::process::Command;

/// Write `src` to a temp `.aipl` file and run `aipl doc` on it, returning
/// stdout. `tag` keeps two tests in one process from sharing a file — and, since
/// the file's own documentation prints under its path, from sharing a heading.
fn run_doc_named(tag: &str, src: &str) -> String {
    let path = std::env::temp_dir().join(format!("aipl_doc_{tag}_{}.aipl", std::process::id()));
    std::fs::write(&path, src).expect("write temp source");
    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .arg("doc")
        .arg(&path)
        .output()
        .expect("spawn aipl doc");
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "`aipl doc` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn run_doc(src: &str) -> String {
    run_doc_named("items", src)
}

/// The file's own block — the one a blank line detaches from what follows —
/// prints first, under the file's name, since it is about no declaration.
#[test]
fn prints_the_files_own_documentation_first() {
    let src = "\
# What this file is for.

import { print } from builtins;

# Says hello.
pub fn hello() !prints { print(\"hi\"); }
";
    let out = run_doc_named("module", src);
    let file = out.find(".aipl\n    What this file is for.\n").expect(&out);
    let hello = out.find("hello\n    Says hello.\n").expect(&out);
    assert!(file < hello, "the file's own docs lead:\n{out}");
}

/// A block that runs straight into the first declaration documents it, and the
/// file gets no heading of its own.
#[test]
fn a_block_above_the_first_declaration_is_not_the_files() {
    let out = run_doc_named("attached", "# Says hello.\npub fn hello() -> i64 { 1 }\n");
    assert!(out.starts_with("hello\n    Says hello.\n"), "{out}");
    assert!(!out.contains(".aipl"), "{out}");
}

#[test]
fn prints_docs_and_skips_undocumented() {
    let src = "\
import { wrapping_add as + } from builtins;
# Adds two integers.
pub fn add(a: i64, b: i64) -> i64 { a + b }
fn helper(x: i64) -> i64 { x }
# Doubles n.
# Across two lines.
pub fn doubled(n: i64) -> i64 { n + n }
# A point in the plane.
struct Point { x: i64 }
# Either something or nothing.
variant Maybe =
    # Something.
    | Yes
    | No
";
    let out = run_doc(src);
    // Single-line doc, indented under the function name.
    assert!(
        out.contains("add\n    Adds two integers.\n"),
        "missing add doc:\n{out}"
    );
    // Multi-line doc, re-indented per line.
    assert!(
        out.contains("doubled\n    Doubles n.\n    Across two lines.\n"),
        "missing doubled doc:\n{out}"
    );
    // Undocumented functions are skipped entirely.
    assert!(!out.contains("helper"), "undocumented fn leaked:\n{out}");
    // Documentation is no longer a function-only thing.
    assert!(
        out.contains("Point\n    A point in the plane.\n"),
        "missing struct doc:\n{out}"
    );
    assert!(
        out.contains("Maybe\n    Either something or nothing.\n"),
        "missing variant doc:\n{out}"
    );
    // A documented case is printed as `Variant.Case`; an undocumented one is
    // skipped like any other undocumented declaration.
    assert!(
        out.contains("Maybe.Yes\n    Something.\n"),
        "missing case doc:\n{out}"
    );
    assert!(
        !out.contains("Maybe.No"),
        "undocumented case leaked:\n{out}"
    );
}
