//! A call whose *receiver* is another call: `xs.filter(p).map(f)` walks the
//! elements twice and builds an array between the two passes, so the pair
//! collapses into `filter_map`, which selects and maps in one — and, when the
//! source is uniquely owned and the element sizing allows, into the source's own
//! buffer.
//!
//! The receiver may reach here under the `let`s inlining leaves in front of an
//! inlined body; the chain is found beneath them and the fused call put back
//! there (see [`build`]).

use aipl_syntax::ast::{Callee, Expr, ExprKind};

use super::{through_bindings, under_bindings};
use crate::sink::mentions_free;

/// One fusable chain shape: a call to `outer` whose receiver is a call to
/// `inner` becomes a single call to `into`, taking the inner call's arguments
/// followed by the outer's remaining ones.
///
/// Both surface forms reach this the same way: `xs.filter(p).map(f)` and
/// `map(filter(xs, p), f)` fold to the same argument lists, so a row does not
/// have to say which was written.
struct ChainFusion {
    inner: Callee,
    outer: Callee,
    into: Callee,
}

/// Every chain shape the pass knows. See the module docs for how to add one.
const CHAIN_FUSIONS: &[ChainFusion] = &[
    ChainFusion {
        inner: Callee::Filter,
        outer: Callee::Map,
        into: Callee::FilterMap,
    },
    // `xs.map(f).find_if(p)`: the answer may be the first element, and `map`
    // would have applied `f` to every one and built the array first.
    ChainFusion {
        inner: Callee::Map,
        outer: Callee::FindIf,
        into: Callee::MapFindIf,
    },
    // `xs.map(f).join(sep=s)`: `map` builds an array of every piece and `join`
    // then measures and copies them into a second buffer; `map_join` appends
    // each piece into one buffer as it is produced. The outer's three
    // separators (the loader has filled the omitted ones) follow `f`, which is
    // `map_join`'s parameter order.
    ChainFusion {
        inner: Callee::Map,
        outer: Callee::Join,
        into: Callee::MapJoin,
    },
    // `s.split(sep).map(f)`: `split` builds a block holding every part — a
    // retained view each — for `map` to walk once and drop; `split_map` maps
    // each part as the cut is made, over a cursor that materializes nothing.
    // The site that matters is `src.lines().map(f)`, which is this once `lines`
    // has inlined.
    ChainFusion {
        inner: Callee::Split,
        outer: Callee::Map,
        into: Callee::SplitMap,
    },
    // `s.split(sep).len()`: `split` builds an array of every part — a retained
    // view each — only for it to be counted and dropped; `split_len` is the
    // count pass alone. The site that matters is `src.lines().len()`, the line
    // number of an `assert` in a `.test` block, computed once per assertion
    // over the whole source prefix; `lines` is `split("\n")`, so this is what
    // it becomes once `lines` has inlined.
    ChainFusion {
        inner: Callee::Split,
        outer: Callee::Len,
        into: Callee::SplitLen,
    },
    // `v.to_str().len()`: `to_str` measures the rendering, allocates exactly
    // that, writes it, and hands back a `str` whose only use is to be measured
    // again and dropped — so the buffer, the copy and the refcount all pay for
    // a number the first pass already had. `to_str_len` is that measure pass
    // alone.
    ChainFusion {
        inner: Callee::ToStr,
        outer: Callee::Len,
        into: Callee::ToStrLen,
    },
];

