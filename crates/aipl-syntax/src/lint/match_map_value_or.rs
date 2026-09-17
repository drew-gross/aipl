use super::match_value_or::constant_default;
use crate::ast::{Callee, Expr, ExprKind, Pattern};
use crate::Error;

/// Whether `e` reads the name `binder` anywhere.
fn reads(e: &Expr, binder: &str) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n == binder)
        || crate::children(e).iter().any(|c| reads(c, binder))
}

/// `match (o) { some(v) => f(v), none => d }` — an optional's payload
/// transformed on the way out, with a fallback for the empty case. That is a
/// `map` and a `value_or`: `o.map(|v| f(v)).value_or(d)`, which says what
/// happens to the value and what stands in for it as two named steps rather
/// than one `match` naming both cases.
///
/// When the `some` arm wraps its result in `some(..)` and the `none` arm is
/// `none` — `some(v) => some(f(v)), none => none` — the `match` is a `map` and
/// nothing else, and the advice says just that.
///
/// The `some` arm must do *something* with the payload: the identity arm
/// `some(v) => v` is [`match_value_or`](super::match_value_or())'s shape, and
/// a `bool` answered with `none => false` is
/// [`match_is_some_and`](super::match_is_some_and())'s, so neither is taken up
/// here. The default is held to the same bar `match_value_or` holds its own
/// to ([`constant_default`]): `value_or`'s default is an argument, evaluated
/// either way, so only a default that costs nothing to evaluate eagerly is
/// advised. A `none => ()` arm is a `match` run for effect, not a value with a
/// fallback; a `some` arm that never reads its binding asks only whether the
/// optional is empty; and a `some` arm yielding an optional of its own over a
/// `none => none` is a bind, which `map` would nest rather than flatten — all
/// three are left alone.
pub(super) fn match_map_value_or(e: &Expr, src: &str, hits: &mut Vec<Error>) {
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
    // The identity arm is `value_or`'s shape, and a `none => false` is
    // `is_some_and`'s; a unit `none` arm is a statement `match`. A `some` arm
    // that never reads its binding maps nothing — that is an `is_some`
    // question, with a value per answer, and no `map` improves on it.
    if matches!(&some_arm.body.kind, ExprKind::Ident(v) if v == binder) {
        return;
    }
    if matches!(none_arm.body.kind, ExprKind::Bool(false) | ExprKind::Unit) {
        return;
    }
    if !reads(&some_arm.body, binder) {
        return;
    }
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its
    // parens (see `slice_from_zero`), so splicing it would produce advice
    // that doesn't parse.
    let receiver = match &scrut.kind {
        ExprKind::Ident(name) => name.as_str(),
        _ => "",
    };
    // `some(v) => some(..), none => none`: a `map` with nothing to default.
    let wraps =
        matches!(&some_arm.body.kind, ExprKind::Call(Callee::Some, args, _) if args.len() == 1);
    if matches!(none_arm.body.kind, ExprKind::None) {
        if wraps {
            hits.push(Error::at(
                format!(
                    "this `match` maps the optional's payload and keeps `none` — write \
                     \"{receiver}.map(|{binder}| ..)\" with the value the `some` arm wraps \
                     (or append #[allow] to this line to keep it)"
                ),
                none_arm.body.span.clone(),
            ));
        }
        // Otherwise the `some` arm yields an optional of its own — a bind,
        // not a map, and `map(..).value_or(none)` would nest the two.
        return;
    }
    if !constant_default(&none_arm.body) {
        return;
    }
    // A single-token default splices back from source verbatim; anything
    // wider has a span that stops at its last token (see `match_value_or`),
    // so the message names it instead.
    let default: Option<&str> = matches!(
        none_arm.body.kind,
        ExprKind::Num(_)
            | ExprKind::Bool(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Ident(_)
    )
    .then(|| src.get(none_arm.body.span.start..none_arm.body.span.end))
    .flatten();
    let advice = match default {
        Some(d) => format!(
            "write \"{receiver}.map(|{binder}| ..).value_or({d})\" with the `some` arm's body \
             as the mapping"
        ),
        None => format!(
            "write it as \"{receiver}.map(|{binder}| ..).value_or(..)\" with the `some` arm's \
             body as the mapping and the `none` arm's value as the default"
        ),
    };
    // Point at the `none` arm's default, not the whole `match`: `#[allow]` is
    // line-scoped, and the default is the one part of the shape that is
    // reliably a single line — the `some` arm's body is whatever the mapping
    // is, and may run to several.
    hits.push(Error::at(
        format!(
            "this `match` transforms the optional's payload or falls back to a default — \
             {advice} (or append #[allow] to this line to keep it)"
        ),
        none_arm.body.span.clone(),
    ));
}
