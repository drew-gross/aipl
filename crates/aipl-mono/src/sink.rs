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

use std::collections::HashSet;

use aipl_syntax::ast::{Expr, ExprKind, Item, MatchArm, Program};

use crate::{ConcreteFn, MonoProgram};

/// Builtins whose call can abort the program, so deferring one past a branch
/// could turn a program that dies into one that doesn't. `__assert` is what
/// `assert(c)` lowers to, and its whole purpose is to abort.
///
/// `/` and `%` used to be here. Both are total now — a zero divisor and
/// `i64::MIN / -1` answer `MAX` and, for `%`, the dividend and 0 — so either can
/// be deferred like any other arithmetic. Nothing else in the language aborts,
/// which is why this list is down to one entry.
const ABORTING_BUILTINS: &[&str] = &["__assert"];

/// Sink every binding in `program` that only one branch of the following
/// `if`/`match` uses. `effectful` is the set of functions whose signature
/// declares an effect — the same set [`crate::fuse_operations`] takes, so pass
/// the checker's declarations and builtin `!prints` is included.
pub fn sink_bindings(program: &Program, effectful: &HashSet<String>) -> Program {
    let blocked = undeferrable_fns(program, effectful);
    Program {
        // Rewrites bodies/items only; the file map carries through unchanged.
        sources: program.sources.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => {
                    let mut f = f.clone();
                    f.body = sink_expr(&f.body, &blocked);
                    f.test_body = f.test_body.as_ref().map(|x| sink_expr(x, &blocked));
                    Item::Fn(f)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// Every function whose call must not be deferred: one that declares an effect,
/// one that can abort, and — transitively — everyone who calls such a function.
///
/// Seeded with [`ABORTING_BUILTINS`] and `effectful`, then closed over the call
/// graph. A name that is neither defined here nor in the seeds is a builtin
/// that neither aborts nor has effects (indexing yields `none` rather than
/// trapping, `int_parse` an `i64?`), so it is safe to defer.
pub(crate) fn undeferrable_fns(program: &Program, effectful: &HashSet<String>) -> HashSet<String> {
    let bodies: Vec<(&str, &Expr)> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some((f.name.as_str(), &f.body)),
            _ => None,
        })
        .collect();
    close_over_calls(&bodies, effectful)
}

/// [`undeferrable_fns`]'s fixpoint, over whichever `(name, body)` pairs the
/// caller has — source items before monomorphization, concrete instances after.
fn close_over_calls(bodies: &[(&str, &Expr)], effectful: &HashSet<String>) -> HashSet<String> {
    let mut blocked: HashSet<String> = effectful.clone();
    blocked.extend(ABORTING_BUILTINS.iter().map(|s| (*s).to_string()));
    // A fixpoint rather than one pass: `a` calling `b` calling a divider makes
    // `a` undeferrable too, and the items are in no particular order.
    loop {
        let mut grew = false;
        for (name, body) in bodies {
            if !blocked.contains(*name) && reaches_blocked(body, &blocked) {
                blocked.insert((*name).to_string());
                grew = true;
            }
        }
        if !grew {
            return blocked;
        }
    }
}

/// Whether evaluating `e` can reach something in `blocked` — a call to one of
/// them, or a `shim`, which *installs* an effect and so is an effect happening.
///
/// `%` used to be listed here structurally (it resolves to the operator, not to
/// a call) because it trapped on a zero divisor. It is total now, so arithmetic
/// no longer pins a binding in place at all.
fn reaches_blocked(e: &Expr, blocked: &HashSet<String>) -> bool {
    let here = match &e.kind {
        ExprKind::Call(name, _, _) => blocked.contains(name),
        ExprKind::Shim(..) => true,
        _ => false,
    };
    here || crate::children(e)
        .iter()
        .any(|c| reaches_blocked(c, blocked))
}

/// Whether `value` may be moved into a branch — see the module docs.
pub(crate) fn can_defer(value: &Expr, blocked: &HashSet<String>) -> bool {
    fn moves_control_or_writes(e: &Expr) -> bool {
        matches!(
            &e.kind,
            // Leaves the function from where it sits.
            ExprKind::Try(_) | ExprKind::Return(_)
            // May write a binding declared outside the value.
            | ExprKind::Assign(..)
            // May not terminate, and its body may do either of the above.
            | ExprKind::For(..) | ExprKind::While(..)
        ) || crate::children(e)
            .iter()
            .any(|c| moves_control_or_writes(c))
    }
    !reaches_blocked(value, blocked) && !moves_control_or_writes(value)
}

/// Rebuild `e` with its children sunk first, then sink `e` itself. Bottom-up so
/// a chain of bindings collapses inward one at a time.
fn sink_expr(e: &Expr, blocked: &HashSet<String>) -> Expr {
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        let sunk = sink_expr(child, blocked);
        *child = sunk;
    }
    sink_here(out, blocked)
}

