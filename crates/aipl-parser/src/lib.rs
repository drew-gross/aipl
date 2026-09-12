//! The AIPL parser's entry points, and what surrounds a parse.
//!
//! The parser itself is written in AIPL — `crates/aipl-codegen/src/grammar_aipl.aipl`
//! is the grammar as data, `parse.aipl` the driver that runs it, and `ast.aipl`
//! the tree it lowers to — and is compiled into the checked-in dogfood artifact
//! the compiler links. This crate is the Rust side of that boundary: the hook
//! the compiler installs to reach it (`set_parse_hook`), the two rewrites that
//! follow a parse but are no part of one (`post_parse`), the lexer's token
//! types as they cross the FFI, and the other dogfooded helpers a parse leans
//! on (section stripping, companion files, assertion locations).
//!
//! It used to hold a second parser — a gazelle LR(1) grammar with 86 build
//! actions — which the AIPL one replaced once it built the same `Program` on
//! every corpus file (`PARSER_LIBRARY.md`, stage 5). Nothing here parses by
//! hand any more, and there is **no native fallback**: a parse without the
//! hook installed is a panic, not a slower path.

use std::path::Path;

use aipl_syntax::ast::{Expr, ExprKind, Item, Program};
use aipl_syntax::{Error, Span};

/// The delimiter a [`LexedTokenKind::StrLit`] was written with — the mirror of
/// `lex_aipl.aipl`'s `StrStyle`. Lets a consumer (the autoformatter) recover the
/// original spelling from the decoded value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexedStrStyle {
    /// `"..."`
    Quoted,
    /// `"""..."""` (de-dented)
    TripleQuoted,
    /// `` `...` `` (interpolation-free template)
    Backtick,
    /// ```` ```...``` ```` (interpolation-free raw template, de-dented)
    TripleBacktick,
}

/// A token kind produced by the dogfooded AIPL lexer (`lex_aipl.aipl`),
/// mirrored arm-for-arm from its `AiplTok` variant so the FFI marshaling is a
/// direct name match. Value-carrying arms hold the decoded value: a `StrLit`'s
/// escape-decoded (and, for a `Triple`/`TripleBacktick` style, de-dented)
/// contents plus its delimiter style, an int literal's value, a char literal's
/// byte. The `RawTemplate*` interpolated-segment arms hold their de-dented
/// value too (their rule's `finalize` is `dedent_segments`).
/// `Space`/comments/`AllowMarker` only ever appear in [`LexedOutput::trivia`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexedTokenKind {
    Space,
    LineComment,
    BlockComment,
    AllowMarker,
    /// `# text` — one line of a doc comment, carrying the line's text with the
    /// `#` and one optional separating space removed. A token, not trivia; see
    /// the `DOC` terminal.
    DocComment(String),
    Name(String),
    IntLit(i64),
    StrLit(String, LexedStrStyle),
    CharTok(u8),
    TemplateHead(String),
    TemplateMid(String),
    TemplateTail(String),
    RawTemplateHead(String),
    RawTemplateMid(String),
    RawTemplateTail(String),
    True,
    False,
    None,
    Fn,
    Let,
    Mut,
    Set,
    Pub,
    Import,
    From,
    As,
    For,
    While,
    Match,
    Return,
    Shim,
    Struct,
    Variant,
    If,
    Else,
    Builtins,
    EqEq,
    Ne,
    Arrow,
    FatArrow,
    AndAnd,
    OrOr,
    Pipe,
    DotDot,
    PlusPlusPlus,
    PlusPlus,
    MinusMinus,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Period,
    Comma,
    Colon,
    Semi,
    Question,
    Hash,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
}

/// One token from the dogfooded AIPL lexer: its [`LexedTokenKind`] and source
/// byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedToken {
    pub kind: LexedTokenKind,
    pub span: Span,
}

