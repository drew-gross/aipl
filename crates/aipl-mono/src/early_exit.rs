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

use aipl_syntax::ast::{Expr, ExprKind, MatchArm};

use crate::children;

/// `body` rewritten to compute the same value with no `return` in it, or `None`
/// when its early exits are not a shape this can express (see the module
/// docs). A body with no early exit at all comes back unchanged.
///
/// The caller is the inliner, in two places that must agree: the gate that
/// decides a function is a candidate, and the expansion that places its body.
/// Both ask this, so neither can accept what the other cannot do.
pub(crate) fn without_early_exits(body: &Expr) -> Option<Expr> {
    if contains_try(body) {
        return None;
    }
    if !contains_return(body) {
        return Some(body.clone());
    }
    in_value_position(body)
}

/// Rewrite `e`, which sits where its value is the value of the whole inlined
/// expression. That position is what makes the rewrite possible: a `return`
/// here has nowhere further to go, so it is nothing but its value.
fn in_value_position(e: &Expr) -> Option<Expr> {
    match &e.kind {
        // The tail rule. A `return` whose value itself exits is not something
        // the language can produce, so it is refused rather than reasoned about.
        ExprKind::Return(value) => match contains_return(value) {
            true => None,
            false => Some(value.as_ref().clone()),
        },
        ExprKind::Seq(first, rest) => {
            if always_returns(first) {
                // Whatever follows is unreachable, so the sequence *is* the
                // first statement. Dropping `rest` is not an optimization
                // here — keeping it would leave code after a value.
                return in_value_position(first);
            }
            if let Some(rewritten) = guard(e, first, rest) {
                return Some(rewritten);
            }
            // An exit anywhere else in a statement — inside a loop, under a
            // lambda, in a `match` used as a guard — is one of the shapes this
            // does not express.
            if contains_return(first) {
                return None;
            }
            Some(Expr::rebuilt(
                ExprKind::Seq(first.clone(), Box::new(in_value_position(rest)?)),
                e,
            ))
        }
        // Both arms are in value position, so a `return` in either is a tail
        // return. The condition is not, and an exit in it is refused.
        ExprKind::If(cond, then, els) if !contains_return(cond) => Some(Expr::rebuilt(
            ExprKind::If(
                cond.clone(),
                Box::new(in_value_position(then)?),
                Box::new(in_value_position(els)?),
            ),
            e,
        )),
        // A `let`'s body is in value position; its initializer is not.
        ExprKind::Let(name, ty, value, body) if !contains_return(value) => Some(Expr::rebuilt(
            ExprKind::Let(
                name.clone(),
                ty.clone(),
                value.clone(),
                Box::new(in_value_position(body)?),
            ),
            e,
        )),
        ExprKind::LetMut(name, ty, value, body) if !contains_return(value) => Some(Expr::rebuilt(
            ExprKind::LetMut(
                name.clone(),
                ty.clone(),
                value.clone(),
                Box::new(in_value_position(body)?),
            ),
            e,
        )),
        ExprKind::Match(scrutinee, arms) if !contains_return(scrutinee) => {
            let arms: Option<Vec<MatchArm>> = arms
                .iter()
                .map(|arm| {
                    Some(MatchArm {
                        body: in_value_position(&arm.body)?,
                        ..arm.clone()
                    })
                })
                .collect();
            Some(Expr::rebuilt(ExprKind::Match(scrutinee.clone(), arms?), e))
        }
        // Anything else is only acceptable if it does not exit at all. In
        // particular this is where a `return` under a `for`, a `while` or a
        // lambda lands.
        _ => match contains_return(e) {
            true => None,
            false => Some(e.clone()),
        },
    }
}

/// The guard rule: `if (c) { return v; }; rest` → `if (c) { v } else { rest }`.
///
/// `whole` is the `Seq` being replaced, and what the result inherits its span
/// and recorded type from — the type of the sequence is the type of `rest`,
/// which is the type the rewritten `if` now has. Taking it from the statement
/// `if` instead would record `unit` for an expression that produces a value.
///
/// Exactly one arm may exit. The other is the one that falls through, and
/// `rest` moves into it; if that arm computes something of its own first, it
/// keeps it, so an arm that is there for its effects is not quietly dropped.
fn guard(whole: &Expr, first: &Expr, rest: &Expr) -> Option<Expr> {
    let ExprKind::If(cond, then, els) = &first.kind else {
        return None;
    };
    if contains_return(cond) {
        return None;
    }
    let (exits, falls_through) = match (always_returns(then), always_returns(els)) {
        // `(true, true)` never reaches here — the caller handles a statement
        // that always returns before asking.
        (true, false) => (then, els),
        (false, true) => (els, then),
        _ => return None,
    };
    // The arm that falls through may hold an exit of its own somewhere this
    // cannot reach, in which case the whole shape is refused.
    if contains_return(falls_through) {
        return None;
    }
    let continued = Box::new(then_also(falls_through, in_value_position(rest)?));
    let exited = Box::new(in_value_position(exits)?);
    let (then, els) = match always_returns(then) {
        true => (exited, continued),
        false => (continued, exited),
    };
    Some(Expr::rebuilt(ExprKind::If(cond.clone(), then, els), whole))
}

/// `rest`, preceded by `before` when there is anything there to keep. A guard's
/// fall-through arm is almost always the empty `{}` the parser records as
/// `unit`, and sequencing that ahead of the continuation would add a statement
/// that says nothing.
fn then_also(before: &Expr, rest: Expr) -> Expr {
    match before.kind {
        ExprKind::Unit => rest,
        _ => Expr::rebuilt(
            ExprKind::Seq(Box::new(before.clone()), Box::new(rest)),
            before,
        ),
    }
}

/// Whether every path through `e` ends in a `return`, so nothing after it runs.
///
/// Deliberately not transparent to a loop or a lambda: a `return` in a `for`
/// body runs only on the iterations that reach it, and one in a lambda leaves
/// the lambda. Both answer `false` here, which is what keeps [`guard`] from
/// treating "contains a return" as "always returns".
fn always_returns(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Return(_) => true,
        ExprKind::Seq(first, rest) => always_returns(first) || always_returns(rest),
        ExprKind::If(_, then, els) => always_returns(then) && always_returns(els),
        ExprKind::Let(_, _, _, body) | ExprKind::LetMut(_, _, _, body) => always_returns(body),
        // Exhaustive by the time any pass runs (the checker said so), so every
        // arm returning means the `match` does.
        ExprKind::Match(_, arms) => {
            !arms.is_empty() && arms.iter().all(|a| always_returns(&a.body))
        }
        _ => false,
    }
}

fn contains_return(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Return(_)) || children(e).iter().any(|c| contains_return(c))
}

fn contains_try(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Try(_)) || children(e).iter().any(|c| contains_try(c))
}
