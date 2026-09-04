use super::constant_value;
use crate::ast::{Expr, ExprKind, Pattern};
use crate::Error;

/// `match (o) { some(v) => ok(v), none => err(e) }` — an optional turned into a
/// result by naming both cases. That is exactly `o.value_or_err(e)`.
///
/// The `none` arm building an `err` is what identifies the shape: it is the
/// bridge from the optional world to the result world, and `value_or_err` is
/// the name of that bridge. Arm order doesn't matter.
///
/// The `some` arm is *not* constrained to the identity `ok(v)`, because the
/// rewrite does not need it to be. `some(v) => ok(f(v))` is
/// `ok(f(o.value_or_err(e)?))` — the same `value_or_err`, with `?` doing the
/// early return the `none` arm was doing by hand. The advice says which of the
/// two it is, since the second reads quite differently from the first.
///
/// The error must be built from constants — see
/// [`constant_value`](super::constant_value()), the same guard
/// [`match_value_or`](super::match_value_or()) applies to its default, for the
/// same reason. `value_or_err`'s error is an ordinary call argument, evaluated
/// whether or not the optional is empty, while a `none` arm runs only when it
/// is; advising the rewrite for an error that *does* work would move that work
/// onto the success path.
///
/// That is not hypothetical. `grammar.aipl`'s `link_rule` builds its error by
/// interpolating the rule name, and the `match` builds it only when a lookup
/// fails; rewritten, it allocated a string on every *successful* lookup — and
/// then leaked it, which is how the guard came to be written. The leak is a
/// separate compiler bug, but even without it the rewrite was the wrong advice.
pub(super) fn match_value_or_err(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Match(scrut, arms) = &e.kind else {
        return;
    };
    if arms.len() != 2 {
        return;
    }
    let arm_named = |want: &str| {
        arms.iter()
            .find(|a| matches!(&a.pattern, Pattern::Ctor { name, .. } if name == want))
    };
    let (Some(some_arm), Some(none_arm)) = (arm_named("some"), arm_named("none")) else {
        return;
    };
    let Pattern::Ctor { bindings, .. } = &some_arm.pattern else {
        return;
    };
    let Pattern::Ctor {
        bindings: none_binds,
        ..
    } = &none_arm.pattern
    else {
        return;
    };
    // `some` binds exactly its payload; `none` binds nothing.
    let [binder] = &bindings[..] else {
        return;
    };
    if !none_binds.is_empty() {
        return;
    }
    // The `none` arm must build an `err` from one value. Anything else is a
    // `match` that does something this rule has no rewrite for.
    let ExprKind::Call(none_name, none_args, _) = &none_arm.body.kind else {
        return;
    };
    let [error] = &none_args[..] else {
        return;
    };
    if none_name != "err" {
        return;
    }
    if !constant_value(error) {
        return;
    }
    // Whether the `some` arm is the identity `ok(v)` — the shape that rewrites
    // to a bare `value_or_err`, with no `?` and nothing left around it.
    let passes_through = match &some_arm.body.kind {
        ExprKind::Call(name, args, _) => match &args[..] {
            [arg] => name == "ok" && matches!(&arg.kind, ExprKind::Ident(v) if v == binder),
            _ => false,
        },
        _ => false,
    };
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its parens
    // (see `slice_from_zero`), so splicing it would produce advice that doesn't
    // parse.
    let receiver = match &scrut.kind {
        ExprKind::Ident(name) => name.as_str(),
        _ => "",
    };
    // Same rule for the error value, and for the same reason `match_value_or`
    // applies it to its default: a single token splices back verbatim, escapes
    // and quotes included, while anything wider has a span that stops at its
    // last token — before a construction's closing brace — so quoting it would
    // print advice that doesn't parse. Then the message names it instead.
    let quoted: Option<&str> = matches!(
        error.kind,
        ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Ident(_)
    )
    .then(|| src.get(error.span.start..error.span.end))
    .flatten();
    let advice = match (passes_through, quoted) {
        (true, Some(q)) => format!("write \"{receiver}.value_or_err({q})\" instead"),
        (true, None) => {
            format!("write it as \"{receiver}.value_or_err(..)\" with that arm's error")
        }
        (false, Some(q)) => format!(
            "write \"{receiver}.value_or_err({q})?\" and use the payload directly — `?` does \
             the early return this arm is doing by hand"
        ),
        (false, None) => format!(
            "write it as \"{receiver}.value_or_err(..)?\" with that arm's error, and use the \
             payload directly — `?` does the early return this arm is doing by hand"
        ),
    };
    // Point at the `none => err(..)` arm's body, not the whole `match`.
    // `#[allow]` is line-scoped (see `check`), so a hit spanning the multi-line
    // `match` could never be squelched. This arm is the one that identifies the
    // shape — the `some` arm varies — and its `err` call always starts on the
    // arm's own line, which therefore takes a trailing marker and keeps it
    // through `aipl fmt`.
    hits.push(Error::at(
        format!(
            "this `match` turns an optional into a result — {advice} \
             (or append #[allow] to this line to keep it)"
        ),
        none_arm.body.span.clone(),
    ));
}
