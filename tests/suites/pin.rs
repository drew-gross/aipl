//! The pinner (`aipl_mono::pin`) against the checker, over the corpus.
//!
//! `oracle_report` is an author helper for the typed-IR project (`TYPED_IR.md`): it runs the
//! checker's frontend on every `.aipl` file under `tests/cases/` and `crates/`, pins what the
//! checker accepted, and prints how much of it the pinner covers and what it stumbles on.

use aipl::mono::pin::{pin, Failure};
use aipl::DebugOptions;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn aipl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            aipl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "aipl") {
            out.push(path);
        }
    }
}

#[test]
#[ignore = "author helper: prints how much of the corpus the pinner covers"]
fn oracle_report() {
    aipl::install_parser_hooks();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    aipl_files(&root.join("tests/cases"), &mut files);
    aipl_files(&root.join("crates"), &mut files);
    if let Ok(only) = std::env::var("AIPL_CASE") {
        files.retain(|f| only.split(',').any(|o| f.to_string_lossy().contains(o)));
    }
    let (mut programs, mut skipped) = (0usize, 0usize);
    let (mut pinned, mut gaps, mut rejected) = (0usize, 0usize, 0usize);
    let mut gap_kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut rejections: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, String> = BTreeMap::new();
    for file in &files {
        let Ok(program) = aipl::loader::load_program(file, DebugOptions::OFF) else {
            skipped += 1;
            continue;
        };
        let Ok(frontend) = aipl::codegen::frontend(&program) else {
            skipped += 1;
            continue;
        };
        programs += 1;
        let result = pin(&frontend.check_program);
        let user: std::collections::HashSet<String> = frontend
            .program
            .items
            .iter()
            .filter_map(|i| match i {
                aipl::ast::Item::Fn(f) => Some(f.name.text.clone()),
                _ => None,
            })
            .collect();
        let failed: std::collections::HashSet<&str> =
            result.failures.iter().map(|(n, _)| n.as_str()).collect();
        pinned += user.iter().filter(|n| !failed.contains(n.as_str())).count();
        for (name, failure) in &result.failures {
            if !user.contains(name) {
                continue;
            }
            match failure {
                Failure::Gap(what, _) => {
                    gaps += 1;
                    *gap_kinds.entry(what.clone()).or_default() += 1;
                    examples
                        .entry(what.clone())
                        .or_insert_with(|| format!("{} in {name}", file.display()));
                }
                Failure::Rejected(e) => {
                    rejected += 1;
                    *rejections.entry(e.message.clone()).or_default() += 1;
                    examples
                        .entry(e.message.clone())
                        .or_insert_with(|| format!("{} in {name}", file.display()));
                }
            }
        }
    }
    println!(
        "files: {} programs checked: {programs} (skipped {skipped})",
        files.len()
    );
    println!("functions: pinned {pinned}, gap {gaps}, rejected-but-checker-accepted {rejected}");
    let top = |m: &BTreeMap<String, usize>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.into_iter()
            .take(40)
            .map(|(k, n)| format!("{n:6}  {k}\n          e.g. {}", examples[k]))
            .collect::<Vec<_>>()
            .join("\n")
    };
    println!("--- gaps ---\n{}", top(&gap_kinds));
    println!("--- rejections ---\n{}", top(&rejections));
}
