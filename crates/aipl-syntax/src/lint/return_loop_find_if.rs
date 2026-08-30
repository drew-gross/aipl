use crate::ast::{Expr, ExprKind};
use crate::Error;

use super::{imported_as, lone_stmt, spans_its_text};

/// ```text
/// for (let x : xs) {
///     if (matches(x)) {
///         return some(x);
///     };
/// }
/// none
/// ```
///
/// — a loop that hands back the first element passing a test, and `none` when
/// no element does. That is `xs.find_if(|x| matches(x))`: one expression that
/// says the answer *is* the first match, rather than a loop the reader has to
/// run in their head to find out that the early return is the only way out and
/// that falling off the end means "not found".
///
/// The shape has to be *exactly* this, because anything else is something
/// `find_if` would drop: the loop body is an else-less `if` around a lone
/// `return`, the returned value is `some` of the loop variable itself
/// (`some(f(x))` is a `find_if` *and* a map, which the advice would not
/// spell), and the loop is immediately followed by the `none` that makes
/// falling off the end the not-found answer. A guard that propagates with `?`
/// is left alone too — see [`lambda_safe`](super::lambda_safe()).
///
/// What the shape *does* leave open is where the loop sits: the `none` may be
/// the enclosing block's tail or an explicit `return none;`, since a reader
/// can't tell those apart either.
pub(super) fn return_loop_find_if(
    e: &Expr,
    src: &str,
    find_if: Option<&str>,
    hits: &mut Vec<Error>,
) {
    // A `for` is folded as `Seq(For, rest)` (see `wrap_stmt`), and `rest` is
    // what the loop falls through to.
    let ExprKind::Seq(first, rest) = &e.kind else {
        return;
    };
    let ExprKind::For(var, iterable, loop_body) = &first.kind else {
        return;
    };
    if !is_none_answer(rest) {
        return;
    }
    // An else-less `if` around the return: an `else`, or a second statement,
    // is work `find_if` has nowhere to put.
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
    // `some(x)` of the loop variable itself, and nothing else: `some(f(x))`
    // finds *and* maps, `some(y)` hands back something the loop merely saw.
    let ExprKind::Call(some, args, _) = &value.kind else {
        return;
    };
    if some != "some" || args.len() != 1 {
        return;
    }
    if !matches!(&args[0].kind, ExprKind::Ident(n) if n == var) {
        return;
    }
    if !super::lambda_safe(cond) {
        return;
    }
    // Quote the iterable back only where its span really covers its text — see
    // [`spans_its_text`](super::spans_its_text()) — and otherwise leave the
    // placeholder `push_loop_pipeline` uses.
    let recv = spans_its_text(iterable)
        .then(|| &src[iterable.span.clone()])
        .unwrap_or("<iterable>");
    // Like `++` and `is_nonempty`, the builtin has to be in scope before the
    // advice can be followed, so a file that hasn't imported it is told to.
    let (name, import) = match find_if {
        Some(local) => (local, ""),
        None => ("find_if", ", importing `find_if` from builtins"),
    };
    // Point at the `return`, not the loop. `#[allow]` is line-scoped (see
    // `allow_squelch`), so a hit spanning the loop could never be squelched:
    // the marker would have to go on the `for (..) {` header, and `aipl fmt`
    // relocates one written there onto a line of its own, where it squelches
    // nothing. `return some(x);` is a short statement that always occupies one
    // line, and it is the line that makes this a search for the first match.
    hits.push(Error::at(
        format!(
            "this loop returns the first element passing a test, and \"none\" otherwise — \
             write \"return {recv}.{name}(|{var}| ..);\" and drop the loop{import} \
             (or append #[allow] to this line to keep it)"
        ),
        ret.span.clone(),
    ));
}

/// Whether `rest` — what the loop falls through to — is the not-found answer:
/// a bare `none` as the block's tail, or an explicit `return none;`. Anything
/// else, and the fall-through does work `find_if` would skip.
fn is_none_answer(rest: &Expr) -> bool {
    match &rest.kind {
        ExprKind::None => true,
        // `return none;` folds as `Seq(Return(none), <unreachable rest>)`.
        ExprKind::Seq(stmt, _) => {
            matches!(&stmt.kind, ExprKind::Return(v) if matches!(v.kind, ExprKind::None))
        }
        _ => false,
    }
}

/// The local name this file's builtins import gives `find_if`, or `None` when
/// it never imported it — in which case the advice names the import too.
///
/// The lint itself needs no import to fire: every part of the shape it matches
/// (`for`, `if`, `return`, `some`, `none`) is language syntax, not a builtin
/// some other function could be shadowing.
pub(super) fn find_if_name(program: &crate::ast::Program) -> Option<String> {
    imported_as(program, "find_if")
}
