//! Push a `len(..)` inward until it reaches something that already knows the
//! answer, so the sequence it was measuring never has to be built.
//!
//! `is_empty(..)` and `is_nonempty(..)` are the same question asked for one
//! bit, and take the same road: a `len` that stops at an array literal answers
//! with its element count, and these answer with `true` or `false`. Only the rows
//! that are about a *count* — the spread accumulator, `filter`, `map_join`, the
//! optional `map` — are `len`'s alone; the control-flow rows and the
//! length-preserving calls are shared. See [`Ask`]. So a function that builds a
//! table of arrays, asked `is_nonempty` of its result, becomes a `match` that
//! answers `true` or `false` per arm and never builds one.
//!
//! An *index* — `f(..)[i]`, a `T?` out of a `T[]` — is the third question, and
//! the one that needs the most care ([`fold_index`]): it goes through `if`/`match`
//! only while the index is a literal or a name, and out of a literal only when
//! picking the element cannot change a type.
//!
//! `len` is the one builtin that reads nothing but a count, and a surprising
//! amount of code hands it a sequence assembled for no other purpose:
//! `group_styles(r).len()` in `grammar.aipl` builds a `ListStyle[]` of every
//! group in a rule tree — recursively, one array per node — so that `link_rule`
//! can ask how many there are. Nothing ever looks at an element.
//!
//! So this pass treats `len(e)` as a question about `e`'s *shape* rather than
//! its value, and answers it from the shape where it can:
//!
//! ```text
//! len([])                          →  0
//! len([a, b])                      →  2
//! len([a, ..xs, ..ys])             →  1 + len(xs) + len(ys)
//! len(xs.map(f))                   →  len(xs)              (one out per one in)
//! len(xs.filter(p))                →  xs.count_if(p)
//! len(xs.map(f).join())            →  Σ len(f(x)) for x in xs
//! len(opt.map(f).value_or([]))     →  match (opt) { some(x) => len(f(x)), none => 0 }
//! len(if (c) { a } else { b })     →  if (c) { len(a) } else { len(b) }
//! len(match (s) { .. })            →  match (s) { .. each arm measured .. }
//! ```
//!
//! The control-flow rows are what make the rest reach anything: a function that
//! builds an array builds it in the arms of a `match`, so the `len` has to get
//! past the `match` before it ever meets an array literal. Every rewrite leaves
//! a `len(..)` of something smaller, and the pass re-folds its own output, so
//! one `len` walks as deep as the shapes go in a single visit.
//!
//! Where it stops is a `len` of a *call*: this pass cannot measure
//! `len(group_styles(r))`, because the answer is inside another function's body.
//! That is [`crate::counting_variants`]'s half — it mints the counting variant
//! `group_styles$len` and rewrites the call — and the two halves feed each other
//! through the pass manager's rounds, which is what turns one `len` at the top
//! into a whole recursive family that never allocates.
//!
//! # Why an array literal's spread is not a case here
//!
//! `[a, ..xs]` never reaches this pass as an array literal. The loader desugars
//! any literal holding a spread into a reserve-and-append block
//! (`desugar_spread` in `aipl-loader`), and the size it reserves past the seed
//! is *already* the sum of the spread operands' lengths — the loader needs it to
//! size the allocation. So the rewrite for that shape does not compute anything:
//! it drops the appends and keeps the arithmetic the loader wrote
//! ([`spread_accumulator`]).
//!
//! # What makes it safe
//!
//! Measuring instead of building means some expressions are never evaluated —
//! the elements of a literal, the per-element results of a `map`. That is
//! unobservable for pure operands and a behaviour change for anything else, so
//! every dropped expression must clear [`can_defer`], the same bar the loop
//! family sets on a mapping function it moves. The check costs a missed fold and
//! never a wrong one.
//!
//! Two rows deserve their reasoning spelled out, because what they drop is less
//! obvious than an element:
//!
//! - **`len(xs.map(f))` → `len(xs)`** drops every call to `f`. It is sound on
//!   the count because the language guarantees it, and which builtins make that
//!   promise is [`Callee::preserves_length`] — stated there rather than here
//!   because the loader's array-spread desugaring needs the same fact for the
//!   same reason, and the two must not drift.
//! - **`len(xs.map(f).join())`** keeps every call to `f` — the sum calls it once
//!   per element, exactly as `map_join` does — and discards only the pieces.
//!   `f` is still required to be pure, uniformly with the rest of the table,
//!   rather than this row carrying a weaker rule of its own.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Expr, ExprKind, Item, MatchArm, Pattern, Primitive, Program, Type};

