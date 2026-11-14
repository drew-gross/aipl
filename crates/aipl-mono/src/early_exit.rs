//! Rewriting a function's early exits away, so the inliner can take it.
//!
//! A `return` means "leave the function". Move the body to a call site and it
//! still means that — except the function it would now leave is the *caller*,
//! which is a different program. So the inliner used to refuse any body with a
//! `return` in it anywhere, and in this language that is a lot of bodies: a
//! guard clause is the idiomatic way to handle the uninteresting case first,
//! and the compiler's own AIPL is written that way throughout.
//!
//! The exit does not have to stay an exit, though. A `return` is only an exit
//! because of where it sits; the value it carries is the value the call
//! produces, and an expression can say that without leaving anything:
//!
//! ```text
//! fn clamped(x: i64) -> i64 {              if (x > 10) { 10 } else { x * 2 }
//!     if (x > 10) {                   →
//!         return 10;                       (as an expression, in place of
//!     };                                    the call)
//!     x * 2
//! }
//! ```
//!
//! That is all [`without_early_exits`] does: hand back a body that computes the
//! same value with no `return` in it, or `None` when the exits are not of a
//! shape it can express. The inliner then places an ordinary expression, and
//! every pass downstream — which is most of the optimizer — needs to know
//! nothing about any of this.
//!
//! # What it can express
//!
//! Two rules, and the second is the one that pays:
//!
//! - **A `return` in tail position is just its value.** `return v` at the end
//!   of a body is `v`.
//! - **A guard clause becomes the `if` it already is.** `if (c) { return v; };
//!   rest` is `if (c) { v } else { rest }`. The continuation moves into the arm
//!   that falls through, which is where it was always going.
//!
//! Those compose through everything that is transparent to a value: a `let`
//! (whose own initializer must be exit-free), the arms of an `if` or `match` in
//! tail position, and a statement sequence whose earlier statements do not
//! exit.
//!
//! # What it cannot, and why that is left alone
//!
//! - **A `return` inside a loop.** It leaves the loop *and* the function, and
//!   an expression has no way to say "stop iterating and be this value" — that
//!   needs a `break` carrying a value, which the language does not have. This
//!   is the common shape in the `builtin_*` sources (`for (let x : self) { if
//!   (!pred(x)) { return false; } }`), so it is the main thing still refused.
//! - **`?`.** It is an early exit whose *destination* depends on the enclosing
//!   function's error type, and whose rewrite depends on what the call site
//!   does with the result: inside `f(x)?` the propagation lands in the same
//!   place either way and the body could go in untouched, while in `let y =
//!   f(x);` it must become a `match` that yields the error instead. That is a
//!   call-site question, and this is a body rewrite, so `?` is refused here
//!   rather than half-handled.
//! - **A `match` used as a guard.** `match (..) { a => return v, b => {} };
//!   rest` would have to copy `rest` into every arm that falls through, which
//!   is a code-size decision and not a free rewrite like the `if` case, where
//!   there is exactly one such arm. Rare enough in practice to wait for a
//!   reason: the corpus has 268 `if` guards and 3 `else if` chains.

// The same rewrite over the typed tree, for the inlining that happens after monomorphization.

use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TyRepr};

use crate::typed_util::any_child;

/// [`without_early_exits`] over the typed tree.
pub(crate) fn without_early_exits_typed<T: TyRepr>(body: &TExpr<T>) -> Option<TExpr<T>> {
    if contains_try_typed(body) {
        return None;
    }
    if !contains_return_typed(body) {
        return Some(body.clone());
    }
    in_value_position_typed(body)
}

/// The type of the first of `branches` that produces a value (unit when none does).
fn valued<T: TyRepr>(branches: &[&TExpr<T>]) -> T {
    branches
        .iter()
        .map(|b| b.ty.clone())
        .find(|t| !t.is_unit())
        .unwrap_or(T::unit())
}

