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
//! answer is decided. Each *eliminator* is pushed through everything that
//! merely carries its operand's value (a `let`/`mut`/`set` chain, a `;`
//! sequence, an `if`, a `match`) to the leaves, and where a leaf is a *known
//! constructor* the pair cancels. Three eliminators, four constructor
//! families:
//!
//! - `?` over the optional and result constructors: `ok(v)?` and `some(v)?`
//!   are `v` — nothing to test, the tag is a literal — while `err(e)?` and
//!   `none?` stay as the early return `?` always was, now taken from inside
//!   the branch (codegen knows a literal under `?` and emits no test for it
//!   either).
//! - `match` over the optional and result constructors and over a variant's
//!   cases: `match (some(p)) { some(v) => body, .. }` is `let v = p; body`,
//!   `match (Pair(a, b)) { Pair(x, y) => body, .. }` is
//!   `let x = a; let y = b; body` — the arm the leaf selects, with its binders
//!   bound to the leaf's payloads in order.
//! - Field access over a struct construction: `(S { a: x, b: y }).a` is `x`.
//!   The other fields are never built, so the retain the construction gave
//!   each of them and the release the struct's drop paid back are both gone
//!   — unless a field does something (an effectful call, an `assert`), in
//!   which case it is kept as a statement ahead of the projected value, in
//!   its original order.
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
//! selects it. `?` and a field access have nothing to copy, but a `match` is
//! admitted only while the copies stay small (`max_duplicated`, the same
//! threshold that admits a small function to inlining), and never when a
//! binding the scrutinee introduces would capture a name an arm reads — the
//! arm would move into that binding's scope, and inlined bodies keep their own
//! `let` names. A leaf whose payloads read one of the arm's own binders is
//! refused for the same reason: the binders are bound one after another, so a
//! later payload would see an earlier binder instead of the name it meant.
//!
//! The walk never enters a loop, a lambda, or a shim body, whose value is not
//! the tail's. Post-mono only: the shapes are created by inlining — the
//! AIPL-implemented builtins (`value_or_err`, `value_or`, `is_some_and`,
//! `map_ok`, `map_err`, `try_map`), which mono instantiates and the post-mono
//! inliner folds into their callers, and any small function that builds a
//! struct or variant its caller immediately takes apart. By then `if let` has
//! been lowered to `match`, every pattern is a constructor, a binding, or a
//! wildcard, and an ignored payload is a `_` binder per slot. A user's own
//! `f(x)?` where `f`'s body ends in `ok(..)` and was inlined pre-mono is caught
//! at the same time, since one walk sees the whole program.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Expr, ExprKind, FieldInit, MatchArm, Pattern};

use crate::sink::{can_defer, undeferrable_fns_post_mono};
use crate::subst::read_names;
use crate::{body_size, ConcreteFn, MonoProgram};

/// Eliminate every known-constructor pair in `program`. `max_duplicated` bounds
/// how much arm text a pushed `match` may copy; `builtin_effects` is the effect
/// declarations under their pre-mono names (the set
/// [`crate::sink_bindings_post_mono`] takes), which decides whether a field a
/// projection discards can be dropped or must be kept for what it does. See
/// the module docs.
pub fn eliminate_known_constructors_post_mono(
    program: &MonoProgram,
    max_duplicated: usize,
    builtin_effects: &HashSet<String>,
) -> MonoProgram {
    let limits = Limits {
        max_duplicated,
        blocked: undeferrable_fns_post_mono(program, builtin_effects),
    };
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| ConcreteFn {
                body: rewrite(&f.body, &limits),
                ..f.clone()
            })
            .collect(),
        ..program.clone()
    }
}

/// What holds a push back: the arm-copy budget, and the functions whose call
/// cannot be dropped or deferred (see [`can_defer`]).
struct Limits {
    max_duplicated: usize,
    blocked: HashSet<String>,
}

/// Rebuild `e` with its children rewritten first, then rewrite `e` itself.
/// Bottom-up, so an eliminator whose operand only took the shape by an inner
/// rewrite (an `ok(..)?` inside an arm, a projection of a projection) is
/// pushed too.
fn rewrite(e: &Expr, limits: &Limits) -> Expr {
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = rewrite(child, limits);
    }
    match &out.kind {
        ExprKind::Try(inner) if ends_in(inner, is_optional_or_result) => push_try(inner, &out),
        ExprKind::Match(scrutinee, arms)
            if ends_in(scrutinee, is_matchable) && match_can_move(scrutinee, arms, limits) =>
        {
            push_match(scrutinee, arms, &out)
        }
        ExprKind::Field(base, field)
            if ends_in(base, is_struct) && field_can_move(base, field, limits) =>
        {
            push_field(base, field, &out, limits)
        }
        _ => out,
    }
}