/// What the dogfooded AIPL lexer returns for a whole source: the emitted
/// token stream, and the trivia side-channel (comments and `#[allow]`
/// markers, in source order — whitespace is skipped outright and appears in
/// neither).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedOutput {
    pub tokens: Vec<LexedToken>,
    pub trivia: Vec<LexedToken>,
}

/// A hard lex error from the dogfooded AIPL lexer, with the source byte span
/// it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedError {
    pub message: String,
    pub span: Span,
}

/// The dogfooded lexer, installed by the compiler (via [`set_lex_hook`]).
static LEX_HOOK: std::sync::OnceLock<fn(&str) -> Result<LexedOutput, LexedError>> =
    std::sync::OnceLock::new();

/// The dogfooded strip-then-lex lexer, installed via [`set_lex_stripped_hook`].
static LEX_STRIPPED_HOOK: std::sync::OnceLock<fn(&str) -> Result<LexedOutput, LexedError>> =
    std::sync::OnceLock::new();

/// Install the raw lexer hook. The compiler points this at the dogfooded AIPL
/// `lex_aipl`, run through the embedding FFI. First install wins (the hook is
/// process-global).
pub fn set_lex_hook(f: fn(&str) -> Result<LexedOutput, LexedError>) {
    let _ = LEX_HOOK.set(f);
}

/// Install the strip-then-lex hook. The compiler points this at the dogfooded
/// AIPL `lex_aipl_stripped` (which strips trailing `--- section ---` blocks
/// then lexes, both dogfooded steps in one FFI crossing). First install wins.
pub fn set_lex_stripped_hook(f: fn(&str) -> Result<LexedOutput, LexedError>) {
    let _ = LEX_STRIPPED_HOOK.set(f);
}

/// Lex `src` as-is through the installed dogfooded AIPL lexer. There is no
/// native fallback: this panics if the hook isn't installed (call
/// `install_parser_hooks` first).
pub fn lex_aipl(src: &str) -> Result<LexedOutput, LexedError> {
    let hook = LEX_HOOK
        .get()
        .expect("lex hook not installed before lexing (call install_parser_hooks)");
    hook(src)
}

/// Strip trailing `--- section ---` test blocks from `src`, then lex — through
/// the installed dogfooded AIPL `lex_aipl_stripped`. No native fallback: panics
/// if the hook isn't installed (call `install_parser_hooks` first).
pub fn lex_aipl_stripped(src: &str) -> Result<LexedOutput, LexedError> {
    let hook = LEX_STRIPPED_HOOK
        .get()
        .expect("strip-lex hook not installed before lexing (call install_parser_hooks)");
    hook(src)
}

/// If `line` is a `--- name ---` test-section marker, return the trimmed
/// inner name. Used by the cases test harness to delimit sections; the
/// compiler treats any such marker as a hard cutoff (see
/// [`strip_test_sections`]).
///
/// A line is a marker iff it starts with `---` at column 0 (no leading
/// whitespace) and, once trailing whitespace is trimmed, ends with `---`
/// with a non-empty inner segment.
///
/// The marker logic is dogfooded — the AIPL `parse_test_section_header`, run
/// through the embedding FFI via the installed hook. There is **no native
/// fallback**: it panics if the hook isn't installed, so install it (via
/// `install_parser_hooks`) before parsing. (`strip_test_sections` runs this on
/// every line of every parse, so any in-process parse needs the hook.)
pub fn parse_test_section_header(line: &str) -> Option<String> {
    let hook = TEST_SECTION_HEADER_HOOK.get().expect(
        "test-section-header hook not installed before parsing (call install_parser_hooks)",
    );
    hook(line)
}

/// The test-section-header parser, installed by the compiler (via
/// [`set_test_section_header_hook`]) to dogfood the AIPL
/// `parse_test_section_header`. Required — see [`parse_test_section_header`].
static TEST_SECTION_HEADER_HOOK: std::sync::OnceLock<fn(&str) -> Option<String>> =
    std::sync::OnceLock::new();

