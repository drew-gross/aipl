//! A loop over a derived array: `for (let x : xs.map(f)) { body }` builds the
//! whole mapped array — one allocation, one pass calling `f` on every element —
//! only to walk it once and throw it away. It collapses into a loop over `xs`
//! itself that applies `f` at the top of each iteration:
//!
//! ```text
//! for (let x : xs.map(f)) { body }        →   for (let $e : xs) { let x = f($e); body }
//! for (let x : xs.filter(p)) { body }     →   for (let $e : xs) { if (p($e)) { let x = $e; body } }
//! for (let x : xs.filter_map(p, f)) { .. }→   for (let $e : xs) { if (p($e)) { let x = f($e); .. } }
//! ```
//!
//! No intermediate exists any more: the element goes straight from `xs` through
//! `f` into `body`. A lambda is spliced in as the expression it is (its
//! parameter renamed to the fresh loop variable); a function name becomes a
//! call on it. `filter_map` is here because the chain fusion runs first and has
//! already turned a `.filter(p).map(f)` receiver into it.
//!
//! A loop over `s.split(sep)` is relabelled rather than restructured: the
//! parts are already windows into `s`, so what the array cost was the block
//! holding them and a retain and drop per part. The fused loop asks a cursor
//! for one part per iteration instead ([`Callee::SplitIter`]), and codegen
//! walks it — the same cuts, in the same order, with nothing between the
//! source and the body. This is what `for (let l : src.lines())` becomes, since
//! `lines` is `split("\n")` and reaches the pass inlined. Its one guard is
//! below: the array held a retained view of the source and the cursor does not,
//! so a body that writes what the source is read from keeps the loop as
//! written.
//!
//! A loop over `xs.tuple_windows()` is the same idea with state instead of a
//! function: each pair is the previous element with the current one, so the
//! loop carries the previous element along and builds the pair on the stack
//! per iteration ([`build_windows`]):
//!
//! ```text
//! for (let w : xs.tuple_windows()) { body }
//!   →   let $src = xs;
//!       if (let some($first) = $src[0]) {
//!           mut $prev = $first;
//!           mut $skip = true;
//!           for (let $cur : $src) {
//!               if ($skip) { set $skip = false; }
//!               else { let w = ($prev, $cur); body; set $prev = $cur; };
//!           }
//!       };
//! ```
//!
//! The first element is peeled off by indexing so `$prev` starts as a value of
//! the element type rather than an optional — the pass runs before types are
//! known, so it could not annotate a `mut $prev: T? = none;` — and the loop
//! then skips that element rather than pairing it with itself. Nothing here can
//! change behaviour: no function is called, `xs` is still evaluated once, and
//! the pairs reach `body` in the same order.
//!
//! # What makes this safe
//!
//! The original computes every `f(e)` *before* the first `body` runs; fused,
//! they interleave — `f(e0)`, `body`, `f(e1)`, `body`. Two things could tell
//! the difference, and each is refused:
//!
//! - **`f` doing anything observable.** If `f` prints, its output would now be
//!   interleaved with the body's; if it aborts, the body would now have run on
//!   the earlier elements first. So `f` and `p` must satisfy
//!   [`can_defer`](crate::sink::can_defer) — the same bar a binding has to
//!   clear before it moves later — and a named function must not be in the
//!   undeferrable set. The *body* may do what it likes: with a pure `f` there
//!   is nothing for its effects to reorder against, and a body that returns or
//!   aborts early only skips work nobody could have observed.
//! - **`body` writing something `f` reads.** Originally every `f(e)` saw the
//!   bindings as they were before the loop; fused, later calls would see the
//!   body's updates. Any `set` in the body to a name `f` mentions blocks the
//!   rewrite — deliberately coarse, since the cost is a missed fusion rather
//!   than a wrong one. The receiver `xs` needs no such guard: a `for` evaluates
//!   its iterable once, before the first iteration, in both spellings.
//!
//! Only the mapping function is required to be pure, so this fires on the
//! common shape — a loop that prints or accumulates over a mapped array — which
//! an effect check over the whole loop would refuse every time.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, Expr, ExprKind, MatchArm, Pattern, Primitive, Type};

use super::{through_bindings, under_bindings};
use crate::sink::{can_defer, mentions_free};
use crate::subst::{assigned_names, read_names};

/// One fusable loop shape: a `for` over a call to `over` becomes a `for` over
/// that call's receiver, with the call's arguments — a predicate, a mapping, or
/// both — applied per element instead.
struct LoopFusion {
    over: Callee,
    /// Which of the call's remaining arguments are a `(T) -> bool` predicate
    /// and a `(T) -> U` mapping: `(keep, map)` as argument positions after the
    /// receiver.
    keep: Option<usize>,
    map: Option<usize>,
}

