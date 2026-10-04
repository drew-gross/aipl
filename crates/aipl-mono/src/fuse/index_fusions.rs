//! An *index* of a mapped sequence: `xs.map(f)[i]` applies `f` to every element
//! and builds the whole mapped array, only to read one element back out of it
//! and drop the rest. So the pair collapses into `xs[i].map(f)` — the
//! *optional's* `map`, which allocates nothing and calls `f` once.
//!
//! The rewrite is the same node in, the same node out: an index of a call
//! becomes a call on an index. What makes it worth a family is the arithmetic it
//! deletes — `n` calls to `f` and one array become one call and nothing — and
//! that the saving grows with the receiver while the source expression does not
//! change size, so nothing about the written form hints at the cost.
//!
//! The motivating site is the dogfooded parser. `own_texts` is
//! `self.tokens.map(|t| src[t])`, small enough that `inline_small` folds it into
//! its callers, and several of those read one token out of it —
//! `node.own_texts(src)[0] == some("||")`. Inlined, that is exactly this shape:
//! every token in the node was being sliced out of the source text to answer a
//! question about one of them.
//!
//! # Both readings of `Index` fuse, and it does not matter which this is
//!
//! `recv[i]` is element access when `i` is an integer and *slicing* when `i` is
//! the builtin `Span` ([`ExprKind::Index`]), and which one it is depends on a
//! type nothing before monomorphization knows. The rewrite is sound either way,
//! so the pass does not need to find out:
//!
//! - as an element, `xs.map(f)[i]` and `xs[i].map(f)` are both `f` applied to
//!   the `i`th element when there is one and `none` when there is not — `map`
//!   preserves length *and* position, so in-bounds is the same question of
//!   either receiver;
//! - as a slice, `xs.map(f)[span]` and `xs[span].map(f)` are both `f` over the
//!   elements in `span`, clamped the same way for the same reason.
//!
//! The element reading is where the win is (an array becomes an optional); the
//! slice reading still maps `span`'s elements instead of all of them.
//!
//! # Why `preserves_length` is the wrong predicate
//!
//! [`Callee::preserves_length`](aipl_syntax::ast::Callee::preserves_length)
//! names the calls that answer with one element per element in, which is what
//! `fold_lengths` wants — but indexing asks *which* element, not how many.
//! `xs.sort()[i]` and `xs.reverse()[i]` preserve length and move elements, so
//! the `i`th of the result is not built from the `i`th of the receiver and
//! neither fuses. `map` is the call that preserves position as well, so this
//! family names it directly rather than reaching for the set that happens to
//! contain it.
//!
//! # Why only the mapping function has to be pure
//!
//! Fusing *drops* `f`'s calls for every element but the one read, so `f` faces
//! the loop family's bar rather than the whole-expression effect check: no
//! effects and no aborts, closed over the call graph
//! ([`loop_fusions::is_pure`]). An `f` that printed per element would print once
//! instead of `n` times; one that aborted on an element nobody reads would stop
//! aborting.
//!
//! The receiver and the index are free to have effects, which is why this
//! family guards itself instead of deferring to [`super::has_effect`]. Both are
//! evaluated exactly once either way, and the only movement is that the index is
//! evaluated *ahead* of `f`'s calls rather than after them — which a pure `f`
//! cannot observe. (The same argument the loop family makes, in the same
//! direction: there the mapping calls move later, here all but one of them stop
//! happening.)
//!
//! # The one thing it changes about an invalid program
//!
//! `map` has no set form, and monomorphization is what refuses one
//! (`expand_map`) — so `s.map(f)[0]` over a set reaches this pass having passed
//! the checker. Fusing it moves the `map` onto `s[0]`, and the error that comes
//! back names the index (`cannot index a value of type #{i64}`) rather than the
//! `map`. Both are errors on the same line and neither program compiles, so
//! unlike [`crate::fold_lengths`] — which would have *deleted* the `map` and
//! answered with a count — there is nothing here to guard against.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Expr, ExprKind};

use super::{loop_fusions, through_bindings, under_bindings};
use crate::sink::mentions_free;

/// `xs.map(f)[i]` as `xs[i].map(f)`, or `None` when `e` is not that shape or
/// fusing it would change what the program observably does.
///
/// The mapped call may sit under the `let`s inlining leaves in front of an
/// inlined body ([`through_bindings`]) — which is the shape at the motivating
/// site, since `own_texts` arrives here as
/// `let self: Parts = node; let src: str = src; map(self.tokens, |t| src[t])`.
///
/// The index moves *under* those bindings with the call, so it may not mention a
/// name they bind: a parameter named `src` would otherwise capture an index
/// written against the caller's `src`. That is the same guard
/// [`super::chain_fusions::build`] carries over the arguments it moves, and like
/// that one it is belt-and-braces — `inline_single_use_bindings` runs ahead of
/// this pass and substitutes such a binding away, so nothing in the corpus (nor
/// in the dogfooded compiler) actually reaches it. It stays because soundness
/// here should not rest on an earlier pass happening to run first;
/// `optimizations/index_map_fusion_effects` pins the answer the collision must
/// still give.
pub(super) fn build(whole: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let ExprKind::Index(recv, index) = &whole.kind else {
        return None;
    };
    let (bindings, inner) = through_bindings(recv);
    let ExprKind::Call(Callee::Map, args, method_style) = &inner.kind else {
        return None;
    };
    // Exactly the receiver and the function: a `map` carrying anything else is
    // not the call this reasoning was written about.
    let [xs, f] = args.as_slice() else {
        return None;
    };
    if !loop_fusions::is_pure(f, blocked) {
        return None;
    }
    if bindings.iter().any(|b| mentions_free(index, b.name)) {
        return None;
    }
    // Spanned as the whole index expression: that is the source the user wrote,
    // and what any later diagnostic should point at.
    let indexed = Expr::rebuilt(ExprKind::Index(Box::new(xs.clone()), index.clone()), whole);
    let mapped = Expr::rebuilt(
        ExprKind::Call(Callee::Map, vec![indexed, f.clone()], *method_style),
        whole,
    );
    Some(under_bindings(bindings, whole, mapped))
}
