use crate::ast::{Expr, ExprKind};
use crate::Error;

use super::{pushed_element, spans_its_text};

/// Whether `e` mentions the binding `acc` anywhere.
fn mentions(e: &Expr, acc: &str) -> bool {
    let mut hit = false;
    crate::each_subexpr(e, &mut |x| {
        if matches!(&x.kind, ExprKind::Ident(n) if n == acc) {
            hit = true;
        }
    });
    hit
}

/// The source text that spells `e`, or `placeholder` when its span doesn't
/// cover it. [`spans_its_text`](super::spans_its_text()) settles the name and
/// field-path forms; a literal is added on top of them here, and *checked*
/// against the source rather than assumed, because the parser also synthesizes
/// literals carrying the span of whatever desugared into them — the `1` an `++`
/// expands to holds the `++`'s own span, and splicing that back would quote the
/// wrong text. A literal that fails its check simply falls back to the
/// placeholder.
fn quote<'a>(e: &Expr, src: &'a str, placeholder: &'a str) -> &'a str {
    let text = src.get(e.span.clone()).unwrap_or("");
    let spells_it = match &e.kind {
        ExprKind::Num(n) => text.parse::<i64>() == Ok(*n),
        ExprKind::Bool(b) => text == if *b { "true" } else { "false" },
        ExprKind::Char(_) => text.len() >= 3 && text.starts_with('\'') && text.ends_with('\''),
        // A `"""` block spells itself too — it is raw source either way — but
        // only while it stays on one line, which is all a message can hold.
        ExprKind::Str(_) => {
            text.len() >= 2 && text.starts_with('"') && text.ends_with('"') && !text.contains('\n')
        }
        _ => spans_its_text(e),
    };
    if spells_it {
        text
    } else {
        placeholder
    }
}

/// One element of the rewritten literal. A spread stays a spread — the seed may
/// itself be a literal holding one (`[..a, 3]`), and its elements come across
/// unchanged.
fn elem_text(e: &Expr, src: &str) -> String {
    match &e.kind {
        ExprKind::Spread(inner) => format!("..{}", quote(inner, src, "<array>")),
        _ => quote(e, src, "<element>").to_string(),
    }
}

/// What the seed contributes to the front of the literal. A seed that is
/// *already* a literal is spliced open rather than spread into itself — `mut xs
/// = [5]; set xs.push(6); xs` is `[5, 6]`, not `[..[5], 6]` — which also covers
/// the empty seed, whose contribution is nothing at all.
fn seed_parts(seed: &Expr, src: &str) -> Vec<String> {
    match &seed.kind {
        ExprKind::ArrayLit(xs) => xs.iter().map(|x| elem_text(x, src)).collect(),
        _ => vec![format!("..{}", quote(seed, src, "<array>"))],
    }
}

/// ```text
/// mut out = xs;
/// set out.push(x);
/// out
/// ```
///
/// — an array bound `mut` only to append to it and hand it straight back.
/// That is the literal `[..xs, x]`: one expression that says what the array
/// *is*, rather than a binding the reader has to follow through two more
/// statements to find out that appending is all that ever happens to it.
///
/// A run of pushes collapses the same way (`[..xs, a, b]`), and a seed that is
/// itself a literal is spliced open rather than spread into itself — `mut out:
/// i64[] = []; set out.push(x); out` is `[x]`, and `mut out = [5]; ...` is
/// `[5, x]`.
///
/// The shape has to be *exactly* this: the pushes immediately follow the
/// `mut`, and the binding itself is the very next thing, so nothing can read
/// `out` half-built or touch it afterwards. A pushed element may not mention
/// `out` either (`set out.push(out.len());` is a running index) — the
/// literal has no binding for it to refer to. And this file's `push` must be
/// the builtin, so a *user* function of that name is not mistaken for it.
///
/// Unlike [`push_loop_pipeline`](super::push_loop_pipeline()) there is no
/// blind spot about the receiver's type: `push` is declared
/// `fn __builtin_push<T: any>(mut self: T[], x: T)`, so an `out` that is
/// pushed to is an array, and `[..out]` always type-checks.
pub(super) fn push_array_literal(e: &Expr, src: &str, push: &str, hits: &mut Vec<Error>) {
    let ExprKind::LetMut(acc, _, seed, body) = &e.kind else {
        return;
    };
    // Every push, in order, for as long as they run consecutively. An
    // annotation on the `mut` is allowed (it is often what gives an empty
    // seed its element type) and simply goes away with the binding.
    let mut elems: Vec<&Expr> = Vec::new();
    let mut rest = &**body;
    while let Some((elem, next)) = pushed_element(rest, acc, push) {
        elems.push(elem);
        rest = next;
    }
    if elems.is_empty() {
        return;
    }
    // What follows the pushes must be the binding and nothing else: any
    // other statement could read or rebind `out`, and the literal would have
    // nowhere to put it.
    if !matches!(&rest.kind, ExprKind::Ident(n) if n == acc) {
        return;
    }
    if elems.iter().any(|x| mentions(x, acc)) {
        return;
    }
    // A quotable seed or element is spliced back in; anything else leaves a
    // placeholder, the way `push_loop_pipeline` writes `<iterable>`.
    let mut parts = seed_parts(seed, src);
    parts.extend(elems.iter().map(|x| elem_text(x, src)));
    let literal = format!("[{}]", parts.join(", "));
    // Point at the seed, not the whole binding. `#[allow]` is line-scoped
    // (see [`check`](super::check())), and the seed is where the shape
    // starts — the same anchor, for the same reason, as
    // `push_loop_pipeline`.
    hits.push(Error::at(
        format!(
            "\"{acc}\" is seeded and then only appended to before being returned — \
             write \"{literal}\" and drop the mut \
             (or append #[allow] to this line to keep it)"
        ),
        seed.span.clone(),
    ));
}
