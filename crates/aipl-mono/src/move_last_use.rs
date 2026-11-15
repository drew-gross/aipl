//! Whether a parameter can be handed on to the next callee: the analysis that decides if an
//! instance owning a parameter forwards it. The marking itself is `move_last_use_typed`.

use std::collections::HashSet;

use aipl_syntax::ast::{Expr, ExprKind, MatchArm};

use crate::{children, count_ident};

fn mentions(name: &str, e: &Expr) -> bool {
    count_ident(name, e) > 0
}

fn is_name(e: &Expr, name: &str) -> bool {
    matches!(&e.kind, ExprKind::Ident(n) if n == name)
}

fn binds(arm: &MatchArm, name: &str) -> bool {
    arm.pattern.bindings().iter().any(|b| b == name)
}

/// Whether every occurrence of `name` in `e` is a direct call argument — or,
/// for a struct, a read of one of its fields outside `heap_fields` — with none
/// under a lambda and no rebinding of the name. `as_argument` says whether `e`
/// itself sits in argument position.
fn only_call_arguments(
    name: &str,
    e: &Expr,
    as_argument: bool,
    heap_fields: &HashSet<String>,
) -> bool {
    let all = |es: &[&Expr]| {
        es.iter()
            .all(|c| only_call_arguments(name, c, false, heap_fields))
    };
    match &e.kind {
        ExprKind::Ident(n) => n != name || as_argument,
        // A copy of a scalar field is not a borrow of the value.
        ExprKind::Field(obj, f) if is_name(obj, name) => !heap_fields.contains(f),
        // `let a = name` is a move or an alias; `mark` decides which, and an
        // alias withdraws everything. Either way it is not a borrow that
        // disqualifies here.
        ExprKind::Let(n, _, value, body) | ExprKind::LetMut(n, _, value, body)
            if n != name && is_name(value, name) =>
        {
            only_call_arguments(name, body, false, heap_fields)
        }
        ExprKind::Call(_, args, _) => args
            .iter()
            .all(|a| only_call_arguments(name, a, true, heap_fields)),
        ExprKind::Lambda(params, body) => {
            params.iter().all(|p| p.name != name) && !mentions(name, body)
        }
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) if n == name => false,
        ExprKind::For(n, index, _, _) if aipl_syntax::for_binds(n, index, name) => false,
        // The target of `set name = ..` is a place, not a read of the value;
        // a field path (`set name.f = ..`) reads it, and is checked as one.
        ExprKind::Assign(lhs, value, rest) => {
            (is_name(lhs, name) || only_call_arguments(name, lhs, false, heap_fields))
                && all(&[value, rest])
        }
        ExprKind::Match(scrutinee, arms) => {
            arms.iter().all(|a| !binds(a, name))
                && only_call_arguments(name, scrutinee, false, heap_fields)
                && arms
                    .iter()
                    .all(|a| only_call_arguments(name, &a.body, false, heap_fields))
        }
        ExprKind::IfLet(arm, scrutinee, else_body) => {
            !binds(arm, name) && all(&[scrutinee, &arm.body, else_body])
        }
        _ => children(e).all(|c| only_call_arguments(name, c, false, heap_fields)),
    }
}

/// Whether a struct parameter `name` can be held by an instance that *owns* it and
/// forwarded on piecemeal: every use is a call argument (a borrow, or — at its
/// last use — a move), a read of a field that owns no heap, or the parameter
/// returned as the function's value. A read of a heap field, a capture, or the
/// value used any other way (`mut`, a `for` over it, a rebinding) disqualifies,
/// as for any other borrow.
///
/// The caller has a reference to give away exactly when its argument is fresh or
/// at its own last use; an instance that owns the parameter may then hand it to
/// the next callee at *its* last use, so ownership travels down a chain of calls
/// that each use `self` more than once.
pub(crate) fn param_forwarded(name: &str, body: &Expr, heap_fields: &HashSet<String>) -> bool {
    forwarded(name, body, true, heap_fields)
}

fn forwarded(name: &str, e: &Expr, tail: bool, heap_fields: &HashSet<String>) -> bool {
    let rec = |x: &Expr| forwarded(name, x, false, heap_fields);
    let rec_tail = |x: &Expr| forwarded(name, x, tail, heap_fields);
    match &e.kind {
        // The parameter as the function's value is a move out, like a call's
        // last use; anywhere else a bare mention is a use this cannot place.
        ExprKind::Ident(n) if n == name => tail,
        ExprKind::Let(n, _, value, body) | ExprKind::LetMut(n, _, value, body) if n != name => {
            // `let a = name` is a move or an alias, decided by `mark`.
            (is_name(value, name) || rec(value)) && rec_tail(body)
        }
        ExprKind::Seq(first, rest) => rec(first) && rec_tail(rest),
        // The target of `set name = ..` is a place, not a read; what follows is
        // where the binding may end.
        ExprKind::Assign(lhs, value, rest) => {
            (is_name(lhs, name) || only_call_arguments(name, lhs, false, heap_fields))
                && rec(value)
                && rec_tail(rest)
        }
        ExprKind::If(cond, then_body, else_body) => {
            rec(cond) && rec_tail(then_body) && rec_tail(else_body)
        }
        ExprKind::Match(scrutinee, arms) => {
            arms.iter().all(|a| !binds(a, name))
                && rec(scrutinee)
                && arms.iter().all(|a| rec_tail(&a.body))
        }
        ExprKind::Return(value) => forwarded(name, value, true, heap_fields),
        // A value built at the tail from the binding — `Out { scopes, n }`,
        // `(xs, 1)`, `[xs]`, and any of those handed to a call (`ok(Out { xs })`)
        // — takes its reference as the function's value does: nothing runs
        // afterwards that could read the binding. Only when the name occurs once
        // in the whole construction, so no sibling is evaluated between another's
        // read of it and the move; a name read twice keeps the borrow.
        ExprKind::Construct(..)
        | ExprKind::TupleLit(..)
        | ExprKind::ArrayLit(..)
        | ExprKind::SetLit(..)
        | ExprKind::DictLit(..)
        | ExprKind::Call(..)
            if tail && count_ident(name, e) == 1 =>
        {
            let call = matches!(&e.kind, ExprKind::Call(..));
            children(e).all(|part| {
                if is_name(part, name) {
                    true
                } else if is_composite_literal(part) {
                    forwarded(name, part, true, heap_fields)
                } else if call {
                    only_call_arguments(name, part, true, heap_fields)
                } else {
                    forwarded(name, part, true, heap_fields)
                }
            })
        }
        _ => only_call_arguments(name, e, false, heap_fields),
    }
}

/// Whether `e` builds a value out of its parts: a struct, tuple, array, set or
/// dictionary literal.
fn is_composite_literal(e: &Expr) -> bool {
    matches!(
        &e.kind,
        ExprKind::Construct(..)
            | ExprKind::TupleLit(..)
            | ExprKind::ArrayLit(..)
            | ExprKind::SetLit(..)
            | ExprKind::DictLit(..)
    )
}
