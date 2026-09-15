//! Known-constructor elimination: an eliminator meeting the constructor it
//! takes apart, at every leaf of what it examines.
//!
//! The shape this exists for is `x.value_or_err(e)?`, which inlines to
//! `match (x) { some(v) => ok(v), none => err(e) }?`. Compiled as written, the
//! `match` builds a result value — a `{tag, payload}` on the stack — in *both*
//! arms, and the `?` then reads the tag straight back and copies the payload out
//! again: a construction and a test that exist only to hand a value from one arm
//! to the line after the `match`. The hand-written form this replaced,
//! `match (x) { some(v) => v, none => { return err(e); } }`, never built that
//! value, which is why the rewrite to `value_or_err` cost ~100 bytes per site
//! until this pass.
//!
//! The fix is the same for every such pair: move the eliminator to where the
//! answer is decided. Each *eliminator* — a `?`, or a `match` on an optional or
//! result — is pushed through everything that merely carries its operand's
//! value (a `let`/`mut`/`set` chain, a `;` sequence, an `if`, a `match`) to the
//! leaves, and where a leaf is a *known constructor* the pair cancels:
//!
//! - `ok(v)?` is `v`, and `some(v)?` is `v` — nothing to test, the tag is a
//!   literal. `err(e)?` and `none?` stay as the early return `?` always was,
//!   now taken from inside the branch (codegen knows a literal under `?` and
//!   emits no test for it either).
//! - `match (some(p)) { some(v) => body, .. }` is `let v = p; body`, and
//!   `match (none) { none => body, .. }` is `body`: the arm the leaf selects,
//!   with its binder bound to the leaf's payload.
//!
//! For that to be a rewrite of the *whole* expression, every branch the
//! eliminator reaches has to end in a constructor it knows: then each leaf
//! yields the payload, leaves, or runs its arm, and the wrapper is gone. A
//! branch that ends in anything else (a call returning a result, a binding
//! holding one) blocks the push for the whole expression — the eliminator stays
//! where it was, since pushing it down there would only duplicate the test at
//! every leaf.
//!
//! Pushing a `match` duplicates its arms: an arm is copied to every leaf that
//! selects it. `?` has nothing to copy, but a `match` is admitted only while the
//! copies stay small (`max_duplicated`, the same threshold that admits a small
//! function to inlining), and never when a binding the scrutinee introduces
//! would capture a name an arm reads — the arm would move into that binding's
//! scope, and inlined bodies keep their own `let` names.
//!
//! The walk never enters a loop, a lambda, or a shim body, whose value is not
//! the tail's. Post-mono only: the shapes are created by inlining the
//! AIPL-implemented builtins (`value_or_err`, `value_or`, `is_some_and`,
//! `map_ok`, `map_err`, `try_map`), which mono instantiates and the post-mono
//! inliner folds into their callers; by then `if let` has been lowered to
//! `match` and every pattern is a constructor, a binding, or a wildcard. A
//! user's own `f(x)?` where `f`'s body ends in `ok(..)` and was inlined pre-mono
//! is caught at the same time, since one walk sees the whole program.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Expr, ExprKind, MatchArm, Pattern};

use crate::subst::read_names;
use crate::{body_size, ConcreteFn, MonoProgram};

/// Eliminate every known-constructor pair in `program`. `max_duplicated` bounds
/// how much arm text a pushed `match` may copy (see the module docs).
pub fn eliminate_known_constructors_post_mono(
    program: &MonoProgram,
    max_duplicated: usize,
) -> MonoProgram {
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| ConcreteFn {
                body: rewrite(&f.body, max_duplicated),
                ..f.clone()
            })
            .collect(),
        ..program.clone()
    }
}

