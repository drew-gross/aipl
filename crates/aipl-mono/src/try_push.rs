//! `?` pushed into the branches that build what it unwraps.
//!
//! `value_or_err` is `match (self) { some(v) => ok(v), none => err(e) }`, and
//! `x.value_or_err(e)?` inlines to that `match` with a `?` after it. Compiled as
//! written, the `match` builds a result value — a `{tag, payload}` on the stack
//! — in *both* arms, and the `?` then reads the tag straight back and copies
//! the payload out again: a construction and a test that exist only to hand a
//! value from one arm to the line after the `match`. The hand-written form this
//! replaced, `match (x) { some(v) => v, none => { return err(e); } }`, never
//! built that value, which is why the rewrite to `value_or_err` cost ~100 bytes
//! per site until this pass.
//!
//! So the `?` moves to where the answer is decided. `ok(v)?` is `v` — nothing to
//! test, since the tag is a literal — and `err(e)?` is the early return `?`
//! always was, now taken from inside the arm. For that to be a rewrite of the
//! *whole* expression, every branch the `?` reaches has to end in one of the two
//! constructors: then each arm yields the payload or leaves, exactly as the
//! hand-written `match` did, and the wrapper is gone. A branch that ends in
//! anything else (a call returning a result, a binding holding one) is left
//! alone, `?` and all — pushing the `?` down there would only duplicate the
//! test at every leaf.
//!
//! The push runs through everything that merely *carries* its tail's value: a
//! `let`/`mut`/`set` chain, a `;` sequence, `if`, `if let`, `match`. It never
//! enters a loop, a lambda, or a shim body, whose value is not the tail's.
//!
//! Post-mono only: the shape is created by inlining the AIPL-implemented
//! builtins (`value_or_err`, `map_ok`, `map_err`, `try_map`), which mono
//! instantiates and the post-mono inliner folds into their callers. A user's
//! own `f(x)?` where `f`'s body ends in `ok(..)` and was inlined pre-mono is
//! caught at the same time, since one walk sees the whole program.

use aipl_syntax::ast::{Callee, Expr, ExprKind, MatchArm};

use crate::{ConcreteFn, MonoProgram};

/// Push every `?` in `program` into the constructor-ending branches under it.
/// See the module docs.
pub fn push_try_post_mono(program: &MonoProgram) -> MonoProgram {
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| ConcreteFn {
                body: rewrite(&f.body),
                ..f.clone()
            })
            .collect(),
        ..program.clone()
    }
}

/// Rebuild `e` with its children rewritten first, then rewrite `e` itself.
/// Bottom-up, so a `?` whose operand only took the shape by an inner rewrite
/// (an `ok(..)?` inside an arm, say) is pushed too.
fn rewrite(e: &Expr) -> Expr {
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = rewrite(child);
    }
    let ExprKind::Try(inner) = &out.kind else {
        return out;
    };
    if !ends_in_ctor(inner) {
        return out;
    }
    push(inner, &out)
}

/// Whether every branch of `e` ends in an `ok(..)` or `err(..)` construction —
/// the condition under which pushing a `?` through it removes the result value
/// rather than duplicating the test.
fn ends_in_ctor(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call(Callee::Ok, args, _) => args.len() <= 1,
        ExprKind::Call(Callee::Err, args, _) => args.len() == 1,
        ExprKind::Let(_, _, _, body)
        | ExprKind::LetMut(_, _, _, body)
        | ExprKind::Assign(_, _, body)
        | ExprKind::Seq(_, body) => ends_in_ctor(body),
        ExprKind::If(_, t, f) | ExprKind::IfLet(_, t, f) => ends_in_ctor(t) && ends_in_ctor(f),
        ExprKind::Match(_, arms) => arms.iter().all(|a| ends_in_ctor(&a.body)),
        _ => false,
    }
}

/// `e?` with the `?` applied at each constructor `e` ends in — `ok(v)` becomes
/// `v`, `err(x)` becomes `err(x)?` — and everything on the way there rebuilt
/// around the pushed form. `try_at` is the original `?` expression, whose span
/// each pushed `?` keeps so a diagnostic still points at the operator written.
/// Only called on an `e` that [`ends_in_ctor`].
fn push(e: &Expr, try_at: &Expr) -> Expr {
    let kind = match &e.kind {
        ExprKind::Call(Callee::Ok, args, _) => match &args[..] {
            [v] => return v.clone(),
            _ => ExprKind::Unit,
        },
        ExprKind::Call(Callee::Err, _, _) => {
            return Expr::rebuilt(ExprKind::Try(Box::new(e.clone())), try_at);
        }
        ExprKind::Let(n, ty, v, body) => ExprKind::Let(
            n.clone(),
            ty.clone(),
            v.clone(),
            Box::new(push(body, try_at)),
        ),
        ExprKind::LetMut(n, ty, v, body) => ExprKind::LetMut(
            n.clone(),
            ty.clone(),
            v.clone(),
            Box::new(push(body, try_at)),
        ),
        ExprKind::Assign(lhs, v, body) => {
            ExprKind::Assign(lhs.clone(), v.clone(), Box::new(push(body, try_at)))
        }
        ExprKind::Seq(a, body) => ExprKind::Seq(a.clone(), Box::new(push(body, try_at))),
        ExprKind::If(c, t, f) => ExprKind::If(
            c.clone(),
            Box::new(push(t, try_at)),
            Box::new(push(f, try_at)),
        ),
        ExprKind::IfLet(arm, t, f) => ExprKind::IfLet(
            arm.clone(),
            Box::new(push(t, try_at)),
            Box::new(push(f, try_at)),
        ),
        ExprKind::Match(scrut, arms) => ExprKind::Match(
            scrut.clone(),
            arms.iter()
                .map(|a| MatchArm {
                    body: push(&a.body, try_at),
                    ..a.clone()
                })
                .collect(),
        ),
        _ => unreachable!("`push` is only called where `ends_in_ctor` holds"),
    };
    // The pushed form has the tail's *payload* type, not its result type, so
    // the type mono stamped on the wrapper no longer describes it; the `?`'s
    // own stamp is the one that does.
    Expr::rebuilt(kind, try_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aipl_syntax::ast::Pattern;

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

    #[test]
    fn ok_loses_its_wrapper_and_err_keeps_the_try() {
        let inlined = e(ExprKind::Match(
            Box::new(id("o")),
            vec![
                arm("some", &["v"], call(Callee::Ok, vec![id("v")])),
                arm("none", &[], call(Callee::Err, vec![id("e")])),
            ],
        ));
        let out = rewrite(&e(ExprKind::Try(Box::new(inlined))));
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
    fn a_branch_that_is_not_a_constructor_keeps_the_try_outside() {
        let mixed = e(ExprKind::Match(
            Box::new(id("o")),
            vec![
                arm("some", &["v"], call(Callee::Ok, vec![id("v")])),
                arm("none", &[], call(Callee::User("fallback".into()), vec![])),
            ],
        ));
        let out = rewrite(&e(ExprKind::Try(Box::new(mixed))));
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
        let out = rewrite(&e(ExprKind::Try(Box::new(inner))));
        let ExprKind::Let(_, _, _, body) = &out.kind else {
            panic!("expected the let to survive, got {out:?}");
        };
        assert!(matches!(body.kind, ExprKind::Unit));
    }
}