fn in_value_position_typed<T: TyRepr>(e: &TExpr<T>) -> Option<TExpr<T>> {
    match &e.kind {
        TExprKind::Return(value) => match contains_return_typed(value) {
            true => None,
            false => Some(value.as_ref().clone()),
        },
        TExprKind::Seq(first, rest) => {
            if always_returns_typed(first) {
                return in_value_position_typed(first);
            }
            if let Some(rewritten) = guard_typed(e, first, rest) {
                return Some(rewritten);
            }
            if contains_return_typed(first) {
                return None;
            }
            let rest = in_value_position_typed(rest)?;
            let ty = rest.ty.clone();
            Some(TExpr::rebuilt(
                TExprKind::Seq(first.clone(), Box::new(rest)),
                ty,
                e,
            ))
        }
        TExprKind::If(cond, then, els) if !contains_return_typed(cond) => {
            let (then, els) = (
                in_value_position_typed(then)?,
                in_value_position_typed(els)?,
            );
            let ty = valued(&[&then, &els]);
            Some(TExpr::rebuilt(
                TExprKind::If(cond.clone(), Box::new(then), Box::new(els)),
                ty,
                e,
            ))
        }
        TExprKind::Let(name, value, body) if !contains_return_typed(value) => {
            let body = in_value_position_typed(body)?;
            let ty = body.ty.clone();
            let mut out = TExpr::rebuilt(
                TExprKind::Let(name.clone(), value.clone(), Box::new(body)),
                ty,
                e,
            );
            out.annotation = e.annotation.clone();
            Some(out)
        }
        TExprKind::LetMut(name, value, body) if !contains_return_typed(value) => {
            let body = in_value_position_typed(body)?;
            let ty = body.ty.clone();
            let mut out = TExpr::rebuilt(
                TExprKind::LetMut(name.clone(), value.clone(), Box::new(body)),
                ty,
                e,
            );
            out.annotation = e.annotation.clone();
            Some(out)
        }
        TExprKind::Match(scrutinee, arms) if !contains_return_typed(scrutinee) => {
            let arms: Option<Vec<TMatchArm<T>>> = arms
                .iter()
                .map(|arm| {
                    Some(TMatchArm {
                        body: in_value_position_typed(&arm.body)?,
                        ..arm.clone()
                    })
                })
                .collect();
            let arms = arms?;
            let ty = valued(&arms.iter().map(|a| &a.body).collect::<Vec<_>>());
            Some(TExpr::rebuilt(
                TExprKind::Match(scrutinee.clone(), arms),
                ty,
                e,
            ))
        }
        _ => match contains_return_typed(e) {
            true => None,
            false => Some(e.clone()),
        },
    }
}

fn guard_typed<T: TyRepr>(whole: &TExpr<T>, first: &TExpr<T>, rest: &TExpr<T>) -> Option<TExpr<T>> {
    let TExprKind::If(cond, then, els) = &first.kind else {
        return None;
    };
    if contains_return_typed(cond) {
        return None;
    }
    let (exits, falls_through) = match (always_returns_typed(then), always_returns_typed(els)) {
        (true, false) => (then, els),
        (false, true) => (els, then),
        _ => return None,
    };
    if contains_return_typed(falls_through) {
        return None;
    }
    let continued = Box::new(then_also_typed(
        falls_through,
        in_value_position_typed(rest)?,
    ));
    let exited = Box::new(in_value_position_typed(exits)?);
    let (then, els) = match always_returns_typed(then) {
        true => (exited, continued),
        false => (continued, exited),
    };
    let ty = valued(&[&then, &els]);
    Some(TExpr::rebuilt(
        TExprKind::If(cond.clone(), then, els),
        ty,
        whole,
    ))
}

fn then_also_typed<T: TyRepr>(before: &TExpr<T>, rest: TExpr<T>) -> TExpr<T> {
    match before.kind {
        TExprKind::Unit => rest,
        _ => {
            let ty = rest.ty.clone();
            TExpr::rebuilt(
                TExprKind::Seq(Box::new(before.clone()), Box::new(rest)),
                ty,
                before,
            )
        }
    }
}

fn always_returns_typed<T: TyRepr>(e: &TExpr<T>) -> bool {
    match &e.kind {
        TExprKind::Return(_) => true,
        TExprKind::Seq(first, rest) => always_returns_typed(first) || always_returns_typed(rest),
        TExprKind::If(_, then, els) => always_returns_typed(then) && always_returns_typed(els),
        TExprKind::Let(_, _, body) | TExprKind::LetMut(_, _, body) => always_returns_typed(body),
        TExprKind::Match(_, arms) => {
            !arms.is_empty() && arms.iter().all(|a| always_returns_typed(&a.body))
        }
        _ => false,
    }
}

fn contains_return_typed<T: TyRepr>(e: &TExpr<T>) -> bool {
    matches!(e.kind, TExprKind::Return(_)) || any_child(e, contains_return_typed)
}

fn contains_try_typed<T: TyRepr>(e: &TExpr<T>) -> bool {
    matches!(e.kind, TExprKind::Try(_)) || any_child(e, contains_try_typed)
}
