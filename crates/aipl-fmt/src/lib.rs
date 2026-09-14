//! The AIPL auto-formatter: canonical source layout (gofmt-style — the
//! formatter, not the author, decides line breaks) driven by a width limit.
//!
//! The formatter is **dogfooded end to end**: `format_source.aipl` is the whole
//! pipeline — split off the trailing `--- section ---` blocks (re-attached
//! byte-for-byte, since their bodies are expectations where even trailing
//! whitespace can matter), strip per-line trailing whitespace, validate with
//! the real parser (its errors are the good ones, and anything it accepts the
//! walker must handle), lay the code out (`walker.aipl` walks the token stream
//! into a `Doc` tree and `doc.aipl` prints it), normalize to exactly one
//! trailing newline, and verify the output holds exactly the input's tokens
//! and comments, refusing to emit otherwise. It is reached here through the
//! FFI as [`aipl_codegen::format_source`]; there is no native fallback, and
//! this module is only the Rust-facing signature around that call.
//!
//! Style (see the repo discussion): 4-space indent; width-limited groups that
//! either fit on one line or block-indent one element per line with a trailing
//! comma; imports hoisted to the top (builtins first, then paths sorted; names
//! within a list sorted by imported name, operators first); exactly one blank
//! line between top-level items; call-site keyword arguments spelled tight
//! (`f(1, k=1)`) but declaration defaults spaced (`k: i64 = 1`). String, char,
//! number, and template literals — and everything inside a template's
//! interpolations — are emitted verbatim from the source.

use aipl_syntax::Error;

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
/// Needs no parser hooks: the pipeline lexes and parses in-engine, where the
/// dogfooded lexer reaches the `"""` de-denter directly.
pub fn format_source(src: &str, opts: &FmtOptions) -> Result<String, Error> {
    aipl_codegen::format_source(src, opts.max_width)
}