/// Install the test-section-header parser. The compiler points this at the
/// dogfooded AIPL `parse_test_section_header`, run through the embedding FFI.
/// First install wins (the hook is process-global).
pub fn set_test_section_header_hook(f: fn(&str) -> Option<String>) {
    let _ = TEST_SECTION_HEADER_HOOK.set(f);
}

/// Return the portion of `src` before the first `--- section ---` test
/// marker. The cases test harness uses these markers to bundle expected
/// stdout/stderr/exit/errors after the AIPL code in a single file; the
/// compiler ignores them so `aipl run/ir/build` can be pointed at a test
/// fixture directly without any prep step.
///
/// The marker scan is dogfooded — the AIPL `strip_test_sections` (`str -> str`,
/// like this function), run through the embedding FFI via the installed hook,
/// returns the kept prefix; since that's a byte-prefix of `src` we re-borrow it
/// as `&src[..kept.len()]`. There is **no native fallback**: it panics if the
/// hook isn't installed, so install it (via `install_parser_hooks`) before
/// parsing. (`parse` and `lex_tokens` call this on every parse — see
/// [`set_strip_test_sections_hook`].)
pub fn strip_test_sections(src: &str) -> &str {
    let hook = STRIP_TEST_SECTIONS_HOOK.get().expect(
        "strip-test-sections hook not installed before parsing (call install_parser_hooks)",
    );
    // The returned prefix ends on a line boundary (after a `\n`, or all of `src`),
    // so its byte length is a valid char boundary to re-borrow from `src`.
    &src[..hook(src).len().min(src.len())]
}

/// The `--- file: <path> ---` companion sources declared in `src`, as
/// `(relative path, contents)` — the sibling files a case needs on disk beside
/// it (imported modules, fixtures its tests read). Empty when there are none.
///
/// Section bodies keep their inner blank lines but drop trailing newlines, which
/// is how the test harness has always written them; a companion whose path is
/// empty or contains a backslash is skipped, since both would resolve
/// unpredictably (the harness asserts on those instead, where a fixture author
/// is there to fix it).
///
/// Shared so the corpus harness and `aipl check` stage companions the same way
/// rather than each parsing the markers itself.
///
/// Dogfooded — the AIPL `companion_files`, run through the embedding FFI via the
/// installed hook. There is **no native fallback**: it panics if the hook isn't
/// installed, so install it (via `install_parser_hooks`) first, exactly like
/// [`parse_test_section_header`].
/// `Err(message)` when a `file:` marker names a path that can't be staged (empty,
/// or containing a backslash) — refusing is better than staging a case somewhere
/// unintended, and it matches what the corpus harness already asserts.
pub fn companion_files(src: &str) -> Result<Vec<(String, String)>, String> {
    let hook = COMPANION_FILES_HOOK
        .get()
        .expect("companion-files hook not installed (call install_parser_hooks)");
    hook(src)
}

/// The companion-file extractor, installed by the compiler (via
/// [`set_companion_files_hook`]) to dogfood the AIPL `companion_files`.
/// Required — see [`companion_files`].
#[allow(clippy::type_complexity)]
static COMPANION_FILES_HOOK: std::sync::OnceLock<
    fn(&str) -> Result<Vec<(String, String)>, String>,
> = std::sync::OnceLock::new();

/// Install the companion-file extractor. The compiler points this at the
/// dogfooded AIPL `companion_files`, run through the embedding FFI. First
/// install wins (the hook is process-global).
pub fn set_companion_files_hook(f: fn(&str) -> Result<Vec<(String, String)>, String>) {
    let _ = COMPANION_FILES_HOOK.set(f);
}

/// Write `companions` (from [`companion_files`]) under `dir`, creating parent
/// directories as needed. Used to give a case's tests the sibling files they
/// expect to find in the working directory.
pub fn stage_companions(dir: &Path, companions: &[(String, String)]) -> std::io::Result<()> {
    for (rel, contents) in companions {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
    }
    Ok(())
}

