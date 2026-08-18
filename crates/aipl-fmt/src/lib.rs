//! The AIPL auto-formatter: canonical source layout (gofmt-style — the
//! formatter, not the author, decides line breaks) driven by a width limit.
//!
//! The layout itself is **dogfooded**: `walker.aipl` walks the token stream into
//! a `Doc` tree and `doc.aipl` prints it, reached here through the FFI as
//! [`aipl_codegen::format_program`]. There is no native fallback — this module
//! is only the wrapper around that call:
//!
//! 1. split off the trailing `--- section ---` blocks, which are re-attached
//!    byte-for-byte (their bodies are expectations where even trailing
//!    whitespace can matter),
//! 2. strip per-line trailing whitespace, so every span the walker copies
//!    verbatim refers to the cleaned text,
//! 3. validate with the real parser — its errors are the good ones, and
//!    anything it accepts the walker must handle,
//! 4. lay the code out (the dogfooded call),
//! 5. normalize to exactly one trailing newline, and
//! 6. verify the output holds exactly the input's tokens and comments.
//!
//! Style (see the repo discussion): 4-space indent; width-limited groups that
//! either fit on one line or block-indent one element per line with a trailing
//! comma; imports hoisted to the top (builtins first, then paths sorted; names
//! within a list sorted by imported name, operators first); exactly one blank
//! line between top-level items; call-site keyword arguments spelled tight
//! (`f(1, k=1)`) but declaration defaults spaced (`k: i64 = 1`). String, char,
//! number, and template literals — and everything inside a template's
//! interpolations — are emitted verbatim from the source.

use aipl_parser::{lex_signatures_and_comments, FmtTokenKind};
use aipl_syntax::{Error, Span};

/// Options for [`format_source`]. More knobs may grow here; construct with
/// `FmtOptions::default()` and override fields.
#[derive(Debug, Clone)]
pub struct FmtOptions {
    /// Maximum line width the layout aims for (long verbatim atoms — string
    /// literals, templates — may still exceed it).
    pub max_width: usize,
}

impl Default for FmtOptions {
    fn default() -> Self {
        FmtOptions { max_width: 100 }
    }
}

/// Format AIPL source to the canonical style. The output always ends in
/// exactly one newline (an empty program formats to just that). The input's
/// trailing `--- section ---` blocks (if any) are preserved byte-for-byte —
/// they follow that newline, and whatever they end with is theirs; trailing
/// whitespace in the source portion is removed (the language rejects it, so
/// fixing it can't change an accepted program's meaning).
///
/// Requires the parser hooks (`aipl::install_parser_hooks`) — lexing a
/// `"""` raw string runs the dogfooded de-denter.
pub fn format_source(src: &str, opts: &FmtOptions) -> Result<String, Error> {
    // The pipeline itself is dogfooded (`format_source.aipl`), in the two halves
    // this wraps around the checks that stay in Rust. `fmt_prepare` splits off
    // the trailing sections and strips trailing whitespace; `fmt_layout` lays the
    // rest out and normalizes the final newline.
    let input = aipl_codegen::fmt_prepare(src);

    // Validate with the real parser *before* laying out: its errors are the good
    // ones, and anything it accepts the walker must handle. This is why the AIPL
    // side is two entries — a Rust step sits in the middle of the pipeline.
    aipl_parser::parse(&input.cleaned)?;

    let out = aipl_codegen::fmt_layout(&input.cleaned, opts.max_width)?;

    // Safety net: the output must contain exactly the input's tokens and
    // comments (imports may be reordered, so compare as multisets). Any
    // mismatch is a formatter bug — refuse to emit rather than corrupt code.
    // Runs before the sections are re-attached, so it compares like with like.
    verify_same_tokens(&input.cleaned, &out)?;

    Ok(out + &input.sections)
}

/// Lex both texts and compare token/comment content as multisets (imports may
/// legitimately reorder). Tokens are compared by *semantic value* (see
/// [`lex_signatures_and_comments`]), so the formatter's value-preserving
/// whitespace edits inside a raw block are not flagged, while any change to a
/// literal's value is. Errors with a formatter-bug message on mismatch.
fn verify_same_tokens(input: &str, output: &str) -> Result<(), Error> {
    let (in_toks, in_comments) = lex_signatures_and_comments(input)?;
    let (out_toks, out_comments) = lex_signatures_and_comments(output)
        .map_err(|e| Error::msg(format!("formatter produced unlexable output: {e}")))?;
    // Trailing commas are normalized by design (dropped when a list renders
    // flat, added when it breaks), so commas don't participate in the check.
    let texts = |toks: &[(FmtTokenKind, String)]| -> Vec<String> {
        let mut v: Vec<String> = toks
            .iter()
            .filter(|(_, sig)| sig != ",")
            .map(|(k, sig)| format!("{k:?} {sig}"))
            .collect();
        v.sort();
        v
    };
    let ctexts = |src: &str, cs: &[Span]| -> Vec<String> {
        let mut v: Vec<String> = cs.iter().map(|sp| src[sp.clone()].to_string()).collect();
        v.sort();
        v
    };
    let before = texts(&in_toks);
    let after = texts(&out_toks);
    if before != after {
        // Name a few tokens from the symmetric difference, to make the bug
        // report (and debugging) concrete.
        let missing: Vec<&String> = before.iter().filter(|t| !after.contains(t)).collect();
        let added: Vec<&String> = after.iter().filter(|t| !before.contains(t)).collect();
        return Err(Error::msg(format!(
            "formatter would not preserve the token stream (formatter bug); no output written. \
             lost: {:?}; gained: {:?}",
            &missing[..missing.len().min(5)],
            &added[..added.len().min(5)],
        )));
    }
    if ctexts(input, &in_comments) != ctexts(output, &out_comments) {
        return Err(Error::msg(
            "formatter would not preserve comments (formatter bug); no output written",
        ));
    }
    Ok(())
}
