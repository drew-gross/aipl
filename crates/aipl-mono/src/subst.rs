//! Binding inlining: substitute a binding read exactly once into its use site.
//!
//! ```text
//! let kept = xs.filter(p);          →     xs.filter(p).map(f)
//! kept.map(f)
//! ```
//!
//! The binding disappears and its value moves to where it was read. Nothing is
//! duplicated — there is exactly one reader, so the value moves rather than
//! being copied.
//!
//! # Why this is worth doing
//!
//! Not for the binding itself. Codegen already emits the same instructions for
//! `let a = f(x); g(a)` as for `g(f(x))` — a single-use binding costs nothing
//! by itself, and removing one changes no code.
//!
//! It is worth doing because **a binding hides a shape from the passes that
//! look for shapes**. [`crate::fuse_operations`] rewrites a call whose receiver
//! is another call; `xs.filter(p).map(f)` is one call on another and fuses into
//! a single pass, while `let kept = xs.filter(p); kept.map(f)` is a call on a
//! *name* and fuses into nothing — same computation, one of the two spellings
//! optimized. Constant folding is blocked the same way (it does not propagate
//! through bindings, by its own module docs). So this pass exists to put the
//! program into the spelling the later passes can read, which is why it runs
//! **before** them.
//!
//! # What makes this safe
//!
//! The value currently runs where the binding is written; afterwards it runs at
//! the use site, so it moves *later*, past whatever the body does first. That
//! is the same move [`crate::sink_bindings`] makes, and it reuses that pass's
//! test for it ([`can_defer`]): no effects, no abort, no control flow of its
//! own, no assignment. A value nobody can observe cannot tell that it moved.
//!
//! Three conditions are this pass's own:
//!
//! - **Exactly one free occurrence.** Two would duplicate the work; zero is a
//!   dead binding, which is a different optimization and left alone.
//! - **That occurrence runs exactly once.** Inside a loop body or a lambda it
//!   would run per iteration or per call instead of once — a pessimization for
//!   a loop, and for a lambda a change in when it runs at all. Occurrences
//!   there pin the binding in place. A *branch* is fine and is a further win:
//!   the arms that don't read it stop computing it, which is what
//!   [`crate::sink_bindings`] is for.
//! - **Nothing in between writes what the value reads.** Moving the value past
//!   a `set` to a binding it reads would compute it from the new value instead
//!   of the old. Any assignment anywhere in the body to a name the value
//!   mentions blocks the substitution — deliberately coarse, since the cost is
//!   a missed rewrite rather than a wrong one.
//!
//! # Deliberately out of scope
//!
//! **Annotated bindings don't substitute.** `let out: i64[] = [];` is an empty
//! literal whose element type comes from the annotation, and the annotation
//! does not survive into an argument position. The rule is broader than that
//! one case on purpose: a declared type can also widen (a parameter typed
//! `any[]` bound to an `i64[]` argument), and either way the annotation is
//! carrying information that substituting would drop. This is the failure mode
//! `TODO.txt` records under *type checker* — "inlining or constant folding
//! causes behaviour to change because after inlining type information is lost"
//! — so an annotation is read here as a reason not to.
//!
//! **Pre-monomorphization only.** The win is in the shape-matching passes, and
//! [`crate::fuse_operations`] and [`crate::fold_constants`] both run before
//! mono. The one post-mono shape pass,
//! [`crate::eliminate_known_constructors_post_mono`], sees through the
//! bindings inlining leaves in front of a `match` itself — they carry a type
//! annotation this pass refuses to drop, and it needs that annotation — so a
//! post-mono run of this pass would buy nothing there either.

use crate::passes::Scope;
use std::collections::HashSet;

use aipl_syntax::ast::{Expr, ExprKind, Item, Program};
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TProgram, Ty, TyRepr};

use crate::sink::{can_defer, undeferrable_fns};