/// The section stripper, installed by the compiler (via
/// [`set_strip_test_sections_hook`]) to dogfood the AIPL `strip_test_sections`.
/// Required — see [`strip_test_sections`]. Returns the kept prefix (a byte-prefix
/// of its input).
static STRIP_TEST_SECTIONS_HOOK: std::sync::OnceLock<fn(&str) -> String> =
    std::sync::OnceLock::new();

/// Install the section stripper. The compiler points this at the dogfooded AIPL
/// `strip_test_sections`, run through the embedding FFI. First install wins (the
/// hook is process-global).
pub fn set_strip_test_sections_hook(f: fn(&str) -> String) {
    let _ = STRIP_TEST_SECTIONS_HOOK.set(f);
}

/// Split `src` into `(main, sections)` at the first `--- name ---` test-section
/// marker line — the counterpart of [`strip_test_sections`] that also returns the
/// stripped-off sections (empty when there are none). Dogfooded via the AIPL
/// `split_test_sections` through the embedding FFI; both halves are byte-
/// substrings of `src`, so they're re-borrowed from it. No native fallback —
/// panics if the hook isn't installed (call `install_parser_hooks` first).
pub fn split_test_sections(src: &str) -> (&str, &str) {
    let hook = SPLIT_TEST_SECTIONS_HOOK.get().expect(
        "split-test-sections hook not installed before parsing (call install_parser_hooks)",
    );
    // The main half ends on a line boundary, so its byte length is a valid char
    // boundary at which to re-borrow both halves from `src`.
    let cut = hook(src).0.len().min(src.len());
    (&src[..cut], &src[cut..])
}

/// The section splitter, installed by the compiler (via
/// [`set_split_test_sections_hook`]) to dogfood the AIPL `split_test_sections`.
/// Required — see [`split_test_sections`]. Returns `(main, sections)`, both
/// byte-substrings of its input.
static SPLIT_TEST_SECTIONS_HOOK: std::sync::OnceLock<fn(&str) -> (String, String)> =
    std::sync::OnceLock::new();

/// Install the section splitter. The compiler points this at the dogfooded AIPL
/// `split_test_sections`, run through the embedding FFI. First install wins (the
/// hook is process-global).
pub fn set_split_test_sections_hook(f: fn(&str) -> (String, String)) {
    let _ = SPLIT_TEST_SECTIONS_HOOK.set(f);
}

/// Coarse classification of a lexed token, used by the syntax-highlighting
/// test to verify the TextMate grammar at `assets/aipl.tmLanguage.json`
/// assigns sensible scopes. Comments and whitespace are not represented —
/// the lexer skips them — and are verified separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Reserved word: `fn`, `if`, `else`, `struct`, `import`, `from`,
    /// `let`, `for`, `mut`, `set`, `match`, `builtins`.
    Keyword,
    /// `true`, `false`, `none`.
    Constant,
    /// Built-in type names — lexically identifiers (`i64`, `bool`, `char`,
    /// `str`, `any`) but the highlighter scopes them as types.
    BuiltinType,
    /// User-defined identifier (function/struct/var/etc.).
    Identifier,
    /// Integer literal.
    Number,
    /// `"..."` literal.
    Str,
    /// `'.'` literal.
    Char,
    /// Operators: `+ - * / % == != < > <= >= && || ! -> =>`.
    Operator,
    /// Brackets, separators, sigils: `( ) { } [ ] , ; : . ? =`.
    Punctuation,
    /// A `# text` doc comment. Unlike `//` and `/* */`, which are trivia and
    /// never reach a token stream, this one is a token — so a consumer that
    /// walks tokens (the highlighter's oracle) has to expect it.
    Comment,
}