/// Rebuild `e` with its children rewritten first, then rewrite `e` itself.
/// Bottom-up, so an eliminator whose operand only took the shape by an inner
/// rewrite (an `ok(..)?` inside an arm, say) is pushed too.
fn rewrite(e: &Expr, max_duplicated: usize) -> Expr {
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = rewrite(child, max_duplicated);
    }
    match &out.kind {
        ExprKind::Try(inner) if ends_in_constructor(inner) => push_try(inner, &out),
        ExprKind::Match(scrutinee, arms)
            if ends_in_constructor(scrutinee)
                && match_can_move(scrutinee, arms, max_duplicated) =>
        {
            push_match(scrutinee, arms, &out)
        }
        _ => out,
    }
}

/// The constructor a leaf is, if it is one this pass knows how to take apart:
/// `ok(v)`/`ok()`, `err(e)`, `some(v)`, or the `none` literal. The payload is
/// `None` for the nullary forms.
enum Known<'a> {
    Ok(Option<&'a Expr>),
    Err(&'a Expr),
    Some(&'a Expr),
    None,
}

impl Known<'_> {
    /// The constructor name a [`Pattern::Ctor`] matches this leaf by.
    fn name(&self) -> &'static str {
        match self {
            Known::Ok(_) => "ok",
            Known::Err(_) => "err",
            Known::Some(_) => "some",
            Known::None => "none",
        }
    }

    fn payload(&self) -> Option<&Expr> {
        match self {
            Known::Ok(p) => *p,
            Known::Err(p) | Known::Some(p) => Some(p),
            Known::None => None,
        }
    }
}

fn known(e: &Expr) -> Option<Known<'_>> {
    Some(match &e.kind {
        ExprKind::Call(Callee::Ok, args, _) => match &args[..] {
            [] => Known::Ok(None),
            [v] => Known::Ok(Some(v)),
            _ => return None,
        },
        ExprKind::Call(Callee::Err, args, _) => match &args[..] {
            [v] => Known::Err(v),
            _ => return None,
        },
        ExprKind::Call(Callee::Some, args, _) => match &args[..] {
            [v] => Known::Some(v),
            _ => return None,
        },
        ExprKind::None => Known::None,
        _ => return None,
    })
}

/// Whether every branch of `e` ends in a constructor this pass knows — the
/// condition under which pushing an eliminator through it removes the
/// constructed value rather than duplicating the test.
fn ends_in_constructor(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Let(_, _, _, body)
        | ExprKind::LetMut(_, _, _, body)
        | ExprKind::Assign(_, _, body)
        | ExprKind::Seq(_, body) => ends_in_constructor(body),
        ExprKind::If(_, t, f) => ends_in_constructor(t) && ends_in_constructor(f),
        ExprKind::Match(_, arms) => arms.iter().all(|a| ends_in_constructor(&a.body)),
        _ => known(e).is_some(),
    }
}

