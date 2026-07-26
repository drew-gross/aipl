//! Differential test: the dogfooded AIPL lexer (`lex_aipl.aipl`) vs the compiler's
//! hand-written Rust lexer.
//!
//! The AIPL lexer is now wired into the compiler for the highlighter path
//! (`aipl::lex_tokens` runs it via the dogfood hook), and this test is what
//! guards that: [`dogfood_lex_hook_matches_rust_lexer_on_corpus`] compares the
//! *production* hook path against the native Rust lexer
//! (`aipl::lex_tokens_native`, retained as the reference until the parser flip
//! deletes it) over the whole corpus, on every `cargo test`. The older
//! scaffolding still here — `compile_lexer`/`aipl_dump` (freshly FFI-compiling
//! `lex_aipl.aipl` and calling `lex_aipl_tokens`) and the `#[ignore]`d
//! [`report_lexer_differences`] burn-down — tracks the working-tree source
//! (rather than the checked-in IR) and predates the wiring.
//!
//! The comparison is at *category + span* granularity (keyword / ident / number /
//! str / char / constant / operator / punct), matching the Rust lexer's own
//! `classify`; `categorize` maps each `AiplTok` case to that granularity.
//! `BuiltinType` folds into `ident` because that's a highlighter-only refinement —
//! the Rust *lexer* emits a plain identifier for `i64`/`bool`/etc., exactly as the
//! AIPL lexer does — so only genuine lexer divergences remain.

use aipl::{Engine, FfiValue};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const LEXER_AIPL: &str = include_str!("../crates/aipl-codegen/src/lexer.aipl");
const LEX_AIPL: &str = include_str!("../crates/aipl-codegen/src/lex_aipl.aipl");
const STRIP_TEST_SECTIONS_AIPL: &str =
    include_str!("../crates/aipl-codegen/src/strip_test_sections.aipl");
const PARSE_TEST_SECTION_HEADER_AIPL: &str =
    include_str!("../crates/aipl-codegen/src/parse_test_section_header.aipl");

/// Compile the AIPL lexer into an FFI engine exposing `lex_aipl_tokens`. Beyond
/// the lexer itself (`lex_aipl.aipl` + its `lexer.aipl` library) this pulls in
/// `strip_test_sections.aipl` and its `parse_test_section_header.aipl` dep,
/// which `lex_aipl.aipl` imports for its `lex_aipl_stripped` entry. The
/// trailing `--- performance ---` sections are stripped by the loader's parse,
/// so the raw `include_str!`d sources load as-is.
fn compile_lexer() -> Engine {
    aipl::install_parser_hooks();
    Engine::compile_sources(&[
        ("./lex_aipl.aipl", LEX_AIPL),
        ("./lexer.aipl", LEXER_AIPL),
        ("./strip_test_sections.aipl", STRIP_TEST_SECTIONS_AIPL),
        (
            "./parse_test_section_header.aipl",
            PARSE_TEST_SECTION_HEADER_AIPL,
        ),
    ])
    .expect("compile AIPL lexer for differential test")
}

/// The `(String, FfiValue)` field named `name` in a marshaled struct.
fn field<'a>(fields: &'a [(String, FfiValue)], name: &str) -> &'a FfiValue {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("struct has no field {name:?}"))
}

fn as_struct(v: &FfiValue) -> &[(String, FfiValue)] {
    match v {
        FfiValue::Struct(fields) => fields,
        other => panic!("expected a struct, got {other:?}"),
    }
}

fn as_int(v: &FfiValue) -> i64 {
    match v {
        FfiValue::Int(i) => *i,
        other => panic!("expected an int, got {other:?}"),
    }
}

/// The `(start, end)` of a `Span` struct value.
fn span_bounds(v: &FfiValue) -> (i64, i64) {
    let s = as_struct(v);
    (as_int(field(s, "start")), as_int(field(s, "end")))
}

/// Coarsen an `AiplTok` variant case name to the category granularity the Rust
/// lexer's `classify` produces (the operator/punctuation split included), so a
/// token that agrees dumps identically on both sides and only real divergences
/// surface. Every arm below maps to a category and the lexer now covers the whole
/// grammar; the sole remaining divergence is a non-ASCII char literal, where the
/// library's byte-oriented `CharLit` flags "more than one character" at a
/// different span than the Rust lexer's dedicated non-ASCII error.
fn categorize(case: &str) -> &'static str {
    match case {
        "Fn" | "Let" | "Mut" | "Set" | "Pub" | "Import" | "From" | "As" | "For" | "While"
        | "Match" | "Return" | "Struct" | "Variant" | "If" | "Else" | "Builtins" => "keyword",
        "Name" => "ident",
        "IntLit" => "number",
        "StrLit" | "RawStrLit" => "str",
        "CharTok" => "char",
        // The Rust lexer's `classify` folds every template-literal piece
        // (head/middle/tail, and a bare interpolation-free template) into `Str`.
        "TemplateStr" | "TemplateHead" | "TemplateMid" | "TemplateTail" | "RawTemplateStr"
        | "RawTemplateHead" | "RawTemplateMid" | "RawTemplateTail" => "str",
        "True" | "False" | "None" => "constant",
        "EqEq" | "Ne" | "Arrow" | "FatArrow" | "AndAnd" | "OrOr" | "Pipe" | "DotDot"
        | "PlusPlusPlus" | "PlusPlus" | "Eq" | "Lt" | "Le" | "Gt" | "Ge" | "Bang" | "Plus"
        | "Minus" | "Star" | "Slash" | "Percent" => "operator",
        "Period" | "Comma" | "Colon" | "Semi" | "Question" | "Hash" | "LParen" | "RParen"
        | "LBrace" | "RBrace" | "LBracket" | "RBracket" => "punct",
        "Space" | "LineComment" | "BlockComment" | "AllowMarker" => "trivia",
        other => panic!("unknown AiplTok case {other:?}"),
    }
}

