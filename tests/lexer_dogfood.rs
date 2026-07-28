//! Token-level tests for the dogfooded AIPL lexer (`lex_aipl.aipl`).
//!
//! The lexer is now the compiler's *only* lexer — the hand-written Rust scanner
//! this file once differential-tested against has been deleted. What remains
//! guards the lexer against itself: [`dogfood_lex_hook_matches_fresh_compile_on_corpus`]
//! checks that the production path (the checked-in `dogfood.clif` through the
//! installed hook) agrees with a fresh compile of the working-tree lexer source
//! over the whole corpus, and [`dogfood_lex_hook_returns_trivia`] checks the
//! trivia side-channel.
//!
//! The corpus comparison is at *category + span* granularity (keyword / ident /
//! number / str / char / constant / operator / punct); `categorize` maps each
//! `AiplTok` case to that granularity, folding the built-in type names
//! (`i64`/`bool`/…) into `ident` because that is a highlighter-only refinement,
//! not a lexer distinction.

use aipl::{Engine, FfiValue};
use std::fs;
use std::path::{Path, PathBuf};

/// Compile the AIPL lexer into an FFI engine exposing `lex_aipl_tokens`. Takes
/// the lexer's transitive dependency files straight from the canonical
/// `DOGFOOD_SOURCES` (so their contents never desync a copy here), reordered so
/// `lex_aipl.aipl` is the callable root. It's the lexer's own dependency closure
/// rather than *all* of `DOGFOOD_SOURCES` because the other dogfood files (e.g.
/// `caret_block.aipl`) recurse deeply enough to overflow the default test-thread
/// stack when compiled. If `lex_aipl.aipl` gains a new import, add its file here
/// (a missing one fails the compile loudly). Trailing `--- performance ---`
/// sections are stripped by the loader's parse.
fn compile_lexer() -> Engine {
    aipl::install_parser_hooks();
    const LEXER_DEPS: &[&str] = &[
        "./lex_aipl.aipl",
        "./lexer.aipl",
        "./unescape.aipl",
        "./strip_test_sections.aipl",
        "./parse_test_section_header.aipl",
        "./process_raw_string.aipl",
        "./dedent.aipl",
        "./lines.aipl",
        "./trim_prefix.aipl",
        "./trim_end_while.aipl",
        "./trim_suffix.aipl",
    ];
    let mut sources: Vec<(&str, &str)> = aipl::codegen::DOGFOOD_SOURCES
        .iter()
        .copied()
        .filter(|(name, _)| LEXER_DEPS.contains(name))
        .collect();
    sources.sort_by_key(|(name, _)| *name != "./lex_aipl.aipl");
    Engine::compile_sources(&sources).expect("compile AIPL lexer for differential test")
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

/// Coarsen an `AiplTok` variant case name to a category granularity (the
/// operator/punctuation split included), so a token dumps identically whether it
/// came from a fresh compile or the production hook and only real divergences
/// surface. `BuiltinType` isn't a lexer category — `i64`/`bool`/… lex as plain
/// identifiers — so type names fold into `ident`.
fn categorize(case: &str) -> &'static str {
    match case {
        "Fn" | "Let" | "Mut" | "Set" | "Pub" | "Import" | "From" | "As" | "For" | "While"
        | "Match" | "Return" | "Struct" | "Variant" | "If" | "Else" | "Builtins" => "keyword",
        "Name" => "ident",
        "IntLit" => "number",
        // Every string literal — `"..."`, `"""..."""`, and an interpolation-free
        // `` `...` ``/```` ```...``` ```` template — is one `StrLit` (its
        // delimiter kept as a style, dropped for this category dump).
        "StrLit" => "str",
        "CharTok" => "char",
        // A template-literal piece (head/middle/tail) folds into `str`.
        "TemplateHead" | "TemplateMid" | "TemplateTail" | "RawTemplateHead" | "RawTemplateMid"
        | "RawTemplateTail" => "str",
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

/// A fresh compile of the lexer source's dump of `src`, built from the *actual*
/// token array `lex_aipl_tokens` returns: one `START END CATEGORY` line per
/// token, or a single `ERR START END` line for a `LexError`.
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

/// The freshly-compiled lexer produces the expected tokens for a small
/// all-supported snippet, and agrees with the production hook on a richer one
/// (keywords, idents incl. a `BuiltinType`, an arrow operator, punctuation).
#[test]
fn aipl_lexer_dumps_supported_subset() {
    let engine = compile_lexer();

    let src = "let x = 42;";
    assert_eq!(
        aipl_dump(&engine, src),
        "0 3 keyword\n4 5 ident\n6 7 operator\n8 10 number\n10 11 punct\n",
    );

    let src2 = "fn f(n: i64) -> i64 { n }";
    assert_eq!(aipl_dump(&engine, src2), hook_dump(src2));
}

/// The production lex path (the checked-in dogfood IR through the installed hook,
/// marshaled into the mirrored Rust token types) agrees with a fresh compile of
/// the working-tree lexer source over the whole corpus, at category + span
/// granularity — so a lexer-source change not reflected in the checked-in
/// `dogfood.clif` is caught here on every `cargo test`.
#[test]
fn dogfood_lex_hook_matches_fresh_compile_on_corpus() {
    let engine = compile_lexer();
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

    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
        let full = fs::read_to_string(f).expect("read case file");
        let stripped = aipl::strip_test_sections(&full).to_string();
        assert_eq!(
            aipl_dump(&engine, &stripped),
            hook_dump(&stripped),
            "production lex hook diverges from a fresh compile of the lexer source in {rel}"
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
