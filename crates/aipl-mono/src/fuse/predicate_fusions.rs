//! A `map` or a `filter` feeding a call that asks a *predicate* of each element:
//! `xs.map(f).any(p)` builds an array of every `f(x)` for `any` to read the front
//! of and drop, and `xs.filter(p).count_if(q)` builds an array of the survivors
//! only to count some of them. Either is one pass over `xs` with the two
//! functions composed into one, so the pair collapses into `xs.any(|x| p(f(x)))`
//! and `xs.count_if(|x| p(x) && q(x))` — no array in between, and `any` stops at
//! the first hit without having mapped the rest.
//!
//! The composed function is built exactly as [`super::compose_fusions`] builds
//! the one for `map(f).map(g)`, and for the same reasons it is not a row of
//! [`super::chain_fusions`]' table: the fused call is the *consumer* with a
//! function nobody wrote, not a third builtin. The two shapes it composes are:
//!
//! ```text
//! consumer(xs.map(f), g)     →  consumer(xs, |x| { let y = f(x); g(y) })
//! consumer(xs.filter(p), g)  →  consumer(xs, |x| if (p(x)) { g(x) } else { <miss> })
//! ```
//!
//! where `<miss>` is the answer for an element `filter` would have dropped:
//! `false` for the consumers that ask "does one match" (`any`, `count_if`,
//! `find_if`, and `filter` itself, which keeps an element only when both
//! predicates hold), `true` for `all`, which a dropped element cannot falsify,
//! and `none` for `find_map`.
//!
//! # The ends of a stage
//!
//! `first()` and `last()` are consumers that ask no predicate and want one
//! element, which makes them the same fusion with the predicate left out:
//!
//! ```text
//! xs.map(f).first()            →  xs.first().map(f)    (the optional's `map`)
//! xs.map(f).last()             →  xs.last().map(f)
//! xs.filter(p).first()         →  xs.find_if(p)
//! xs.filter(p).last()          →  xs.reverse().find_if(p)
//! xs.filter(p).map(f).first()  →  xs.find_if(p).map(f)
//! xs.filter(p).map(f).last()   →  xs.reverse().find_if(p).map(f)
//! ```
//!
//! The first two are [`super::index_fusions`]' `xs.map(f)[i]` for the ends: `f`
//! is applied to one element instead of all of them, and nothing is built. The
//! last two stop at the answer instead of filtering the whole array and reading
//! one end of the survivors; `reverse().find_if` walks backwards in place.
//! `f` and `p` face the same bar as above, and for the same reason — their calls
//! on every other element stop happening.
//!
//! # Emptiness
//!
//! `xs.filter(p).is_nonempty()` asks whether *any* element passes, and builds the
//! array of every one that does to find out. It is `xs.any(p)`, which stops at
//! the first; `is_empty` is the same question negated:
//!
//! ```text
//! xs.filter(p).is_nonempty()  →  xs.any(p)
//! xs.filter(p).is_empty()     →  !xs.any(p)
//! ```
//!
//! A `filter_map(p, f)` is the same with `f` dropped (it only maps the survivors,
//! and nothing reads them), so `f` faces the purity bar too. A `map` is not here:
//! `xs.map(f).is_empty()` is `xs.is_empty()` only when `xs` is an array, and
//! `map` takes a set receiver only to refuse it.
//!
//! # Which consumers
//!
//! - after a `map`: `any`, `all`, `count_if`, `find_map` and `find_index`. (`find_if`
//!   is a row of the chain table, `map_find_if`, and a `filter` of a `map` keeps
//!   the *mapped* values — which no predicate over `xs` can produce — so neither
//!   is here.)
//! - after a `filter`: `any`, `all`, `count_if`, `find_if`, `find_map` and
//!   `filter`. A `filter` rejecting an element answers `none` to `find_map`.
//! - after a `filter(p).map(f)` — which is a `filter_map` by the time this pass
//!   reaches the consumer: `any`, `all`, `count_if`. The composed function runs
//!   `p`, then `f` on the survivors, then the consumer's predicate.
//!
//! # When it is refused
//!
//! Both functions face the bar [`super::compose_fusions`] sets: pure and unable
//! to abort ([`is_pure`]), because their calls move. The unfused program ran the
//! first stage on every element before the consumer saw any; the fused one
//! interleaves them, and a consumer that short-circuits (`any`, `all`,
//! `find_if`) never runs the first stage on the elements past the answer.
//! Neither is unobservable for a function that prints or that can abort.
//!
//! A function whose value takes its type from where it sits (`|x| none`, an empty
//! `[]`) would lose it when bound with a plain `let`, so those are refused too
//! ([`contains_context_literal`]).
//!
//! The stage may sit under the `let`s an inlined call leaves in front of its body
//! (see [`through_bindings`]); the fused call goes back beneath them.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Expr, ExprKind, LambdaParam};

use super::loop_fusions::{apply, is_pure};
use super::{through_bindings, under_bindings};
use crate::sink::mentions_free;

