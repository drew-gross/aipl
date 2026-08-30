use crate::ast::{Expr, ExprKind};
use crate::Error;

pub(super) fn eta_lambda(e: &Expr, hits: &mut Vec<Error>) {
    let ExprKind::Lambda(params, body) = &e.kind else {
        return;
    };
    let ExprKind::Call(name, args, _) = &body.kind else {
        return;
    };
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
