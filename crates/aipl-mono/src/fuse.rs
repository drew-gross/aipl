//! Operation fusion: rewriting a composite expression into one builtin that
//! computes the same answer with less work.
//!
//! There are two families, each with its own table.
//!
//! [`FUSIONS`] is `count` against a comparison. `xs.count(x)` always walks the
//! whole collection, but `xs.count(x) < 4` is settled the moment a fourth match
//! appears — so the pair collapses into `count_is_less_than`, which stops there.
//! The same holds for every comparison, in both operand orders.
//!
//! [`SLICE_FUSIONS`] is a call on a sliced receiver. `xs[i..].starts_with(p)`
//! builds the whole tail of `xs` — for an array, a fresh block with every
//! element copied — only to look at its first few elements, so it collapses into
//! `starts_with_at`, which compares in place from `i`.
//!
//! # Adding a fusion
//!
//! Add a row to the table for its family and implement the builtin it names. A
//! [`FUSIONS`] row says "*this* call, compared with *this* operator, is *that*
//! call"; a [`SLICE_FUSIONS`] row says "*this* call, on a receiver sliced from
//! `i`, is *that* call, with `i` as its last argument". Either way the driver
//! handles the rest — finding the shape (in both operand orders, for a
//! comparison) and refusing when it would change observable behaviour. Nothing
//! else needs to know the family exists.
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

use std::collections::HashSet;

use aipl_syntax::ast::{Expr, ExprKind, Item, Program};

/// One fusable shape: a call to `call`, compared with `op` against a second
/// operand, becomes a call to `into` taking that operand as a trailing argument.
///
/// `op` is written with the call on the **left**; the driver mirrors it.
struct Fusion {
    call: &'static str,
    op: char,
    into: &'static str,
}

/// Every shape the pass knows. See the module docs for how to add one.
const FUSIONS: &[Fusion] = &[
    Fusion {
        call: "__builtin_count",
        op: '<',
        into: "__builtin_count_is_less_than",
    },
    Fusion {
        call: "__builtin_count",
        op: 'L',
        into: "__builtin_count_is_at_most",
    },
    Fusion {
        call: "__builtin_count",
        op: '>',
        into: "__builtin_count_is_greater_than",
    },
    Fusion {
        call: "__builtin_count",
        op: 'G',
        into: "__builtin_count_is_at_least",
    },
    Fusion {
        call: "__builtin_count",
        op: 'E',
        into: "__builtin_count_is_equal",
    },
    Fusion {
        call: "__builtin_count",
        op: 'N',
        into: "__builtin_count_is_not_equal",
    },
];

/// One fusable slice shape: a call to `call` whose receiver is an open-ended
/// slice becomes a call to `into` on the *unsliced* receiver, with the slice's
/// start appended as a trailing argument.
///
/// Only an open-ended `xs[i..]` qualifies. A bounded `xs[i..j]` is a different
/// receiver — the end truncates it — so `into` would have to take that bound
/// too, which none of these do.
struct SliceFusion {
    call: &'static str,
    into: &'static str,
}

/// Every slice shape the pass knows. See the module docs for how to add one.
const SLICE_FUSIONS: &[SliceFusion] = &[SliceFusion {
    call: "__builtin_starts_with",
    into: "__builtin_starts_with_at",
}];

/// `a OP b` and `b flip(OP) a` are the same claim, which is what lets one
/// [`FUSIONS`] row cover both operand orders. `==`/`!=` are symmetric.
fn flip(op: char) -> char {
    match op {
        '<' => '>',
        '>' => '<',
        'L' => 'G',
        'G' => 'L',
        other => other,
    }
}

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
        // The call on the left reads with `op` as written; on the right, the same
        // claim with the operator mirrored.
        ExprKind::Binop(l, op, r) => build(l, *op, r, e).or_else(|| build(r, flip(*op), l, e)),
        ExprKind::Call(..) => build_slice(e),
        _ => None,
    }?;
    // An effect *anywhere* in `e` rules the rewrite out, wherever the call sits:
    // fusing reorders the evaluation within it.
    (!has_effect(e, effectful)).then_some(fused)
}

/// `recv[start..].call(rest..)` as a call to the [`SLICE_FUSIONS`] row's `into`,
/// taking the unsliced receiver and `start` as its last argument.
fn build_slice(whole: &Expr) -> Option<Expr> {
    let ExprKind::Call(name, args, method_style) = &whole.kind else {
        return None;
    };
    let f = SLICE_FUSIONS.iter().find(|f| f.call == name)?;
    let (recv, rest) = args.split_first()?;
    // Open-ended only: a bounded slice is a different receiver (see
    // [`SliceFusion`]).
    let ExprKind::Slice(obj, start, None) = &recv.kind else {
        return None;
    };
    let mut fused = vec![(**obj).clone()];
    fused.extend(rest.iter().cloned());
    fused.push((**start).clone());
    // Spanned as the whole call: that is the source the user wrote, and what any
    // later diagnostic should point at.
    Some(Expr::rebuilt(
        ExprKind::Call(f.into.to_string(), fused, *method_style),
        whole,
    ))
}

/// `call OP bound` as a fused call, when `call` is a [`FUSIONS`] row for `OP`.
fn build(call: &Expr, op: char, bound: &Expr, whole: &Expr) -> Option<Expr> {
    let ExprKind::Call(name, args, method_style) = &call.kind else {
        return None;
    };
    let f = FUSIONS.iter().find(|f| f.call == name && f.op == op)?;
    let mut fused = args.clone();
    fused.push(bound.clone());
    // Spanned as the whole comparison: that is the source the user wrote, and
    // what any later diagnostic should point at.
    Some(Expr::rebuilt(
        ExprKind::Call(f.into.to_string(), fused, *method_style),
        whole,
    ))
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