/// The call feeding the consumer, with the functions it applies per element.
enum Stage<'a> {
    /// `map(xs, f)`.
    Map { f: &'a Expr },
    /// `filter(xs, p)`.
    Filter { p: &'a Expr },
    /// `filter_map(xs, p, f)` — what `xs.filter(p).map(f)` has already become.
    FilterMap { p: &'a Expr, f: &'a Expr },
}

impl<'a> Stage<'a> {
    /// The stage `callee(args)` is, with the array it reads.
    fn of(callee: &Callee, args: &'a [Expr]) -> Option<(&'a Expr, Stage<'a>)> {
        match (callee, args) {
            (Callee::Map, [source, f]) => Some((source, Stage::Map { f })),
            (Callee::Filter, [source, p]) => Some((source, Stage::Filter { p })),
            (Callee::FilterMap, [source, p, f]) => Some((source, Stage::FilterMap { p, f })),
            _ => None,
        }
    }

    /// The functions the stage applies, which face the purity bar.
    fn functions(&self) -> Vec<&'a Expr> {
        match self {
            Stage::Map { f } => vec![f],
            Stage::Filter { p } => vec![p],
            Stage::FilterMap { p, f } => vec![p, f],
        }
    }

    /// The function the composed one takes its parameter from: the first the
    /// stage applies, which is what the stage handed each element.
    fn first_function(&self) -> &'a Expr {
        match self {
            Stage::Map { f } => f,
            Stage::Filter { p } | Stage::FilterMap { p, .. } => p,
        }
    }

    /// The function that produces a new value from an element, if the stage has
    /// one — the one that cannot be bound with a plain `let` when its value takes
    /// its type from where it sits.
    fn mapping(&self) -> Option<&'a Expr> {
        match self {
            Stage::Map { f } | Stage::FilterMap { f, .. } => Some(f),
            Stage::Filter { .. } => None,
        }
    }

    /// Whether `consumer` is one this family fuses after the stage.
    fn feeds(&self, consumer: &Callee) -> bool {
        match self {
            // `find_index` is here only for a `map`: it preserves position, so the
            // index in `xs` is the index in the mapped array. A `filter` does
            // not, and its index is among the survivors.
            Stage::Map { .. } => matches!(
                consumer,
                Callee::Any | Callee::All | Callee::CountIf | Callee::FindMap | Callee::FindIndex
            ),
            Stage::FilterMap { .. } => matches!(
                consumer,
                Callee::Any | Callee::All | Callee::CountIf | Callee::FindMap
            ),
            Stage::Filter { .. } => matches!(
                consumer,
                Callee::Any
                    | Callee::All
                    | Callee::CountIf
                    | Callee::FindIf
                    | Callee::FindMap
                    | Callee::Filter
            ),
        }
    }
}

/// What a dropped element answers: the value of the composed function where the
/// stage is a `filter` and rejected the element. `true` for `all`, which a
/// dropped element cannot falsify; `none` for `find_map`, whose function answers
/// with an optional; `false` for the rest, which ask whether one matches.
fn miss(consumer: &Callee) -> ExprKind {
    match consumer {
        Callee::All => ExprKind::Bool(true),
        Callee::FindMap => ExprKind::None,
        _ => ExprKind::Bool(false),
    }
}

/// `xs.map(f).first()` and its siblings — see "The ends of a stage" above.
pub(super) fn build_end(whole: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let ExprKind::Call(end, args, method_style) = &whole.kind else {
        return None;
    };
    let [receiver] = args.as_slice() else {
        return None;
    };
    if !matches!(end, Callee::First | Callee::Last) {
        return None;
    }
    let (bindings, inner) = through_bindings(receiver);
    let ExprKind::Call(stage_callee, inner_args, _) = &inner.kind else {
        return None;
    };
    let (source, stage) = Stage::of(stage_callee, inner_args)?;
    if !stage.functions().into_iter().all(|f| is_pure(f, blocked)) {
        return None;
    }
    let rebuilt = |kind| Expr::rebuilt(kind, whole);
    // The array the search walks: `xs` for a `first`, and `xs` backwards for a
    // `last`.
    let walked = |source: &Expr| match end {
        Callee::First => source.clone(),
        _ => rebuilt(ExprKind::Call(Callee::Reverse, vec![source.clone()], false)),
    };
    // `f` applied to the optional element, when the stage maps.
    let mapped = |found: Expr, f: &Expr| {
        rebuilt(ExprKind::Call(
            Callee::Map,
            vec![found, f.clone()],
            *method_style,
        ))
    };
    let find = |walked: Expr, p: &Expr| {
        rebuilt(ExprKind::Call(
            Callee::FindIf,
            vec![walked, p.clone()],
            *method_style,
        ))
    };
    let call = match stage {
        // The optional's `map`: the end element when there is one, mapped.
        Stage::Map { f } => {
            let element = rebuilt(ExprKind::Call(end.clone(), vec![source.clone()], false));
            mapped(element, f)
        }
        Stage::Filter { p } => find(walked(source), p),
        Stage::FilterMap { p, f } => mapped(find(walked(source), p), f),
    };
    Some(under_bindings(bindings, whole, call))
}

