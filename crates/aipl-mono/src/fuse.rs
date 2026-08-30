//! Operation fusion: rewriting a composite expression into one builtin that
//! computes the same answer with less work.
//!
//! There are three families, each in its own file with its own table:
//!
//! - [`comparison_fusions`] — a call against a comparison. `xs.count(x)` always
//!   walks the whole collection, but `xs.count(x) < 4` is settled the moment a
//!   fourth match appears, so the pair collapses into `count_is_less_than`,
//!   which stops there. The same holds for every comparison, in both operand
//!   orders.
//! - [`chain_fusions`] — a call whose *receiver* is another call.
//!   `xs.filter(p).map(f)` walks the elements twice and builds an array between
//!   the two passes, so the pair collapses into `filter_map`, which selects and
//!   maps in one — and, when the source is uniquely owned and the element sizing
//!   allows, into the source's own buffer.
//! - [`slice_fusions`] — a call on a sliced receiver. `xs[i..].starts_with(p)`
//!   builds the whole tail of `xs` — for an array, a fresh block with every
//!   element copied — only to look at its first few elements, so it collapses
//!   into `starts_with_at`, which compares in place from `i`.
//!
//! # Adding a fusion
//!
//! Add a row to the table in its family's file and implement the builtin it
//! names. A comparison row says "*this* call, compared with *this* operator, is
//! *that* call"; a slice row says "*this* call, on a receiver sliced from `i`,
//! is *that* call, with `i` as its last argument"; a chain row says "*this*
//! call, on the result of *that* one, is *this third* call, taking both argument
//! lists". Either way the driver here handles the rest — finding the shape (in
//! both operand orders, for a comparison) and refusing when it would change
//! observable behaviour. Nothing else needs to know the family exists.
//!
//! A whole new family is a new file next to those three, exporting a
//! `build(..) -> Option<Expr>` that [`try_fuse`] calls; everything downstream of
//! the shape match — the effect guard, the bottom-up traversal — is shared.
//!
//! # Why effects block it
//!
//! Fusing changes *how much* of the input is examined, and reorders the
//! evaluation around the traversal — the comparison's other operand moves ahead
//! of it, a slice bound moves behind the remaining arguments. Neither is
//! observable for pure operands — but if evaluating any part prints, reads, or
//! writes, the program's observable behaviour would change, and an optimization
//! that does that is a bug rather than an optimization. So any effect anywhere
//! in the expression disables the rewrite. The check is deliberately syntactic
//! and conservative: it costs a missed fusion, never a wrong one.

mod chain_fusions;
mod comparison_fusions;
mod slice_fusions;

use std::collections::HashSet;

use aipl_syntax::ast::{Expr, ExprKind, Item, Program};

/// Rewrite every fusable shape in `program`.
///
/// `effectful` names the functions whose call carries an effect — the caller
/// supplies it because the effect declarations live with the builtin signatures,
/// which this crate does not parse.
pub fn fuse_operations(program: &Program, effectful: &HashSet<String>) -> Program {
    let mut out = program.clone();
    for item in &mut out.items {
        if let Item::Fn(f) = item {
            fuse_expr(&mut f.body, effectful);
            if let Some(t) = f.test_body.as_mut() {
                fuse_expr(t, effectful);
            }
        }
    }
    out
}

/// Bottom-up: children first, so a fusion can be built from an already-fused
/// sub-expression rather than racing it.
fn fuse_expr(e: &mut Expr, effectful: &HashSet<String>) {
    for c in crate::children_mut(e) {
        fuse_expr(c, effectful);
    }
    if let Some(fused) = try_fuse(e, effectful) {
        *e = fused;
    }
}

/// The fused form of `e`, if it is one of the fusable shapes and fusing it
/// cannot change what the program observably does.
///
/// Shape first, effects second: this runs on every node of every body, and the
/// effect check walks a whole subtree — worth paying only once a shape matched.
fn try_fuse(e: &Expr, effectful: &HashSet<String>) -> Option<Expr> {
    let fused = match &e.kind {
        ExprKind::Binop(l, op, r) => comparison_fusions::build(l, *op, r, e),
        ExprKind::Call(..) => slice_fusions::build(e).or_else(|| chain_fusions::build(e)),
        _ => None,
    }?;
    // An effect *anywhere* in `e` rules the rewrite out, wherever the call sits:
    // fusing reorders the evaluation within it.
    (!has_effect(e, effectful)).then_some(fused)
}

/// Whether evaluating `e` can do anything observable — call a function that
/// declares an effect, or install a shim.
///
/// Syntactic and conservative on purpose (see the module docs): an unknown name
/// is treated as effect-free because every call this pass can actually fuse
/// resolves to a declared signature, and being wrong in the other direction
/// would silently disable the pass rather than announce itself.
fn has_effect(e: &Expr, effectful: &HashSet<String>) -> bool {
    let here = match &e.kind {
        ExprKind::Call(name, _, _) => effectful.contains(name),
        // A shim installs handlers for an effect; that is the effect happening.
        ExprKind::Shim(..) => true,
        _ => false,
    };
    here || crate::children(e).iter().any(|c| has_effect(c, effectful))
}

/// The functions in `items` whose signature declares an effect — what
/// [`fuse_operations`] wants for its `effectful` argument. Pass the declarations
/// the checker saw, so builtin signatures (`print` is `!prints`) are included.
pub fn effectful_fns(items: &[Item]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) if !f.sig.effects.is_empty() => Some(f.name.clone()),
            _ => None,
        })
        .collect()
}
