use crate::ast::{Expr, ExprKind, Pattern};
use crate::Error;

use super::spans_its_text;

/// ```text
/// let Field(base, field) = x else {
///     return none;
/// };
/// ```
///
/// — a variant value taken apart, with `none` as the answer when it is another
/// case. That is `let Field(base, field) = x.as_case(Field)?;`, which says the
/// same without the block. (A hand-written `if (let K(..) = x) { rest } else {
/// return none; }` lowers to the same tree and is the same program.)
///
/// It has to be the function's *tail* (see `tail_matches`), where the `?` and the
/// block's `return` both leave the function with the same answer, and the `else`
/// has to be exactly `return none;`: a block that does anything first, or returns
/// something else, is a different program. Only a variant case's constructor
/// pattern is advised — an optional's `some(v)` has no `as_case`, and an array or
/// tuple pattern is not a case.
///
/// A case with no payload to bind has no pattern of its own to write in a `let`
/// (a bare name would be a binding), so it is asked of the value alone:
/// `x.as_case(Empty)?;`.
///
/// The hit points at the `return none;` line, a short statement the formatter
/// keeps in place, so `#[allow]` squelches it.
pub(super) fn let_else_as_case(e: &Expr, src: &str, as_case: Option<&str>, hits: &mut Vec<Error>) {
    let ExprKind::IfLet(arm, scrutinee, otherwise) = &e.kind else {
        return;
    };
    let Pattern::Ctor {
        name,
        bindings,
        ignore_payload,
    } = &arm.pattern
    else {
        return;
    };
    if matches!(name.as_str(), "some" | "none" | "ok" | "err") {
        return;
    }
    // `return none;` and nothing else: the statement, then the block's unit tail.
    let leave = match &otherwise.kind {
        ExprKind::Seq(first, rest) if matches!(rest.kind, ExprKind::Unit) => first,
        _ => return,
    };
    let ExprKind::Return(value) = &leave.kind else {
        return;
    };
    if !matches!(value.kind, ExprKind::None) {
        return;
    }
    let case = name.split('@').next().unwrap_or(name);
    let receiver = if spans_its_text(scrutinee) {
        src[scrutinee.span.clone()].to_string()
    } else {
        "<value>".to_string()
    };
    let (method, import) = match as_case {
        Some(local) => (local, String::new()),
        None => ("as_case", ", importing `as_case` from builtins".to_string()),
    };
    let call = format!("{receiver}.{method}({case})?");
    let statement = if bindings.is_empty() || *ignore_payload {
        format!("{call};")
    } else {
        format!("let {case}({}) = {call};", bindings.join(", "))
    };
    hits.push(Error::at(
        format!(
            "this `else` only gives up with none — write \"{statement}\" and drop the block{import}"
        ),
        leave.span.clone(),
    ));
}