/// Every loop shape the pass knows. See the module docs for how to add one.
const LOOP_FUSIONS: &[LoopFusion] = &[
    LoopFusion {
        over: Callee::Map,
        keep: None,
        map: Some(0),
    },
    LoopFusion {
        over: Callee::Filter,
        keep: Some(0),
        map: None,
    },
    LoopFusion {
        over: Callee::FilterMap,
        keep: Some(0),
        map: Some(1),
    },
];

/// `for (let var : over(recv, fs..)) { body }` as a loop over `recv` applying
/// the [`LOOP_FUSIONS`] row's functions per element.
///
/// `blocked` is the undeferrable set — effectful and aborting functions, closed
/// over the call graph — that a mapping function must stay clear of.
pub(super) fn build(whole: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let ExprKind::For(var, index, iterable, body) = &whole.kind else {
        return None;
    };
    // The derived array may sit under the `let`s an inlined call leaves in
    // front of its body ([`through_bindings`]) — `for (let l : src.lines())` is
    // a `for` over `let self: str = src; split(self, "\n")`, which is the shape
    // the whole family would otherwise miss at exactly the sites inlining
    // creates. The loop is rebuilt under those bindings, which changes nothing
    // about when they run: a `for` evaluates its iterable, and so everything the
    // iterable is built from, before the first iteration either way. What it
    // changes is the *body*'s scope — it now sits inside them — so a body
    // mentioning a bound name keeps the loop as written.
    let (bindings, iterable) = through_bindings(iterable);
    if bindings
        .iter()
        .any(|b| b.name != var && mentions_free(body, b.name))
    {
        return None;
    }
    let rebuild = |fused: Expr| under_bindings(bindings, whole, fused);
    let ExprKind::Call(name, args, _) = &iterable.kind else {
        return None;
    };
    if *name == Callee::TupleWindows {
        let [recv] = args.as_slice() else {
            return None;
        };
        return Some(rebuild(build_windows(whole, var, index, recv, body)));
    }
    if *name == Callee::Split {
        // `for (let part : s.split(sep))`: the parts are handed to the body one
        // at a time by a cursor instead of an array of every one being built
        // first — a codegen matter, so the iterable is only relabelled here
        // (see `Callee::SplitIter`). Nothing the body can observe moves: `s` and
        // `sep` are still evaluated once, in that order, before the first
        // iteration, and the parts reach the body in the same order. They are
        // the same windows into `s`, too — the array's elements were views, not
        // copies.
        let [recv, sep] = args.as_slice() else {
            return None;
        };
        // One thing does change: the array of parts held a *retained* view of
        // the source, where the cursor holds it without a reference of its own,
        // exactly as the char cursor holds the string a `for (let c : s)` walks.
        // So a body that writes a binding the source or the separator is read
        // from could free the bytes the walk is still cutting. Refuse that, the
        // way the mapping families refuse a body that writes what `f` reads.
        let mut written = HashSet::new();
        assigned_names(body, &mut written);
        let mut read = HashSet::new();
        read_names(recv, &mut read);
        read_names(sep, &mut read);
        if read.iter().any(|n| written.contains(n)) {
            return None;
        }
        let wrapped = Expr::rebuilt(
            ExprKind::Call(Callee::SplitIter, vec![recv.clone(), sep.clone()], false),
            iterable,
        );
        return Some(rebuild(Expr::rebuilt(
            ExprKind::For(var.clone(), index.clone(), Box::new(wrapped), body.clone()),
            whole,
        )));
    }
    if *name == Callee::Reverse {
        // `for (let v : xs.reverse())`: the walk runs backwards instead of a
        // reversed view being built — a codegen matter, so the iterable is only
        // relabelled here (see `Callee::ReverseIter`). Nothing observable changes:
        // `xs` is evaluated once either way, and the elements reach the body
        // in the same order.
        let [recv] = args.as_slice() else {
            return None;
        };
        let wrapped = Expr::rebuilt(
            ExprKind::Call(Callee::ReverseIter, vec![recv.clone()], false),
            iterable,
        );
        return Some(rebuild(Expr::rebuilt(
            ExprKind::For(var.clone(), index.clone(), Box::new(wrapped), body.clone()),
            whole,
        )));
    }
    // Every fusion below loops over the *source* sequence with the derivation
    // folded into the body, so an index binder would change what it counts: over
    // `xs.filter(p)` it numbers the survivors, and over `xs` with the test
    // inlined it would number every element tested. `tuple_windows` declines
    // above for the same reason. What is exempt is the two *relabellings*,
    // `split` and `reverse`: those walk exactly the same elements, so the index
    // numbers exactly what it did. Declining is always sound — this is an
    // optimization.
    if index.is_some() {
        return None;
    }
    let f = LOOP_FUSIONS.iter().find(|f| f.over == *name)?;
    let (recv, fns) = args.split_first()?;
    // Wrong arity is mono's error to report, not a shape to rewrite.
    if fns.len() != f.keep.is_some() as usize + f.map.is_some() as usize {
        return None;
    }
    let keep = f.keep.map(|i| &fns[i]);
    let map = f.map.map(|i| &fns[i]);
    // Each function must be pure (see the module docs), and must not read
    // anything the body writes.
    let mut written = HashSet::new();
    assigned_names(body, &mut written);
    for func in [keep, map].into_iter().flatten() {
        if !is_pure(func, blocked) {
            return None;
        }
        let mut read = HashSet::new();
        read_names(func, &mut read);
        if read.iter().any(|n| written.contains(n)) {
            return None;
        }
    }

    // The fresh loop variable: the element as `recv` yields it, before `f`.
    let elem = format!("$fuse{}_{}", crate::next_inline_id(), var);
    let elem_ref = || Expr::new(ExprKind::Ident(elem.clone()), iterable.span.clone());
    // `let var = f($e); body` — or `let var = $e; body` with no mapping. The
    // binding is spanned as the call it replaces: that is what the user wrote.
    let mapped = match map {
        Some(func) => apply(func, elem_ref())?,
        None => elem_ref(),
    };
    let mut inner = Expr::new(
        ExprKind::Let(var.clone(), None, Box::new(mapped), body.clone()),
        iterable.span.clone(),
    );
    if let Some(pred) = keep {
        // `if (p($e)) { ..inner.. }` — the body's value is discarded either way,
        // so both branches yield unit.
        let unit = || Expr::new(ExprKind::Unit, iterable.span.clone());
        inner = Expr::new(
            ExprKind::If(
                Box::new(apply(pred, elem_ref())?),
                Box::new(Expr::new(
                    ExprKind::Seq(Box::new(inner), Box::new(unit())),
                    iterable.span.clone(),
                )),
                Box::new(unit()),
            ),
            iterable.span.clone(),
        );
    }
    Some(rebuild(Expr::rebuilt(
        ExprKind::For(elem, None, Box::new(recv.clone()), Box::new(inner)),
        whole,
    )))
}

