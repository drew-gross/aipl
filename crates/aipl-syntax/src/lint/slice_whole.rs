use super::end_is_receiver_len;
use crate::ast::{Expr, ExprKind};
use crate::Error;

/// `x[0..]` / `x[..x.len()]` / `x[0..x.len()]` — a slice whose range covers
/// the receiver end to end, which is what `x` already is. The slice is not
/// free: an array slice allocates a fresh block and copies every element
/// (`aipl_arr_slice`), so this is a redundant copy as well as a redundant
/// spelling. Dropping it is safe because AIPL arrays have value semantics —
/// mutating an aliased binding copies first (see
/// `cases/arrays/push_aliased_array`), so `y = x` cannot be surprised by a
/// later `set x[..]` the way a shared reference could.
///
/// This is the whole-range corner of the three slice lints, and it takes
/// priority over both: [`slice_to_len`](super::slice_to_len()) would advise
/// `x[0..]` and [`slice_from_zero`](super::slice_from_zero()) `x[..x.len()]`,
/// each of which is still this shape and would need a second round trip. Both
/// bow out when this one applies, so a full-range slice produces exactly one
/// hit whichever way it was written.
///
/// A start of `0` counts however it was written: `x[..y]` synthesizes a zero
/// with an empty span, and unlike its siblings this lint has no reason to
/// tell that from a written `0` — both cover the whole receiver, and the
/// advice names neither bound.
pub(super) fn slice_whole(e: &Expr, src: &str, hits: &mut Vec<Error>) {
    let ExprKind::Slice(obj, start, end) = &e.kind else {
        return;
    };
    if !matches!(start.kind, ExprKind::Num(0)) {
        return;
    }
    // An absent end already runs to the length; a present one has to say so.
    if let Some(end) = end {
        if !end_is_receiver_len(end, obj) {
            return;
        }
    }
    // Quote the receiver only when its span really ends where the `[`
    // begins. A call-shaped span stops before its parens (the trap
    // [`slice_from_zero`] documents for the end bound), so splicing
    // `f(x)[0..]` would advise the bare `f`. Checking the source rather than
    // the expression kind keeps every complete receiver quotable —
    // `x.field`, `x.0` and `xs[i]` all name themselves fine.
    let quotable = src[obj.span.end..].trim_start().starts_with('[');
    let use_it = if quotable {
        format!("use \"{}\" itself", &src[obj.span.clone()])
    } else {
        "use the receiver itself".to_string()
    };
    // Point at a bound rather than the whole expression: `#[allow]` is
    // line-scoped (see `allow_squelch`), and a receiver may span lines, so
    // the expression's own span could start on a line that can't carry the
    // marker. A bound sits inside the brackets, always on the line the
    // slice is written on. The written `0` is the natural target; when it
    // was elided (`x[..x.len()]`) its span is empty, so the end bound —
    // which must be present in that form — stands in.
    let span = if start.span.is_empty() {
        end.as_ref()
            .expect("elided start implies an end bound")
            .span
            .clone()
    } else {
        start.span.clone()
    };
    hits.push(Error::at(
        format!(
            "slice covers the whole receiver — {use_it} \
             (or append #[allow] to this line to keep it)"
        ),
        span,
    ));
}