/// The constructor a leaf is, if it is one this pass knows how to take apart.
enum Known<'a> {
    /// `ok(v)` or `ok()`.
    Ok(Option<&'a Expr>),
    /// `err(e)`.
    Err(&'a Expr),
    /// `some(v)`.
    Some(&'a Expr),
    /// The `none` literal.
    None,
    /// A variant case, `Case(a, b)` or the nullary `Case`, named as the loader
    /// qualifies it (`Case@Variant`).
    Variant {
        name: &'a str,
        payloads: Vec<&'a Expr>,
    },
    /// A struct construction, `S { a: x, b: y }`.
    Struct(&'a [FieldInit]),
}

/// The constructor families each eliminator takes apart.
fn is_optional_or_result(k: &Known<'_>) -> bool {
    matches!(
        k,
        Known::Ok(_) | Known::Err(_) | Known::Some(_) | Known::None
    )
}

fn is_matchable(k: &Known<'_>) -> bool {
    is_optional_or_result(k) || matches!(k, Known::Variant { .. })
}

fn is_struct(k: &Known<'_>) -> bool {
    matches!(k, Known::Struct(_))
}

impl Known<'_> {
    /// The bare case name a [`Pattern::Ctor`] matches this leaf by — for a
    /// variant case, without its `@Variant` qualifier, which the scrutinee's
    /// type already fixes (codegen compares the same way).
    fn case_name(&self) -> &str {
        match self {
            Known::Ok(_) => "ok",
            Known::Err(_) => "err",
            Known::Some(_) => "some",
            Known::None => "none",
            Known::Variant { name, .. } => name.split('@').next().unwrap_or(name),
            Known::Struct(_) => "",
        }
    }

    /// The payload expressions, in slot order.
    fn payloads(&self) -> Vec<&Expr> {
        match self {
            Known::Ok(p) => p.iter().copied().collect(),
            Known::Err(p) | Known::Some(p) => vec![p],
            Known::None => Vec::new(),
            Known::Variant { payloads, .. } => payloads.clone(),
            Known::Struct(fields) => fields.iter().map(|f| &f.value).collect(),
        }
    }
}

/// Whether `name` is a variant constructor reference: the loader qualifies
/// every one as `Case@Variant`, and nothing else can contain an `@`.
fn is_variant_constructor(name: &str) -> bool {
    name.contains('@')
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
        ExprKind::Call(Callee::User(name), args, false) if is_variant_constructor(name) => {
            Known::Variant {
                name,
                payloads: args.iter().collect(),
            }
        }
        ExprKind::Ident(name) if is_variant_constructor(name) => Known::Variant {
            name,
            payloads: Vec::new(),
        },
        ExprKind::Construct(_, fields) => Known::Struct(fields),
        _ => return None,
    })
}

/// Whether every branch of `e` ends in a constructor `accepts` — the condition
/// under which pushing an eliminator through it removes the constructed value
/// rather than duplicating the test.
fn ends_in(e: &Expr, accepts: fn(&Known<'_>) -> bool) -> bool {
    match &e.kind {
        ExprKind::Let(_, _, _, body)
        | ExprKind::LetMut(_, _, _, body)
        | ExprKind::Assign(_, _, body)
        | ExprKind::Seq(_, body) => ends_in(body, accepts),
        ExprKind::If(_, t, f) => ends_in(t, accepts) && ends_in(f, accepts),
        ExprKind::Match(_, arms) => arms.iter().all(|a| ends_in(&a.body, accepts)),
        _ => known(e).is_some_and(|k| accepts(&k)),
    }
}

/// The leaves of `e` (which [`ends_in`] some constructor family), in order.
fn leaves<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match &e.kind {
        ExprKind::Let(_, _, _, body)
        | ExprKind::LetMut(_, _, _, body)
        | ExprKind::Assign(_, _, body)
        | ExprKind::Seq(_, body) => leaves(body, out),
        ExprKind::If(_, t, f) => {
            leaves(t, out);
            leaves(f, out);
        }
        ExprKind::Match(_, arms) => {
            for a in arms {
                leaves(&a.body, out);
            }
        }
        _ => out.push(e),
    }
}

/// Rebuild the value-carrying structure of `e` — which [`ends_in`] the
/// constructors `at_leaf` handles — with `at_leaf` applied to each. `like`
/// lends its span (and type stamp) to every rebuilt node: it is the eliminator
/// being pushed, whose position is what a diagnostic should still point at, and
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
            let leaf = known(e).expect("`push` is only called where `ends_in` holds");
            return at_leaf(e, leaf);
        }
    };
    Expr::rebuilt(kind, like)
}

