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

/// How far the pinner and the checker disagree over `dirs`: functions the pinner cannot type yet,
/// functions it rejects that the checker (and the file's own expectations) accept, and programs
/// the checker rejects that it accepts.
struct Disagreement {
    gaps: usize,
    false_rejections: usize,
    misses: usize,
    report: String,
}

fn oracle(dirs: &[&str]) -> Disagreement {
    aipl::install_parser_hooks();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in dirs {
        aipl_files(&root.join(dir), &mut files);
    }
    if let Ok(only) = std::env::var("AIPL_CASE") {
        files.retain(|f| only.split(',').any(|o| f.to_string_lossy().contains(o)));
    }
    let (mut programs, mut skipped) = (0usize, 0usize);
    let mut both_reject = 0usize;
    let mut pinner_misses: Vec<String> = Vec::new();
    let (mut pinned, mut gaps, mut rejected) = (0usize, 0usize, 0usize);
    let mut gap_kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut rejections: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, String> = BTreeMap::new();
    for file in &files {
        let Ok(program) = aipl::loader::load_program(file, DebugOptions::OFF) else {
            skipped += 1;
            continue;
        };
        let frontend = match aipl::codegen::frontend(&program) {
            Ok(f) => f,
            Err(errors) => {
                let first = errors
                    .first()
                    .map(|e| e.message.lines().next().unwrap_or("").to_string())
                    .unwrap_or_default();
                // The checker rejects this program: the pinner must find something wrong with it too.
                match aipl::codegen::lowered_for_check(&program) {
                    Ok((_, lowered)) => {
                        let result = pin(&lowered);
                        let found = result
                            .failures
                            .iter()
                            .any(|(_, f)| matches!(f, Failure::Rejected(_)));
                        if found {
                            both_reject += 1;
                        } else {
                            let gap = result
                                .failures
                                .iter()
                                .any(|(_, f)| matches!(f, Failure::Gap(..)));
                            pinner_misses.push(format!(
                                "{}{} -- {first}",
                                file.display(),
                                if gap { " (a gap hides it)" } else { "" }
                            ));
                        }
                    }
                    Err(_) => skipped += 1,
                }
                continue;
            }
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
                Failure::Rejected(_)
                    if std::fs::read_to_string(file)
                        .is_ok_and(|src| src.contains("--- check --compile ---")) =>
                {
                    // The file records that it does not compile; the checker stops later.
                }
                Failure::Rejected(e) => {
                    rejected += 1;
                    *rejections.entry(e.message.clone()).or_default() += 1;
                    let at = e
                        .span
                        .as_ref()
                        .and_then(|sp| {
                            let src = std::fs::read_to_string(file).ok()?;
                            let text = src.get(sp.start..sp.end.min(sp.start + 60))?;
                            Some(format!(" at `{}`", text.lines().next().unwrap_or("")))
                        })
                        .unwrap_or_default();
                    examples
                        .entry(e.message.clone())
                        .or_insert_with(|| format!("{} in {name}{at}", file.display()));
                }
            }
        }
    }
    use std::fmt::Write;
    let mut report = String::new();
    writeln!(
        report,
        "files: {} programs checked: {programs} (skipped {skipped})",
        files.len()
    )
    .unwrap();
    writeln!(
        report,
        "functions: pinned {pinned}, gap {gaps}, rejected-but-checker-accepted {rejected}"
    )
    .unwrap();
    let top = |m: &BTreeMap<String, usize>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.into_iter()
            .take(40)
            .map(|(k, n)| format!("{n:6}  {k}\n          e.g. {}", examples[k]))
            .collect::<Vec<_>>()
            .join("\n")
    };
    writeln!(
        report,
        "checker-rejected programs: both reject {both_reject}, pinner accepts {}",
        pinner_misses.len()
    )
    .unwrap();
    writeln!(report, "--- gaps ---\n{}", top(&gap_kinds)).unwrap();
    writeln!(report, "--- rejections ---\n{}", top(&rejections)).unwrap();
    writeln!(
        report,
        "--- the pinner accepts what the checker rejects ---\n{}",
        pinner_misses.join("\n")
    )
    .unwrap();
    Disagreement {
        gaps,
        false_rejections: rejected,
        misses: pinner_misses.len(),
        report,
    }
}

/// Author helper: how much of the whole corpus (cases and the compiler's own sources) the pinner
/// covers, and what it stumbles on. `AIPL_CASE=a,b` narrows it by path.
#[test]
#[ignore = "author helper: prints how much of the corpus the pinner covers"]
fn oracle_report() {
    println!("{}", oracle(&["tests/cases", "crates"]).report);
}

/// The pinner types every program the checker accepts, and rejects every program it rejects.
/// (The compiler's own sources are left to `oracle_report`: they are most of the cost.)
#[test]
fn the_pinner_agrees_with_the_checker() {
    let d = oracle(&["tests/cases"]);
    assert!(
        d.gaps == 0 && d.false_rejections == 0 && d.misses == 0,
        "the pinner and the checker disagree:\n{}",
        d.report
    );
}