/// Rebuild the value-carrying structure of `e` — which [`ends_in_constructor`]
/// — with `at_leaf` applied to each constructor it ends in. `like` lends its
/// span (and type stamp) to every rebuilt node: it is the eliminator being
/// pushed, whose position is what a diagnostic should still point at, and
/// whose type — the payload's, not the constructed value's — is what the
/// rebuilt nodes now have.
fn push(e: &Expr, like: &Expr, at_leaf: &mut dyn FnMut(&Expr, Known<'_>) -> Expr) -> Expr {
    let kind = match &e.kind {
        ExprKind::Let(n, ty, v, body) => ExprKind::Let(
            n.clone(),
            ty.clone(),
            v.clone(),
            Box::new(push(body, like, at_leaf)),
        ),
        ExprKind::LetMut(n, ty, v, body) => ExprKind::LetMut(
            n.clone(),
            ty.clone(),
            v.clone(),
            Box::new(push(body, like, at_leaf)),
        ),
        ExprKind::Assign(lhs, v, body) => {
            ExprKind::Assign(lhs.clone(), v.clone(), Box::new(push(body, like, at_leaf)))
        }
        ExprKind::Seq(a, body) => ExprKind::Seq(a.clone(), Box::new(push(body, like, at_leaf))),
        ExprKind::If(c, t, f) => ExprKind::If(
            c.clone(),
            Box::new(push(t, like, at_leaf)),
            Box::new(push(f, like, at_leaf)),
        ),
        ExprKind::Match(scrutinee, arms) => ExprKind::Match(
            scrutinee.clone(),
            arms.iter()
                .map(|a| MatchArm {
                    body: push(&a.body, like, at_leaf),
                    ..a.clone()
                })
                .collect(),
        ),
        _ => {
            let leaf = known(e).expect("`push` is only called where `ends_in_constructor` holds");
            return at_leaf(e, leaf);
        }
    };
    Expr::rebuilt(kind, like)
}

/// `inner?` with the `?` applied at each constructor `inner` ends in: `ok(v)`
/// and `some(v)` become `v` (`ok()` becomes unit), `err(e)` and `none` keep
/// the `?` — the early return, now taken from inside the branch. `try_at` is
/// the original `?` expression.
fn push_try(inner: &Expr, try_at: &Expr) -> Expr {
    push(inner, try_at, &mut |leaf, kind| match kind {
        Known::Ok(Some(v)) | Known::Some(v) => v.clone(),
        Known::Ok(None) => Expr::rebuilt(ExprKind::Unit, try_at),
        Known::Err(_) | Known::None => Expr::rebuilt(ExprKind::Try(Box::new(leaf.clone())), try_at),
    })
}

/// The arm of `arms` a leaf constructor named `name` selects: the first whose
/// pattern names it, or the first catch-all. Post-mono every pattern is one of
/// these three, and the `match` is exhaustive, so a leaf always finds one.
fn arm_for<'a>(arms: &'a [MatchArm], name: &str) -> Option<&'a MatchArm> {
    arms.iter().find(|a| match &a.pattern {
        Pattern::Ctor { name: n, .. } => n == name,
        Pattern::Wildcard | Pattern::Bind(_) => true,
        _ => false,
    })
}

/// Whether pushing `arms` into `scrutinee` is both possible and worth it: every
/// leaf selects an arm whose pattern this pass can bind, the copies stay within
/// `max_duplicated`, and no binding the scrutinee introduces captures a name an
/// arm reads.
fn match_can_move(scrutinee: &Expr, arms: &[MatchArm], max_duplicated: usize) -> bool {
    let mut leaves = Vec::new();
    leaf_names(scrutinee, &mut leaves);
    let mut copied = 0usize;
    let mut used: Vec<&MatchArm> = Vec::new();
    for name in &leaves {
        let Some(arm) = arm_for(arms, name) else {
            return false;
        };
        // A constructor pattern binds at most its one payload slot (these are
        // the optional and result constructors, which carry one value).
        if matches!(&arm.pattern, Pattern::Ctor { bindings, .. } if bindings.len() > 1) {
            return false;
        }
        if used.iter().any(|u| std::ptr::eq(*u, arm)) {
            copied += body_size(&arm.body);
        } else {
            used.push(arm);
        }
    }
    if copied > max_duplicated {
        return false;
    }
    let mut read = HashSet::new();
    for arm in arms {
        read_names(&arm.body, &mut read);
    }
    let mut bound = HashSet::new();
    bound_names(scrutinee, &mut bound);
    bound.is_disjoint(&read)
}

/// The constructor names of every leaf of `e` (which [`ends_in_constructor`]),
/// one per leaf.
fn leaf_names(e: &Expr, out: &mut Vec<&'static str>) {
    match &e.kind {
        ExprKind::Let(_, _, _, body)
        | ExprKind::LetMut(_, _, _, body)
        | ExprKind::Assign(_, _, body)
        | ExprKind::Seq(_, body) => leaf_names(body, out),
        ExprKind::If(_, t, f) => {
            leaf_names(t, out);
            leaf_names(f, out);
        }
        ExprKind::Match(_, arms) => {
            for a in arms {
                leaf_names(&a.body, out);
            }
        }
        _ => out.push(known(e).expect("a leaf is a known constructor").name()),
    }
}

