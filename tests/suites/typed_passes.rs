//! The passes written over the pinned tree, against the `Expr` passes they replace: over every
//! program of the corpus, a typed pass must leave each function as the `Expr` pass does, apart from
//! what the trees say about types.

use aipl::ast::{Item, Program};
use aipl::mono::passes::Scope;
use aipl::typed::{TItem, TProgram};
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

/// What a pre-monomorphization pass is handed: the program as the frontend leaves it, its pinned
/// form, and the declarations that stand ahead of it.
struct Input {
    program: Program,
    pinned: TProgram,
    builtins: Vec<Item>,
}

fn inputs() -> Vec<(PathBuf, Input)> {
    aipl::install_parser_hooks();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    aipl_files(&root.join("tests/cases"), &mut files);
    aipl_files(&root.join("examples"), &mut files);
    let mut out = Vec::new();
    for file in files {
        let Ok(loaded) = aipl::loader::load_program_rooted(&file, root, DebugOptions::OFF) else {
            continue;
        };
        let Ok(frontend) = aipl::codegen::frontend(&loaded) else {
            continue;
        };
        let builtins = frontend.check_program.items
            [..frontend.check_program.items.len() - frontend.program.items.len()]
            .to_vec();
        // The program the passes are handed, with the declarations ahead of it, as the pipeline
        // pins it.
        let pinned = aipl::mono::pin::pin_after(&builtins, &frontend.program);
        if !pinned.failures.is_empty() {
            eprintln!(
                "does not pin: {}: {:?}",
                file.display(),
                pinned
                    .failures
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
            );
            continue;
        }
        let mut program = frontend.program.clone();
        for item in &mut program.items {
            if let Item::Fn(f) = item {
                f.test_body = None;
            }
        }
        let mut pinned = pinned.program;
        for item in &mut pinned.items {
            if let TItem::Fn(f) = item {
                f.test_body = None;
            }
        }
        out.push((
            file,
            Input {
                program,
                pinned,
                builtins,
            },
        ));
    }
    out
}

/// Whether `after` is not `before`, as far as the shape of any function body says.
fn changed(before: &Program, after: &Program) -> bool {
    let shape = |p: &Program| -> Vec<String> {
        p.items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(f) => {
                    let mut body = f.body.clone();
                    aipl::mono::probe::without_stamps(&mut body);
                    Some(format!(
                        "{} {}",
                        f.name.text,
                        numberless(&format!("{body:?}"))
                    ))
                }
                _ => None,
            })
            .collect()
    };
    shape(before) != shape(after)
}