/// Tokenize `input` and classify each token for syntax-highlighter
/// verification. Strips test-section markers first (the lexer doesn't
/// understand them), so the caller only sees AIPL source tokens.
///
/// Lexing is dogfooded: the section stripping *and* the lexing both happen in
/// the AIPL [`lex_aipl_stripped`] via one hook crossing (no native fallback).
/// A [`LexedError`] becomes an [`Error`] at its span.
pub fn lex_tokens(input: &str) -> Result<Vec<(TokenKind, Span)>, Error> {
    let out = lex_aipl_stripped(input).map_err(|e| Error::at(e.message, e.span))?;
    Ok(out
        .tokens
        .into_iter()
        .map(|t| (classify_lexed(&t.kind), t.span))
        .collect())
}

/// A [`TokenKind`] refined for the formatter: template-literal pieces are kept
/// distinct instead of folded into `Str`. The formatter copies a template
/// verbatim from its head to its matching tail, so it must see the piece
/// boundaries — and it can't recover them from token text (an empty segment's
/// `TemplateMiddle` is the single character `{`, identical to a brace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtTokenKind {
    Plain(TokenKind),
    /// `` `text{ `` — opens a template literal, through the first `{`.
    TemplateHead,
    /// `}text{` between two interpolations. Its span runs brace to brace — from
    /// the `}` that closed the previous interpolation through the `{` that opens
    /// the next — so a template's tokens tile it with no gap. The `}` gets no
    /// token of its own; it is this one's first byte.
    TemplateMiddle,
    /// `` }text` `` — closes the template literal.
    TemplateTail,
}

/// Map a dogfooded-lexer token kind to the formatter's [`FmtTokenKind`]: an
/// interpolated-template piece (regular *or* raw) keeps its head/middle/tail
/// position, and every other kind — including all four merged `StrLit` string
/// forms — folds to `Plain(classify_lexed(..))` (a `StrLit` classifies to
/// `Str`). Mirrors the native lexer, whose `classify` folds an interpolation-
/// free template to `Str` and collapses raw template pieces to the same
/// head/middle/tail as regular ones.
fn fmt_kind(k: &LexedTokenKind) -> FmtTokenKind {
    use LexedTokenKind as K;
    match k {
        K::TemplateHead(_) | K::RawTemplateHead(_) => FmtTokenKind::TemplateHead,
        K::TemplateMid(_) | K::RawTemplateMid(_) => FmtTokenKind::TemplateMiddle,
        K::TemplateTail(_) | K::RawTemplateTail(_) => FmtTokenKind::TemplateTail,
        other => FmtTokenKind::Plain(classify_lexed(other)),
    }
}

/// The comment spans in a dogfooded-lexer run's trivia, in source order — the
/// line and block comments *and* the `#[allow]` markers. The native tokenizer
/// records a `#[allow]` in its comment sink too ("so the formatter carries it
/// through, glued to its line like a trailing comment"), so every trivia record
/// is a comment for the formatter's purposes. (Whitespace is skipped outright
/// and never reaches the trivia channel.)
fn comment_spans(out: &LexedOutput) -> Vec<Span> {
    out.trivia.iter().map(|t| t.span.clone()).collect()
}

/// Tokenize `input` for the formatter: every token plus the span of every
/// comment, both in source order (token text is recovered from the span, so
/// literals stay verbatim). Lexing is dogfooded ([`lex_aipl`], via the installed
/// hook). Unlike [`lex_tokens`] the input is taken as-is — no test-section
/// stripping — because the formatter splits trailing `--- section ---` blocks
/// off itself and must account for every byte it is given.
#[allow(clippy::type_complexity)]
pub fn lex_tokens_and_comments(
    input: &str,
) -> Result<(Vec<(FmtTokenKind, Span)>, Vec<Span>), Error> {
    let out = lex_aipl(input).map_err(|e| Error::at(e.message, e.span))?;
    let toks = out
        .tokens
        .iter()
        .map(|t| (fmt_kind(&t.kind), t.span.clone()))
        .collect();
    Ok((toks, comment_spans(&out)))
}

