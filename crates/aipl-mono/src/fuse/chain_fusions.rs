//! A call whose *receiver* is another call: `xs.filter(p).map(f)` walks the
//! elements twice and builds an array between the two passes, so the pair
//! collapses into `filter_map`, which selects and maps in one — and, when the
//! source is uniquely owned and the element sizing allows, into the source's own
//! buffer.
//!
//! # Reductions
//!
//! A chain may instead end in a *reduction* — a builtin that reduces its receiver
//! to one value, `xs.map(f).minimum()`, rather than producing another sequence.
//! Which builtins are reductions, and how each combines two values, is not a table
//! here but a property of the callee ([`Callee::reduction_step`]), because it is a
//! fact about what the builtin means that other passes may use too. They are one
//! fusion, not one per reduction: `map_reduce(xs, f, step)` maps and combines in a
//! single pass with no mapped array, and what makes it `minimum` rather than
//! anything else is only the `step` (`min`). So adding the next reduction is a line
//! in `reduction_step`, not a row here and not a builtin.
//!
//! What feeds a reduction is a table here, [`STAGES`]: the call whose result
//! the reduction consumes, and the fused builtin that does that call's work and
//! the reduction's together. `map` becomes `map_reduce`; `filter_map` — which is
//! what `filter(p).map(f)` has already become by the time the reduction is
//! reached — becomes `filter_map_reduce`. Each further stage that should feed a
//! reduction is a row there and a builtin that takes the same `step`; the reductions
//! do not change, and neither does a stage's row when a reduction is added.
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
    // `xs.join(sep=s).is_nonempty()`: `join` measures every piece, allocates the
    // whole result and copies each piece in, for a caller that asks only
    // whether anything is there. `join_is_nonempty` stops at the first piece
    // with something in it, and allocates nothing. The outer has no arguments of
    // its own, so the fused call takes `join`'s: the receiver and the three
    // separators.
    ChainFusion {
        inner: Callee::Join,
        outer: Callee::IsNonempty,
        into: Callee::JoinIsNonempty,
    },
    // `xs.join(sep=s).is_empty()`: the same fusion for the other half of the
    // question. Every fusion on `is_nonempty` has a row here for `is_empty`, and
    // the reverse — they are one question asked two ways, and a program should
    // not get a different cost for choosing between them.
    ChainFusion {
        inner: Callee::Join,
        outer: Callee::IsEmpty,
        into: Callee::JoinIsEmpty,
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
    // `xs.sort().first()` is the smallest element and `xs.sort().last()` the
    // largest: `sort` copies the array and orders all of it (a pass of
    // comparisons per level of the merge) to hand back one end of it. `minimum`
    // and `maximum` are the one pass that finds it. Equal elements are the same
    // value, so which of them comes first cannot be told apart.
    ChainFusion {
        inner: Callee::Sort,
        outer: Callee::First,
        into: Callee::Minimum,
    },
    ChainFusion {
        inner: Callee::Sort,
        outer: Callee::Last,
        into: Callee::Maximum,
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
    if outer.is_some_and(|o| stands_aside_for(whole, name, o)) {
        return None;
    }
    let (recv, rest) = args.split_first()?;
    let (bindings, inner) = through_bindings(recv);
    let ExprKind::Call(inner_name, inner_args, _) = &inner.kind else {
        return None;
    };
    // Either a row of the chain table, or a `map` feeding a reduction — which is
    // a call to `map_reduce` with the reduction's step as one more argument.
    let (into, step) = match row(inner_name, name) {
        Some(f) => (f.into.clone(), None),
        None => {
            let step = name.reduction_step()?;
            let stage = stage_row(inner_name)?;
            (stage.into.clone(), Some(step))
        }
    };
    if bindings
        .iter()
        .any(|b| rest.iter().any(|a| mentions_free(a, b.name)))
    {
        return None;
    }
    let mut fused = inner_args.clone();
    fused.extend(rest.iter().cloned());
    // The step is the builtin itself, passed as a function value: spelled as the
    // name a call to it resolves from, which is how a program names one.
    if let Some(step) = step {
        fused.push(Expr::new(
            ExprKind::Ident(step.name().to_string()),
            whole.span.clone(),
        ));
    }
    // Spanned as the whole chain: that is the source the user wrote, and what
    // any later diagnostic should point at. The bindings the call moves under
    // are rebuilt the same way, since the value they now produce is the fused
    // call's.
    let call = Expr::rebuilt(ExprKind::Call(into, fused, *method_style), whole);
    Some(under_bindings(bindings, whole, call))
}

/// A call whose result a reduction can consume, and the fused builtin that does
/// its work and the reduction's in one pass: the stage's own arguments followed
/// by the reduction's `step`.
struct Stage {
    stage: Callee,
    into: Callee,
}

/// Every stage that can feed a reduction. See the module docs.
const STAGES: &[Stage] = &[
    Stage {
        stage: Callee::Map,
        into: Callee::MapReduce,
    },
    // `xs.filter(p).map(f).<reduction>()`, once `filter(p).map(f)` has fused.
    Stage {
        stage: Callee::FilterMap,
        into: Callee::FilterMapReduce,
    },
];

/// The [`STAGES`] row for `stage`, if it can feed a reduction.
fn stage_row(stage: &Callee) -> Option<&'static Stage> {
    STAGES.iter().find(|s| s.stage == *stage)
}

/// Whether `whole` — a call to `name`, the receiver of a call to `outer` — leaves
/// itself for that parent to fuse (see "Standing aside for the parent" on
/// [`build`]).
///
/// A `map` feeding a *reduction* is the one exception to "the parent wins". A
/// `map` whose own receiver is a `filter` has a better partner in that `filter`:
/// they fuse into `filter_map`, and that — a [`STAGES`] row of its own — is what
/// feeds the reduction, as `filter_map_reduce`: one pass, nothing built. Taking the
/// reduction first instead, `map_reduce` over the `filter`'s result, would leave
/// the `filter` a separate pass over every element and an array of the survivors
/// besides.
fn stands_aside_for(whole: &Expr, name: &Callee, outer: &Callee) -> bool {
    if row(name, outer).is_some() {
        return true;
    }
    stage_row(name).is_some()
        && outer.is_reduction()
        && !(*name == Callee::Map && has_filter_receiver(whole))
}

/// Whether the first argument of `call` — its receiver — is a `filter`, possibly
/// under the `let`s an inlined call leaves in front of its body.
fn has_filter_receiver(call: &Expr) -> bool {
    let ExprKind::Call(_, args, _) = &call.kind else {
        return false;
    };
    let Some(receiver) = args.first() else {
        return false;
    };
    let (_, inner) = through_bindings(receiver);
    matches!(&inner.kind, ExprKind::Call(Callee::Filter, ..))
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
