//! `xs.map(f).map(g)` is `xs.map(|x| g(f(x)))`: the first `map` builds an array of
//! every `f(x)` for the second to walk once and drop, and the composed `map`
//! builds the final array alone. Mapping twice is the commonest way to build a
//! pipeline, and each stage after the first is an allocation and a pass the
//! program never asked for.
//!
//! This is not a row in [`super::chain_fusions`]' table, though it is a chain.
//! A row says "this call on that call is a *third* call taking both argument
//! lists", and the third call here is not one of the two: it is a `map` of a
//! function nobody wrote, built from theirs. And a row has a cost beyond its
//! shape. The inner `map` stands aside whenever its parent pairs with it under
//! some row, on the promise that the parent will fuse — a promise this family
//! cannot make, since it refuses whenever either function is not safe to move.
//! Standing aside then would also cost `xs.filter(p).map(f)` its own fusion into
//! `filter_map`. So this is tried before the table, on its own terms, and the
//! table never hears of it.
//!
//! Composing first is also what lets a longer chain finish the job:
//! `xs.map(f).map(g).join()` becomes `xs.map(g∘f).join()` at the second `map`,
//! before the `join` is reached, and then `map_join` at the `join`.
//!
//! # Building the function
//!
//! `|x| { let y = f(x); g(y) }`, with fresh names, `f` and `g` spliced in by the
//! same [`apply`] the loop family uses: a lambda becomes its body with the
//! parameter renamed, a bare name becomes a call. Neither body is duplicated,
//! and the intermediate is a binding rather than a nested call because `apply`
//! takes an identifier.
//!
//! # When it is refused
//!
//! The calls move: `f` runs on the second element after `g` has run on the first,
//! where the unfused program ran every `f` first. That is unobservable for
//! functions that do nothing observable and cannot abort, which is the bar the
//! loop family sets on a mapping function ([`is_pure`]) and this one sets on both.
//!
//! Not every `f` can be bound with a plain `let`: one whose value takes its type
//! from where it sits (`|x| none`, an empty `[]`) would lose it, so those are
//! refused ([`contains_context_literal`]).
//!
//! The receiver may be an array, an optional or a span — `map` takes all three —
//! and so may the result of the first, so nothing here depends on which. A set
//! receiver is an error in monomorphization either way, and composing leaves
//! the receiver where it was, so it is still reported.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Expr, ExprKind, LambdaParam};

use super::loop_fusions::{apply, is_pure};
use super::{through_bindings, under_bindings};
use crate::sink::mentions_free;

/// `xs.map(f).map(g)` as `xs.map(|x| { let y = f(x); g(y) })`, or `None` when
/// `whole` is not that shape or cannot be rewritten without changing what the
/// program does.
///
/// `blocked` is the loop family's bar: the functions whose call is observable or
/// may abort, closed over the call graph.
pub(super) fn build(whole: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let ExprKind::Call(Callee::Map, args, method_style) = &whole.kind else {
        return None;
    };
    let [receiver, g] = args.as_slice() else {
        return None;
    };
    // The inner `map` may sit under the `let`s an inlined call leaves in front
    // of its body; the composed call goes back beneath them, as for a chain.
    let (bindings, inner) = through_bindings(receiver);
    let ExprKind::Call(Callee::Map, inner_args, _) = &inner.kind else {
        return None;
    };
    let [source, f] = inner_args.as_slice() else {
        return None;
    };
    if bindings.iter().any(|b| mentions_free(g, b.name)) {
        return None;
    }
    if !is_pure(f, blocked) || !is_pure(g, blocked) {
        return None;
    }
    if matches!(&f.kind, ExprKind::Lambda(_, body) if crate::contains_context_literal(body)) {
        return None;
    }

    let id = aipl_syntax::fresh_id();
    let (x, y) = (format!("$compose{id}_x"), format!("$compose{id}_y"));
    let ident = |name: &str| Expr::new(ExprKind::Ident(name.to_string()), whole.span.clone());
    let f_of_x = apply(f, ident(&x))?;
    let g_of_y = apply(g, ident(&y))?;
    // The element's written type, if `f` wrote one, is the composed function's:
    // it is what the first `map` handed `f`. `g`'s is dropped — it described the
    // intermediate, which is now a binding the checker already typed.
    let param_ty = match &f.kind {
        ExprKind::Lambda(params, _) => params.first().and_then(|p| p.ty.clone()),
        _ => None,
    };
    let param = LambdaParam {
        name: x,
        ty: param_ty,
        span: f.span.clone(),
    };
    let body = Expr::rebuilt(
        ExprKind::Let(y, None, Box::new(f_of_x), Box::new(g_of_y)),
        g,
    );
    let composed = Expr::rebuilt(ExprKind::Lambda(vec![param], Box::new(body)), g);
    // Spanned as the whole chain: that is the source the user wrote, and what
    // any later diagnostic should point at.
    let call = Expr::rebuilt(
        ExprKind::Call(Callee::Map, vec![source.clone(), composed], *method_style),
        whole,
    );
    Some(under_bindings(bindings, whole, call))
}