/// Substitute every single-use binding in `program` into its use site.
///
/// `effectful` is the set of functions whose signature declares an effect — the
/// same set [`crate::fuse_operations`] and [`crate::sink_bindings`] take, so
/// pass the checker's declarations and builtin `!prints` is included.
/// `scope` names the functions to rewrite: the pass is body-local, so a
/// function the scope leaves out would come back unchanged anyway (see
/// [`crate::passes`]), and skipping it saves the walk rather than changing the
/// answer.
pub fn inline_single_use_bindings(
    program: &Program,
    effectful: &HashSet<String>,
    scope: &Scope,
) -> Program {
    let blocked = undeferrable_fns(program, effectful);
    Program {
        // Rewrites bodies/items only; the file map and the file's own
        // documentation carry through unchanged.
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) if scope.covers(&f.name) => {
                    let mut f = f.clone();
                    f.body = subst_expr(&f.body, &blocked);
                    f.test_body = f.test_body.as_ref().map(|x| subst_expr(x, &blocked));
                    Item::Fn(f)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// [`inline_single_use_bindings`] on the pinned program.
pub fn inline_single_use_bindings_pinned(
    program: &TProgram,
    effectful: &HashSet<String>,
    scope: &Scope,
) -> TProgram {
    let blocked = crate::sink::undeferrable_fns_pinned(program, effectful);
    crate::typed_util::map_pinned_bodies(
        program,
        |name| scope.covers(name),
        |_, body| {
            let taken = std::mem::replace(body, TExpr::new(TExprKind::Unit, Ty::Unit, 0..0));
            *body = subst_pinned(taken, &blocked);
        },
    )
}

/// [`subst_expr`] over the pinned tree.
fn subst_pinned(mut e: TExpr<Ty>, blocked: &HashSet<String>) -> TExpr<Ty> {
    crate::typed_util::each_child_mut(&mut e, &mut |child| {
        let taken = std::mem::replace(child, TExpr::new(TExprKind::Unit, Ty::Unit, 0..0));
        *child = subst_pinned(taken, blocked);
    });
    let TExprKind::Let(name, value, body) = &e.kind else {
        return e;
    };
    if e.annotation.is_some() || !crate::sink::can_defer_typed(value, blocked) {
        return e;
    }
    let uses = crate::typed_util::uses_of(body, name);
    if uses.count != 1 || uses.repeated {
        return e;
    }
    let mut written = HashSet::new();
    crate::typed_util::assigned_names(body, &mut written);
    if !written.is_empty() {
        let mut read = HashSet::new();
        crate::typed_util::read_names(value, &mut read);
        if read.intersection(&written).next().is_some() {
            return e;
        }
    }
    substitute_typed(body, name, value)
}

/// [`substitute`] over the typed tree.
pub(crate) fn substitute_typed<T: TyRepr>(e: &TExpr<T>, name: &str, value: &TExpr<T>) -> TExpr<T> {
    let keep = |kind: TExprKind<T>| {
        let mut out = TExpr::rebuilt(kind, e.ty.clone(), e);
        out.annotation = e.annotation.clone();
        out.unsettled = e.unsettled;
        out
    };
    match &e.kind {
        TExprKind::Ident(n) if n == name => value.clone(),
        TExprKind::Let(n, v, body) | TExprKind::LetMut(n, v, body) => {
            let v = Box::new(substitute_typed(v, name, value));
            let body = if n == name {
                body.clone()
            } else {
                Box::new(substitute_typed(body, name, value))
            };
            keep(match &e.kind {
                TExprKind::Let(..) => TExprKind::Let(n.clone(), v, body),
                _ => TExprKind::LetMut(n.clone(), v, body),
            })
        }
        TExprKind::For(n, index, iterable, body) => {
            let iterable = Box::new(substitute_typed(iterable, name, value));
            let body = if aipl_syntax::for_binds(n, index, name) {
                body.clone()
            } else {
                Box::new(substitute_typed(body, name, value))
            };
            keep(TExprKind::For(n.clone(), index.clone(), iterable, body))
        }
        TExprKind::Match(scrutinee, arms) => {
            let scrutinee = Box::new(substitute_typed(scrutinee, name, value));
            let arms = arms
                .iter()
                .map(|a| {
                    let shadowed = a.pattern.bindings().iter().any(|b| b == name);
                    TMatchArm {
                        body: if shadowed {
                            a.body.clone()
                        } else {
                            substitute_typed(&a.body, name, value)
                        },
                        ..a.clone()
                    }
                })
                .collect();
            keep(TExprKind::Match(scrutinee, arms))
        }
        TExprKind::Lambda(params, body) => {
            if params.iter().any(|p| p.name == name) {
                e.clone()
            } else {
                let body = Box::new(substitute_typed(body, name, value));
                keep(TExprKind::Lambda(params.clone(), body))
            }
        }
        _ => {
            let mut out = e.clone();
            crate::typed_util::each_child_mut(&mut out, &mut |child| {
                let done = substitute_typed(child, name, value);
                *child = done;
            });
            out
        }
    }
}

/// Rebuild `e` with its children substituted first, then substitute `e` itself.
///
/// Bottom-up so a chain collapses in one traversal: in `let a = f(); let b =
/// g(a); h(b)` the inner binding goes first, which leaves `a` with one reader
/// on the way out, and the result is `h(g(f()))`.
fn subst_expr(e: &Expr, blocked: &HashSet<String>) -> Expr {
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        let done = subst_expr(child, blocked);
        *child = done;
    }
    subst_here(out, blocked)
}

/// One substitution step at `e` itself, or `e` unchanged when the shape or the
/// safety conditions don't hold.
fn subst_here(e: Expr, blocked: &HashSet<String>) -> Expr {
    let ExprKind::Let(name, ty, value, body) = &e.kind else {
        return e;
    };
    // An annotation is carrying type information that would not survive the
    // move — see the module docs.
    if ty.is_some() {
        return e;
    }
    if !can_defer(value, blocked) {
        return e;
    }
    let uses = uses_of(body, name);
    if uses.count != 1 || uses.repeated {
        return e;
    }
    // Moving the value later must not move it past a write to something it
    // reads.
    let mut written = HashSet::new();
    assigned_names(body, &mut written);
    if !written.is_empty() {
        let mut read = HashSet::new();
        read_names(value, &mut read);
        if read.intersection(&written).next().is_some() {
            return e;
        }
    }
    substitute(body, name, value)
}

/// The tally [`subst_here`] needs: how many times `name` occurs free in an
/// expression, and whether any occurrence sits somewhere that runs other than
/// exactly once.
pub(crate) struct Uses {
    pub(crate) count: usize,
    pub(crate) repeated: bool,
}

pub(crate) fn uses_of(e: &Expr, name: &str) -> Uses {
    let mut uses = Uses {
        count: 0,
        repeated: false,
    };
    walk_uses(e, name, false, &mut uses);
    uses
}

/// Count free occurrences of `name`, tracking whether each is under something
/// that re-runs it. An occurrence under a binder that re-introduces the same
/// name belongs to that binder, not ours, and is not counted — the same
/// shadowing rule [`crate::sink`]'s `mentions_free` applies.
fn walk_uses(e: &Expr, name: &str, repeated: bool, out: &mut Uses) {
    match &e.kind {
        ExprKind::Ident(n) if n == name => {
            out.count += 1;
            out.repeated |= repeated;
        }
        ExprKind::Let(n, _, value, body) | ExprKind::LetMut(n, _, value, body) => {
            walk_uses(value, name, repeated, out);
            if n != name {
                walk_uses(body, name, repeated, out);
            }
        }
        // The iterable runs once; the body runs per element.
        ExprKind::For(n, index, iterable, body) => {
            walk_uses(iterable, name, repeated, out);
            if !aipl_syntax::for_binds(n, index, name) {
                walk_uses(body, name, true, out);
            }
        }
        // A `while` re-tests its condition, so both halves repeat.
        ExprKind::While(cond, body) => {
            walk_uses(cond, name, true, out);
            walk_uses(body, name, true, out);
        }
        ExprKind::Match(scrutinee, arms) => {
            walk_uses(scrutinee, name, repeated, out);
            for a in arms {
                if !a.pattern.bindings().iter().any(|b| b == name) {
                    walk_uses(&a.body, name, repeated, out);
                }
            }
        }
        // A lambda body runs when the lambda is called — zero times, or many.
        ExprKind::Lambda(params, body) => {
            if !params.iter().any(|p| p.name == name) {
                walk_uses(body, name, true, out);
            }
        }
        _ => {
            for c in crate::children(e) {
                walk_uses(c, name, repeated, out);
            }
        }
    }
}

/// `e` with the free occurrences of `name` replaced by `value`, respecting
/// shadowing (a `let`/`for`/match-arm/lambda binder of `name` stops the walk).
///
/// The replacement keeps `value`'s own span and stamped type: it is the same
/// expression, now written where it is used.
///
/// [`uses_of`] finds exactly one occurrence before the single-use inliner calls this, so it
/// replaces exactly one.
///
/// One shape it deliberately does not reach: an occurrence inside a `set`
/// target. The fallback arm walks [`crate::children_mut`], whose `Assign` case
/// yields only the value and the continuation, not the lvalue. Callers that
/// cannot tolerate a missed occurrence must exclude assigned names first — see
/// [`assigns_to`].
pub(crate) fn substitute(e: &Expr, name: &str, value: &Expr) -> Expr {
    match &e.kind {
        ExprKind::Ident(n) if n == name => value.clone(),
        // The binder cases, which stop the walk when they re-introduce `name`.
        // Everything else has no binding structure to respect and goes through
        // the generic child rebuild below.
        ExprKind::Let(n, ty, v, body) | ExprKind::LetMut(n, ty, v, body) => {
            let v = Box::new(substitute(v, name, value));
            let body = if n == name {
                body.clone()
            } else {
                Box::new(substitute(body, name, value))
            };
            let kind = match &e.kind {
                ExprKind::Let(..) => ExprKind::Let(n.clone(), ty.clone(), v, body),
                _ => ExprKind::LetMut(n.clone(), ty.clone(), v, body),
            };
            Expr::rebuilt(kind, e)
        }
        ExprKind::For(n, index, iterable, body) => {
            let iterable = Box::new(substitute(iterable, name, value));
            let body = if aipl_syntax::for_binds(n, index, name) {
                body.clone()
            } else {
                Box::new(substitute(body, name, value))
            };
            Expr::rebuilt(ExprKind::For(n.clone(), index.clone(), iterable, body), e)
        }
        ExprKind::Match(scrutinee, arms) => {
            let scrutinee = Box::new(substitute(scrutinee, name, value));
            let arms = arms
                .iter()
                .map(|a| {
                    let shadowed = a.pattern.bindings().iter().any(|b| b == name);
                    aipl_syntax::ast::MatchArm {
                        body: if shadowed {
                            a.body.clone()
                        } else {
                            substitute(&a.body, name, value)
                        },
                        ..a.clone()
                    }
                })
                .collect();
            Expr::rebuilt(ExprKind::Match(scrutinee, arms), e)
        }
        ExprKind::Lambda(params, body) => {
            if params.iter().any(|p| p.name == name) {
                e.clone()
            } else {
                let body = Box::new(substitute(body, name, value));
                Expr::rebuilt(ExprKind::Lambda(params.clone(), body), e)
            }
        }
        _ => {
            let mut out = e.clone();
            for child in crate::children_mut(&mut out) {
                let done = substitute(child, name, value);
                *child = done;
            }
            out
        }
    }
}

/// Every binding written anywhere in `e`: the *root* of each `set` target, so
/// `set w.pos = ..` and `set w[i] = ..` both report `w`.
pub(crate) fn assigned_names(e: &Expr, out: &mut HashSet<String>) {
    if let ExprKind::Assign(lhs, _, _) = &e.kind {
        if let Some(root) = root_name(lhs) {
            out.insert(root.to_string());
        }
    }
    for c in crate::children(e) {
        assigned_names(c, out);
    }
}

/// The binding an assignment target ultimately names, reached through the field
/// and element steps that can precede it.
fn root_name(e: &Expr) -> Option<&str> {
    match &e.kind {
        ExprKind::Ident(n) => Some(n),
        ExprKind::Field(x, _) | ExprKind::Index(x, _) | ExprKind::Slice(x, _, _) => root_name(x),
        _ => None,
    }
}

/// Every name `e` reads. Shadowing is ignored: this only ever adds names, and
/// an extra one costs a missed substitution rather than a wrong one.
pub(crate) fn read_names(e: &Expr, out: &mut HashSet<String>) {
    if let ExprKind::Ident(n) = &e.kind {
        out.insert(n.clone());
    }
    for c in crate::children(e) {
        read_names(c, out);
    }
}