/// `body`, preceded by the evaluation of each of `payloads` that does
/// something — one this pass cannot drop (see [`can_defer`]) — in their
/// original order. A payload that only computes a value is left out: the
/// constructor would have owned it, and nothing else ever looked at it.
fn keep_effects(payloads: &[&Expr], body: Expr, like: &Expr, limits: &Limits) -> Expr {
    payloads
        .iter()
        .rev()
        .filter(|p| !can_defer(p, &limits.blocked))
        .fold(body, |rest, p| {
            Expr::rebuilt(ExprKind::Seq(Box::new((*p).clone()), Box::new(rest)), like)
        })
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
        Known::Variant { .. } | Known::Struct(_) => {
            unreachable!("`?` only ever meets optional and result constructors")
        }
    })
}

/// The arm of `arms` a leaf constructor named `case` selects: the first whose
/// pattern names it, or the first catch-all. Post-mono every pattern is one of
/// these three, and the `match` is exhaustive, so a leaf always finds one.
fn arm_for<'a>(arms: &'a [MatchArm], case: &str) -> Option<&'a MatchArm> {
    arms.iter().find(|a| match &a.pattern {
        Pattern::Ctor { name, .. } => name.split('@').next() == Some(case),
        Pattern::Wildcard | Pattern::Bind(_) => true,
        _ => false,
    })
}

/// Whether pushing `arms` into `scrutinee` is both possible and worth it: every
/// leaf selects an arm whose binders line up with its payloads, no payload
/// reads one of those binders, the copies stay within `max_duplicated`, and no
/// binding the scrutinee introduces captures a name an arm reads.
fn match_can_move(scrutinee: &Expr, arms: &[MatchArm], limits: &Limits) -> bool {
    let mut ends = Vec::new();
    leaves(scrutinee, &mut ends);
    let mut copied = 0usize;
    let mut used: Vec<&MatchArm> = Vec::new();
    for leaf in ends {
        let kind = known(leaf).expect("a leaf is a known constructor");
        let Some(arm) = arm_for(arms, kind.case_name()) else {
            return false;
        };
        let payloads = kind.payloads();
        if let Pattern::Ctor { bindings, .. } = &arm.pattern {
            if bindings.len() != payloads.len() {
                return false;
            }
            let mut read = HashSet::new();
            for p in &payloads {
                read_names(p, &mut read);
            }
            if bindings.iter().any(|b| read.contains(b)) {
                return false;
            }
        }
        if used.iter().any(|u| std::ptr::eq(*u, arm)) {
            copied += body_size(&arm.body);
        } else {
            used.push(arm);
        }
    }
    if copied > limits.max_duplicated {
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

/// Every name a binding or pattern anywhere in `e` introduces — the names an
/// arm body pushed into `e` would find in scope.
fn bound_names(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) => {
            out.insert(n.clone());
        }
        ExprKind::Match(_, arms) => {
            for a in arms {
                out.extend(a.pattern.bindings());
            }
        }
        _ => {}
    }
    for c in crate::children(e) {
        bound_names(c, out);
    }
}

/// `match (scrutinee) { arms }` with the `match` applied at each constructor
/// `scrutinee` ends in: the leaf's arm, with each binder bound to the payload
/// in its slot. A `_` binder binds nothing, and a payload no binder takes is
/// still evaluated for whatever it does. `match_at` is the original `match`
/// expression.
fn push_match(scrutinee: &Expr, arms: &[MatchArm], match_at: &Expr) -> Expr {
    push(scrutinee, match_at, &mut |leaf, kind| {
        let arm = arm_for(arms, kind.case_name()).expect("checked by `match_can_move`");
        let payloads = kind.payloads();
        let bind = |name: String, value: Expr, body: Expr| {
            Expr::rebuilt(
                ExprKind::Let(name, None, Box::new(value), Box::new(body)),
                match_at,
            )
        };
        let evaluate = |value: Expr, body: Expr| {
            Expr::rebuilt(ExprKind::Seq(Box::new(value), Box::new(body)), match_at)
        };
        match &arm.pattern {
            // Binders are bound first to last, so the body sees them all; the
            // payloads' own evaluation order is preserved the same way.
            Pattern::Ctor { bindings, .. } => bindings.iter().zip(payloads).rev().fold(
                arm.body.clone(),
                |body, (binder, payload)| {
                    if binder == "_" {
                        evaluate(payload.clone(), body)
                    } else {
                        bind(binder.clone(), payload.clone(), body)
                    }
                },
            ),
            // A catch-all binder names the whole constructed value, which is
            // therefore still built.
            Pattern::Bind(name) => bind(name.clone(), leaf.clone(), arm.body.clone()),
            _ => payloads
                .iter()
                .rev()
                .fold(arm.body.clone(), |body, payload| {
                    evaluate((*payload).clone(), body)
                }),
        }
    })
}

