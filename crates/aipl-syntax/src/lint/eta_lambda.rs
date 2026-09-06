use crate::ast::{Expr, ExprKind};
use crate::Error;

pub(super) fn eta_lambda(e: &Expr, hits: &mut Vec<Error>) {
    let ExprKind::Lambda(params, body) = &e.kind else {
        return;
    };
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
    if !crate::operator_named_forms(name).is_empty() {
        return;
    }
    if args.len() != params.len() || params.iter().any(|p| &p.name == name) {
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
