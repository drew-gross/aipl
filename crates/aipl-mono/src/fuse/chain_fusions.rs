//! A call whose *receiver* is another call: `xs.filter(p).map(f)` walks the
//! elements twice and builds an array between the two passes, so the pair
//! collapses into `filter_map`, which selects and maps in one — and, when the
//! source is uniquely owned and the element sizing allows, into the source's own
//! buffer.
//!
//! The receiver may reach here under the `let`s inlining leaves in front of an
//! inlined body; the chain is found beneath them and the fused call put back
//! there (see [`build`]).

use aipl_syntax::ast::{Callee, Expr, ExprKind, Type};

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
];

/// `inner(recv, ..).outer(..)` as a single call to the [`CHAIN_FUSIONS`] row's
/// `into`, taking the inner argument list followed by the outer's rest.
///
/// Bottom-up traversal means the receiver has already been fused if it could
/// be, so a row composes with the others rather than racing them.
///
/// The inner call may sit under `let`s: inlining a function binds each of its
/// parameters ahead of its body, with the parameter's declared type, and the
/// binding inliner deliberately leaves an annotated binding alone — so
/// `s.lines().len()` reaches here as `(let self: str = s; split(self, "\n")).len()`,
/// and the chain would otherwise be hidden at exactly the sites inlining
/// creates. The outer call moves in under the bindings, which changes nothing
/// about when anything runs: the bound values were evaluated before the outer's
/// remaining arguments already, as the receiver always is. What it could change
/// is what a name in those arguments refers to, so an argument mentioning a
/// bound name keeps the shape as written.
pub(super) fn build(whole: &Expr) -> Option<Expr> {
    let ExprKind::Call(name, args, method_style) = &whole.kind else {
        return None;
    };
    let (recv, rest) = args.split_first()?;
    let (bindings, inner) = through_bindings(recv);
    let ExprKind::Call(inner_name, inner_args, _) = &inner.kind else {
        return None;
    };
    let f = CHAIN_FUSIONS
        .iter()
        .find(|f| f.outer == *name && f.inner == *inner_name)?;
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
    Some(bindings.into_iter().rev().fold(call, |body, b| {
        Expr::rebuilt(
            ExprKind::Let(
                b.name.to_string(),
                b.ty.clone(),
                Box::new(b.value.clone()),
                Box::new(body),
            ),
            whole,
        )
    }))
}

/// One `let` a receiver sits under — see [`build`].
struct Binding<'a> {
    name: &'a str,
    ty: &'a Option<Type>,
    value: &'a Expr,
}

/// The `let`s leading `e`, outermost first, and the expression under them.
fn through_bindings(e: &Expr) -> (Vec<Binding<'_>>, &Expr) {
    let mut bindings = Vec::new();
    let mut e = e;
    while let ExprKind::Let(name, ty, value, body) = &e.kind {
        bindings.push(Binding { name, ty, value });
        e = body;
    }
    (bindings, e)
}

#[cfg(test)]
mod tests {
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
        let out = build(&whole).expect("`split(..).len()` fuses through the binding");
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
        assert!(build(&whole).is_none());
    }
}
