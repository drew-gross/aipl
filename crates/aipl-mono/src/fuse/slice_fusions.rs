//! A call on a sliced receiver: `xs[i..].starts_with(p)` builds the whole tail
//! of `xs` only to look at its first few elements, so it collapses into
//! `starts_with_at`, which compares in place from `i`.

use aipl_syntax::ast::{Expr, ExprKind};

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

/// `recv[start..].call(rest..)` as a call to the [`SLICE_FUSIONS`] row's `into`,
/// taking the unsliced receiver and `start` as its last argument.
pub(super) fn build(whole: &Expr) -> Option<Expr> {
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
