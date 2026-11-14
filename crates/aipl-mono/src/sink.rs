//! Code sinking: move a binding's computation into the branch that uses it.
//!
//! `let x = <value>;` followed directly by a branch whose *one* arm mentions
//! `x` computes `<value>` on every path, including the paths that throw it
//! away. Sinking rewrites
//!
//! ```text
//! let x = f(y);                       if (c) {
//! if (c) { g(x) } else { 0 }    →         let x = f(y); g(x)
//!                                     } else { 0 }
//! ```
//!
//! so the work happens only where its result is wanted. Nothing is duplicated:
//! exactly one arm uses the binding, so the value moves rather than being
//! copied, and the arms that ignored it stop paying for it.
//!
//! Chains fall out of the bottom-up traversal. `let a = ..; let b = ..; if ..`
//! sinks `b` first, which leaves `a`'s body a branch again, so `a` is
//! considered on the way out.
//!
//! # What makes this safe
//!
//! The binding moves to the *front* of the arm, so it still runs before
//! everything the arm does — nothing inside the branch is reordered against it.
//! Only two things change: the value is computed after the condition instead of
//! before it, and on the other arm it is not computed at all. So the value has
//! to be one whose evaluation nobody can observe:
//!
//! - **No effects.** A `!prints`/`!reads`/`!writes` call that stopped happening
//!   on the untaken arm is a behaviour change, not an optimization.
//! - **No abort.** Deferring a division by zero (or a failed `assert`) past a
//!   branch turns a program that dies into one that returns — so a value that
//!   can abort stays put. This is tracked through the call graph: a function
//!   that divides is unsafe to defer, as is every function that calls one.
//! - **No control flow of its own.** A `?` or `return` inside the value leaves
//!   the function from where it sits, and moving it moves the exit.
//! - **No assignment.** A `set` inside the value may write a binding declared
//!   outside it, which is a side effect by another name.
//!
//! The condition itself needs no such restriction: it keeps its place, and a
//! value that can't be observed can't tell it moved.
//!
//! # Deliberately out of scope
//!
//! A branch reached only *after* other statements (`let x = ..; stmt; if ..`)
//! doesn't sink: moving the value past `stmt` needs a dependency check this
//! pass doesn't do. A branch followed by other statements does, because that
//! direction is free — when the tail never reads the binding, narrowing its
//! scope to the statement that does reorders nothing at all, and what is left
//! is the shape above.
//!
//! `mut` bindings don't sink either — a later `set` from outside the branch
//! would be left referring to a binding that no longer exists there.

use crate::passes::Scope;
use std::collections::HashSet;

use aipl_syntax::ast::Callee;
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TyRepr};

use crate::MonoProgram;

/// Builtins whose call can abort the program, so deferring one past a branch
/// could turn a program that dies into one that doesn't. `__assert` is what
/// `assert(c)` lowers to, and its whole purpose is to abort.
///
/// `/` and `%` used to be here. Both are total now — a zero divisor and
/// `i64::MIN / -1` answer `MAX` and, for `%`, the dividend and 0 — so either can
/// be deferred like any other arithmetic. Nothing else in the language aborts,
/// which is why this list is down to one entry.
const ABORTING_BUILTINS: &[Callee] = &[Callee::Assert];

/// [`sink_bindings`] on the pinned program.
pub fn sink_bindings_pinned(
    program: &aipl_syntax::typed::TProgram,
    effectful: &HashSet<String>,
    scope: &Scope,
) -> aipl_syntax::typed::TProgram {
    let blocked = undeferrable_fns_pinned(program, effectful);
    crate::typed_util::map_pinned_bodies(
        program,
        |name| scope.covers(name),
        |_, body| {
            let taken = std::mem::replace(body, placeholder());
            *body = sink_typed(taken, &blocked);
        },
    )
}

/// [`undeferrable_fns`] over the pinned program.
pub(crate) fn undeferrable_fns_pinned(
    program: &aipl_syntax::typed::TProgram,
    effectful: &HashSet<String>,
) -> HashSet<String> {
    let bodies: Vec<(&str, &TExpr<aipl_syntax::typed::Ty>)> = program
        .items
        .iter()
        .filter_map(|it| match it {
            aipl_syntax::typed::TItem::Fn(f) => Some((f.name.text.as_str(), &f.body)),
            _ => None,
        })
        .collect();
    close_over_calls(&bodies, effectful, reaches_blocked_typed)
}