/// Tokenize `input` for the formatter's *preservation check*: each token as
/// `(kind, signature)` plus every comment span. A token's signature is its
/// **semantic value** — for a string literal (any of the four delimiter forms,
/// all now one `StrLit`) or a template piece, the lexer's decoded value (escapes
/// applied, and a `"""`/```` ``` ```` literal de-dented — done in the lexer's
/// emit); for everything else, the raw source text (recovered from the span).
/// Two spellings that lex to the same value therefore share a signature, so the
/// formatter's value-preserving whitespace edits (re-indenting a raw block's
/// content or its closing delimiter) don't register as changes, while any real
/// change to a literal's value does. Input is taken as-is (no section
/// stripping), like [`lex_tokens_and_comments`]. Every string/template value —
/// including an interpolated raw template's segments — arrives already decoded
/// and de-dented from the lexer, so re-indenting a raw block is value-preserving.
#[allow(clippy::type_complexity)]
pub fn lex_signatures_and_comments(
    input: &str,
) -> Result<(Vec<(FmtTokenKind, String)>, Vec<Span>), Error> {
    use LexedTokenKind as K;
    let out = lex_aipl(input).map_err(|e| Error::at(e.message, e.span))?;
    let toks = out
        .tokens
        .iter()
        .map(|t| {
            let sig = match &t.kind {
                K::StrLit(v, _)
                | K::TemplateHead(v)
                | K::TemplateMid(v)
                | K::TemplateTail(v)
                | K::RawTemplateHead(v)
                | K::RawTemplateMid(v)
                | K::RawTemplateTail(v) => v.clone(),
                _ => input[t.span.clone()].to_string(),
            };
            (fmt_kind(&t.kind), sig)
        })
        .collect();
    Ok((toks, comment_spans(&out)))
}

/// Coarse-classify a dogfooded-lexer token kind, exactly as [`classify`] does
/// for the native `Terminal` — including the identifier-text refinement that
/// scopes the built-in type names (`i64`/`bool`/`char`/…) as `BuiltinType`.
/// The trivia kinds (`Space`/comments/`AllowMarker`) never appear in the token
/// stream (they ride the trivia side-channel), so reaching one is a bug.
fn classify_lexed(k: &LexedTokenKind) -> TokenKind {
    use LexedTokenKind as K;
    match k {
        K::Fn
        | K::If
        | K::Else
        | K::Struct
        | K::Variant
        | K::Import
        | K::From
        | K::As
        | K::Pub
        | K::Let
        | K::For
        | K::While
        | K::Mut
        | K::Set
        | K::Match
        | K::Return
        | K::Shim
        | K::Builtins => TokenKind::Keyword,
        K::True | K::False | K::None => TokenKind::Constant,
        K::Name(s) => match s.as_str() {
            "bool" | "char" | "str" | "any" => TokenKind::BuiltinType,
            _ if aipl_syntax::int_bits(s).is_some() => TokenKind::BuiltinType,
            _ => TokenKind::Identifier,
        },
        K::IntLit(_) => TokenKind::Number,
        K::StrLit(_, _)
        | K::TemplateHead(_)
        | K::TemplateMid(_)
        | K::TemplateTail(_)
        | K::RawTemplateHead(_)
        | K::RawTemplateMid(_)
        | K::RawTemplateTail(_) => TokenKind::Str,
        K::CharTok(_) => TokenKind::Char,
        K::EqEq
        | K::Ne
        | K::Arrow
        | K::FatArrow
        | K::AndAnd
        | K::OrOr
        | K::Pipe
        | K::DotDot
        | K::PlusPlusPlus
        | K::PlusPlus
        | K::MinusMinus
        | K::PlusEq
        | K::MinusEq
        | K::StarEq
        | K::SlashEq
        | K::Eq
        | K::Lt
        | K::Le
        | K::Gt
        | K::Ge
        | K::Bang
        | K::Plus
        | K::Minus
        | K::Star
        | K::Slash
        | K::Percent => TokenKind::Operator,
        K::Period
        | K::Comma
        | K::Colon
        | K::Semi
        | K::Question
        | K::Hash
        | K::LParen
        | K::RParen
        | K::LBrace
        | K::RBrace
        | K::LBracket
        | K::RBracket => TokenKind::Punctuation,
        // A doc comment *is* in the token stream (see the `DOC` terminal), and
        // to a highlighter it is a comment like any other.
        K::DocComment(_) => TokenKind::Comment,
        K::Space | K::LineComment | K::BlockComment | K::AllowMarker => {
            unreachable!("trivia kind {k:?} in the token stream")
        }
    }
}