/// `for (let var : recv.tuple_windows()) { body }` as the previous-element loop
/// in the module docs.
///
/// An `index` binder is carried rather than declined, and it is the one fusion
/// here where that takes real work: the fused loop walks the *source*, which has
/// one element more than it has windows, so a per-iteration counter would be
/// wrong. The counter lives outside the loop and is bumped only in the branch
/// that actually produces a window, which numbers the windows exactly as the
/// unfused loop does. Worth the work because declining is not free: `unescape`
/// in `aipl-codegen` is an indexed `tuple_windows` loop in the lexer's path, and
/// without the fusion it allocates the whole window array (measured: 6
/// allocations became 25, and instructions rose by a third).
fn build_windows(
    whole: &Expr,
    var: &str,
    index: &Option<String>,
    recv: &Expr,
    body: &Expr,
) -> Expr {
    let id = crate::next_inline_id();
    let sp = || recv.span.clone();
    let name = |what: &str| format!("$fuse{id}_{what}");
    let ident = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), sp());
    let unit = || Expr::new(ExprKind::Unit, sp());
    let (src, first, prev, skip, cur, widx) = (
        name("src"),
        name("first"),
        name("prev"),
        name("skip"),
        name("cur"),
        name("widx"),
    );
    // `set $skip = false;`
    let unskip = Expr::new(
        ExprKind::Assign(
            Box::new(ident(&skip)),
            Box::new(Expr::new(ExprKind::Bool(false), sp())),
            Box::new(unit()),
        ),
        sp(),
    );
    // `let var = ($prev, $cur); body; set $prev = $cur;` — the pair is spanned as
    // the call it replaces, so a diagnostic about it points at what was written.
    let pair = Expr::new(ExprKind::TupleLit(vec![ident(&prev), ident(&cur)]), sp());
    // `set $widx = $widx + 1;`, in this branch only — the branch that produced a
    // window. Nothing when the loop has no index binder.
    let bump = match index {
        None => unit(),
        Some(_) => Expr::new(
            ExprKind::Assign(
                Box::new(ident(&widx)),
                Box::new(crate::op_call(
                    Callee::WrappingAdd,
                    vec![ident(&widx), Expr::new(ExprKind::Num(1), sp())],
                    sp(),
                )),
                Box::new(unit()),
            ),
            sp(),
        ),
    };
    let advance = Expr::new(
        ExprKind::Assign(
            Box::new(ident(&prev)),
            Box::new(ident(&cur)),
            Box::new(bump),
        ),
        sp(),
    );
    let run = Expr::new(
        ExprKind::Seq(Box::new(body.clone()), Box::new(advance)),
        sp(),
    );
    // The index is re-bound immutably per window, inside the pair's binding, so
    // the body cannot move the counter out from under the loop.
    let run = match index {
        None => run,
        Some(i) => Expr::new(
            ExprKind::Let(i.clone(), None, Box::new(ident(&widx)), Box::new(run)),
            sp(),
        ),
    };
    let paired = Expr::new(
        ExprKind::Let(var.to_string(), None, Box::new(pair), Box::new(run)),
        sp(),
    );
    let step = Expr::new(
        ExprKind::If(Box::new(ident(&skip)), Box::new(unskip), Box::new(paired)),
        sp(),
    );
    // A loop's own value is an `i64`; sequenced to unit so both sides of the
    // `if let` below agree.
    let loop_ = Expr::new(
        ExprKind::Seq(
            Box::new(Expr::rebuilt(
                ExprKind::For(cur, None, Box::new(ident(&src)), Box::new(step)),
                whole,
            )),
            Box::new(unit()),
        ),
        sp(),
    );
    let carried = Expr::new(
        ExprKind::LetMut(
            prev,
            None,
            Box::new(ident(&first)),
            Box::new(Expr::new(
                ExprKind::LetMut(
                    skip,
                    None,
                    Box::new(Expr::new(ExprKind::Bool(true), sp())),
                    Box::new(loop_),
                ),
                sp(),
            )),
        ),
        sp(),
    );
    // `if (let some($first) = $src[0]) { .. }` — no first element, no pairs.
    let peeled = Expr::new(
        ExprKind::IfLet(
            Box::new(MatchArm {
                pattern: Pattern::Ctor {
                    name: "some".to_string(),
                    bindings: vec![first],
                    ignore_payload: false,
                },
                body: carried,
                span: sp(),
            }),
            Box::new(Expr::new(
                ExprKind::Index(
                    Box::new(ident(&src)),
                    Box::new(Expr::new(ExprKind::Num(0), sp())),
                ),
                sp(),
            )),
            Box::new(unit()),
        ),
        sp(),
    );
    // The window counter, outside the loop so it survives across iterations and
    // is re-declared per entry to this loop — which is what resets it when the
    // loop is nested inside another.
    let counted = match index {
        None => peeled,
        Some(_) => Expr::new(
            ExprKind::LetMut(
                widx,
                Some(Type::Primitive(Primitive::U64)),
                Box::new(Expr::new(ExprKind::Num(0), sp())),
                Box::new(peeled),
            ),
            sp(),
        ),
    };
    Expr::rebuilt(
        ExprKind::Let(src, None, Box::new(recv.clone()), Box::new(counted)),
        whole,
    )
}

