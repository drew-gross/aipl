use crate::ast::{Callee, Expr, ExprKind, Pattern};
use crate::Error;

use super::spans_its_text;

/// ```text
/// match (shape) {
///     Circle(..) => some(shape),
///     _ => none,
/// }
/// ```
///
/// — a variant value handed back when it is one case and `none` otherwise. That
/// is `shape.as_case(Circle)`, which says so without the second arm, and whose
/// result is typed as the one case it holds.
///
/// The shape is exact: two arms, the first a variant case's constructor pattern
/// (not `some`/`ok`/`err`, which are optionals and results rather than cases of a
/// variant the program declares) whose body is `some(x)` of the very value
/// matched, and the second the catch-all giving `none`. What the first arm binds
/// is no matter, since the body does not read it. An arm that returns anything
/// but the scrutinee — a payload, a rebuilt value — is a different program, and
/// `as_case` is not its spelling.
///
/// `as_case` is the name the file imports it under, when it does; the advice
/// names the import otherwise.
///
/// The hit points at the `_ => none` arm, a short line the formatter keeps in
/// place, so `#[allow]` squelches it.
pub(super) fn match_case_as_case(
    e: &Expr,
    src: &str,
    as_case: Option<&str>,
    hits: &mut Vec<Error>,
) {
    let ExprKind::Match(scrutinee, arms) = &e.kind else {
        return;
    };
    let [case_arm, other_arm] = arms.as_slice() else {
        return;
    };
    let Pattern::Ctor { name, .. } = &case_arm.pattern else {
        return;
    };
    if matches!(name.as_str(), "some" | "none" | "ok" | "err") {
        return;
    }
    if !matches!(other_arm.pattern, Pattern::Wildcard)
        || !matches!(other_arm.body.kind, ExprKind::None)
    {
        return;
    }
    let ExprKind::Call(Callee::Some, args, _) = &case_arm.body.kind else {
        return;
    };
    let [kept] = args.as_slice() else {
        return;
    };
    if kept != &**scrutinee {
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
    hits.push(Error::at(
        format!(
            "this match only asks whether the value is {case} — write \"{receiver}.{method}({case})\" \
             and drop the match{import}"
        ),
        other_arm.span.clone(),
    ));
}