/// Whether projecting `field` out of every struct `base` ends in is possible:
/// each leaf has the field, and every other field can either be dropped (see
/// [`can_defer`]) or kept as a statement. The only thing refused is a field
/// that leaves the function from where it sits, which no statement placed
/// ahead of the projected value could reproduce.
fn field_can_move(base: &Expr, field: &str, limits: &Limits) -> bool {
    let mut ends = Vec::new();
    leaves(base, &mut ends);
    ends.iter().all(|leaf| {
        let Some(Known::Struct(fields)) = known(leaf) else {
            return false;
        };
        fields.iter().any(|f| f.name == field)
            && fields
                .iter()
                .filter(|f| f.name != field)
                .all(|f| can_defer(&f.value, &limits.blocked) || keepable(&f.value))
    })
}

/// Whether an expression can stand as a statement ahead of another value:
/// anything that does not transfer control out of the function, or write a
/// binding, from where it sits. (`can_defer` refuses those too, plus anything
/// effectful; here the effectful ones are exactly what a kept statement is
/// for.)
fn keepable(e: &Expr) -> bool {
    !matches!(
        &e.kind,
        ExprKind::Try(_) | ExprKind::Return(_) | ExprKind::Assign(..)
    ) && crate::children(e).iter().all(|c| keepable(c))
}