/// The dogfooded AIPL parser, installed by the compiler (via [`set_parse_hook`]):
/// a whole file to its `Program` and the spans of its `#[allow]` markers.
static PARSE_HOOK: std::sync::OnceLock<fn(&str) -> Result<(Program, Vec<Span>), Error>> =
    std::sync::OnceLock::new();

/// Install the parser. The compiler points this at the dogfooded
/// `aipl_parse_file` (`grammar_aipl.aipl`), run through the embedding FFI and
/// rebuilt into a `Program` on this side. First install wins (the hook is
/// process-global).
pub fn set_parse_hook(f: fn(&str) -> Result<(Program, Vec<Span>), Error>) {
    let _ = PARSE_HOOK.set(f);
}

/// Parse `input` — a whole source file, expected-output sections and all — to
/// its [`Program`], with the two post-parse rewrites applied.
///
/// Everything a parse involves happens on the AIPL side of the hook: the
/// sections are stripped, trailing whitespace is refused, the source is lexed
/// losslessly and parsed, and the tree is lowered. What comes back is that
/// `Program`, which [`post_parse`] then finishes.
pub fn parse(input: &str) -> Result<Program, Error> {
    parse_with_allows(input).map(|(program, _)| program)
}

/// [`parse`], additionally returning the spans of every `#[allow]`
/// lint-squelch marker in the file — the loader hands them to
/// [`aipl_syntax::lint`]'s pass, which drops lint errors squelched by a
/// same-line marker. The markers are trivia, so no production ever sees one;
/// they ride the parser's return value as its one side-channel.
pub fn parse_with_allows(input: &str) -> Result<(Program, Vec<Span>), Error> {
    let hook = PARSE_HOOK
        .get()
        .expect("parse hook not installed before parsing (call install_parser_hooks)");
    let (mut program, allows) = hook(input)?;
    post_parse(&mut program, strip_test_sections(input));
    Ok((program, allows))
}

/// The two rewrites that follow a parse but are no part of one.
///
/// Neither belongs to a grammar. `bake_asserts` needs the source text, and
/// `promote_type_vars` needs a declaration's own signature, so both are things
/// done *to* a `Program` once it exists — which is why they stayed on this side
/// of the hook when the parser moved to AIPL, and why they are a named function:
/// [`parse_with_allows`] calls it on what the hook hands back.
///
/// `src` must be the string the program's spans are relative to — the
/// section-stripped source, not the whole file.
pub fn post_parse(program: &mut Program, src: &str) {
    // Bake `assert(cond)` calls inside `.test({ .. })` bodies into
    // `__assert(cond, "input:LINE: TEXT")`, capturing each assertion's source
    // location while the source is in hand, for the `check` failure report.
    // Only test bodies are rewritten, so a bare `assert(..)` elsewhere stays an
    // unknown call — `assert` is effectively test-only.
    for item in &mut program.items {
        if let Item::Fn(f) = item {
            if let Some(test_body) = &mut f.test_body {
                bake_asserts(test_body, src);
            }
        }
    }

    // A declaration's own type parameters stop being ordinary names here, at the
    // one point every path shares: source files reach the checker through the
    // loader, but the builtin signatures are parsed directly.
    aipl_syntax::promote_type_vars(program);
}

