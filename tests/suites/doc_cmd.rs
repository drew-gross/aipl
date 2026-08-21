//! The `aipl doc` command: prints each function's attached `.doc("..")`
//! documentation and skips undocumented ones. (A CLI surface the `.aipl` cases
//! framework, which only `run`s/`check`s, can't exercise.)

use std::process::Command;

/// Write `src` to a temp `.aipl` file and run `aipl doc` on it, returning stdout.
fn run_doc(src: &str) -> String {
    let path = std::env::temp_dir().join(format!("aipl_doc_{}.aipl", std::process::id()));
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

#[test]
fn prints_docs_and_skips_undocumented() {
    let src = "\
import { wrapping_add as + } from builtins;
pub fn add(a: i64, b: i64) -> i64 { a + b }.doc(\"Adds two integers.\")
fn helper(x: i64) -> i64 { x }
pub fn doubled(n: i64) -> i64 { n + n }.doc(\"\"\"
Doubles n.
Across two lines.
\"\"\")
";
    let out = run_doc(src);
    // Single-line doc, de-dented and indented under the function name.
    assert!(
        out.contains("add\n    Adds two integers.\n"),
        "missing add doc:\n{out}"
    );
    // Multi-line raw-string doc, de-dented then re-indented per line.
    assert!(
        out.contains("doubled\n    Doubles n.\n    Across two lines.\n"),
        "missing doubled doc:\n{out}"
    );
    // Undocumented functions are skipped entirely.
    assert!(!out.contains("helper"), "undocumented fn leaked:\n{out}");
}
