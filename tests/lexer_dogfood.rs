//! Differential test: the dogfooded AIPL lexer (`lex_aipl.aipl`) vs the compiler's
//! hand-written Rust lexer.
//!
//! A cautious first step toward dogfooding the AIPL lexer *inside* the compiler:
//! nothing here is wired into compilation. We compile the AIPL lexer once through
//! the embedding FFI and call `lex_aipl_dump`, which serializes its token stream
//! to a canonical `START END CATEGORY` dump (a lex error becomes one `ERR START
//! END` line); the Rust side produces the same dump from `aipl::lex_tokens`.
//! Comparing the two over the test corpus yields a burn-down list of where the
//! AIPL lexer still diverges — run `report_lexer_differences` (below) to see it.
//!
//! The comparison is at *category + span* granularity (keyword / ident / number /
//! str / char / constant / operator / punct), matching the Rust lexer's own
//! `classify`. `BuiltinType` folds into `ident` because that's a highlighter-only
//! refinement — the Rust *lexer* emits a plain identifier for `i64`/`bool`/etc.,
//! exactly as the AIPL lexer does — so only genuine lexer divergences remain.

use aipl::{Engine, FfiValue};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const LEXER_AIPL: &str = include_str!("../crates/aipl-codegen/src/lexer.aipl");
const LEX_AIPL: &str = include_str!("../crates/aipl-codegen/src/lex_aipl.aipl");

/// Compile `lexer.aipl` + `lex_aipl.aipl` into an FFI engine exposing
/// `lex_aipl_dump`. The trailing `--- performance ---` sections are stripped by
/// the loader's parse, so the raw `include_str!`d sources load as-is.
fn compile_lexer() -> Engine {
    aipl::install_parser_hooks();
    Engine::compile_sources(&[("./lex_aipl.aipl", LEX_AIPL), ("./lexer.aipl", LEXER_AIPL)])
        .expect("compile AIPL lexer for differential test")
}

/// The AIPL lexer's canonical dump of `src`.
fn aipl_dump(engine: &Engine, src: &str) -> String {
    match engine.call_values("lex_aipl_dump", &[FfiValue::Str(src.to_string())]) {
        Ok(FfiValue::Str(s)) => s,
        other => panic!("lex_aipl_dump returned {other:?}"),
    }
}

/// The Rust lexer's canonical dump of `src`, in the same format `lex_aipl_dump`
/// produces (see the module docs for the `BuiltinType` → `ident` fold).
fn rust_dump(src: &str) -> String {
    use aipl::TokenKind::*;
    match aipl::lex_tokens(src) {
        Ok(tokens) => {
            let mut out = String::new();
            for (kind, span) in tokens {
                let cat = match kind {
                    Keyword => "keyword",
                    Constant => "constant",
                    Identifier | BuiltinType => "ident",
                    Number => "number",
                    Str => "str",
                    Char => "char",
                    Operator => "operator",
                    Punctuation => "punct",
                };
                out.push_str(&format!("{} {} {}\n", span.start, span.end, cat));
            }
            out
        }
        Err(e) => {
            let (start, end) = e.span.map(|s| (s.start, s.end)).unwrap_or((0, 0));
            format!("ERR {start} {end}\n")
        }
    }
}

/// Lex `src` through both lexers on the same (test-section-stripped) input.
fn both_dumps(engine: &Engine, full: &str) -> (String, String) {
    let stripped = aipl::strip_test_sections(full).to_string();
    (rust_dump(&stripped), aipl_dump(engine, &stripped))
}

/// The scaffolding works, and both lexers agree on a snippet that uses only
/// tokens the AIPL lexer already supports.
#[test]
fn aipl_lexer_matches_rust_on_supported_subset() {
    let engine = compile_lexer();

    let src = "let x = 42;";
    assert_eq!(
        aipl_dump(&engine, src),
        "0 3 keyword\n4 5 ident\n6 7 operator\n8 10 number\n10 11 punct\n",
    );
    assert_eq!(rust_dump(src), aipl_dump(&engine, src));

    // A richer all-supported snippet: keywords, idents (incl. a `BuiltinType`),
    // an arrow operator, and punctuation.
    let src2 = "fn f(n: i64) -> i64 { n }";
    assert_eq!(rust_dump(src2), aipl_dump(&engine, src2));
}