use crate::fuse::loop_fusions::{apply, is_pure};
use crate::fuse::{through_bindings, under_bindings};
use crate::passes::Scope;
use crate::sink::{can_defer, mentions_free, undeferrable_fns};

/// Fold every measurable `len(..)` in `program`.
///
/// `effectful` names the functions whose call carries an effect — the caller
/// supplies it because the effect declarations live with the builtin
/// signatures, which this crate does not parse.
/// `scope` names the functions to rewrite: the pass is body-local, so a
/// function the scope leaves out would come back unchanged anyway (see
/// [`crate::passes`]).
pub fn fold_lengths(program: &Program, effectful: &HashSet<String>, scope: &Scope) -> Program {
    let blocked = undeferrable_fns(program, effectful);
    Program {
        // Rewrites bodies only; the file map and the file's own documentation
        // carry through unchanged.
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) if scope.covers(&f.name) => {
                    let mut f = f.clone();
                    f.body = fold_expr(&f.body, &blocked);
                    f.test_body = f.test_body.as_ref().map(|t| fold_expr(t, &blocked));
                    Item::Fn(f)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// `e` with every measurable `len(..)` inside it folded.
///
/// Top-down, and it re-folds what it produces: each rewrite leaves a `len` of
/// something smaller, so one visit follows a `len` all the way down to the
/// literals rather than needing a round of the pass manager per level.
pub(crate) fn fold_expr(e: &Expr, blocked: &HashSet<String>) -> Expr {
    fold_in(e, &mut Vec::new(), blocked)
}

/// [`fold_expr`] carrying the hoisted keyword arguments that hold an empty
/// sequence — see [`is_empty_sequence`] for why that one fact has to travel.
fn fold_in(e: &Expr, empty: &mut Vec<String>, blocked: &HashSet<String>) -> Expr {
    if let ExprKind::Call(callee, args, _) = &e.kind {
        if let (Some(ask), [arg]) = (Ask::of(callee), args.as_slice()) {
            if let Some(folded) = fold_length(e, arg, empty, blocked, ask) {
                return fold_in(&folded, empty, blocked);
            }
        }
    }
    if let ExprKind::Index(operand, index) = &e.kind {
        if let Some(folded) = fold_index(e, operand, index, blocked) {
            return fold_in(&folded, empty, blocked);
        }
    }
    // A `let` is in scope for its body and nothing else, so it is recorded for
    // exactly that descent.
    if let ExprKind::Let(name, ty, value, body) = &e.kind {
        let value = fold_in(value, empty, blocked);
        let noted = note_empty(name, &value, empty);
        let body = fold_in(body, empty, blocked);
        empty.truncate(empty.len() - noted as usize);
        return Expr::rebuilt(
            ExprKind::Let(name.clone(), ty.clone(), Box::new(value), Box::new(body)),
            e,
        );
    }
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = fold_in(child, empty, blocked);
    }
    out
}

/// Record `name` if it is a hoisted keyword argument bound to an empty
/// sequence; the `bool` is whether anything was pushed.
fn note_empty(name: &str, value: &Expr, empty: &mut Vec<String>) -> bool {
    let is_kwarg = name.starts_with(aipl_syntax::KWARG_ARG_PREFIX);
    let push = is_kwarg && matches!(&value.kind, ExprKind::ArrayLit(e) if e.is_empty());
    if push {
        empty.push(name.to_string());
    }
    push
}

/// Whether [`fold_expr`] would rewrite anything in `e` — what
/// [`crate::counting_variants`] asks before minting a counting variant whose
/// whole point is that this pass can measure its body.
pub(crate) fn folds_anything(e: &Expr, blocked: &HashSet<String>) -> bool {
    fold_expr(e, blocked) != *e
}

/// Whether evaluating `e` may be skipped entirely.
///
/// [`can_defer`] is most of it — nothing observable, nothing that leaves the
/// expression. The extra condition is about a diagnostic rather than about
/// behaviour: whether a `map` is *legal* depends on its receiver's type, which
/// nothing before monomorphization knows, and monomorphization refuses a set
/// receiver (`expand_map`: a set's `map` is the one case whose result can be
/// shorter than its receiver, so `set_map` is the name for it). A `map` this
/// pass deleted is a `map` monomorphization never sees — so
/// `[..s.map(f)].len()` would stop being the error it is and start answering
/// with a count. Dropping a subtree that holds one is therefore refused, and
/// `len(xs.map(f))` is not folded at all even though `map` does preserve
/// length.
///
/// The cost is a missed fold wherever a legitimate array's `map` sits inside
/// something measured. The loader's own use of
/// [`Callee::preserves_length`](aipl_syntax::ast::Callee::preserves_length) is
/// not affected: it sizes an allocation from the receiver's length and leaves
/// the `map` itself in place to be evaluated, so monomorphization still sees
/// it.
fn can_drop(e: &Expr, blocked: &HashSet<String>) -> bool {
    can_defer(e, blocked) && !consumes_a_map(e)
}

/// Whether `e` holds a `map` call anywhere — see [`can_drop`].
fn consumes_a_map(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Call(Callee::Map, ..))
        || crate::children(e).iter().any(|c| consumes_a_map(c))
}

