use crate::ast::{Expr, ExprKind, Pattern};
use crate::Error;

/// `|x| f(x)` / `|x| x.f()` — a lambda whose body only forwards its
/// parameters, unchanged and in order, to a single call. A named function
/// (or a function-typed value) can be passed directly, so recommend passing
/// `f` itself. Purely syntactic: the call's arguments must be exactly the
/// lambda parameters as bare identifiers, in order and in full — a captured
/// or reordered argument, an extra argument, or an unused parameter all
/// leave it un-flagged. The callee name must not itself be one of the
/// parameters (that's self-application, `|x| x(x)`, not forwarding). Method
/// form (`x.f()`) is stored as the free call `f(x)`, so it's covered too.
/// `match (o) { some(v) => p, none => false }` — an optional matched only to
/// answer a yes/no question, with `none` spelled as `false`. That is exactly
/// `o.is_some_and(|v| p)`, which says the same thing in one line and without
/// naming the `none` case at all.
///
/// The `none` arm being the literal `false` is what makes this recognizable
/// without types: arms must agree on a type, so a `false` arm proves the
/// whole `match` is a `bool`, and `some`/`none` patterns only ever match an
/// optional. Arm order doesn't matter — `none` may be written first.
///
/// Two near misses are deliberately left alone, because neither has an
/// `is_some_and` spelling that improves on what is written: an arm that
/// computes its `bool` some other way (`none => flag`) still has to run that
/// expression, and `some(v) => true` ignores the payload entirely — that one
/// is an `is_some`, which is not a builtin.
pub(super) fn match_is_some_and(e: &Expr, hits: &mut Vec<Error>) {
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
    if !none_binds.is_empty() || !matches!(none_arm.body.kind, ExprKind::Bool(false)) {
        return;
    }
    // `some(v) => true` is not an `is_some_and` — it asks only whether the
    // optional is populated, ignoring the payload. The honest rewrite is an
    // `is_some`, which doesn't exist as a builtin; advising
    // `is_some_and(|v| true)` here would be worse than what's written. Left
    // alone until there is something better to point at.
    if matches!(some_arm.body.kind, ExprKind::Bool(true)) {
        return;
    }
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text. A call-shaped scrutinee's span stops before its
    // parens (see `slice_from_zero`), so splicing it would produce advice
    // that doesn't parse.
    let call = match &scrut.kind {
        ExprKind::Ident(name) => format!("{name}.is_some_and(|{binder}| ..)"),
        _ => format!(".is_some_and(|{binder}| ..)"),
    };
    // Point at the `none => false` arm, not the whole `match`. `#[allow]` is
    // line-scoped (see `allow_squelch`), so a hit spanning several lines
    // could never be squelched: the only line able to carry the marker is
    // the `match (..) {` header, and the formatter moves a marker written
    // there onto its own line, where it squelches nothing. The `false` arm
    // is a literal, so it is always one line and always takes a trailing
    // marker — and it is the part that makes this shape recognizable.
    hits.push(Error::at(
        format!(
            "this `match` only asks whether the optional holds a value the arm \
             accepts — write \"{call}\" instead (or append #[allow] to this line \
             to keep it)"
        ),
        none_arm.body.span.clone(),
    ));
}