/// One dump's first line that differs from the other's, reduced to a burn-down
/// signature (spans dropped so divergences of the same shape group together).
struct Divergence {
    line: usize,
    rust: String,
    aipl: String,
    signature: String,
}

/// The 3rd field of a dump line is its category; a line may instead be `ERR ...`,
/// and a missing line (one dump ran out) reads as `EOF`.
fn tag(line: Option<&str>) -> &str {
    match line {
        None => "EOF",
        Some(l) if l.starts_with("ERR") => "ERR",
        Some(l) => l.split(' ').nth(2).unwrap_or("?"),
    }
}

/// The first line at which `rust` and `aipl` disagree (the caller only calls this
/// when they aren't identical).
fn first_divergence(rust: &str, aipl: &str) -> Divergence {
    let r: Vec<&str> = rust.lines().collect();
    let a: Vec<&str> = aipl.lines().collect();
    for i in 0..r.len().max(a.len()) {
        let (rl, al) = (r.get(i).copied(), a.get(i).copied());
        if rl != al {
            let (rt, at) = (tag(rl), tag(al));
            // Same category but a different line means the token boundaries (spans)
            // diverged; different categories are a token-kind divergence.
            let signature = if rt == at {
                format!("{rt}: span/boundary")
            } else {
                format!("{rt} → {at}")
            };
            return Divergence {
                line: i + 1,
                rust: rl.unwrap_or("<eof>").to_string(),
                aipl: al.unwrap_or("<eof>").to_string(),
                signature,
            };
        }
    }
    unreachable!("first_divergence called on identical dumps")
}

/// Every `.aipl` file under `dir`, recursively.
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

/// Burn-down report: compare the AIPL and Rust lexers over the whole corpus and
/// print where (and how) they diverge. `#[ignore]`d — the AIPL lexer is known to
/// be incomplete, so this is a tracking report, not a pass/fail gate. Run with:
///   cargo test --test lexer_dogfood -- --ignored report_lexer_differences
/// It prints the report, then fails intentionally so the output is shown even
/// without `--nocapture` (mirroring the `fill_expected` helper).
#[test]
#[ignore = "differential burn-down report; run explicitly"]
fn report_lexer_differences() {
    let engine = compile_lexer();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for sub in ["tests/cases", "examples", "crates"] {
        collect_aipl(&root.join(sub), &mut files);
    }
    files.sort();

    let mut matching = 0usize;
    let mut diffs: Vec<(String, Divergence)> = Vec::new();
    // signature -> (count, first example "file:line")
    let mut signatures: BTreeMap<String, (usize, String)> = BTreeMap::new();

    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
        let full = fs::read_to_string(f).expect("read case file");
        let (rd, ad) = both_dumps(&engine, &full);
        if rd == ad {
            matching += 1;
            continue;
        }
        let div = first_divergence(&rd, &ad);
        let entry = signatures
            .entry(div.signature.clone())
            .or_insert((0, String::new()));
        entry.0 += 1;
        if entry.1.is_empty() {
            entry.1 = format!("{rel}:{}", div.line);
        }
        diffs.push((rel, div));
    }

    let mut report = String::new();
    report.push_str("=== AIPL lexer vs Rust lexer — differential burn-down ===\n");
    report.push_str(&format!(
        "corpus: {} files    matching: {}    differing: {}\n\n",
        files.len(),
        matching,
        diffs.len(),
    ));

    // Signatures, most common first, as the burn-down categories.
    report.push_str("--- first-divergence signatures (most common first) ---\n");
    let mut by_count: Vec<_> = signatures.iter().collect();
    by_count.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(b.0)));
    for (sig, (count, example)) in by_count {
        report.push_str(&format!("  {count:>4}  {sig:<24}  e.g. {example}\n"));
    }

    // Per-file first divergence (the raw dump lines, spans included).
    report.push_str("\n--- per-file first divergence ---\n");
    for (rel, div) in &diffs {
        report.push_str(&format!(
            "  {rel}  (line {})  rust=[{}]  aipl=[{}]\n",
            div.line, div.rust, div.aipl,
        ));
    }

    println!("{report}");
    // Fail intentionally so the report is surfaced (this is a report, not a gate).
    panic!(
        "lexer burn-down: {}/{} files match ({} differ) — see report above",
        matching,
        files.len(),
        diffs.len(),
    );
}