/// `base.field` with the projection applied at each struct `base` ends in:
/// the field's initializer, behind whichever other fields must still run.
fn push_field(base: &Expr, field: &str, field_at: &Expr, limits: &Limits) -> Expr {
    push(base, field_at, &mut |_, kind| {
        let Known::Struct(fields) = kind else {
            unreachable!("a field is only ever projected out of a struct")
        };
        let projected = fields
            .iter()
            .find(|f| f.name == field)
            .expect("checked by `field_can_move`");
        let others: Vec<&Expr> = fields
            .iter()
            .filter(|f| f.name != field)
            .map(|f| &f.value)
            .collect();
        keep_effects(&others, projected.value.clone(), field_at, limits)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_duplicated: usize) -> Limits {
        Limits {
            max_duplicated,
            blocked: HashSet::from(["loud".to_string()]),
        }
    }

    fn e(kind: ExprKind) -> Expr {
        Expr::new(kind, 0..0)
    }

    fn id(n: &str) -> Expr {
        e(ExprKind::Ident(n.into()))
    }

    fn call(c: Callee, args: Vec<Expr>) -> Expr {
        e(ExprKind::Call(c, args, false))
    }

    fn user(name: &str, args: Vec<Expr>) -> Expr {
        call(Callee::User(name.into()), args)
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

    fn branch(then: Expr, otherwise: Expr) -> Expr {
        e(ExprKind::If(
            Box::new(id("c")),
            Box::new(then),
            Box::new(otherwise),
        ))
    }

    fn construct(fields: &[(&str, Expr)]) -> Expr {
        e(ExprKind::Construct(
            "S".into(),
            fields
                .iter()
                .map(|(n, v)| FieldInit {
                    name: (*n).to_string(),
                    value: v.clone(),
                })
                .collect(),
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
        let out = rewrite(&e(ExprKind::Try(Box::new(inlined))), &limits(8));
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
        let optional = branch(call(Callee::Some, vec![id("x")]), e(ExprKind::None));
        let out = rewrite(&e(ExprKind::Try(Box::new(optional))), &limits(8));
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
                arm("none", &[], user("fallback", vec![])),
            ],
        ));
        let out = rewrite(&e(ExprKind::Try(Box::new(mixed))), &limits(8));
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
        let out = rewrite(&e(ExprKind::Try(Box::new(inner))), &limits(8));
        let ExprKind::Let(_, _, _, body) = &out.kind else {
            panic!("expected the let to survive, got {out:?}");
        };
        assert!(matches!(body.kind, ExprKind::Unit));
    }

    #[test]
    fn match_on_optional_branches_binds_the_payload_per_leaf() {
        // `value_or` inlined over `if (c) { some(x) } else { none }`.
        let scrutinee = branch(call(Callee::Some, vec![id("x")]), e(ExprKind::None));
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], id("v")), arm("none", &[], id("d"))],
        ));
        let out = rewrite(&matched, &limits(8));
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
        let scrutinee = branch(
            call(Callee::Some, vec![id("x")]),
            branch(call(Callee::Some, vec![id("y")]), e(ExprKind::None)),
        );
        let big = user("f", vec![user("g", vec![id("v")])]);
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], big), arm("none", &[], id("d"))],
        ));
        // Two `some` leaves select the same two-operation arm: one copy of it.
        assert!(matches!(
            rewrite(&matched, &limits(1)).kind,
            ExprKind::Match(..)
        ));
        assert!(matches!(
            rewrite(&matched, &limits(2)).kind,
            ExprKind::If(..)
        ));
    }

    #[test]
    fn a_scrutinee_binding_an_arm_reads_blocks_the_move() {
        let scrutinee = e(ExprKind::Let(
            "d".into(),
            None,
            Box::new(id("inner")),
            Box::new(branch(call(Callee::Some, vec![id("d")]), e(ExprKind::None))),
        ));
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("some", &["v"], id("v")), arm("none", &[], id("d"))],
        ));
        assert!(matches!(
            rewrite(&matched, &limits(8)).kind,
            ExprKind::Match(..)
        ));
    }

    #[test]
    fn match_on_variant_cases_binds_each_payload_slot() {
        let scrutinee = branch(user("Pair@V", vec![id("a"), id("b")]), id("Nil@V"));
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![
                arm("Pair@V", &["x", "_"], id("x")),
                arm("Nil@V", &[], id("d")),
            ],
        ));
        let out = rewrite(&matched, &limits(8));
        let ExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the match pushed into the `if`, got {out:?}");
        };
        // `let x = a; b; x` — the ignored slot is still evaluated.
        let ExprKind::Let(name, _, payload, body) = &t.kind else {
            panic!("expected `let x = a; ..`, got {t:?}");
        };
        assert_eq!(name, "x");
        assert!(matches!(&payload.kind, ExprKind::Ident(a) if a == "a"));
        assert!(matches!(&body.kind, ExprKind::Seq(first, rest)
            if matches!(&first.kind, ExprKind::Ident(b) if b == "b")
                && matches!(&rest.kind, ExprKind::Ident(x) if x == "x")));
        assert!(matches!(&f.kind, ExprKind::Ident(d) if d == "d"));
    }

    #[test]
    fn a_payload_reading_an_arm_binder_blocks_the_move() {
        // `Pair(x, x)` against `Pair(x, y)`: binding `x` first would make the
        // second payload read the binder instead of the outer `x`.
        let scrutinee = user("Pair@V", vec![id("x"), id("x")]);
        let matched = e(ExprKind::Match(
            Box::new(scrutinee),
            vec![arm("Pair@V", &["x", "y"], id("y"))],
        ));
        assert!(matches!(
            rewrite(&matched, &limits(8)).kind,
            ExprKind::Match(..)
        ));
    }

    #[test]
    fn field_of_a_construction_is_its_initializer() {
        let projected = e(ExprKind::Field(
            Box::new(branch(
                construct(&[("a", id("x")), ("b", user("pure", vec![]))]),
                construct(&[("a", id("y")), ("b", id("z"))]),
            )),
            "a".into(),
        ));
        let out = rewrite(&projected, &limits(8));
        let ExprKind::If(_, t, f) = &out.kind else {
            panic!("expected the projection pushed into the `if`, got {out:?}");
        };
        assert!(matches!(&t.kind, ExprKind::Ident(x) if x == "x"));
        assert!(matches!(&f.kind, ExprKind::Ident(y) if y == "y"));
    }

    #[test]
    fn a_dropped_field_that_does_something_is_kept_as_a_statement() {
        let projected = e(ExprKind::Field(
            Box::new(construct(&[("a", id("x")), ("b", user("loud", vec![]))])),
            "a".into(),
        ));
        let out = rewrite(&projected, &limits(8));
        assert!(matches!(&out.kind, ExprKind::Seq(first, rest)
            if matches!(&first.kind, ExprKind::Call(Callee::User(n), _, _) if n == "loud")
                && matches!(&rest.kind, ExprKind::Ident(x) if x == "x")));
    }

    #[test]
    fn a_field_that_leaves_the_function_blocks_the_move() {
        let projected = e(ExprKind::Field(
            Box::new(construct(&[
                ("a", id("x")),
                ("b", e(ExprKind::Try(Box::new(id("r"))))),
            ])),
            "a".into(),
        ));
        assert!(matches!(
            rewrite(&projected, &limits(8)).kind,
            ExprKind::Field(..)
        ));
    }
}