/// Every name a binding or pattern anywhere in `e` introduces — the names an
/// arm body pushed into `e` would find in scope.
fn bound_names(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) => {
            out.insert(n.clone());
        }
        ExprKind::Match(_, arms) => {
            for a in arms {
                for b in a.pattern.bindings() {
                    out.insert(b);
                }
            }
        }
        _ => {}
    }
    for c in crate::children(e) {
        bound_names(c, out);
    }
}

/// `match (scrutinee) { arms }` with the `match` applied at each constructor
/// `scrutinee` ends in: the leaf's arm, with its binder (if any) bound to the
/// leaf's payload. A payload the arm does not bind is still evaluated, for
/// whatever it does. `match_at` is the original `match` expression.
fn push_match(scrutinee: &Expr, arms: &[MatchArm], match_at: &Expr) -> Expr {
    push(scrutinee, match_at, &mut |_, kind| {
        let arm = arm_for(arms, kind.name()).expect("checked by `match_can_move`");
        let binder = match &arm.pattern {
            Pattern::Ctor { bindings, .. } => bindings.first().cloned(),
            Pattern::Bind(name) => Some(name.clone()),
            _ => None,
        };
        match (binder, kind.payload()) {
            (Some(name), Some(payload)) => Expr::rebuilt(
                ExprKind::Let(
                    name,
                    None,
                    Box::new(payload.clone()),
                    Box::new(arm.body.clone()),
                ),
                match_at,
            ),
            (None, Some(payload)) => Expr::rebuilt(
                ExprKind::Seq(Box::new(payload.clone()), Box::new(arm.body.clone())),
                match_at,
            ),
            // A `Bind` pattern on a nullary leaf would name a `none` it never
            // reads; binding the literal keeps the arm as written.
            (Some(name), None) => Expr::rebuilt(
                ExprKind::Let(
                    name,
                    None,
                    Box::new(Expr::rebuilt(ExprKind::None, match_at)),
                    Box::new(arm.body.clone()),
                ),
                match_at,
            ),
            (None, None) => arm.body.clone(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: ExprKind) -> Expr {
        Expr::new(kind, 0..0)
    }

    fn id(n: &str) -> Expr {
        e(ExprKind::Ident(n.into()))
    }

    fn call(c: Callee, args: Vec<Expr>) -> Expr {
        e(ExprKind::Call(c, args, false))
    }

    fn arm(name: &str, bindings: &[&str], body: Expr) -> MatchArm {
        MatchArm {
            pattern: Pattern::Ctor {
                name: name.into(),
                bindings: bindings.iter().map(|b| (*b).to_string()).collect(),
                ignore_payload: false,
            },
            body,
            span: 0..0,
        }
    }

    fn optional_branch(then: Expr, otherwise: Expr) -> Expr {
        e(ExprKind::If(
            Box::new(id("c")),
            Box::new(then),
            Box::new(otherwise),
        ))
    }

    #[test]
    fn try_on_result_arms_unwraps_ok_and_keeps_err() {
        let inlined = e(ExprKind::Match(
            Box::new(id("o")),
            vec![
                arm("some", &["v"], call(Callee::Ok, vec![id("v")])),
                arm("none", &[], call(Callee::Err, vec![id("e")])),
            ],
        ));
        let out = rewrite(&e(ExprKind::Try(Box::new(inlined))), 8);
        let ExprKind::Match(_, arms) = &out.kind else {
            panic!("the `?` should have been pushed into the match, got {out:?}");
        };
        assert!(matches!(&arms[0].body.kind, ExprKind::Ident(v) if v == "v"));
        assert!(matches!(
            &arms[1].body.kind,
            ExprKind::Try(inner) if matches!(&inner.kind, ExprKind::Call(Callee::Err, _, _))
        ));
    }

    #[test]
    fn try_on_optional_branches_unwraps_some_and_keeps_none() {
        let branch = optional_branch(call(Callee::Some, vec![id("x")]), e(ExprKind::None));
        let out = rewrite(&e(ExprKind::Try(Box::new(branch))), 8);
        let ExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the `?` inside the `if`, got {out:?}");
        };
        assert!(matches!(&t.kind, ExprKind::Ident(v) if v == "x"));
        assert!(matches!(&f.kind, ExprKind::Try(inner) if matches!(inner.kind, ExprKind::None)));
    }

    #[test]
    fn a_branch_that_is_not_a_constructor_keeps_the_eliminator_outside() {
        let mixed = e(ExprKind::Match(
            Box::new(id("o")),
            vec![
                arm("some", &["v"], call(Callee::Ok, vec![id("v")])),
                arm("none", &[], call(Callee::User("fallback".into()), vec![])),
            ],
        ));
        let out = rewrite(&e(ExprKind::Try(Box::new(mixed))), 8);
        assert!(matches!(out.kind, ExprKind::Try(_)));
    }

    #[test]
    fn the_push_runs_through_bindings_ahead_of_the_tail() {
        let inner = e(ExprKind::Let(
            "e".into(),
            None,
            Box::new(id("msg")),
            Box::new(call(Callee::Ok, vec![])),
        ));
        let out = rewrite(&e(ExprKind::Try(Box::new(inner))), 8);
        let ExprKind::Let(_, _, _, body) = &out.kind else {
            panic!("expected the let to survive, got {out:?}");
        };
        assert!(matches!(body.kind, ExprKind::Unit));
    }

    #[test]
    fn match_on_optional_branches_binds_the_payload_per_leaf() {
        // `value_or` inlined over `if (c) { some(x) } else { none }`.
        let scrutinee = optional_branch(call(Callee::Some, vec![id("x")]), e(ExprKind::None));
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], id("v")), arm("none", &[], id("d"))],
        ));
        let out = rewrite(&matched, 8);
        let ExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the match pushed into the `if`, got {out:?}");
        };
        let ExprKind::Let(name, _, payload, body) = &t.kind else {
            panic!("expected `let v = x; v`, got {t:?}");
        };
        assert_eq!(name, "v");
        assert!(matches!(&payload.kind, ExprKind::Ident(x) if x == "x"));
        assert!(matches!(&body.kind, ExprKind::Ident(v) if v == "v"));
        assert!(matches!(&f.kind, ExprKind::Ident(d) if d == "d"));
    }

    #[test]
    fn a_match_whose_copies_would_be_too_large_stays() {
        let scrutinee = optional_branch(
            call(Callee::Some, vec![id("x")]),
            optional_branch(call(Callee::Some, vec![id("y")]), e(ExprKind::None)),
        );
        let big = call(
            Callee::User("f".into()),
            vec![call(Callee::User("g".into()), vec![id("v")])],
        );
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], big), arm("none", &[], id("d"))],
        ));
        // Two `some` leaves select the same two-operation arm: one copy of it.
        assert!(matches!(rewrite(&matched, 1).kind, ExprKind::Match(..)));
        assert!(matches!(rewrite(&matched, 2).kind, ExprKind::If(..)));
    }

    #[test]
    fn a_scrutinee_binding_an_arm_reads_blocks_the_move() {
        let scrutinee = e(ExprKind::Let(
            "d".into(),
            None,
            Box::new(id("inner")),
            Box::new(optional_branch(
                call(Callee::Some, vec![id("d")]),
                e(ExprKind::None),
            )),
        ));
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], id("v")), arm("none", &[], id("d"))],
        ));
        assert!(matches!(rewrite(&matched, 8).kind, ExprKind::Match(..)));
    }
}