/// One sinking step at `e` itself, or `e` unchanged when the shape or the
/// safety conditions don't hold.
fn sink_here(e: Expr, blocked: &HashSet<String>) -> Expr {
    let ExprKind::Let(name, ty, value, body) = &e.kind else {
        return e;
    };
    if !can_defer(value, blocked) {
        return e;
    }
    // The binding, re-formed around whichever arm turned out to use it.
    let rebind = |arm: &Expr| {
        Expr::rebuilt(
            ExprKind::Let(
                name.clone(),
                ty.clone(),
                value.clone(),
                Box::new(arm.clone()),
            ),
            &e,
        )
    };
    match &body.kind {
        ExprKind::If(cond, then, els) => {
            // A condition that reads the binding pins it in place: the condition
            // is evaluated before either arm exists.
            if mentions_free(cond, name) {
                return e;
            }
            match (mentions_free(then, name), mentions_free(els, name)) {
                (true, false) => Expr::rebuilt(
                    ExprKind::If(cond.clone(), Box::new(rebind(then)), els.clone()),
                    body,
                ),
                (false, true) => Expr::rebuilt(
                    ExprKind::If(cond.clone(), then.clone(), Box::new(rebind(els))),
                    body,
                ),
                // Used by both arms (sinking would duplicate the work) or by
                // neither (a dead binding, which is a different optimization).
                _ => e,
            }
        }
        ExprKind::Match(scrutinee, arms) => {
            if mentions_free(scrutinee, name) {
                return e;
            }
            // An arm that re-binds the name shadows ours, so "uses it" would be
            // reading the wrong binding. Rare enough not to be worth untangling.
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
            let arms = arms
                .iter()
                .enumerate()
                .map(|(i, a)| MatchArm {
                    body: if i == only {
                        rebind(&a.body)
                    } else {
                        a.body.clone()
                    },
                    ..a.clone()
                })
                .collect();
            Expr::rebuilt(ExprKind::Match(scrutinee.clone(), arms), body)
        }
        // `let x = v; <branch>; rest` — the binding is scoped over the whole
        // remainder, but if `rest` never reads it the scope can narrow to the
        // statement that does, which may then be a branch to sink into. This
        // reorders nothing: `v`, then the statement, then `rest`, either way.
        ExprKind::Seq(first, rest) if !mentions_free(rest, name) => {
            let narrowed = sink_here(rebind(first), blocked);
            Expr::rebuilt(ExprKind::Seq(Box::new(narrowed), rest.clone()), body)
        }
        _ => e,
    }
}

/// Whether `name` occurs *free* in `e` — an occurrence under a binder that
/// re-introduces the same name belongs to that binder, not to ours.
fn mentions_free(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Let(n, _, value, body) | ExprKind::LetMut(n, _, value, body) => {
            mentions_free(value, name) || (n != name && mentions_free(body, name))
        }
        ExprKind::For(n, iterable, body) => {
            mentions_free(iterable, name) || (n != name && mentions_free(body, name))
        }
        ExprKind::Match(scrutinee, arms) => {
            mentions_free(scrutinee, name)
                || arms.iter().any(|a| {
                    !a.pattern.bindings().iter().any(|b| b == name) && mentions_free(&a.body, name)
                })
        }
        ExprKind::Lambda(params, body) => {
            !params.iter().any(|p| p.name == name) && mentions_free(body, name)
        }
        _ => crate::children(e).iter().any(|c| mentions_free(c, name)),
    }
}

/// Sink again over the *monomorphized* program, after post-mono inlining.
///
/// The pre-mono run cannot see two things. AIPL-implemented builtins
/// (`AIPL_BUILTIN_SOURCES`) are loaded on demand *during* monomorphization, so
/// at that point they are not in the program at all; and the lambdas mono lifts
/// into their own functions, together with any instance that ends up called
/// once, are only folded into their callers afterwards. Either can expose a
/// binding that one branch reads and the others ignore.
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
pub fn sink_bindings_post_mono(
    program: &MonoProgram,
    builtin_effects: &HashSet<String>,
) -> MonoProgram {
    let mut effectful = builtin_effects.clone();
    effectful.extend(
        program
            .fns
            .iter()
            .filter(|f| !f.effects.is_empty())
            .map(|f| f.name.clone()),
    );
    let bodies: Vec<(&str, &Expr)> = program
        .fns
        .iter()
        .map(|f| (f.name.as_str(), &f.body))
        .collect();
    let blocked = close_over_calls(&bodies, &effectful);
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| ConcreteFn {
                body: sink_expr(&f.body, &blocked),
                ..f.clone()
            })
            .collect(),
        ..program.clone()
    }
}
