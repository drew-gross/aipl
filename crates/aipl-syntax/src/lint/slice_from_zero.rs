use super::end_is_receiver_len;
use crate::ast::{Expr, ExprKind};
use crate::Error;

/// `x[0..y]` — a start bound of literal `0` is what the open-ended form
/// already means; recommend `x[..y]`. The mirror of [`slice_to_len`](super::slice_to_len()), and it
/// leans on the same distinction: an *omitted* start is synthesized as a
/// zero with an **empty** span, so requiring a non-empty span is what
/// separates a written `0` from an elided one.
///
/// Only flagged when there is an end bound. `x[0..]` is left alone on
/// purpose: `x[..]` is not a form, so there would be nothing to recommend.
pub(super) fn slice_from_zero(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Slice(obj, start, Some(end)) = &e.kind else {
        return;
    };
    if !matches!(start.kind, ExprKind::Num(0)) || start.span.is_empty() {
        return;
    }
    // `x[0..x.len()]` is the whole receiver — [`slice_whole`]'s to report,
    // and in one step rather than the two this lint's `x[..x.len()]` would
    // start.
    if end_is_receiver_len(end, obj) {
        return;
    }
    // The end bound is deliberately *not* quoted back: a call-shaped end
    // (`s.len()`) has a span that stops before its parens, so splicing the
    // source text produced malformed advice like `s[..s.len]`.
    let recv = &src[obj.span.clone()];
    hits.push(Error::at(
        format!(
            "slice starts at 0 — drop it for the open-ended \"{recv}[..end]\" \
             (or append #[allow] to this line to keep it)"
        ),
        start.span.clone(),
    ));
}