/// `inner(recv, ..).outer(..)` as a single call to the [`CHAIN_FUSIONS`] row's
/// `into`, taking the inner argument list followed by the outer's rest.
///
/// Bottom-up traversal means the receiver has already been fused if it could
/// be, so a row composes with the others rather than racing them.
///
/// The inner call may sit under `let`s ([`through_bindings`]) — so
/// `s.lines().len()` reaches here as
/// `(let self: str = s; split(self, "\n")).len()`. The outer call moves in
/// under the bindings, which changes nothing about when anything runs: the
/// bound values were evaluated before the outer's remaining arguments already,
/// as the receiver always is. What it could change is what a name in those
/// arguments refers to, so an argument mentioning a bound name keeps the shape
/// as written.
///
/// # Standing aside for the parent
///
/// Rows stack: `Map` is the `outer` of one row and the `inner` of two others,
/// so a call can be either half of a pair. Traversal is bottom-up, so without a
/// word from the parent the inner rewrite would win simply by arriving first —
/// and it is the worse of the two. `s.split(sep).map(f).join(j)` would become
/// `split_map(..).join(..)`, an array of mapped pieces measured and copied into
/// a second buffer, where `map_join(split(..), f, j)` appends each piece into
/// one buffer as it is produced.
///
/// So when `outer` — the callee of the call this one is the receiver of — pairs
/// with this call under some row, this call stands aside and the parent fuses
/// instead. Nothing is lost: the parent's row is the one that sees both halves.
pub(super) fn build(whole: &Expr, outer: Option<&Callee>) -> Option<Expr> {
    let ExprKind::Call(name, args, method_style) = &whole.kind else {
        return None;
    };
    if outer.is_some_and(|o| row(name, o).is_some()) {
        return None;
    }
    let (recv, rest) = args.split_first()?;
    let (bindings, inner) = through_bindings(recv);
    let ExprKind::Call(inner_name, inner_args, _) = &inner.kind else {
        return None;
    };
    let f = row(inner_name, name)?;
    if bindings
        .iter()
        .any(|b| rest.iter().any(|a| mentions_free(a, b.name)))
    {
        return None;
    }
    let mut fused = inner_args.clone();
    fused.extend(rest.iter().cloned());
    // Spanned as the whole chain: that is the source the user wrote, and what
    // any later diagnostic should point at. The bindings the call moves under
    // are rebuilt the same way, since the value they now produce is the fused
    // call's.
    let call = Expr::rebuilt(ExprKind::Call(f.into.clone(), fused, *method_style), whole);
    Some(under_bindings(bindings, whole, call))
}

/// The row pairing an `inner` call with an `outer` one, if there is one.
fn row(inner: &Callee, outer: &Callee) -> Option<&'static ChainFusion> {
    CHAIN_FUSIONS
        .iter()
        .find(|f| f.inner == *inner && f.outer == *outer)
}

#[cfg(test)]
mod tests {
    use aipl_syntax::ast::Type;

    use super::*;

    fn e(kind: ExprKind) -> Expr {
        Expr::new(kind, 0..0)
    }

    fn id(n: &str) -> Expr {
        e(ExprKind::Ident(n.into()))
    }

    fn call(c: Callee, args: Vec<Expr>) -> Expr {
        e(ExprKind::Call(c, args, true))
    }

    /// `let name: str = value; split(name, sep)` — the receiver an inlined
    /// `lines(value)` leaves behind.
    fn inlined_split(name: &str, value: Expr, sep: Expr) -> Expr {
        e(ExprKind::Let(
            name.into(),
            Some(Type::Primitive(aipl_syntax::ast::Primitive::Str)),
            Box::new(value),
            Box::new(call(Callee::Split, vec![id(name), sep])),
        ))
    }

    #[test]
    fn the_chain_is_found_under_an_inlined_parameter_binding() {
        let whole = call(
            Callee::Len,
            vec![inlined_split("self", id("src"), id("nl"))],
        );
        let out = build(&whole, None).expect("`split(..).len()` fuses through the binding");
        let ExprKind::Let(name, ty, value, body) = &out.kind else {
            panic!("the binding should survive, with the fused call under it: {out:?}");
        };
        assert_eq!(name, "self");
        assert!(
            ty.is_some(),
            "the annotation is what the binding inliner kept it for"
        );
        assert!(matches!(&value.kind, ExprKind::Ident(v) if v == "src"));
        assert!(matches!(
            &body.kind,
            ExprKind::Call(Callee::SplitLen, args, _)
                if matches!(&args[0].kind, ExprKind::Ident(v) if v == "self")
        ));
    }

    #[test]
    fn a_remaining_argument_naming_the_binding_is_not_moved_under_it() {
        // `(let f = ..; xs.map(g)).join(sep=f)`: pushed under the `let`, `f` in
        // the separator would name the binding instead of the caller's `f`.
        let receiver = e(ExprKind::Let(
            "f".into(),
            None,
            Box::new(id("inner")),
            Box::new(call(Callee::Map, vec![id("xs"), id("g")])),
        ));
        let whole = call(Callee::Join, vec![receiver, id("f"), id("f"), id("f")]);
        assert!(build(&whole, None).is_none());
    }
}
