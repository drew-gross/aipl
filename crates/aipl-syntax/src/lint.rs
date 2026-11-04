//! Lints: *squelchable* errors. AIPL has no warnings — every diagnostic is an
//! error and fails the compile — but the errors this module produces (and only
//! these) can be squelched by appending `#[allow]` to the offending line. The
//! marker is line-scoped: it silences every lint whose reported span starts on
//! its line, and nothing else. Regular errors (type mismatches, unknown names,
//! parse errors, ...) take no notice of `#[allow]`. A marker on a line where no
//! lint fires is itself an error, so a marker never outlives the code it excused.
//!
//! A lint flags code that is *legal but has a clearly better spelling*; its
//! message must name that better spelling. The loader runs `aipl_mono::check` on every
//! file right after parsing (the markers come from the lexer via
//! `parse_with_allows`), so lints fire before type checking.
//!
//! Every lint is written in AIPL (`crates/aipl-codegen/src/lint/`) and runs with the parse; this
//! module takes the hits they return and applies the squelching.

use crate::{Error, Span};
use std::collections::HashSet;

/// What a lint's message ends with: the way to keep what it flagged. Appended by
/// [`check`] to each hit it returns, so a lint's message says only what is wrong
/// and what to write instead.
const SQUELCH_ADVICE: &str = " (or append #[allow] to this line to keep it)";

/// Drop the `hits` squelched by a same-line `#[allow]` (`allows` are the marker
/// spans the lexer collected) and say how to keep each of the rest. Returns every
/// surviving lint error.
pub fn check(src: &str, allows: &[Span], mut hits: Vec<Error>) -> Result<(), Vec<Error>> {
    let allowed: HashSet<usize> = allows.iter().map(|sp| line_of(src, sp.start)).collect();
    // The lines whose marker squelched a hit; a marker on any other line is
    // reported below.
    let mut squelched: HashSet<usize> = HashSet::new();
    hits.retain(|e| match &e.span {
        Some(sp) => {
            let line = line_of(src, sp.start);
            if allowed.contains(&line) {
                squelched.insert(line);
                false
            } else {
                true
            }
        }
        None => true,
    });
    // Every hit that survives says how to keep it, so no lint spells that out in
    // its own message. A hit with no span has no line for a marker to go on.
    for hit in hits.iter_mut().filter(|e| e.span.is_some()) {
        hit.message.push_str(SQUELCH_ADVICE);
    }
    // A marker is an acknowledgement of a lint that fired, so one with nothing to
    // squelch is stale: the code was fixed, or the marker was never needed. It is
    // not itself squelchable — what it would squelch is itself.
    for sp in allows {
        if !squelched.contains(&line_of(src, sp.start)) {
            hits.push(Error::at(
                "this `#[allow]` squelches nothing — remove it",
                sp.clone(),
            ));
        }
    }
    // Every surviving hit, not just the first: lints are independent
    // findings over the whole file, so there is nothing to recover from and
    // no reason to make the reader re-run to see the next one.
    if hits.is_empty() {
        Ok(())
    } else {
        Err(hits)
    }
}

/// 0-based line number of byte offset `pos` in `src`.
fn line_of(src: &str, pos: usize) -> usize {
    src[..pos.min(src.len())].matches('\n').count()
}
