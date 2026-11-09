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
    // The programs the checker rejects, where the pinner reports the very same errors, and where
    // it does not (what each said).
    let mut same_errors = 0usize;
    let mut other_errors: Vec<String> = Vec::new();
    let mut same_places = 0usize;
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
                            let key = |e: &aipl::Error| {
                                (
                                    e.message.lines().next().unwrap_or("").to_string(),
                                    e.span.as_ref().map(|s| s.start),
                                )
                            };
                            let mut theirs: Vec<_> = errors.iter().map(key).collect();
                            let mut ours: Vec<_> = result
                                .failures
                                .iter()
                                .filter_map(|(_, f)| match f {
                                    Failure::Rejected(e) => Some(key(e)),
                                    Failure::Gap(..) => None,
                                })
                                .collect();
                            theirs.sort();
                            ours.sort();
                            let places = |v: &[(String, Option<usize>)]| {
                                v.iter().map(|(_, at)| *at).collect::<Vec<_>>()
                            };
                            if places(&theirs) == places(&ours) {
                                same_places += 1;
                            }
                            if theirs == ours {
                                same_errors += 1;
                            } else if places(&theirs) != places(&ours) {
                                let show = |v: &[(String, Option<usize>)]| {
                                    v.iter()
                                        .map(|(m, at)| format!("      {at:?} {m}"))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                };
                                other_errors.push(format!(
                                    "{}\n    checker:\n{}\n    pinner:\n{}",
                                    file.display(),
                                    show(&theirs),
                                    show(&ours)
                                ));
                            }
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
        // A `.test` block is checked only when the tests run, as the driver's function of its own,
        // so the pinner's verdict on one is held against that run's, and only computed when needed.
        let mut tests_accepted = None;
        // The program `check` actually runs: each `.test` block is a `__test$<fn>` function there,
        // and the checker accepts it, so the pinner must too.
        let has_tests = program
            .items
            .iter()
            .any(|i| matches!(i, aipl::ast::Item::Fn(f) if f.test_body.is_some()));
        if has_tests {
            let drivers = [
                Some(aipl::codegen::build_test_program(&program)),
                aipl::codegen::build_benchmark_program(&program),
            ];
            for driver in drivers.into_iter().flatten() {
                let Ok(fe) = aipl::codegen::frontend(&driver) else {
                    continue;
                };
                for (name, failure) in pin(&fe.check_program).failures {
                    if !name.starts_with("__test$") {
                        continue;
                    }
                    let (what, bucket) = match failure {
                        Failure::Gap(what, _) => {
                            gaps += 1;
                            (what.clone(), &mut gap_kinds)
                        }
                        Failure::Rejected(e) => {
                            rejected += 1;
                            (e.message.clone(), &mut rejections)
                        }
                    };
                    *bucket.entry(what.clone()).or_default() += 1;
                    examples
                        .entry(what)
                        .or_insert_with(|| format!("{} in {name}", file.display()));
                }
            }
        }
        for (name, failure) in &result.failures {
            let owner = name.strip_suffix(".test");
            if !user.contains(owner.unwrap_or(name)) {
                continue;
            }
            if owner.is_some() {
                let accepted = *tests_accepted.get_or_insert_with(|| {
                    aipl::codegen::frontend(&aipl::codegen::build_test_program(&program)).is_ok()
                        && aipl::codegen::build_benchmark_program(&program)
                            .is_none_or(|b| aipl::codegen::frontend(&b).is_ok())
                });
                if !accepted {
                    continue;
                }
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
    writeln!(
        report,
        "of those, the pinner reports exactly the checker's errors in {same_errors}, at the same \
         places in {same_places}, others in {}",
        other_errors.len()
    )
    .unwrap();
    writeln!(report, "--- gaps ---\n{}", top(&gap_kinds)).unwrap();
    writeln!(report, "--- rejections ---\n{}", top(&rejections)).unwrap();
    writeln!(
        report,
        "--- the pinner's errors differ from the checker's ---\n{}",
        other_errors.join("\n")
    )
    .unwrap();
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
/// The cases, the examples, the projects and the FFI fixtures are held to it. (The compiler's own
/// sources are left to `oracle_report`: they are most of the cost.)
#[test]
fn the_pinner_agrees_with_the_checker() {
    let d = oracle(&["tests/cases", "examples", "projects", "tests/ffi_fixtures"]);
    assert!(
        d.gaps == 0 && d.false_rejections == 0 && d.misses == 0,
        "the pinner and the checker disagree:\n{}",
        d.report
    );
}

/// What the pinner makes of the parts of a program that are not a function's body: a `.test`
/// block, a parameter's default, a struct field's default, and a `return` nothing reads.
#[test]
fn the_pinner_types_tests_defaults_and_diverging_statements() {
    use aipl::typed::{TExprKind, TItem, Ty};
    aipl::install_parser_hooks();
    let dir = std::env::temp_dir().join(format!("aipl-pin-parts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("parts.aipl");
    std::fs::write(
        &file,
        r#"import { equal as ==, wrapping_add as + } from builtins;

struct Point { x: i64, y: i64 = 3 }

fn scaled(n: i64, by: i64 = 2) -> i64 {
    return n;
    n + by
}.test({
    assert(scaled(1) == 3);
})
"#,
    )
    .unwrap();
    let program = aipl::loader::load_program(&file, DebugOptions::OFF).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let (_, lowered) = aipl::codegen::lowered_for_check(&program).unwrap();
    let pinned = pin(&lowered);
    let failures: Vec<String> = pinned
        .failures
        .iter()
        .map(|(name, f)| match f {
            Failure::Rejected(e) => format!("{name}: {}", e.message),
            Failure::Gap(what, _) => format!("{name}: gap in {what}"),
        })
        .collect();
    assert!(failures.is_empty(), "{failures:?}");
    let mut seen = (false, false, false, false);
    for item in &pinned.program.items {
        match item {
            TItem::Fn(f) if f.name.text == "scaled" => {
                seen.0 = f.test_body.is_some();
                seen.1 = f.sig.params[1]
                    .default
                    .as_ref()
                    .is_some_and(|d| d.ty == Ty::Primitive(aipl::ast::Primitive::I64));
                seen.2 = matches!(&f.body.kind, TExprKind::Seq(first, _) if first.ty == Ty::Never);
            }
            TItem::Struct(s) if s.name.text == "Point" => {
                seen.3 = s.fields[1].default.is_some();
            }
            _ => {}
        }
    }
    assert_eq!(
        seen,
        (true, true, true, true),
        "(test block, parameter default, diverging statement, field default) pinned"
    );
}

/// Author helper: how often monomorphization's inferred types differ from the pinned ones, by the
/// form of the expression, over `tests/cases` (`AIPL_CASE=a,b` narrows it).
#[test]
#[ignore = "author helper: prints where mono's inference and the pinner disagree"]
fn mono_agreement_report() {
    use aipl::mono::probe::Report;
    aipl::install_parser_hooks();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    aipl_files(&root.join("tests/cases"), &mut files);
    if let Ok(only) = std::env::var("AIPL_CASE") {
        files.retain(|f| only.split(',').any(|o| f.to_string_lossy().contains(o)));
    }
    let mut total = Report::default();
    let (mut programs, mut failed) = (0usize, 0usize);
    for file in &files {
        let Ok(program) = aipl::loader::load_program(file, DebugOptions::OFF) else {
            continue;
        };
        let run = std::panic::catch_unwind(|| aipl::codegen::mono_agreement(&program));
        let report = match run {
            Ok(Ok(report)) => report,
            other => {
                failed += 1;
                if failed <= 5 {
                    let why = match other {
                        Ok(Err(errors)) => errors
                            .first()
                            .map(|e| e.message.lines().next().unwrap_or("").to_string())
                            .unwrap_or_default(),
                        _ => "panicked".to_string(),
                    };
                    println!("failed: {} -- {why}", file.display());
                }
                continue;
            }
        };
        programs += 1;
        total.compared += report.compared;
        total.agreed += report.agreed;
        total.unmatched += report.unmatched;
        total.unbound += report.unbound;
        total.collisions += report.collisions;
        total.placeholder += report.placeholder;
        total.instances += report.instances;
        for (form, (n, example)) in report.disagreements {
            let entry = total
                .disagreements
                .entry(form)
                .or_insert((0, format!("{} -- {example}", file.display())));
            entry.0 += n;
        }
    }
    println!(
        "programs {programs} (failed to monomorphize {failed}); instances {}; compared {} agreed {} \
         placeholder {} unbound {} unmatched {} collisions {}",
        total.instances, total.compared, total.agreed, total.placeholder, total.unbound,
        total.unmatched, total.collisions
    );
    let mut rows: Vec<_> = total.disagreements.iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (form, (n, example)) in rows.into_iter().take(40) {
        println!("{n:7}  {form}\n         e.g. {example}");
    }
}

/// Author helper: where an expression's stamp (`Expr::ty`) in the program `frontend` hands on differs
/// from its pinned type. Every node is stamped from the pinner except where the checker had already
/// stamped one, so a difference is a stamp the checker left that the pinner would not have written
/// (`AIPL_CASE=a,b` narrows it).
#[test]
#[ignore = "author helper: prints where the checker's stamps and the pinner disagree"]
fn stamp_agreement_report() {
    aipl::install_parser_hooks();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    aipl_files(&root.join("tests/cases"), &mut files);
    aipl_files(&root.join("crates"), &mut files);
    if let Ok(only) = std::env::var("AIPL_CASE") {
        files.retain(|f| only.split(',').any(|o| f.to_string_lossy().contains(o)));
    }
    let (mut compared, mut agreed, mut unmatched) = (0usize, 0usize, 0usize);
    let mut disagreements: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut forms: BTreeMap<String, usize> = BTreeMap::new();
    for file in &files {
        let Ok(program) = aipl::loader::load_program(file, DebugOptions::OFF) else {
            continue;
        };
        let Ok(Ok(report)) = std::panic::catch_unwind(|| aipl::codegen::stamp_agreement(&program))
        else {
            continue;
        };
        compared += report.compared;
        agreed += report.agreed;
        unmatched += report.unmatched;
        for (form, n) in report.forms {
            *forms.entry(form).or_default() += n;
        }
        for (key, (n, example)) in report.disagreements {
            disagreements
                .entry(key)
                .or_insert((0, format!("{} -- {example}", file.display())))
                .0 += n;
        }
    }
    println!("stamps compared {compared} agreed {agreed} unmatched {unmatched}");
    let mut by_form: Vec<_> = forms.iter().collect();
    by_form.sort_by(|a, b| b.1.cmp(a.1));
    println!("stamped forms: {by_form:?}");
    let mut rows: Vec<_> = disagreements.iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (key, (n, example)) in rows.into_iter().take(40) {
        println!("{n:7}  {key}\n         e.g. {example}");
    }
}