/// `len(arg)` — `whole` being that call — measured without building `arg`, or
/// `None` when nothing here recognizes its shape.
///
/// `arg` may sit under the `let`s an inlined call leaves in front of its body
/// ([`through_bindings`]), so the shape is looked for beneath them and the
/// measurement put back there. The bindings stay: their values are still
/// evaluated, in the same order, and only what they feed is dropped.
fn fold_length(
    whole: &Expr,
    arg: &Expr,
    outer: &[String],
    blocked: &HashSet<String>,
    ask: Ask,
) -> Option<Expr> {
    let (bindings, inner) = through_bindings(arg);
    let counted = {
        // The argument's own `let`s are in scope for the shape beneath them, so
        // a keyword argument hoisted there counts too.
        let mut empty: Vec<String> = outer.to_vec();
        for b in &bindings {
            note_empty(b.name, b.value, &mut empty);
        }
        count_of(whole, inner, &empty, blocked, ask)?
    };
    Some(under_bindings(bindings, whole, counted))
}

/// What is being asked of a sequence by a call this pass folds.
///
/// The question decides two things: the call a branch is asked with (`measure`
/// below) and what a literal answers with. Everything about *getting to* the
/// literal — through `let`s, `if`s, `match`es, the length-preserving calls — is
/// the same for all three, which is why they share a table instead of each
/// having one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// `len(..)`: how many.
    Len,
    /// `is_empty(..)`: whether none.
    IsEmpty,
    /// `is_nonempty(..)`: whether any.
    IsNonempty,
}

impl Ask {
    /// The question `callee` asks, if it is one this pass folds.
    fn of(callee: &Callee) -> Option<Ask> {
        match callee {
            Callee::Len => Some(Ask::Len),
            Callee::IsEmpty => Some(Ask::IsEmpty),
            Callee::IsNonempty => Some(Ask::IsNonempty),
            _ => None,
        }
    }

    /// The call that asks it.
    fn callee(self) -> Callee {
        match self {
            Ask::Len => Callee::Len,
            Ask::IsEmpty => Callee::IsEmpty,
            Ask::IsNonempty => Callee::IsNonempty,
        }
    }

    /// The answer for a sequence of `n` elements, as a literal.
    fn answer(self, n: usize, span: aipl_syntax::Span) -> Expr {
        let kind = match self {
            Ask::Len => ExprKind::Num(n as i64),
            Ask::IsEmpty => ExprKind::Bool(n == 0),
            Ask::IsNonempty => ExprKind::Bool(n != 0),
        };
        Expr::new(kind, span)
    }
}