/// `func` applied to `arg`: a one-parameter lambda is spliced in as its body
/// with the parameter renamed to `arg`'s name; a bare name becomes a call on
/// it. Anything else is not a shape `map` accepts, and is left for mono to
/// report.
pub(crate) fn apply(func: &Expr, arg: Expr) -> Option<Expr> {
    let ExprKind::Ident(arg_name) = &arg.kind else {
        unreachable!("the fused loop variable is always a bare identifier");
    };
    match &func.kind {
        ExprKind::Lambda(params, body) => {
            let [param] = params.as_slice() else {
                return None;
            };
            let map = HashMap::from([(param.name.clone(), arg_name.clone())]);
            Some(crate::rename_params(body, &map))
        }
        ExprKind::Ident(g) => Some(Expr::new(
            ExprKind::Call(Callee::resolve(g.clone()), vec![arg], false),
            func.span.clone(),
        )),
        _ => None,
    }
}

/// Whether calling `func` per element can be moved into the loop unobserved —
/// see the module docs.
pub(crate) fn is_pure(func: &Expr, blocked: &HashSet<String>) -> bool {
    match &func.kind {
        ExprKind::Lambda(_, body) => can_defer(body, blocked),
        ExprKind::Ident(g) => !blocked.contains(g),
        _ => false,
    }
}
