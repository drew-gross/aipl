use crate::ast::{Expr, ExprKind};
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
/// same loop shape returning the element; this one is its indexed twin, and the
/// two are disjoint because a loop returns one or the other.
///
/// **The shape arrives desugared.** `for (let i, x : xs)` is folded into a plain
/// loop over a counter declared just outside it (see `StmtSpec::For` in
/// `aipl-parser`):
///
/// ```text
/// mut __idx$N: u64 = 0;
/// for (let x : xs) { let i = __idx$N; <body>; set __idx$N = __idx$N + 1; }
/// ```
///
/// so what is matched here is that, not what was written. The synthetic counter
/// name is what makes it unmistakable: `$` is not an identifier character, so no
/// source can spell one, and a match on `__idx$` cannot be a user's own loop.
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
    // The counter declaration the fold introduces, and the loop it scopes over.
    let ExprKind::LetMut(tmp, _, seed, rest) = &e.kind else {
        return;
    };
    if !tmp.starts_with("__idx$") || !matches!(seed.kind, ExprKind::Num(0)) {
        return;
    }
    let ExprKind::Seq(first, after) = &rest.kind else {
        return;
    };
    let ExprKind::For(var, iterable, loop_body) = &first.kind else {
        return;
    };
    if !is_none_answer(after) {
        return;
    }
    // `let i = __idx$N;` opening the body, and the `set __idx$N = ..;` closing
    // it. Both are the fold's, and between them is what was written.
    let ExprKind::Let(index, index_ty, counter, counted) = &loop_body.kind else {
        return;
    };
    if index_ty.is_some() || !matches!(&counter.kind, ExprKind::Ident(n) if n == tmp) {
        return;
    }
    let ExprKind::Seq(written, bump) = &counted.kind else {
        return;
    };
    if !is_counter_bump(bump, tmp) {
        return;
    }
    // From here the shape is `return_loop_find_if`'s, read against the index.
    let Some(stmt) = lone_stmt(written) else {
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
    if some != "some" || args.len() != 1 {
        return;
    }
    if !matches!(&args[0].kind, ExprKind::Ident(n) if n == index) {
        return;
    }
    // The predicate sees the element, never the position — so a test that reads
    // either the index or the counter under it has no rewrite here.
    if mentions(cond, index) || mentions(cond, tmp) {
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

/// Whether `e` is the fold's own `set __idx$N = __idx$N + 1;` — the statement it
/// appends to every indexed loop body. Anything else there is the user's, and
/// this is not the shape.
fn is_counter_bump(e: &Expr, tmp: &str) -> bool {
    let ExprKind::Assign(lhs, value, _) = &e.kind else {
        return false;
    };
    if !matches!(&lhs.kind, ExprKind::Ident(n) if n == tmp) {
        return false;
    }
    // The fold calls the canonical builtin rather than emitting a `+`, so that
    // an indexed loop obliges no file to import an operator it never wrote.
    let ExprKind::Call(add, args, _) = &value.kind else {
        return false;
    };
    add == "__builtin_wrapping_add"
        && args.len() == 2
        && matches!(&args[0].kind, ExprKind::Ident(n) if n == tmp)
        && matches!(args[1].kind, ExprKind::Num(1))
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