/// Rewrite each `assert(cond)` within `e` into `__assert(cond, "input:LINE:
/// TEXT")`, where the location string is computed from `src` and the condition's
/// span. Recurses through the whole expression so nested asserts are caught.
fn bake_asserts(e: &mut Expr, src: &str) {
    // Rewrite an `assert(cond)` in place, then recurse into the condition.
    if let ExprKind::Call(name, args, _) = &e.kind {
        if name == "assert" && args.len() == 1 {
            let ExprKind::Call(_, mut args, _) = std::mem::replace(&mut e.kind, ExprKind::Unit)
            else {
                unreachable!()
            };
            let mut cond = args.pop().expect("one arg");
            bake_asserts(&mut cond, src);
            let loc = Expr::new(
                ExprKind::Str(assert_loc(src, cond.span.clone())),
                cond.span.clone(),
            );
            e.kind = ExprKind::Call("__assert".to_string(), vec![cond, loc], false);
            return;
        }
    }
    match &mut e.kind {
        // A shim's bindings are names; asserts can only be in its body.
        ExprKind::Shim(_, _, body) => bake_asserts(body, src),
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args, _)
        | ExprKind::TupleLit(args) => {
            for a in args {
                bake_asserts(a, src);
            }
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                bake_asserts(k, src);
                bake_asserts(v, src);
            }
        }
        ExprKind::Seq(a, b)
        | ExprKind::Let(_, _, a, b)
        | ExprKind::LetMut(_, _, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::Index(a, b)
        | ExprKind::For(_, a, b)
        | ExprKind::While(a, b) => {
            bake_asserts(a, src);
            bake_asserts(b, src);
        }
        ExprKind::If(a, b, c) => {
            bake_asserts(a, src);
            bake_asserts(b, src);
            bake_asserts(c, src);
        }
        ExprKind::Slice(a, b, c) => {
            bake_asserts(a, src);
            bake_asserts(b, src);
            if let Some(c) = c {
                bake_asserts(c, src);
            }
        }
        ExprKind::Neg(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x)
        | ExprKind::KwArg(_, x, _)
        | ExprKind::Spread(x) => bake_asserts(x, src),
        ExprKind::Construct(_, inits) => {
            for fi in inits {
                bake_asserts(&mut fi.value, src);
            }
        }
        ExprKind::Match(scrut, arms) => {
            bake_asserts(scrut, src);
            for arm in arms {
                bake_asserts(&mut arm.body, src);
            }
        }
        ExprKind::IfLet(arm, scrut, else_b) => {
            bake_asserts(scrut, src);
            bake_asserts(&mut arm.body, src);
            bake_asserts(else_b, src);
        }
        ExprKind::Lambda(_, body) => bake_asserts(body, src),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::None
        | ExprKind::Unit => {}
    }
}

/// Format an assertion's source location as `input:LINE: TEXT` (1-based line,
/// the condition's trimmed source text), matching the `input:` filename the rest
/// of the compiler's diagnostics use. Dogfooded: the AIPL `assert_loc`, run
/// through the embedding FFI via the installed hook. There is **no native
/// fallback**: it panics if the hook isn't installed, so install it (via
/// `install_parser_hooks`) before parsing.
fn assert_loc(src: &str, span: Span) -> String {
    let hook = ASSERT_LOC_HOOK
        .get()
        .expect("assert-loc hook not installed before parsing (call install_parser_hooks)");
    hook(src, span)
}

/// The assertion-location formatter, installed by the compiler (via
/// [`set_assert_loc_hook`]) to dogfood the AIPL `assert_loc`. Required — see
/// [`assert_loc`].
static ASSERT_LOC_HOOK: std::sync::OnceLock<fn(&str, Span) -> String> = std::sync::OnceLock::new();

/// Install the assertion-location formatter. The compiler points this at the
/// dogfooded AIPL `assert_loc`, run through the embedding FFI. First install
/// wins (the hook is process-global).
pub fn set_assert_loc_hook(f: fn(&str, Span) -> String) {
    let _ = ASSERT_LOC_HOOK.set(f);
}
