use crate::ast::{Callee, Expr, ExprKind};
use crate::Error;

use super::{imported_as, lone_stmt, spans_its_text};

/// ```text
/// for (let i, x : xs) {
///     if (matches(x)) {
///         return some(i);
///     };
/// }
/// none
/// ```
///
/// — a loop that hands back the *position* of the first element passing a test,
/// and `none` when no element does. That is `xs.find_index(|x| matches(x))`.
/// [`return_loop_find_if`](super::return_loop_find_if()) asks the same of the
/// same loop shape returning the element; this one is its indexed twin. The two
/// are disjoint by construction: that one matches a loop with no index binder
/// and this one a loop with one, so no loop is a candidate for both.
///
/// The shape has to be *exactly* this, for the same reasons the element form
/// gives: the loop body is an else-less `if` around a lone `return`, the
/// returned value is `some` of the index binding itself, and the loop is
/// immediately followed by the `none` that makes falling off the end the
/// not-found answer.
///
/// One condition this form needs that the element form does not: **the test may
/// not mention the index**. `find_index` hands its predicate the element and
/// nothing else, so `if (i > 0 && p(x))` has no rewrite here — it is a scan the
/// builtin cannot express, and it is left alone.
pub(super) fn return_loop_find_index(
    e: &Expr,
    src: &str,
    find_index: Option<&str>,
    hits: &mut Vec<Error>,
) {
    // The loop and what it falls through to. An index binder is what makes this
    // the indexed form rather than `return_loop_find_if`'s, so the pattern
    // requiring one is the whole of what keeps the two apart.
    let ExprKind::Seq(first, after) = &e.kind else {
        return;
    };
    let ExprKind::For(var, Some(index), iterable, loop_body) = &first.kind else {
        return;
    };
    if !is_none_answer(after) {
        return;
    }
    // From here the shape is `return_loop_find_if`'s, read against the index.
    let Some(stmt) = lone_stmt(loop_body) else {
        return;
    };
    let ExprKind::If(cond, then, els) = &stmt.kind else {
        return;
    };
    if !matches!(els.kind, ExprKind::Unit) {
        return;
    }
    let Some(ret) = lone_stmt(then) else {
        return;
    };
    let ExprKind::Return(value) = &ret.kind else {
        return;
    };
    // `some(i)` of the index binding itself, and nothing else: `some(i + 1)`
    // finds *and* adjusts, which the advice would not spell.
    let ExprKind::Call(some, args, _) = &value.kind else {
        return;
    };
    if *some != Callee::Some || args.len() != 1 {
        return;
    }
    if !matches!(&args[0].kind, ExprKind::Ident(n) if n == index) {
        return;
    }
    // The predicate sees the element, never the position — so a test that reads
    // the index has no rewrite here.
    if mentions(cond, index) {
        return;
    }
    if !super::lambda_safe(cond) {
        return;
    }
    // Quote the iterable back only where its span really covers its text — see
    // [`spans_its_text`](super::spans_its_text()) — and otherwise leave the
    // placeholder the other loop lints use.
    let recv = spans_its_text(iterable)
        .then(|| &src[iterable.span.clone()])
        .unwrap_or("<iterable>");
    let (name, import) = match find_index {
        Some(local) => (local, ""),
        None => ("find_index", ", importing `find_index` from builtins"),
    };
    // Point at the `return`, not the loop: `#[allow]` is line-scoped, and a hit
    // spanning the loop could only be squelched from the `for (..) {` header,
    // which `aipl fmt` relocates a marker off. `return some(i);` is one line,
    // and it is the line that makes this a search for the first match.
    hits.push(Error::at(
        format!(
            "this loop returns the position of the first element passing a test, and \"none\" \
             otherwise — write \"return {recv}.{name}(|{var}| ..);\" and drop the loop{import} \
             (or append #[allow] to this line to keep it)"
        ),
        ret.span.clone(),
    ));
}

/// Whether `rest` — what the loop falls through to — is the not-found answer:
/// a bare `none` as the block's tail, or an explicit `return none;`.
fn is_none_answer(rest: &Expr) -> bool {
    match &rest.kind {
        ExprKind::None => true,
        ExprKind::Seq(stmt, _) => {
            matches!(&stmt.kind, ExprKind::Return(v) if matches!(v.kind, ExprKind::None))
        }
        _ => false,
    }
}

/// Whether `name` is read anywhere inside `e`.
fn mentions(e: &Expr, name: &str) -> bool {
    let mut found = false;
    crate::each_subexpr(e, &mut |sub| {
        if matches!(&sub.kind, ExprKind::Ident(n) if n == name) {
            found = true;
        }
    });
    found
}

/// The local name this file's builtins import gives `find_index`, or `None` when
/// it never imported it — in which case the advice names the import too.
pub(super) fn find_index_name(program: &crate::ast::Program) -> Option<String> {
    imported_as(program, "find_index")
}
