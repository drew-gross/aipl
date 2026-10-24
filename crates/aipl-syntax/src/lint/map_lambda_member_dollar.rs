use crate::ast::{Expr, ExprKind};
use crate::{Error, Span};

use super::{mentions, quote};

/// `xs.map(|x| x.name)`, `xs.map(|x| x.0)` and `xs.map(|x| x.f(args))`: a lambda
/// whose whole body reads one member of its parameter, or calls one method on
/// it, is `xs$name`, `xs$0` and `xs$f(args)` — the member written once, with no
/// name for the element to introduce and then repeat.
///
/// Only the shape `$` stands for qualifies. The parameter has to be the receiver
/// of the member and appear nowhere else (`|x| x.f(x)` needs the name), and it has
/// to be unannotated, since `$` has no place to write a type. A body that calls a
/// function on the parameter without the method form (`|x| f(x)`) is
/// [`eta_lambda`]'s, which advises passing `f` directly.
///
/// The hit points at the lambda, a short span that begins on the line the
/// `#[allow]` would be written on. The lint runs before checking, so it cannot
/// tell a sequence from an optional; `$` maps either, as `map` does.
pub(super) fn map_lambda_member_dollar(
    e: &Expr,
    src: &str,
    map: Option<&str>,
    taken: &mut Vec<Span>,
    hits: &mut Vec<Error>,
) {
    let Some(map) = map else {
        return;
    };
    let ExprKind::Call(callee, args, _) = &e.kind else {
        return;
    };
    if callee != map {
        return;
    }
    let [recv, lambda] = args.as_slice() else {
        return;
    };
    let ExprKind::Lambda(params, body) = &lambda.kind else {
        return;
    };
    let [param] = params.as_slice() else {
        return;
    };
    if param.ty.is_some() {
        return;
    }
    let step = match &body.kind {
        ExprKind::Field(inner, name) if is_param(inner, &param.name) => {
            // A tuple slot is the field `_0`, written `0`.
            match name.strip_prefix('_') {
                Some(slot) if slot.chars().all(|c| c.is_ascii_digit()) && !slot.is_empty() => {
                    slot.to_string()
                }
                _ => name.clone(),
            }
        }
        ExprKind::Call(name, call_args, true) => {
            let Some((first, rest)) = call_args.split_first() else {
                return;
            };
            if !is_param(first, &param.name) || rest.iter().any(|a| mentions(a, &param.name)) {
                return;
            }
            let shown = if rest.is_empty() { "" } else { "..." };
            format!("{name}({shown})")
        }
        _ => return,
    };
    let recv = quote(recv, src, "<items>");
    taken.push(lambda.span.clone());
    hits.push(Error::at(
        format!(
            "\"{map}\" with a lambda that only uses a member of its argument — write \
             \"{recv}${step}\" (or append #[allow] to this line to keep it)"
        ),
        lambda.span.clone(),
    ));
}

/// Whether `e` is exactly the lambda's parameter.
fn is_param(e: &Expr, name: &str) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n == name)
}
