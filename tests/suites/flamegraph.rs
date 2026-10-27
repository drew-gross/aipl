//! `aipl run --flamegraph`: a sampled run writes an SVG flame graph. Sampling is
//! not deterministic, so this pins what every run must have — the program's own
//! output untouched, and every sample rooted at `main` — rather than counts.
//! (The layout itself is pinned exactly by the unit tests in
//! `src/flamegraph.rs`.)

use std::process::Command;

/// The tooltip of every box in `svg`, in drawing order: root first, then
/// depth first.
fn titles(svg: &str) -> Vec<&str> {
    svg.split("<title>")
        .skip(1)
        .map(|rest| &rest[..rest.find("</title>").expect("closed title")])
        .collect()
}

#[test]
fn a_profiled_run_roots_every_sample_at_main() {
    let dir = std::env::temp_dir().join(format!("aipl_flamegraph_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(
        dir.join("main.aipl"),
        r#"import { less_than as <, wrapping_add as +, wrapping_sub as -, print } from builtins;

fn fib(n: i64) -> i64 {
    if (n < 2) { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() !prints {
    print(`{fib(35)}`);
}
"#,
    )
    .expect("write main.aipl");

    let out = Command::new(env!("CARGO_BIN_EXE_aipl"))
        .args(["run", "--flamegraph", "profile.svg"])
        .current_dir(&dir)
        .output()
        .expect("spawn aipl");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "9227465\n");
    assert!(out.status.success());

    let svg = std::fs::read_to_string(dir.join("profile.svg")).expect("read profile.svg");
    let titles = titles(&svg);
    let samples = titles[0]
        .strip_prefix("all (")
        .and_then(|t| t.strip_suffix(" samples, 100.00%)"))
        .unwrap_or_else(|| panic!("root box {:?}", titles[0]));
    assert_eq!(titles[1], format!("main ({samples} samples, 100.00%)"));
    assert!(
        titles.iter().any(|t| t.starts_with("fib (")),
        "no fib frame in {titles:?}"
    );
}
