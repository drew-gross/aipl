//! The AIPL grammar written in AIPL (`crates/aipl-codegen/src/grammar_aipl.aipl`)
//! parses the same language as the gazelle LR(1) grammar in `aipl-parser`.
//!
//! This is the differential test `PARSER_LIBRARY.md` exists for, and
//! it is the same shape as [`lexer_dogfood::dogfood_lex_hook_matches_fresh_compile_on_corpus`]:
//! run both implementations over every `.aipl` in the repository and require
//! them to agree, file by file. Agreement here is *acceptance* — the AIPL
//! grammar builds no AST yet — plus, for every file that parses, the
//! losslessness property the concrete tree exists to provide: concatenating its
//! leaf spans returns the source byte for byte.
//!
//! Rejection is half the comparison. The corpus carries ~100 deliberate
//! syntax-error fixtures, and a grammar that accepted everything would agree
//! with gazelle on none of them.

use aipl::{Engine, FfiValue};
use std::fs;
use std::path::{Path, PathBuf};

/// Run `f` on a 64 MB-stack worker. Compiling the grammar's dependency closure
/// (it reaches the parser library *and* the whole lexer) recurses deeper than a
/// test thread's default stack allows — the same reason `lexer_dogfood.rs` and
/// `dogfood_ir.rs` spawn one.
fn on_big_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(f)
        .expect("spawn worker")
        .join()
        .expect("worker panicked");
}

/// Compile `grammar_aipl.aipl` and everything it imports, straight from the
/// working tree. It is deliberately *not* in `DOGFOOD_SOURCE_FILES` — nothing in
/// the compiler runs on it yet — so there is no checked-in IR and no hook: the
/// engine is built here, once per test.
fn compile_grammar() -> Engine {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates")
        .join("aipl-codegen")
        .join("src")
        .join("grammar_aipl.aipl");
    Engine::compile_file(&path).unwrap_or_else(|e| panic!("compile {}: {e:?}", path.display()))
}

/// What the AIPL grammar makes of `src`: empty when it parses and its tree
/// reconstructs the source, else a line starting `error:` or `lossless:`.
fn report(engine: &Engine, src: &str) -> String {
    match engine.call_values("aipl_parse_report", &[FfiValue::Str(src.to_string())]) {
        Ok(FfiValue::Str(s)) => s,
        other => panic!("aipl_parse_report(): {other:?}"),
    }
}

/// Every `.aipl` at or below `dir`, recursively.
fn collect_aipl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_aipl(&path, out);
        } else if path.extension().is_some_and(|e| e == "aipl") {
            out.push(path);
        }
    }
}

/// The corpus files `aipl_parser::parse` rejects in a **build action** rather
/// than in its grammar, paired with a fragment of the message that says so.
///
/// Gazelle's grammar and its semantic checks arrive through one entry point, so
/// `parse` returning `Err` does not always mean "this is not AIPL". Each of
/// these parses fine under the LR grammar *and* under this one, and is then
/// refused by the code that builds the AST — the gazelle grammar's own comments
/// say as much ("validated in the build action"). They are the exact set of
/// checks Stage 5 will have to reproduce when this grammar grows `build`
/// functions, which is why they are listed rather than waved through: the
/// message fragment pins each entry to the check it names, so a file that starts
/// failing for a different reason stops matching and fails the test.
const BUILD_ACTION_REJECTS: &[(&str, &str)] = &[
    // `..` rides on `arg`, shared by calls, array literals and array patterns;
    // only the array literal may keep it.
    (
        "tests/cases/arrays/spread/err_spread_in_call.aipl",
        "spread is only allowed in an array literal",
    ),
    (
        "tests/cases/arrays/spread/err_spread_in_pattern.aipl",
        "spread is only allowed in an array literal",
    ),
    // One `#{ .. }` production covers sets and dicts; the builder picks which.
    (
        "tests/cases/dicts/err_mixed_literal.aipl",
        "can't mix set elements and",
    ),
    // `fn_attr` accepts `.NAME(block)` and `.NAME("str")`; which names take
    // which argument, and that none repeats, is the action's business. The
    // string form is dead syntax kept only so the action can refuse it by name.
    (
        "tests/cases/docs/err_doc_attr.aipl",
        "no longer a function attribute",
    ),
    (
        "tests/cases/docs/err_test_wrong_arg.aipl",
        "`.test` takes a `{ .. }` block",
    ),
    (
        "tests/cases/docs/err_unknown_attr.aipl",
        "unknown function attribute",
    ),
    // Both grammars put the `# ..` lines on `item`, `import` included; that an
    // import has nothing to document is the action's rule.
    (
        "tests/cases/docs/err_doc_on_import.aipl",
        "import cannot be documented",
    ),
    // `T: bound` is `IDENT COLON IDENT` to the grammar; the bound names are the
    // action's.
    (
        "tests/cases/generics/errors/err_unknown_type_param_bound.aipl",
        "unknown type parameter bound",
    ),
    // Alternatives of one arm must bind the same names — a property of the
    // patterns, not of the shape they are written in.
    (
        "tests/cases/match_arms/err_alternation_binding_mismatch.aipl",
        "must bind the same names",
    ),
    // The struct-literal body shorthand needs a struct return type to name,
    // which the grammar cannot see.
    (
        "tests/cases/structs/errors/err_body_shorthand_no_return_type.aipl",
        "needs a struct return type",
    ),
    (
        "tests/cases/structs/errors/err_body_shorthand_non_struct.aipl",
        "needs a struct return type",
    ),
];