/// The AIPL lexer's canonical dump of `src`, built from the *actual* token array
/// `lex_aipl_tokens` returns: one `START END CATEGORY` line per token, or a single
/// `ERR START END` line for a `LexError`.
fn aipl_dump(engine: &Engine, src: &str) -> String {
    let res = engine
        .call_values("lex_aipl_tokens", &[FfiValue::Str(src.to_string())])
        .expect("call lex_aipl_tokens");
    match res {
        // ok: an array of `Token { kind, span }` structs.
        FfiValue::Res(Ok(tokens)) => {
            let tokens = match *tokens {
                FfiValue::Array(ts) => ts,
                other => panic!("lex_aipl_tokens ok payload not an array: {other:?}"),
            };
            let mut out = String::new();
            for tok in &tokens {
                let fields = as_struct(tok);
                let (start, end) = span_bounds(field(fields, "span"));
                let cat = match field(fields, "kind") {
                    FfiValue::Variant(case, _) => categorize(case),
                    other => panic!("token kind not a variant: {other:?}"),
                };
                out.push_str(&format!("{start} {end} {cat}\n"));
            }
            out
        }
        // err: a `LexError { message, span }` struct — only its span is compared.
        FfiValue::Res(Err(e)) => {
            let (start, end) = span_bounds(field(as_struct(&e), "span"));
            format!("ERR {start} {end}\n")
        }
        other => panic!("lex_aipl_tokens returned {other:?}"),
    }
}

/// The Rust lexer's canonical dump of `src`, in the same format `lex_aipl_dump`
/// produces (see the module docs for the `BuiltinType` → `ident` fold). Uses
/// [`aipl::lex_tokens_native`] — the hand-written Rust lexer — because
/// `aipl::lex_tokens` now runs the *dogfooded* lexer, which this test exists to
/// compare *against* the native one.
fn rust_dump(src: &str) -> String {
    use aipl::TokenKind::*;
    match aipl::lex_tokens_native(src) {
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

/// The installed dogfood lex hook's canonical dump of `src` — same format as
/// [`aipl_dump`], but through the *production* path: the checked-in
/// `dogfood.clif` engine plus the FFI marshaling into the mirrored
/// [`aipl::LexedTokenKind`] (whose arm names match `AiplTok`'s case names, so
/// their `Debug` head reuses [`categorize`]).
fn hook_dump(src: &str) -> String {
    match aipl::lex_aipl(src) {
        Ok(out) => {
            let mut s = String::new();
            for t in &out.tokens {
                let dbg = format!("{:?}", t.kind);
                let case = dbg.split('(').next().expect("split never yields nothing");
                s.push_str(&format!(
                    "{} {} {}\n",
                    t.span.start,
                    t.span.end,
                    categorize(case)
                ));
            }
            s
        }
        Err(e) => format!("ERR {} {}\n", e.span.start, e.span.end),
    }
}

/// The production lex path — the checked-in dogfood IR called through the
/// installed hook, marshaled into the mirrored Rust token types — agrees with
/// the Rust lexer over the whole corpus at category + span granularity. This
/// is the hard gate the burn-down report graduated into; unlike the report it
/// runs on every `cargo test`.
#[test]
fn dogfood_lex_hook_matches_rust_lexer_on_corpus() {
    aipl::install_parser_hooks();
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

    // The one known divergence: the AIPL lexer's byte-oriented `CharLit` spans
    // a non-ASCII char literal's error differently than the Rust lexer's
    // dedicated non-ASCII error. Excluded until the error-fidelity pass.
    let excluded = ["tests/cases/chars/err_non_ascii_char.aipl"];

    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
        if excluded.contains(&rel.as_str()) {
            continue;
        }
        let full = fs::read_to_string(f).expect("read case file");
        let stripped = aipl::strip_test_sections(&full).to_string();
        assert_eq!(
            rust_dump(&stripped),
            hook_dump(&stripped),
            "dogfood lex hook diverges from the Rust lexer in {rel}"
        );
    }
}

/// The hook's trivia side-channel carries comments and `#[allow]` markers (in
/// source order) and keeps them out of the token stream; whitespace appears in
/// neither.
#[test]
fn dogfood_lex_hook_returns_trivia() {
    use aipl::LexedTokenKind as K;
    aipl::install_parser_hooks();
    let out = aipl::lex_aipl("x // c\n#[allow] /* b */ y").expect("lexes clean");
    assert_eq!(
        out.tokens
            .iter()
            .map(|t| (t.kind.clone(), t.span.clone()))
            .collect::<Vec<_>>(),
        vec![
            (K::Name("x".to_string()), 0..1),
            (K::Name("y".to_string()), 24..25),
        ],
    );
    assert_eq!(
        out.trivia
            .iter()
            .map(|t| (t.kind.clone(), t.span.clone()))
            .collect::<Vec<_>>(),
        vec![
            (K::LineComment, 2..6),
            (K::AllowMarker, 7..15),
            (K::BlockComment, 16..23),
        ],
    );
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
