use crate::ast::{BinOp, Expr, ExprKind, Pattern};
use crate::Error;

/// Whether a `none` arm's value is built purely from constants, and so costs
/// the same computed eagerly as computed lazily.
///
/// Literals and names, and anything assembled out of them: a struct or
/// variant construction (a range literal `0..0` is one of these — it parses
/// to a `__builtin_Span` construction), an array/set/dict/tuple literal, a
/// field read, a negation. Arithmetic counts too, except `/` and `%`, which
/// trap on a zero divisor — evaluating one eagerly could turn a program that
/// returns into one that dies.
///
/// A *call* never counts, however cheap it looks: what it costs, and whether
/// it has effects of its own, is not visible from here.
fn constant_default(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::None
        | ExprKind::Unit => true,
        ExprKind::Construct(_, inits) => inits.iter().all(|i| constant_default(&i.value)),
        ExprKind::ArrayLit(xs) | ExprKind::SetLit(xs) | ExprKind::TupleLit(xs) => {
            xs.iter().all(constant_default)
        }
        ExprKind::DictLit(pairs) => pairs
            .iter()
            .all(|(k, v)| constant_default(k) && constant_default(v)),
        ExprKind::Field(x, _) | ExprKind::Neg(x) | ExprKind::Not(x) => constant_default(x),
        ExprKind::Binop(a, op, b) => {
            !matches!(op, BinOp::Div | BinOp::Rem) && constant_default(a) && constant_default(b)
        }
        _ => false,
    }
}

/// `match (o) { some(v) => v, none => d }` — an optional unwrapped to its
/// payload with a fallback. That is exactly `o.value_or(d)`.
///
/// The `some` arm must hand back its own binding unchanged; that identity
/// arm is what separates this from a `map`, where the payload is transformed
/// on the way out. Arm order doesn't matter.
///
/// The default must be built from constants — see [`constant_default`].
/// `value_or`'s default is an ordinary call argument, so it is evaluated
/// whether or not the optional is empty, while a `none` arm runs only when
/// it is; advising the rewrite for a default that *does* work would move
/// that work to where it did not happen before.
///
/// The bound used to be a literal or a bare name, which excluded the shape
/// this rule most wants to catch: a `none` arm holding an inline struct or
/// variant. That was not a formatting limit — `value_or` genuinely could not
/// unwrap such an optional, and said so — but it is AIPL-implemented now and
/// generic over every payload.
pub(super) fn match_value_or(e: &Expr, src: &str, hits: &mut Vec<Error>) {
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
    // The identity arm. `some(v) => f(v)` is a `map`, not a `value_or`.
    let ExprKind::Ident(payload) = &some_arm.body.kind else {
        return;
    };
    if payload != binder {
        return;
    }
    if !constant_default(&none_arm.body) {
        return;
    }
    // The default's own span stops at the last *token* the expression
    // started with — for a struct literal, before its closing brace — so
    // splicing it alone yields advice that doesn't parse. The arm's span
    // reaches the end of the arm, so slice from the body's start to there
    // and drop the separator the arm may end with.
    // A single-token default splices back from source verbatim — its span is
    // exactly the token, escapes and quotes included, which reconstructing
    // from the AST would have to re-derive. Anything wider does not: a
    // construction's span stops at its last *token*, before the closing
    // brace, and `MatchArm::span` covers only the pattern, so no range
    // reliably spells the whole default. Rather than emit advice that
    // doesn't parse, the message then names the default instead of quoting
    // it.
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
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its
    // parens (see `slice_from_zero`), so splicing it would produce advice
    // that doesn't parse.
    // Quote the scrutinee back only when it is a bare name, whose span is
    // exactly its text — a call-shaped scrutinee's span stops before its
    // parens (see `slice_from_zero`), so splicing it would produce advice
    // that doesn't parse.
    let receiver = match &scrut.kind {
        ExprKind::Ident(name) => name.as_str(),
        _ => "",
    };
    let advice = match default {
        Some(d) => format!("write \"{receiver}.value_or({d})\" instead"),
        None => {
            format!("write it as \"{receiver}.value_or(..)\" with that arm's value as the default")
        }
    };
    // Point at the `some(v) => v` arm's body, not the whole `match`.
    // `#[allow]` is line-scoped (see `allow_squelch`), so a hit spanning the
    // multi-line `match` could never be squelched. That lone identifier is
    // always one line, so it always takes a trailing marker and keeps it
    // through `aipl fmt` — and it is the arm that identifies the shape, since
    // a default alone is something `value_or` has too.
    hits.push(Error::at(
        format!(
            "this `match` hands back the optional's payload or a default — \
             {advice} (or append #[allow] to this line to keep it)"
        ),
        some_arm.body.span.clone(),
    ));
}
