//! The pinner (`aipl_mono::pin`): what it makes of the parts of a program.

use aipl::mono::pin::{pin, Failure};
use aipl::DebugOptions;

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

/// The pass manager drives a pinned program: a pass written once over `TExpr<T>` rewrites the
/// bodies of a `TProgram` before monomorphization.
#[test]
fn the_pass_manager_runs_a_typed_pass_over_a_pinned_program() {
    use aipl::mono::passes::{optimize, Pass};
    use aipl::typed::{TExprKind, TItem};
    aipl::install_parser_hooks();
    let dir = std::env::temp_dir().join(format!("aipl-pin-pass-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("picks.aipl");
    std::fs::write(
        &file,
        r#"fn picks() -> i64 {
    match ("a") {
        "a" => 1,
        _ => 2,
    }
}
"#,
    )
    .unwrap();
    let program = aipl::loader::load_program(&file, DebugOptions::OFF).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let (_, lowered) = aipl::codegen::lowered_for_check(&program).unwrap();
    let pinned = pin(&lowered).program;
    let passes = vec![Pass::scoped(
        "fold_matches",
        aipl::mono::fold_matches_pinned,
    )];
    let folded = optimize(pinned, &passes, 4, DebugOptions::OFF);
    let body = folded
        .items
        .iter()
        .find_map(|item| match item {
            TItem::Fn(f) if f.name.text == "picks" => Some(&f.body),
            _ => None,
        })
        .unwrap();
    assert!(matches!(body.kind, TExprKind::Int(..)), "{:?}", body.kind);
}