/// Whether any line of `src` ends in a space or tab.
///
/// `aipl_parser::parse` rejects those *before* it parses — a whitespace rule,
/// not a grammar rule — so a fixture that trips it says nothing about whether
/// the two grammars agree, and is compared for nothing else.
fn has_trailing_whitespace(src: &str) -> bool {
    src.lines().any(|l| l.ends_with(' ') || l.ends_with('\t'))
}

#[test]
fn aipl_grammar_matches_gazelle_on_corpus() {
    on_big_stack(|| {
        aipl::install_parser_hooks();
        let engine = compile_grammar();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for sub in ["tests/cases", "examples", "crates"] {
            collect_aipl(&root.join(sub), &mut files);
        }
        files.sort();
        assert!(
            files.len() > 400,
            "corpus went missing? found {} files",
            files.len()
        );

        let mut disagreements: Vec<String> = Vec::new();
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        let mut skipped = 0usize;
        let mut build_action: Vec<&str> = Vec::new();
        for f in &files {
            let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
            let full = fs::read_to_string(f).expect("read case file");
            let source = aipl::strip_test_sections(&full);
            if has_trailing_whitespace(source) {
                skipped += 1;
                continue;
            }
            let gazelle = aipl::parse(&full);
            let ours = report(&engine, &full);
            match (gazelle.is_ok(), ours.is_empty()) {
                (true, true) => accepted += 1,
                (false, false) => rejected += 1,
                (true, false) => disagreements.push(format!(
                    "{rel}: gazelle parsed it, the AIPL grammar did not — {ours}"
                )),
                (false, true) => {
                    let why = gazelle.err().map(|e| e.to_string()).unwrap_or_default();
                    match BUILD_ACTION_REJECTS.iter().find(|(f, _)| *f == rel) {
                        Some((_, fragment)) if why.contains(fragment) => {
                            build_action.push(fragment)
                        }
                        Some((_, fragment)) => disagreements.push(format!(
                            "{rel}: listed as a build-action rejection about {fragment:?}, \
                             but gazelle refused it for another reason — {why}"
                        )),
                        None => disagreements.push(format!(
                            "{rel}: the AIPL grammar parsed it, gazelle did not — {why}"
                        )),
                    }
                }
            }
        }

        assert!(
            disagreements.is_empty(),
            "{} of {} files disagree ({accepted} agreed-accept, {rejected} agreed-reject, \
             {skipped} skipped for trailing whitespace):\n{}",
            disagreements.len(),
            files.len(),
            disagreements
                .iter()
                .take(25)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            accepted > 400,
            "expected most of the corpus to parse; only {accepted} did"
        );
        assert_eq!(
            build_action.len(),
            BUILD_ACTION_REJECTS.len(),
            "every listed build-action rejection must still be one — {} of {} matched",
            build_action.len(),
            BUILD_ACTION_REJECTS.len()
        );
        assert!(
            rejected > 20,
            "expected the corpus's syntax-error fixtures to be rejected by both; \
             only {rejected} were. A grammar that accepts everything agrees with nothing."
        );
    });
}