/// `text` with the number in each fresh name (`$fuse12_x`) removed: two runs number them apart.
fn numberless(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        out.push(chars[i]);
        if chars[i] == '$' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                out.push(chars[j]);
                j += 1;
            }
            if j < chars.len() && chars[j].is_ascii_digit() {
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                out.push('#');
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// The functions on which `expected` (an `Expr` pass's output) and `actual` (a typed pass's) differ.
fn differences(expected: &Program, actual: &TProgram) -> Vec<String> {
    let actual = actual.erase();
    let mut out = Vec::new();
    for item in &expected.items {
        let Item::Fn(want) = item else { continue };
        // The last of that name: a builtin's declaration stands ahead of its source.
        let Some(Item::Fn(got)) = actual
            .items
            .iter()
            .rfind(|i| matches!(i, Item::Fn(f) if f.name.text == want.name.text))
        else {
            out.push(format!("{}: missing from the typed result", want.name.text));
            continue;
        };
        let (mut a, mut b) = (want.body.clone(), got.body.clone());
        aipl::mono::probe::without_stamps(&mut a);
        aipl::mono::probe::without_stamps(&mut b);
        let (a, b) = (numberless(&format!("{a:?}")), numberless(&format!("{b:?}")));
        if a != b {
            out.push(format!("{}:\n  want {a}\n  got  {b}", want.name.text));
        }
    }
    out
}

/// Run the `Expr` pass and the typed pass over every program and report each function they
/// leave differently.
fn agree(pass: &str, on_expr: impl Fn(&Input) -> Program, on_pinned: impl Fn(&Input) -> TProgram) {
    let mut report = Vec::new();
    let mut total = 0;
    let mut fired = 0;
    for (file, input) in inputs() {
        total += 1;
        let expected = on_expr(&input);
        if changed(&input.program, &expected) {
            fired += 1;
        }
        let diffs = differences(&expected, &on_pinned(&input));
        if !diffs.is_empty() {
            report.push(format!("{}: {}", file.display(), diffs[0]));
        }
    }
    eprintln!("{pass}: changed {fired} of {total} programs");
    assert!(
        fired > 0,
        "{pass} changed no program, so agreeing says nothing"
    );
    assert!(
        report.is_empty(),
        "{pass}: {} of {total} programs differ; first:\n{}",
        report.len(),
        report
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The functions with effects or that can abort, as the pipeline computes them.
fn effectful(input: &Input) -> std::collections::HashSet<String> {
    let mut items = input.builtins.clone();
    items.extend(input.program.items.iter().cloned());
    aipl::mono::effectful_fns(&items)
}

#[test]
fn fold_constants_agrees_with_the_expr_pass() {
    agree(
        "fold_constants",
        |i| aipl::mono::fold_constants(&i.program, &Scope::Everything),
        |i| aipl::mono::fold_constants_pinned(&i.pinned, &Scope::Everything),
    );
}

#[test]
fn sink_bindings_agrees_with_the_expr_pass() {
    agree(
        "sink_bindings",
        |i| aipl::mono::sink_bindings(&i.program, &effectful(i), &Scope::Everything),
        |i| aipl::mono::sink_bindings_pinned(&i.pinned, &effectful(i), &Scope::Everything),
    );
}

#[test]
fn fuse_try_with_agrees_with_the_expr_pass() {
    agree(
        "fuse_try_with",
        |i| aipl::mono::fuse_try_with(&i.program),
        |i| aipl::mono::fuse_try_with_pinned(&i.pinned),
    );
}

#[test]
fn lower_try_set_agrees_with_the_expr_pass() {
    agree(
        "lower_try_set",
        |i| aipl::mono::lower_try_set(&i.program),
        |i| aipl::mono::lower_try_set_pinned(&i.pinned),
    );
}

#[test]
fn inline_single_use_bindings_agrees_with_the_expr_pass() {
    agree(
        "inline_single_use_bindings",
        |i| aipl::mono::inline_single_use_bindings(&i.program, &effectful(i), &Scope::Everything),
        |i| {
            aipl::mono::inline_single_use_bindings_pinned(
                &i.pinned,
                &effectful(i),
                &Scope::Everything,
            )
        },
    );
}

#[test]
fn fuse_operations_agrees_with_the_expr_pass() {
    agree(
        "fuse_operations",
        |i| aipl::mono::fuse_operations(&i.program, &effectful(i), &Scope::Everything),
        |i| aipl::mono::fuse_operations_pinned(&i.pinned, &effectful(i), &Scope::Everything),
    );
}

#[test]
fn fold_lengths_agrees_with_the_expr_pass() {
    agree(
        "fold_lengths",
        |i| aipl::mono::fold_lengths(&i.program, &effectful(i), &Scope::Everything),
        |i| aipl::mono::fold_lengths_pinned(&i.pinned, &effectful(i), &Scope::Everything),
    );
}

#[test]
fn counting_variants_agrees_with_the_expr_pass() {
    agree(
        "counting_variants",
        |i| aipl::mono::counting_variants(&i.program, &effectful(i)),
        |i| aipl::mono::counting_variants_pinned(&i.pinned, &effectful(i)),
    );
}

#[test]
fn fold_extends_agrees_with_the_expr_pass() {
    agree(
        "fold_extends",
        |i| aipl::mono::fold_extends(&i.program, &Scope::Everything),
        |i| aipl::mono::fold_extends_pinned(&i.pinned, &Scope::Everything),
    );
}

#[test]
fn appending_variants_agrees_with_the_expr_pass() {
    agree(
        "appending_variants",
        |i| aipl::mono::appending_variants(&i.program),
        |i| aipl::mono::appending_variants_pinned(&i.pinned),
    );
}

#[test]
fn inline_single_use_agrees_with_the_expr_pass() {
    agree(
        "inline_single_use",
        |i| aipl::mono::inline_single_use(&i.program),
        |i| aipl::mono::inline_single_use_pinned(&i.pinned),
    );
}

#[test]
fn inline_small_agrees_with_the_expr_pass() {
    agree(
        "inline_small",
        |i| aipl::mono::inline_small(&i.program, 12),
        |i| aipl::mono::inline_small_pinned(&i.pinned, 12),
    );
}

#[test]
fn inline_inspected_agrees_with_the_expr_pass() {
    agree(
        "inline_inspected",
        |i| {
            aipl::mono::inline_inspected(&i.program, aipl::mono::DEFAULT_INLINE_INSPECTED_MAX_EXPRS)
        },
        |i| {
            aipl::mono::inline_inspected_pinned(
                &i.pinned,
                aipl::mono::DEFAULT_INLINE_INSPECTED_MAX_EXPRS,
            )
        },
    );
}

#[test]
fn lower_reductions_agrees_with_the_expr_pass() {
    agree(
        "lower_reductions",
        |i| aipl::mono::lower_reductions(&i.program),
        |i| aipl::mono::lower_reductions_pinned(&i.pinned),
    );
}
