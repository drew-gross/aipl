use crate::ast::{Expr, ExprKind, SetOrder};
use crate::Error;

use super::line_of;

/// How many spelled-out elements it takes before the string form wins.
///
/// Three, because two is where the two spellings are genuinely a tie: `#{'a',
/// 'b'}` and `"ab".to_set()` are the same width, and the literal keeps each
/// element visibly a `char`. From three on, the quotes and commas outweigh the
/// characters they separate — and the sets that matter in practice (a character
/// class, a set of punctuation, the digits) are far past three, where the
/// literal form is almost unreadable.
const MIN_ELEMENTS: usize = 3;

/// The source spelling of a `str` literal holding exactly `bytes`, or `None`
/// when one of them has no spelling inside one.
///
/// Only the escapes AIPL's own string rule accepts (`\n`, `\t`, `\r`, `\\`,
/// `\"` — see `Delimited` in `lex_aipl.aipl`). A `'` needs none inside double
/// quotes. Anything else non-printable is left alone rather than guessed at:
/// advice that does not lex is worse than no advice.
fn string_literal(bytes: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => return None,
        }
    }
    out.push('"');
    Some(out)
}

/// `#{'a', 'b', 'c'}` — a set of char literals spelled out one quoted element at
/// a time. `"abc".to_set()` is the same set, written as the thing it describes:
/// the characters, in a row, with nothing between them.
///
/// It reads better at every size and it costs nothing. `to_set` on a *string
/// literal* is folded by codegen into the four immediate stores that are a
/// `#{char}`'s whole representation (see the `Callee::ToSet` arm in
/// `aipl-codegen`), which is exactly what the set literal compiles to — the two
/// spellings emit byte-identical IR, so the shorter one is free.
///
/// That was not true when this lint was written: the fold ran *after* the
/// receiver was compiled, so the string form also built a 24-byte string value
/// nothing read. The fold now settles before the receiver, which is what makes
/// the promise above exact.
///
/// **Only the unordered `#{..}`.** An ordered literal is not interchangeable:
/// `to_set` leaves its order to the use site (`SetOrder::Context`), so
/// `#<{'z', 'a', 'm'}` where nothing pins an order — rendered straight into a
/// template, say — would become the *unordered* set and stop sorting. A lint
/// may not advise a rewrite that changes what the program does.
///
/// **Only a literal written on one line**, which is what keeps every hit
/// squelchable. `#[allow]` is line-scoped, and `aipl fmt` breaks a set literal
/// too wide for one line into one element per line — then relocates a trailing
/// marker from inside the braces onto the statement's closing `};`, a line the
/// span does not start on. A literal it keeps on one line carries the marker at
/// the end of that line, where it does squelch. At three char elements a
/// literal is about fifteen columns, so the skipped ones are the very wide ones
/// (~18 elements and up); the corpus has none, and after this lint nobody
/// writes one.
pub(super) fn char_set_literal(e: &Expr, src: &str, to_set: Option<&str>, hits: &mut Vec<Error>) {
    let ExprKind::SetLit(elems, SetOrder::Unordered) = &e.kind else {
        return;
    };
    if elems.len() < MIN_ELEMENTS {
        return;
    }
    // Every element a char *literal*: that is what makes this a char set
    // without asking the type checker (lints run before it), and what lets the
    // advice name the string it would become.
    let mut bytes: Vec<u8> = Vec::with_capacity(elems.len());
    for el in elems {
        let ExprKind::Char(b) = el.kind else {
            return;
        };
        bytes.push(b);
    }
    let Some(literal) = string_literal(&bytes) else {
        return;
    };
    if line_of(src, e.span.start) != line_of(src, e.span.end) {
        return;
    }
    let name = to_set.unwrap_or("to_set");
    // Name the import too when it's missing: following the advice without it
    // only trades this error for the unimported-name one.
    let import = match to_set {
        Some(_) => String::new(),
        None => format!(", importing `{name}` from builtins"),
    };
    // Backticks, not the quotes the other lints wrap their advice in: the
    // advice *is* a string literal, and `""abc".to_set()"` reads as neither.
    hits.push(Error::at(
        format!(
            "a char set of {} elements spelled out — write `{literal}.{name}()`{import} \
             (or append #[allow] to this line to keep it)",
            elems.len()
        ),
        e.span.clone(),
    ));
}