/// `xs.filter(p).is_nonempty()` as `xs.any(p)`, and `is_empty` as its negation —
/// see "Emptiness" above.
pub(super) fn build_emptiness(whole: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let ExprKind::Call(asked, args, method_style) = &whole.kind else {
        return None;
    };
    let [receiver] = args.as_slice() else {
        return None;
    };
    if !matches!(asked, Callee::IsEmpty | Callee::IsNonempty) {
        return None;
    }
    let (bindings, inner) = through_bindings(receiver);
    let ExprKind::Call(stage_callee, inner_args, _) = &inner.kind else {
        return None;
    };
    let (source, stage) = Stage::of(stage_callee, inner_args)?;
    let (Stage::Filter { p } | Stage::FilterMap { p, .. }) = &stage else {
        return None;
    };
    if !stage.functions().into_iter().all(|f| is_pure(f, blocked)) {
        return None;
    }
    let any = Expr::rebuilt(
        ExprKind::Call(
            Callee::Any,
            vec![source.clone(), (*p).clone()],
            *method_style,
        ),
        whole,
    );
    let answer = match asked {
        Callee::IsNonempty => any,
        _ => Expr::rebuilt(ExprKind::Call(Callee::LogicalNot, vec![any], false), whole),
    };
    Some(under_bindings(bindings, whole, answer))
}

/// `consumer(stage(xs, ..), g)` as `consumer(xs, <the stage's functions and `g`
/// composed>)`, or `None` when `whole` is not that shape or cannot be rewritten
/// without changing what the program does.
///
/// `blocked` is the loop family's bar: the functions whose call is observable or
/// may abort, closed over the call graph.
pub(super) fn build(whole: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let ExprKind::Call(consumer, args, method_style) = &whole.kind else {
        return None;
    };
    let [receiver, g] = args.as_slice() else {
        return None;
    };
    let (bindings, inner) = through_bindings(receiver);
    let ExprKind::Call(stage_callee, inner_args, _) = &inner.kind else {
        return None;
    };
    let (source, stage) = Stage::of(stage_callee, inner_args)?;
    if !stage.feeds(consumer) {
        return None;
    }
    if bindings.iter().any(|b| mentions_free(g, b.name)) {
        return None;
    }
    if !is_pure(g, blocked) || !stage.functions().into_iter().all(|f| is_pure(f, blocked)) {
        return None;
    }
    if matches!(stage.mapping().map(|f| &f.kind), Some(ExprKind::Lambda(_, body)) if crate::contains_context_literal(body))
    {
        return None;
    }

    let id = crate::next_inline_id();
    let (x, y) = (format!("$predicate{id}_x"), format!("$predicate{id}_y"));
    let ident = |name: &str| Expr::new(ExprKind::Ident(name.to_string()), whole.span.clone());
    // `let y = f(x); g(y)`.
    let mapped_then_g = |f: &Expr| {
        let f_of_x = apply(f, ident(&x))?;
        let g_of_y = apply(g, ident(&y))?;
        Some(Expr::rebuilt(
            ExprKind::Let(y.clone(), None, Box::new(f_of_x), Box::new(g_of_y)),
            g,
        ))
    };
    // `if (p(x)) { <kept> } else { <miss> }`.
    let guarded = |p: &Expr, kept: Expr| {
        let p_of_x = apply(p, ident(&x))?;
        let missed = Expr::new(miss(consumer), g.span.clone());
        Some(Expr::rebuilt(
            ExprKind::If(Box::new(p_of_x), Box::new(kept), Box::new(missed)),
            g,
        ))
    };
    let body = match &stage {
        Stage::Map { f } => mapped_then_g(f)?,
        Stage::Filter { p } => guarded(p, apply(g, ident(&x))?)?,
        Stage::FilterMap { p, f } => guarded(p, mapped_then_g(f)?)?,
    };
    // The element's written type, if the first function wrote one, is the
    // composed function's: it is what the stage handed that function.
    let param_ty = match &stage.first_function().kind {
        ExprKind::Lambda(params, _) => params.first().and_then(|p| p.ty.clone()),
        _ => None,
    };
    let param = LambdaParam {
        name: x,
        ty: param_ty,
        span: stage.first_function().span.clone(),
    };
    let composed = Expr::rebuilt(ExprKind::Lambda(vec![param], Box::new(body)), g);
    // Spanned as the whole chain: that is the source the user wrote, and what any
    // later diagnostic should point at.
    let call = Expr::rebuilt(
        ExprKind::Call(
            consumer.clone(),
            vec![source.clone(), composed],
            *method_style,
        ),
        whole,
    );
    Some(under_bindings(bindings, whole, call))
}
