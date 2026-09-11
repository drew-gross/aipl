//! The AIPL grammar written in AIPL (`crates/aipl-codegen/src/grammar_aipl.aipl`)
//! over the whole corpus.
//!
//! This file used to be a differential against the gazelle LR(1) parser —
//! acceptance, expression shape, whole-`Program` equality over 490 files,
//! span containment, error messages, side-channels — which is what let the
//! AIPL parser replace it (`PARSER_LIBRARY.md`, stage 5). With gazelle gone
//! there is no second parser to differ from: every test in the suite now runs
//! through the AIPL parser, so *what it parses to* is asserted everywhere
//! programs are compiled, and the grammar's own `.test` blocks assert the
//! lowering shape by shape.
//!
//! What is left here is the one property nothing else checks: **losslessness**.
//! The concrete tree exists so a formatter can put back what an AST throws away,
//! and that only holds if concatenating a tree's leaf spans returns the source
//! byte for byte — asked of every file in the repository, not just the samples
//! the `.test` blocks name.

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

/// Every corpus file the grammar accepts, it accepts losslessly: the concrete
/// tree's leaf spans tile the source and reconstruct it byte for byte.
///
/// Compiled fresh from the working-tree source rather than reached through the
/// checked-in artifact, so a grammar edit is tested here before the artifact is
/// regenerated. Refusals are not checked against anything — a syntax-error
/// fixture is refused by the same parser everywhere else in the suite — but
/// they are counted, so a grammar that accepted everything would fail on the
/// count rather than pass on nothing.
#[test]
fn aipl_grammar_is_lossless_on_corpus() {
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

        let mut lossy: Vec<String> = Vec::new();
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for f in &files {
            let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
            let full = fs::read_to_string(f).expect("read case file");
            let ours = report(&engine, &full);
            if ours.is_empty() {
                accepted += 1;
            } else if ours.starts_with("lossless:") {
                lossy.push(format!("{rel}: {ours}"));
            } else {
                rejected += 1;
            }
        }

        assert!(
            lossy.is_empty(),
            "{} of {} files do not reconstruct from their tree:\n{}",
            lossy.len(),
            files.len(),
            lossy
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
        assert!(
            rejected > 20,
            "expected the corpus's syntax-error fixtures to be refused; only {rejected} were. \
             A grammar that accepts everything is not being tested by this."
        );
    });
}