/// [`undeferrable_fns`] over a monomorphized program: the same closure, seeded
/// with `builtin_effects` (the declarations under their pre-mono names — see
/// [`sink_bindings_post_mono`]) plus every instance that declares an effect.
pub(crate) fn undeferrable_fns_post_mono(
    program: &MonoProgram,
    builtin_effects: &HashSet<String>,
) -> HashSet<String> {
    let mut effectful = builtin_effects.clone();
    effectful.extend(
        program
            .fns
            .iter()
            .filter(|f| !f.effects.is_empty())
            .map(|f| f.name.clone()),
    );
    let bodies: Vec<(&str, &crate::typed_util::Typed)> = program
        .fns
        .iter()
        .map(|f| (f.name.as_str(), &f.body))
        .collect();
    close_over_calls(&bodies, &effectful, reaches_blocked_typed)
}

/// [`undeferrable_fns`]'s fixpoint, over whichever `(name, body)` pairs the
/// caller has — source items before monomorphization, concrete instances after.
fn close_over_calls<B>(
    bodies: &[(&str, &B)],
    effectful: &HashSet<String>,
    reaches: impl Fn(&B, &HashSet<String>) -> bool,
) -> HashSet<String> {
    let mut blocked: HashSet<String> = effectful.clone();
    blocked.extend(ABORTING_BUILTINS.iter().map(|c| c.name().to_string()));
    // A fixpoint rather than one pass: `a` calling `b` calling a divider makes
    // `a` undeferrable too, and the items are in no particular order.
    loop {
        let mut grew = false;
        for (name, body) in bodies {
            if !blocked.contains(*name) && reaches(body, &blocked) {
                blocked.insert((*name).to_string());
                grew = true;
            }
        }
        if !grew {
            return blocked;
        }
    }
}

/// Sink again over the *monomorphized* program, after post-mono inlining.
///
/// The pre-mono run cannot see the lambdas mono lifts into their own
/// functions, nor which instances end up called once: those are only folded
/// into their callers afterwards, and either can expose a binding that one
/// branch reads and the others ignore.
///
/// The case this exists for is `value_or`: its default is an ordinary argument,
/// so a caller evaluates it either way — until the instance is inlined and the
/// binding holding it turns out to sit directly ahead of a `match` whose `none`
/// arm is its only reader.
///
/// `builtin_effects` carries the effect declarations under their *pre-mono*
/// names, which is how `__builtin_print` is still recognized as effectful here:
/// mono mangles user instances but leaves builtin call names alone, so neither
/// set alone covers both.
/// `scope` names the functions to rewrite: the pass is body-local, so a
/// function the scope leaves out would come back unchanged anyway (see
/// [`crate::passes`]), and skipping it saves the walk rather than changing the
/// answer.
pub fn sink_bindings_post_mono(
    program: &MonoProgram,
    builtin_effects: &HashSet<String>,
    scope: &Scope,
) -> MonoProgram {
    let blocked = undeferrable_fns_post_mono(program, builtin_effects);
    crate::typed_util::map_bodies(
        program,
        |name| scope.covers(name),
        |_, body| {
            let taken = std::mem::replace(body, placeholder());
            *body = sink_typed(taken, &blocked);
        },
    )
}

fn placeholder<T: TyRepr>() -> TExpr<T> {
    TExpr::new(TExprKind::Unit, T::unit(), 0..0)
}

/// [`reaches_blocked`] over the typed tree.
fn reaches_blocked_typed<T: TyRepr>(e: &TExpr<T>, blocked: &HashSet<String>) -> bool {
    let here = match &e.kind {
        TExprKind::Call { callee, .. } => blocked.contains(callee.name()),
        TExprKind::Ident(name) => blocked.contains(name),
        TExprKind::Shim(..) => true,
        _ => false,
    };
    here || crate::typed_util::any_child(e, |c| reaches_blocked_typed(c, blocked))
}

