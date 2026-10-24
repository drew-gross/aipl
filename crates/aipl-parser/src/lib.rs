//! The AIPL parser's entry points, and what surrounds a parse.
//!
//! The parser itself is written in AIPL — `crates/aipl-codegen/src/grammar_aipl.aipl`
//! is the grammar as data, `parse.aipl` the driver that runs it, and `ast.aipl`
//! the tree it lowers to — and is compiled into the checked-in dogfood artifact
//! the compiler links. This crate is the Rust side of that boundary: the hook
//! the compiler installs to reach it (`set_parse_hook`), the lexer's token
//! types as they cross the FFI, and the other dogfooded helpers a parse leans
//! on (section stripping, companion files, assertion locations).
//!
//! Nothing here parses by hand, and there is **no native fallback**: a parse
//! without the hook installed is a panic, not a slower path.

use std::path::Path;

use aipl_syntax::ast::Program;
use aipl_syntax::{Error, Span};

/// One token's span and the TextMate scope AIPL's own rule set declares for it —
/// the Rust twin of `lexer.aipl`'s `ScopeSpan`.
///
/// This is the whole of what Rust asks the lexer for. The token *kinds* used to
/// be mirrored here too, arm for arm with `AiplTok` — 80 of them plus a coarse
/// classifier — for one consumer: the test that checks the generated TextMate
/// grammar paints every token sensibly. That test wants the scope, and the rule
/// set already names it per rule, so it asks for that directly and the mirror is
/// gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSpan {
    pub span: Span,
    pub scope: String,
}

/// Anything that stopped a lex. Mirrors `lexer.aipl`'s `LexError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexedError {
    pub message: String,
    pub span: Span,
}

/// The dogfooded scope-span lexer, installed via [`set_token_scopes_hook`].
static TOKEN_SCOPES_HOOK: std::sync::OnceLock<fn(&str) -> Result<Vec<ScopeSpan>, LexedError>> =
    std::sync::OnceLock::new();

/// Install the scope-span hook. The compiler points this at the dogfooded AIPL
/// `aipl_token_scopes`. First install wins (the hook is process-global).
pub fn set_token_scopes_hook(f: fn(&str) -> Result<Vec<ScopeSpan>, LexedError>) {
    let _ = TOKEN_SCOPES_HOOK.set(f);
}

/// Every token's span and declared scope, with trailing `--- section ---`
/// harness blocks stripped first — through the dogfooded AIPL
/// `aipl_token_scopes`. No native fallback: panics if the hook isn't installed
/// (call `install_parser_hooks` first).
pub fn token_scopes(input: &str) -> Result<Vec<ScopeSpan>, Error> {
    let hook = TOKEN_SCOPES_HOOK
        .get()
        .expect("token-scopes hook not installed before lexing (call install_parser_hooks)");
    hook(input).map_err(|e| Error::at(e.message, e.span))
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

/// The dogfooded AIPL parser, installed by the compiler (via [`set_parse_hook`]):
/// a whole file to its `Program` and the spans of its `#[allow]` markers.
static PARSE_HOOK: std::sync::OnceLock<fn(&str) -> Result<(Program, Vec<Span>), Vec<Error>>> =
    std::sync::OnceLock::new();

/// Install the parser. The compiler points this at the dogfooded
/// `parse_and_partial_load` (`parse_and_partial_load.aipl`), run through the embedding FFI and
/// rebuilt into a `Program` on this side. First install wins (the hook is
/// process-global).
pub fn set_parse_hook(f: fn(&str) -> Result<(Program, Vec<Span>), Vec<Error>>) {
    let _ = PARSE_HOOK.set(f);
}

/// Parse `input` — a whole source file, expected-output sections and all — to
/// its [`Program`].
///
/// Everything a parse involves happens on the AIPL side of the hook: the
/// sections are stripped, trailing whitespace is refused, the source is lexed
/// losslessly and parsed, and the tree is lowered. What comes back is that
/// `Program`.
///
/// A file with several syntax errors fails here with the first; a caller that
/// reports them all wants [`parse_with_allows`].
pub fn parse(input: &str) -> Result<Program, Error> {
    parse_with_allows(input)
        .map(|(program, _)| program)
        .map_err(|errors| {
            errors
                .into_iter()
                .next()
                .expect("a failed parse has an error")
        })
}

/// [`parse`], additionally returning the spans of every `#[allow]`
/// lint-squelch marker in the file — the loader hands them to
/// [`aipl_syntax::lint`]'s pass, which drops lint errors squelched by a
/// same-line marker. The markers are trivia, so no production ever sees one;
/// they ride the parser's return value as its one side-channel.
///
/// A file that does not parse fails with *every* syntax error the parser found,
/// in source order and never empty: where the grammar has committed to what
/// must come next, a miss is reported and the parse reads on, so several slips
/// in one file are all reported by one run.
pub fn parse_with_allows(input: &str) -> Result<(Program, Vec<Span>), Vec<Error>> {
    let hook = PARSE_HOOK
        .get()
        .expect("parse hook not installed before parsing (call install_parser_hooks)");
    hook(input)
}
