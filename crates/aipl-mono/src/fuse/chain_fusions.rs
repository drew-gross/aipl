//! A call whose *receiver* is another call: `xs.filter(p).map(f)` walks the
//! elements twice and builds an array between the two passes, so the pair
//! collapses into `filter_map`, which selects and maps in one — and, when the
//! source is uniquely owned and the element sizing allows, into the source's own
//! buffer.

use aipl_syntax::ast::{Expr, ExprKind};

/// One fusable chain shape: a call to `outer` whose receiver is a call to
/// `inner` becomes a single call to `into`, taking the inner call's arguments
/// followed by the outer's remaining ones.
///
/// Both surface forms reach this the same way: `xs.filter(p).map(f)` and
/// `map(filter(xs, p), f)` fold to the same argument lists, so a row does not
/// have to say which was written.
struct ChainFusion {
    inner: &'static str,
    outer: &'static str,
    into: &'static str,
}

/// Every chain shape the pass knows. See the module docs for how to add one.
const CHAIN_FUSIONS: &[ChainFusion] = &[ChainFusion {
    inner: "__builtin_filter",
    outer: "__builtin_map",
    into: "__builtin_filter_map",
}];

/// `inner(recv, ..).outer(..)` as a single call to the [`CHAIN_FUSIONS`] row's
/// `into`, taking the inner argument list followed by the outer's rest.
///
/// Bottom-up traversal means the receiver has already been fused if it could
/// be, so a row composes with the others rather than racing them.
pub(super) fn build(whole: &Expr) -> Option<Expr> {
    let ExprKind::Call(name, args, method_style) = &whole.kind else {
        return None;
    };
    let (recv, rest) = args.split_first()?;
    let ExprKind::Call(inner_name, inner_args, _) = &recv.kind else {
        return None;
    };
    let f = CHAIN_FUSIONS
        .iter()
        .find(|f| f.outer == name && f.inner == inner_name)?;
    let mut fused = inner_args.clone();
    fused.extend(rest.iter().cloned());
    // Spanned as the whole chain: that is the source the user wrote, and what
    // any later diagnostic should point at.
    Some(Expr::rebuilt(
        ExprKind::Call(f.into.to_string(), fused, *method_style),
        whole,
    ))
}
