use crate::ast::{Callee, Expr, ExprKind, Pattern};
use crate::Error;

use super::spans_its_text;

/// The `match` expressions whose value *is* the function's value: the tail of the
/// body, through the statements that lead to it, the branches of an `if`, and the
/// arms of another `match`. A `?` written in place of such a `match` returns from
/// the function with the same answer the `match` would have produced.
pub(super) fn tail_matches<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match &e.kind {
        ExprKind::Let(_, _, _, rest)
        | ExprKind::LetMut(_, _, _, rest)
        | ExprKind::Seq(_, rest)
        | ExprKind::Assign(_, _, rest) => tail_matches(rest, out),
        ExprKind::If(_, then, otherwise) => {
            tail_matches(then, out);
            tail_matches(otherwise, out);
        }
        ExprKind::IfLet(arm, _, otherwise) => {
            tail_matches(&arm.body, out);
            tail_matches(otherwise, out);
        }
        ExprKind::Match(_, arms) => {
            out.push(e);
            for arm in arms {
                tail_matches(&arm.body, out);
            }
        }
        _ => {}
    }
}

/// ```text
/// match (x) {
///     Field(base, field) => rest,
///     _ => none,
/// }
/// ```
///
/// — as the last thing a function does: a variant value taken apart when it is
/// one case, and given up on with `none` otherwise. That is
/// `let Field(base, field) = x else { return none; };` followed by `rest`, which
/// says the early exit first and leaves the rest of the function at one level
/// instead of inside an arm. (A case with no payload to bind has no pattern of
/// its own for that, so the advice is `x.as_case(Empty)?;`.)
///
/// It has to be the function's *tail* (see [`tail_matches`]): the `?` returns
/// `none` from the function, which is only the `match`'s own answer when the
/// `match` is what the function returns. The first arm must be a variant case's
/// constructor pattern (not `some`/`ok`/`err`) and the second the catch-all
/// giving `none`. A first arm that returns the matched value itself,
/// `K(..) => some(x)`, is `match_case_as_case`'s, whose answer is shorter still.
///
/// The hit points at the `_ => none` arm, a short line the formatter keeps in
/// place, so `#[allow]` squelches it.
pub(super) fn match_case_try_as_case(
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
    let Pattern::Ctor {
        name,
        bindings,
        ignore_payload,
    } = &case_arm.pattern
    else {
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
    if matches!(&case_arm.body.kind, ExprKind::Call(Callee::Some, args, _)
        if matches!(args.as_slice(), [kept] if kept == &**scrutinee))
    {
        return;
    }
    let case = name.split('@').next().unwrap_or(name);
    let receiver = if spans_its_text(scrutinee) {
        src[scrutinee.span.clone()].to_string()
    } else {
        "<value>".to_string()
    };
    // A payload the arm names is taken apart by a `let .. else`. A case with none
    // to bind (or one the arm ignores) has no pattern of its own — a bare name
    // would be a binding — so it is asked of the value instead.
    let (statement, import) = if bindings.is_empty() || *ignore_payload {
        match as_case {
            Some(local) => (format!("{receiver}.{local}({case})?;"), String::new()),
            None => (
                format!("{receiver}.as_case({case})?;"),
                ", importing `as_case` from builtins".to_string(),
            ),
        }
    } else {
        (
            format!(
                "let {case}({}) = {receiver} else {{ return none; }};",
                bindings.join(", ")
            ),
            String::new(),
        )
    };
    hits.push(Error::at(
        format!(
            "this match gives up with none unless the value is {case}, and is the last thing the \
             function does — write \"{statement}\" and continue with the arm's body{import}"
        ),
        other_arm.span.clone(),
    ));
}
