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

use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TProgram, Ty, TyRepr};

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

/// The tally [`subst_here`] needs: how many times `name` occurs free in an
/// expression, and whether any occurrence sits somewhere that runs other than
/// exactly once.
pub(crate) struct Uses {
    pub(crate) count: usize,
    pub(crate) repeated: bool,
}