/// The answer to `ask` about `e`, computed from its shape rather than by
/// building it.
fn count_of(
    whole: &Expr,
    e: &Expr,
    empty: &[String],
    blocked: &HashSet<String>,
    ask: Ask,
) -> Option<Expr> {
    let span = || whole.span.clone();
    let measure = |x: &Expr| Expr::new(ExprKind::Call(ask.callee(), vec![x.clone()], true), span());
    // A rewritten node keeps the whole `len(..)` call's span: that is the source
    // the user wrote, and what a later diagnostic should point at.
    let like = |kind| Expr::rebuilt(kind, whole);
    match &e.kind {
        // `[a, b]` — the element count, with nothing evaluated. A literal
        // holding a spread is the accumulator case below, not this one (see the
        // module docs), so every element here is a plain one.
        ExprKind::ArrayLit(elems) => {
            if elems
                .iter()
                .any(|x| matches!(x.kind, ExprKind::Spread(_)) || !can_drop(x, blocked))
            {
                return None;
            }
            Some(ask.answer(elems.len(), span()))
        }
        // `""` — nothing to build, and the text says how many.
        ExprKind::Str(text) if ask != Ask::Len => Some(ask.answer(text.len(), span())),
        // `[a, ..xs]` as the loader left it: the appends go, and the size it
        // reserved past the seed stays, being the length they would have added.
        ExprKind::LetMut(..) if ask == Ask::Len => {
            let acc = spread_accumulator(e)?;
            if !acc.appended.iter().all(|p| can_drop(p.value, blocked)) {
                return None;
            }
            Some(add(measure(acc.seed), acc.extra.clone(), span()))
        }
        // `if`/`match`/`if let` — measure each branch instead of the value they
        // produce. These are what let the rest of the table reach anything: a
        // function that builds a sequence builds it per branch.
        ExprKind::If(c, a, b) => Some(like(ExprKind::If(
            c.clone(),
            Box::new(measure(a)),
            Box::new(measure(b)),
        ))),
        ExprKind::Match(scrutinee, arms) => Some(like(ExprKind::Match(
            scrutinee.clone(),
            arms.iter().map(|a| measured_arm(a, &measure)).collect(),
        ))),
        ExprKind::IfLet(arm, scrutinee, els) => Some(like(ExprKind::IfLet(
            Box::new(measured_arm(arm, &measure)),
            scrutinee.clone(),
            Box::new(measure(els)),
        ))),
        // The statement forms: whatever ran before the value still runs, and the
        // value itself is what gets measured.
        ExprKind::Seq(first, rest) => {
            Some(like(ExprKind::Seq(first.clone(), Box::new(measure(rest)))))
        }
        ExprKind::Assign(lhs, value, rest) => Some(like(ExprKind::Assign(
            lhs.clone(),
            value.clone(),
            Box::new(measure(rest)),
        ))),
        ExprKind::Call(callee, args, method_style) => {
            match callee {
                // The rows below are about a count, which a yes/no cannot use.
                Callee::Filter | Callee::FilterMap | Callee::MapJoin | Callee::ValueOr
                    if ask != Ask::Len =>
                {
                    None
                }
                // `xs.filter(p)` counts rather than collects. `p` runs once per
                // element either way; only the kept elements are not copied,
                // and copying is not observable — so unlike the mapping rows
                // this one needs nothing of `p`. `filter_map`'s mapping *is*
                // dropped, so that one does.
                Callee::Filter | Callee::FilterMap => {
                    let [xs, keep, rest @ ..] = args.as_slice() else {
                        return None;
                    };
                    if *callee == Callee::FilterMap {
                        let [map] = rest else { return None };
                        if !is_pure(map, blocked) {
                            return None;
                        }
                    } else if !rest.is_empty() {
                        return None;
                    }
                    Some(like(ExprKind::Call(
                        Callee::CountIf,
                        vec![xs.clone(), keep.clone()],
                        *method_style,
                    )))
                }
                // `xs.map(f).join()` — the fusion pass has already collapsed
                // the pair, so this is one call (see `fuse::chain_fusions`).
                Callee::MapJoin => sum_of_parts(whole, args, empty, blocked),
                // `opt.map(f).value_or([])` — the only `value_or` worth folding:
                // the default is what pins the result to a sequence, and the
                // `map` is what the rewrite reaches through, so that `f`'s
                // result is measured where it is produced instead of being
                // wrapped, unwrapped and thrown away.
                Callee::ValueOr => optional_count(whole, args, empty, blocked),
                // `xs.sort()`, `xs.reverse()` — one element out per element in
                // ([`Callee::preserves_length`]), so the receiver answers for
                // the result. Whatever the call does per element is dropped
                // outright here, so every remaining argument has to be pure;
                // these two have none and clear that trivially.
                //
                // `map` preserves length too and is deliberately not folded
                // here — see [`consumes_a_map`].
                _ if callee.preserves_length() && *callee != Callee::Map => {
                    let [xs, fns @ ..] = args.as_slice() else {
                        return None;
                    };
                    if !fns.iter().all(|f| is_pure(f, blocked)) {
                        return None;
                    }
                    Some(measure(xs))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// `operand[index]` answered from the shape of `operand` instead of by building
/// it, or `None` when nothing here recognizes the shape.
///
/// ```text
/// [a, b][1]                          →  some(b)
/// [][i]                              →  none
/// match (s) { "x" => [a], _ => [] }[0]
///     →  match (s) { "x" => some(a), _ => none }
/// ```
///
/// Two things make an index touchier than a count, and each has a guard:
///
/// - **The index is read after the operand, in a different place.** Pushing
///   `[i]` into the arms of a `match` evaluates it once the arm has run, so it
///   must be something whose evaluation cannot be told apart from there: a
///   literal, or a name ([`stable_index`]). A name an enclosing `let` binds, that
///   this walk would carry the index under, is a different name there and is
///   refused.
/// - **The element comes out of its array.** A bare number in `[1, 2]` takes its
///   width from the array it sits in (`u8[]`), and `some(1)` on its own is
///   `i64`. The checker has already run, so nothing would catch the change; an
///   element that mentions a number is therefore left where it is
///   ([`mentions_number`]), and the fold is missed rather than wrong. The
///   elements this is for — strings, names, calls — carry their own types.
///
/// An index past the end of a *non-empty* literal is `none` too, but is not
/// folded: nothing records the type of that `none`, and the element type can
/// only be read off an empty literal's recorded type.
///
/// `none` is typed from the empty literal it replaces: an empty `[]` is
/// context-typed, so the checker recorded its type on it ([`Expr::ty`]), and the
/// `none` that stands in for an index into it is that type made optional. An
/// empty literal with no recorded type is not folded.
fn fold_index(
    whole: &Expr,
    operand: &Expr,
    index: &Expr,
    blocked: &HashSet<String>,
) -> Option<Expr> {
    if !stable_index(index) {
        return None;
    }
    let (bindings, inner) = through_bindings(operand);
    if let ExprKind::Ident(name) = &index.kind {
        if bindings.iter().any(|b| b.name == name) {
            return None;
        }
    }
    let picked = index_of(whole, inner, index, blocked)?;
    Some(under_bindings(bindings, whole, picked))
}

/// Whether `index` can be evaluated anywhere in the operand's arms and mean the
/// same: a literal, or a name.
fn stable_index(index: &Expr) -> bool {
    matches!(index.kind, ExprKind::Num(_) | ExprKind::Ident(_))
}

/// Whether `e` mentions a number literal anywhere — see [`fold_index`].
fn mentions_number(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Num(_)) || crate::children(e).iter().any(|c| mentions_number(c))
}

/// `e[index]`, computed from `e`'s shape. `whole` is the index expression, whose
/// span a rewritten node keeps.
fn index_of(whole: &Expr, e: &Expr, index: &Expr, blocked: &HashSet<String>) -> Option<Expr> {
    let like = |kind| Expr::rebuilt(kind, whole);
    let at = |x: &Expr| {
        Expr::rebuilt(
            ExprKind::Index(Box::new(x.clone()), Box::new(index.clone())),
            whole,
        )
    };
    match &e.kind {
        ExprKind::ArrayLit(elems) => {
            let droppable = |skip: Option<usize>| {
                elems.iter().enumerate().all(|(i, x)| {
                    Some(i) == skip
                        || (!matches!(x.kind, ExprKind::Spread(_)) && can_drop(x, blocked))
                })
            };
            match &index.kind {
                ExprKind::Num(k) => {
                    let k = usize::try_from(*k).ok()?;
                    match elems.get(k) {
                        Some(picked) => {
                            if mentions_number(picked) || !droppable(Some(k)) {
                                return None;
                            }
                            Some(like(ExprKind::Call(
                                Callee::Some,
                                vec![picked.clone()],
                                false,
                            )))
                        }
                        None => none_for_empty(whole, e, elems),
                    }
                }
                // Nothing is there at any position.
                _ => none_for_empty(whole, e, elems),
            }
        }
        ExprKind::If(c, a, b) => Some(like(ExprKind::If(
            c.clone(),
            Box::new(at(a)),
            Box::new(at(b)),
        ))),
        ExprKind::Match(scrutinee, arms) => Some(like(ExprKind::Match(
            scrutinee.clone(),
            arms.iter()
                .map(|a| MatchArm {
                    written: a.written.clone(),
                    pattern: a.pattern.clone(),
                    body: at(&a.body),
                    span: a.span.clone(),
                })
                .collect(),
        ))),
        ExprKind::IfLet(arm, scrutinee, els) => Some(like(ExprKind::IfLet(
            Box::new(MatchArm {
                written: arm.written.clone(),
                pattern: arm.pattern.clone(),
                body: at(&arm.body),
                span: arm.span.clone(),
            }),
            scrutinee.clone(),
            Box::new(at(els)),
        ))),
        _ => None,
    }
}

/// `none`, of the element type of the empty literal `e` — or `None` for a
/// literal that has elements, or has no recorded type to take it from.
fn none_for_empty(whole: &Expr, e: &Expr, elems: &[Expr]) -> Option<Expr> {
    if !elems.is_empty() {
        return None;
    }
    let Some(Type::Array(element)) = e.ty.as_deref() else {
        return None;
    };
    if crate::mentions_abstract_type(element) {
        return None;
    }
    Some(Expr::new(ExprKind::None, whole.span.clone()).with_ty(Type::Optional(element.clone())))
}

/// `arm` with its body measured rather than produced.
fn measured_arm(arm: &MatchArm, measure: &dyn Fn(&Expr) -> Expr) -> MatchArm {
    MatchArm {
        written: arm.written.clone(),
        pattern: arm.pattern.clone(),
        body: measure(&arm.body),
        span: arm.span.clone(),
    }
}

/// `a + b`, as the loader's own size arithmetic spells it — dropping the term
/// when one side is the constant zero, which is what `[..xs]` reserves past its
/// seed and would otherwise leave behind.
fn add(a: Expr, b: Expr, span: aipl_syntax::Span) -> Expr {
    match (&a.kind, &b.kind) {
        (ExprKind::Num(0), _) => b,
        (_, ExprKind::Num(0)) => a,
        _ => crate::op_call(Callee::WrappingAdd, vec![a, b], span),
    }
}

/// `len(map_join(xs, f))` as the sum of the pieces' lengths:
///
/// ```text
/// mut $n: u64 = 0;
/// for (let $x : xs) { set $n = $n + len(f($x)); };
/// $n
/// ```
///
/// Only a join with *no* separators folds. A separator contributes its own
/// length once per gap, and how many gaps there are depends on which of the
/// three (`sep`, `final_sep`, `only_sep`) applies to a list of that length —
/// arithmetic worth writing only when something needs it.
fn sum_of_parts(
    whole: &Expr,
    args: &[Expr],
    empty: &[String],
    blocked: &HashSet<String>,
) -> Option<Expr> {
    let [xs, f, seps @ ..] = args else {
        return None;
    };
    // The loader fills every omitted separator, so a plain `join()` arrives with
    // three empty-array arguments rather than none.
    if !seps.iter().all(|s| is_empty_sequence(s, empty)) {
        return None;
    }
    if !is_pure(f, blocked) {
        return None;
    }
    let id = aipl_syntax::fresh_id();
    let span = || whole.span.clone();
    let (total, elem) = (format!("$len{id}_n"), format!("$len{id}_x"));
    let ident = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), span());
    let unit = || Expr::new(ExprKind::Unit, span());
    let piece = Expr::new(
        ExprKind::Call(Callee::Len, vec![apply(f, ident(&elem))?], true),
        span(),
    );
    let step = Expr::new(
        ExprKind::Assign(
            Box::new(ident(&total)),
            Box::new(add(ident(&total), piece, span())),
            Box::new(unit()),
        ),
        span(),
    );
    // A loop's own value is an `i64`, so the running total is read after it
    // rather than out of it.
    let walk = Expr::new(
        ExprKind::Seq(
            Box::new(Expr::new(
                ExprKind::For(elem, None, Box::new(xs.clone()), Box::new(step)),
                span(),
            )),
            Box::new(ident(&total)),
        ),
        span(),
    );
    // Annotated, because `0` on its own would flex to `i64` and every `len` it
    // accumulates is a `u64`.
    Some(Expr::rebuilt(
        ExprKind::LetMut(
            total,
            Some(Type::Primitive(Primitive::U64)),
            Box::new(Expr::new(ExprKind::Num(0), span())),
            Box::new(walk),
        ),
        whole,
    ))
}

/// `len(opt.map(f).value_or([]))` as
/// `match (opt) { some($x) => len(f($x)), none => 0 }`.
///
/// The `map` is reached through rather than measured so that `f`'s result is
/// measured at the point it is produced — which is what lets the rest of the
/// table, and the counting variants, fold it in turn. Measuring the `value_or`
/// alone would leave `len` on a binding, where nothing can be said about it.
fn optional_count(
    whole: &Expr,
    args: &[Expr],
    empty: &[String],
    blocked: &HashSet<String>,
) -> Option<Expr> {
    let [mapped, default] = args else {
        return None;
    };
    if !is_empty_sequence(default, empty) {
        return None;
    }
    let ExprKind::Call(Callee::Map, map_args, _) = &mapped.kind else {
        return None;
    };
    let [opt, f] = map_args.as_slice() else {
        return None;
    };
    if !is_pure(f, blocked) {
        return None;
    }
    let span = || whole.span.clone();
    let bound = format!("$len{}_v", aipl_syntax::fresh_id());
    let some = MatchArm {
        written: Default::default(),
        pattern: Pattern::Ctor {
            name: "some".to_string(),
            bindings: vec![bound.clone()],
            ignore_payload: false,
        },
        body: Expr::new(
            ExprKind::Call(
                Callee::Len,
                vec![apply(f, Expr::new(ExprKind::Ident(bound), span()))?],
                true,
            ),
            span(),
        ),
        span: span(),
    };
    let none = MatchArm {
        written: Default::default(),
        pattern: Pattern::Ctor {
            name: "none".to_string(),
            bindings: Vec::new(),
            ignore_payload: false,
        },
        body: Expr::new(ExprKind::Num(0), span()),
        span: span(),
    };
    Some(Expr::rebuilt(
        ExprKind::Match(Box::new(opt.clone()), vec![some, none]),
        whole,
    ))
}

/// Whether `e` is the empty sequence — a separator that contributes nothing, or
/// the `value_or` default that pins a result to a sequence.
///
/// It may be spelled as a *name*, which is why `empty` travels down the walk:
/// the loader hoists a keyword argument it has to read more than once into a
/// binding and passes that name at each position, so a plain `xs.join()`
/// reaches this pass with its three separators spelled as one `__kwarg$N`
/// rather than as three empty literals. Only those hoisted names are tracked,
/// and `$` is not an identifier character — so nothing a user can write
/// shadows one, and the lookup needs no scope discipline beyond the `let`'s own
/// extent.
fn is_empty_sequence(e: &Expr, empty: &[String]) -> bool {
    match &e.kind {
        ExprKind::ArrayLit(elems) => elems.is_empty(),
        ExprKind::Ident(name) => empty.iter().any(|n| n == name),
        _ => false,
    }
}

/// The reserve-and-append block the loader builds for an array literal holding
/// a spread, taken apart: what it starts from, how much the appends add, and
/// what each one appends.
///
/// Recognized structurally rather than by the accumulator's name: a `mut`
/// bound to an `__arr_reserve`, whose body is nothing but a chain of stores of
/// `__arr_append`/`__arr_concat` to that same binding, ending in a read of it.
/// Nothing else can be that shape, and nothing else is accepted — a store that
/// does not fit, or an operand mentioning the accumulator, keeps the block as
/// written.
pub(crate) struct SpreadAccumulator<'a> {
    /// The reserved seed — the literal's leading plain elements, or the first
    /// spread's operand when the literal starts with one.
    pub(crate) seed: &'a Expr,
    /// What the appends add past the seed: one per plain element, `len(..)` per
    /// spread, as the loader summed it to size the allocation.
    pub(crate) extra: &'a Expr,
    /// Each appended element, in order.
    pub(crate) appended: Vec<AppendedPiece<'a>>,
}

/// One element the accumulator appends: the expression, and whether the literal
/// wrote it as a spread — a whole sequence spliced in — or as a single element.
///
/// Which it is decides what may stand in its place: a sequence or one value. So
/// the distinction travels with the piece rather than being re-derived from the
/// intrinsic's name at each use. [`crate::fold_lengths`] does not care (a length
/// is a length), but [`crate::fold_extends`] appends the pieces into another
/// array and needs `push` for one and `extend` for the other.
pub(crate) struct AppendedPiece<'a> {
    pub(crate) spread: bool,
    pub(crate) value: &'a Expr,
}