/// [`can_defer`] over the typed tree.
pub(crate) fn can_defer_typed<T: TyRepr>(value: &TExpr<T>, blocked: &HashSet<String>) -> bool {
    fn moves_control_or_writes<T: TyRepr>(e: &TExpr<T>) -> bool {
        matches!(
            &e.kind,
            TExprKind::Try(_)
                | TExprKind::Return(_)
                | TExprKind::Assign(..)
                | TExprKind::For(..)
                | TExprKind::While(..)
        ) || crate::typed_util::any_child(e, moves_control_or_writes)
    }
    !reaches_blocked_typed(value, blocked) && !moves_control_or_writes(value)
}

/// [`sink_expr`] over the typed tree.
fn sink_typed<T: TyRepr>(mut e: TExpr<T>, blocked: &HashSet<String>) -> TExpr<T> {
    crate::typed_util::each_child_mut(&mut e, &mut |child| {
        let taken = std::mem::replace(child, placeholder());
        *child = sink_typed(taken, blocked);
    });
    sink_here_typed(e, blocked)
}

/// [`sink_here`] over the typed tree. A binding re-formed around an arm has the arm's type.
fn sink_here_typed<T: TyRepr>(e: TExpr<T>, blocked: &HashSet<String>) -> TExpr<T> {
    let TExprKind::Let(name, value, body) = &e.kind else {
        return e;
    };
    if !can_defer_typed(value, blocked) {
        return e;
    }
    let annotation = e.annotation.clone();
    // The binding, re-formed around whichever arm turned out to use it.
    let rebind = |arm: &TExpr<T>| {
        let mut node = TExpr::rebuilt(
            TExprKind::Let(name.clone(), value.clone(), Box::new(arm.clone())),
            arm.ty.clone(),
            &e,
        );
        node.annotation = annotation.clone();
        node
    };
    use crate::typed_util::{mentions_free, read_names};
    match &body.kind {
        TExprKind::If(cond, then, els) => {
            if mentions_free(cond, name) {
                return e;
            }
            match (mentions_free(then, name), mentions_free(els, name)) {
                (true, false) => TExpr::rebuilt(
                    TExprKind::If(cond.clone(), Box::new(rebind(then)), els.clone()),
                    body.ty.clone(),
                    body,
                ),
                (false, true) => TExpr::rebuilt(
                    TExprKind::If(cond.clone(), then.clone(), Box::new(rebind(els))),
                    body.ty.clone(),
                    body,
                ),
                _ => e,
            }
        }
        TExprKind::Match(scrutinee, arms) => {
            if mentions_free(scrutinee, name) {
                return e;
            }
            if arms
                .iter()
                .any(|a| a.pattern.bindings().iter().any(|b| b == name))
            {
                return e;
            }
            let mut users = arms
                .iter()
                .enumerate()
                .filter(|(_, a)| mentions_free(&a.body, name))
                .map(|(i, _)| i);
            let (Some(only), None) = (users.next(), users.next()) else {
                return e;
            };
            let mut read = HashSet::new();
            read_names(value, &mut read);
            if arms[only]
                .pattern
                .bindings()
                .iter()
                .any(|b| read.contains(b))
            {
                return e;
            }
            let arms = arms
                .iter()
                .enumerate()
                .map(|(i, a)| TMatchArm {
                    body: if i == only {
                        rebind(&a.body)
                    } else {
                        a.body.clone()
                    },
                    ..a.clone()
                })
                .collect();
            TExpr::rebuilt(
                TExprKind::Match(scrutinee.clone(), arms),
                body.ty.clone(),
                body,
            )
        }
        TExprKind::Seq(first, rest) if !mentions_free(rest, name) => {
            let narrowed = sink_here_typed(rebind(first), blocked);
            TExpr::rebuilt(
                TExprKind::Seq(Box::new(narrowed), rest.clone()),
                body.ty.clone(),
                body,
            )
        }
        TExprKind::Let(other, other_value, rest)
            if other != name
                && !mentions_free(other_value, name)
                && !mentions_free(value, other)
                && can_defer_typed(other_value, blocked) =>
        {
            let moved = rebind(rest);
            let sunk = sink_here_typed(moved.clone(), blocked);
            if sunk == moved {
                return e;
            }
            let mut node = TExpr::rebuilt(
                TExprKind::Let(other.clone(), other_value.clone(), Box::new(sunk)),
                body.ty.clone(),
                body,
            );
            node.annotation = body.annotation.clone();
            node
        }
        _ => e,
    }
}
