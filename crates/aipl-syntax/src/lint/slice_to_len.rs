use super::end_is_receiver_len;
use crate::ast::{Expr, ExprKind};
use crate::Error;

/// `x[y..x.len()]` — the end bound is the receiver's own length, which is
/// what the open-ended form already means; recommend `x[y..]`. Purely
/// syntactic: the receiver and the `len` argument must be the same
/// expression (spans ignored), so aliases or computed receivers that merely
/// happen to be equal at runtime are not flagged.
pub(super) fn slice_to_len(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Slice(obj, start, Some(end)) = &e.kind else {
        return;
    };
    if !end_is_receiver_len(end, obj) {
        return;
    }
    // A zero start makes this the *whole* receiver, which is
    // [`slice_whole`]'s to report — and its advice (`x`) is better than the
    // `x[0..]` this one would give. The two are mutually exclusive, so
    // neither has to run before the other.
    if matches!(start.kind, ExprKind::Num(0)) {
        return;
    }
    let recv = &src[obj.span.clone()];
    let st = &src[start.span.clone()];
    hits.push(Error::at(
        format!(
            "slice end is the receiver's whole length — use the open-ended \
             \"{recv}[{st}..]\" (or append #[allow] to this line to keep it)"
        ),
        end.span.clone(),
    ));
}
