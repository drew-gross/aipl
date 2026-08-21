//! Frontend/pipeline suites, merged into one test target.
//!
//! Each suite below was its own `tests/*.rs` integration target. Every extra
//! target is a full static link of the ~77-crate compiler closure (~6s each),
//! and that link — not compiling the test source — dominated build time. They
//! are `mod`s of one target instead, which keeps each suite's imports isolated
//! (crate-root splicing via `include!` collides on duplicate `use`).
//!
//! Consequence: test names are module-qualified — `fmt::format_corpus`, not
//! `format_corpus`. Filter a single suite with `cargo test --test compiler -- fmt::`.
#[path = "suites/check.rs"]
mod check;
#[path = "suites/codegen.rs"]
mod codegen;
#[path = "suites/doc_cmd.rs"]
mod doc_cmd;
#[path = "suites/fmt.rs"]
mod fmt;
#[path = "suites/highlighting.rs"]
mod highlighting;
#[path = "suites/mono.rs"]
mod mono;
#[path = "suites/parser.rs"]
mod parser;
#[path = "suites/shims.rs"]
mod shims;

use std::collections::BTreeMap;
use std::path::Path;

/// Every file in `tests/suites/` must be registered with a `#[path] mod` line in
/// exactly one merged target.
///
/// Cargo auto-discovers only `tests/*.rs` and `tests/*/main.rs`, so a suite added
/// under `tests/suites/` without a `mod` line compiles nothing and runs nothing —
/// it fails silently and looks green, the same hazard `every_case_has_a_test`
/// guards for cases. That makes this a hard failure rather than a warning.
///
/// Only this direction needs checking: a `mod` line naming a missing file is
/// already a compile error. Double-registration is checked too — it would build
/// and run the suite twice, once per target.
#[test]
fn every_suite_is_registered() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");

    let mut on_disk: Vec<String> = std::fs::read_dir(tests.join("suites"))
        .expect("read tests/suites")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| {
            p.file_stem()
                .expect("suite file stem")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();

    // Which target(s) register each suite, keyed by the `suites/<name>.rs` path
    // the `#[path]` attribute carries.
    let mut registered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in std::fs::read_dir(&tests).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_some_and(|x| x == "rs") {
            let target = path
                .file_stem()
                .expect("target file stem")
                .to_string_lossy()
                .into_owned();
            let src = std::fs::read_to_string(&path).expect("read target");
            for name in &on_disk {
                if src.contains(&format!("suites/{name}.rs")) {
                    registered
                        .entry(name.clone())
                        .or_default()
                        .push(target.clone());
                }
            }
        }
    }

    let missing: Vec<&String> = on_disk
        .iter()
        .filter(|n| !registered.contains_key(*n))
        .collect();
    assert!(
        missing.is_empty(),
        "{} suite(s) in tests/suites/ are not registered and therefore never run.\n\
         Add to tests/compiler.rs or tests/dogfood.rs (whichever fits):\n{}",
        missing.len(),
        missing
            .iter()
            .map(|n| format!("    #[path = \"suites/{n}.rs\"] mod {n};"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let doubled: Vec<String> = registered
        .iter()
        .filter(|(_, ts)| ts.len() > 1)
        .map(|(n, ts)| format!("  {n} <- {}", ts.join(", ")))
        .collect();
    assert!(
        doubled.is_empty(),
        "{} suite(s) are registered in more than one target, so they build and run \
         twice — keep one:\n{}",
        doubled.len(),
        doubled.join("\n"),
    );
}
