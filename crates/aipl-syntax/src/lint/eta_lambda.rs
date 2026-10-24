use crate::ast::{Expr, ExprKind};
use crate::{Error, Span};

/// `taken` is the spans of lambdas another lint already advised on: a `map`'s
/// `|x| x.f()` is `xs$f()`, not `xs.map(f)`, and one hit is enough.
pub(super) fn eta_lambda(e: &Expr, taken: &[Span], hits: &mut Vec<Error>) {
    if taken.contains(&e.span) {
        return;
    }
    let ExprKind::Lambda(params, body) = &e.kind else {
        return;
    };
    // A lambda the parser wrote for `xs$f()` names its parameter with a `$`, which
    // no source can; there is no longer spelling for it to be advised into.
    if params.iter().any(|p| p.name.contains('$')) {
        return;
    }
    let ExprKind::Call(name, args, _) = &body.kind else {
        return;
    };

    // An operator use is a call named for its spelling, so `|a, b| a + b` has
    // the shape this lint looks for. It is left alone deliberately: the language
    // does accept `+` as a value (see `cases/lambdas/operator_as_value`), but
    // `xs.fold(0, +)` is a different reading from `|acc, x| acc + x` rather than
    // an obviously better one, and this lint's job is the case where the lambda
    // adds *nothing* — `|x| to_str(x)` against `to_str`. Deciding the operator
    // question belongs to whoever wants to decide it, corpus-wide, on purpose.
    if !crate::operator_named_forms(name.name()).is_empty() {
        return;
    }
    if args.len() != params.len() || params.iter().any(|p| p.name == *name) {
        return;
    }
    for (arg, param) in args.iter().zip(params) {
        let ExprKind::Ident(a) = &arg.kind else {
            return;
        };
        if a != &param.name {
            return;
        }
    }
    hits.push(Error::at(
        format!(
            "lambda only forwards its argument(s) to \"{name}\" — pass \
             \"{name}\" directly (or append #[allow] to this line to keep it)"
        ),
        e.span.clone(),
    ));
}