pub(crate) fn spread_accumulator(e: &Expr) -> Option<SpreadAccumulator<'_>> {
    let ExprKind::LetMut(acc, _, reserved, body) = &e.kind else {
        return None;
    };
    let ExprKind::Call(Callee::ArrReserve, reserve_args, _) = &reserved.kind else {
        return None;
    };
    let [seed, extra] = reserve_args.as_slice() else {
        return None;
    };
    let reads_acc = |x: &Expr| mentions_free(x, acc);
    if reads_acc(seed) || reads_acc(extra) {
        return None;
    }
    let mut appended: Vec<AppendedPiece<'_>> = Vec::new();
    let mut rest = &**body;
    loop {
        match &rest.kind {
            // The block's value: the accumulator itself, and the end of the chain.
            ExprKind::Ident(name) if name == acc => {
                return Some(SpreadAccumulator {
                    seed,
                    extra,
                    appended,
                })
            }
            ExprKind::Assign(lhs, value, next) => {
                if !matches!(&lhs.kind, ExprKind::Ident(name) if name == acc) {
                    return None;
                }
                let ExprKind::Call(
                    intrinsic @ (Callee::ArrAppend | Callee::ArrConcat),
                    call_args,
                    _,
                ) = &value.kind
                else {
                    return None;
                };
                let [receiver, element] = call_args.as_slice() else {
                    return None;
                };
                if !matches!(&receiver.kind, ExprKind::Ident(name) if name == acc)
                    || reads_acc(element)
                {
                    return None;
                }
                appended.push(AppendedPiece {
                    spread: *intrinsic == Callee::ArrConcat,
                    value: element,
                });
                rest = next;
            }
            _ => return None,
        }
    }
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
        e(ExprKind::Call(c, args, false))
    }

    /// `mut acc = __arr_reserve(seed, extra); set acc = <store>; acc` — the
    /// loader's shape, with the one store swapped for whatever a caller wants
    /// to test.
    fn accumulator(store: Expr) -> Expr {
        e(ExprKind::LetMut(
            "__spread$0".into(),
            None,
            Box::new(call(
                Callee::ArrReserve,
                vec![e(ExprKind::ArrayLit(vec![])), e(ExprKind::Num(0))],
            )),
            Box::new(e(ExprKind::Assign(
                Box::new(id("__spread$0")),
                Box::new(store),
                Box::new(id("__spread$0")),
            ))),
        ))
    }

    #[test]
    fn the_loaders_reserve_and_append_block_is_recognized() {
        let whole = accumulator(call(Callee::ArrConcat, vec![id("__spread$0"), id("xs")]));
        let found = spread_accumulator(&whole).expect("this is the shape `[..xs]` desugars to");
        assert!(matches!(&found.seed.kind, ExprKind::ArrayLit(v) if v.is_empty()));
        assert!(matches!(found.extra.kind, ExprKind::Num(0)));
        assert_eq!(found.appended.len(), 1);
    }

    /// The matcher is the whole guard: only the loader builds this shape today,
    /// so a block that merely *looks* like it — a store of something other than
    /// an append, or one that writes a different binding — must be refused
    /// rather than measured by arithmetic that describes something else.
    #[test]
    fn a_block_that_is_not_that_shape_is_refused() {
        // A store of something that is not an append or a concat.
        assert!(spread_accumulator(&accumulator(call(
            Callee::Push,
            vec![id("__spread$0"), id("x")]
        )))
        .is_none());
        // An append whose receiver is not the accumulator.
        assert!(spread_accumulator(&accumulator(call(
            Callee::ArrAppend,
            vec![id("other"), id("x")]
        )))
        .is_none());
        // An appended element that reads the accumulator, so dropping the
        // append would change what the arithmetic is about.
        assert!(spread_accumulator(&accumulator(call(
            Callee::ArrConcat,
            vec![id("__spread$0"), id("__spread$0")]
        )))
        .is_none());
        // A `mut` bound to something other than a reserve.
        assert!(spread_accumulator(&e(ExprKind::LetMut(
            "__spread$0".into(),
            None,
            Box::new(e(ExprKind::ArrayLit(vec![]))),
            Box::new(id("__spread$0")),
        )))
        .is_none());
    }

    #[test]
    fn a_hoisted_keyword_argument_holding_an_empty_sequence_reads_as_one() {
        let empty = vec![format!("{}1", aipl_syntax::KWARG_ARG_PREFIX)];
        assert!(is_empty_sequence(&e(ExprKind::ArrayLit(vec![])), &[]));
        assert!(is_empty_sequence(&id(&empty[0]), &empty));
        // A name that was never recorded says nothing about its value.
        assert!(!is_empty_sequence(&id("sep"), &empty));
        assert!(!is_empty_sequence(
            &e(ExprKind::ArrayLit(vec![e(ExprKind::Num(1))])),
            &empty
        ));
    }
}
