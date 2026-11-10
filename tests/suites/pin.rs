//! The pinner (`aipl_mono::pin`): what it makes of the parts of a program, and how its types
//! compare with monomorphization's inference. See `TYPED_IR.md`.

use aipl::mono::pin::{pin, Failure};
use aipl::DebugOptions;
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
