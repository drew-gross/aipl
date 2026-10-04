//! Push a `set v.extend(..)` inward until it reaches the elements, so the
//! sequence it was going to copy from is never built.
//!
//! `extend` is already the efficient way to append a whole sequence —
//! `aipl_arr_reserve` sizes the destination once and the elements move as a
//! single `memcpy`. What it cannot avoid is its *source*: `set v.extend([a, b])`
//! allocates a two-element array, fills it, copies it into `v`, and frees it.
//! The elements were always going to end up in `v`, so the array between them
//! is pure overhead.
//!
//! So this pass reads the `extend` as a question about the source's *shape* and
//! answers it by appending straight into `v`:
//!
//! ```text
//! set v.extend([]);                      →  (nothing)
//! set v.extend([a, b]);                  →  set v.push(a); set v.push(b);
//! set v.extend([a, ..xs]);               →  set v.push(a); set v.extend(xs);
//! set v.extend(xs.map(f).join());        →  for (let x : xs) { set v.extend(f(x)); }
//! set v.extend(if (c) { a } else { b }); →  if (c) { set v.extend(a); } else { set v.extend(b); }
//! set v.extend(match (s) { .. });        →  match (s) { .. each arm appended in .. }
//! ```
//!
//! One row a reader will expect and not find is `opt.map(f).value_or([])`,
//! which the length folder does have. Rewriting it to
//! `match (opt) { some(x) => set v.extend(f(x)), none => () }` is correct on
//! paper and over-releases in practice: the arm binds the optional's payload and
//! hands it to what becomes a *user* call, and the length folder's version does
//! not because `len` is a builtin and the last-use marker only marks user calls.
//! The symptom is a refcount reaching zero on a live block (`AIPL_RC_TRACE` in
//! `aipl-codegen` shows it directly). Until that is understood the row is left
//! out rather than guessed at — it costs the `sep` branch of a producer like
//! `group_styles`, and nothing else.
//!
//! Every rewrite leaves `extend`s of smaller sources, and the pass re-folds its
//! own output, so one `extend` walks as deep as the shapes go. Where it stops is
//! an `extend` of a *call*: what that produces is inside another function's
//! body. That is [`crate::appending_variants`]'s half — it mints a variant of
//! the callee that appends into a caller's array instead of returning its own —
//! and the two halves feed each other across the pass manager's rounds, exactly
//! as the length pair does.
//!
//! # Why nothing has to be pure
//!
//! Unlike the length folder, this one **drops nothing and reorders nothing**.
//! Every element the source would have produced is still produced, by the same
//! expression, in the same order; the only change is that it lands in `v`
//! directly rather than in a buffer that is then copied into `v`. So there is no
//! `can_defer` here and no purity requirement on a mapping function: a source
//! that prints per element prints exactly as often, in exactly the same order.
//!
//! One thing does change, and is guarded: the source is no longer *fully*
//! evaluated before `v` grows — the appends interleave with it. Only an
//! expression that reads `v` itself can tell, so a source mentioning `v` keeps
//! the `extend` as written.
//!
//! # Why a push needs the receiver's declared type
//!
//! A `str` is the `char` sequence too, and `extend` takes a `str` receiver where
//! `push` does not ("push requires an array, got str"). Rewriting
//! `set s.extend(['c', 'd'])` into pushes would therefore stop compiling. This
//! pass runs before monomorphization and cannot ask what `v` is — so it asks the
//! *declaration* instead, and emits a push only for a receiver annotated as an
//! array ([`ArrayReceivers`]). A `mut` array seeded with `[]` has to be
//! annotated anyway, nothing else being able to pin its element type, so in
//! practice the annotation is there. The rows that append a whole sequence need
//! none of this: `extend` is what they emit, and it takes either receiver.
//!
//! # Why there is no reservation
//!
//! A row that replaces one `extend` with several appends would pay a
//! reallocation per append if each sized the destination exactly — which is what
//! `extend` does, once, deliberately. Measured on an accumulator fed in a loop,
//! splitting one `extend` into many made `bytes allocated` nearly three times
//! worse. What makes the rewrite a win instead is that `push` grows
//! *geometrically*, so a run of them amortizes; and inserting a leading exact
//! `reserve` to recover the single sizing made it worse again, reallocating once
//! per iteration where doubling did not. So the rows here end in `push` or in an
//! `extend` of something smaller, and nothing reserves.

use aipl_syntax::ast::{Callee, Expr, ExprKind, Function, Item, MatchArm, Param, Program, Type};

use crate::fuse::loop_fusions::apply;
use crate::passes::Scope;
use crate::sink::mentions_free;

/// Fold every `set v.extend(..)` in `program` whose source has a shape worth
/// appending directly.
///
/// `scope` names the functions to rewrite: the pass is body-local, so a function
/// the scope leaves out would come back unchanged anyway (see [`crate::passes`]).
pub fn fold_extends(program: &Program, scope: &Scope) -> Program {
    Program {
        // Rewrites bodies only; the file map and the file's own documentation
        // carry through unchanged.
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) if scope.covers(&f.name) => Item::Fn(folded_fn(f)),
                other => other.clone(),
            })
            .collect(),
    }
}

fn folded_fn(f: &Function) -> Function {
    let mut out = f.clone();
    out.body = fold_expr(&f.body, &mut ArrayReceivers::from_params(&f.sig.params));
    out.test_body = f
        .test_body
        .as_ref()
        .map(|t| fold_expr(t, &mut ArrayReceivers::from_params(&f.sig.params)));
    out
}

/// The `mut` bindings whose declaration says they hold an array, so a `push`
/// into one is sure to compile — see the module docs.
///
/// A stack, so a name is answered for only as long as the declaration that put
/// it there is in scope: an inner binding that shadows an array with something
/// else takes the answer away rather than inheriting it.
struct ArrayReceivers {
    receivers: Vec<(String, bool)>,
    /// Hoisted keyword arguments bound to an empty sequence.
    ///
    /// The loader fills an omitted keyword argument by binding the default once
    /// and passing the *name* at each position it fills, so a plain `xs.join()`
    /// reaches this pass with its three separators spelled as one `__kwarg$N`
    /// rather than as three empty literals. Only those hoisted names are
    /// tracked, and `$` is not an identifier character — so nothing a user can
    /// write shadows one.
    empty: Vec<String>,
}

impl ArrayReceivers {
    fn from_params(params: &[Param]) -> ArrayReceivers {
        ArrayReceivers {
            receivers: params
                .iter()
                .filter(|p| p.mutable)
                .map(|p| (p.name.clone(), is_array(&p.ty)))
                .collect(),
            empty: Vec::new(),
        }
    }

    /// Whether `name` is declared to hold an array. Innermost declaration wins.
    fn holds_array(&self, name: &str) -> bool {
        self.receivers
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .is_some_and(|(_, yes)| *yes)
    }

    fn declare(&mut self, name: &str, ty: &Option<Type>) {
        self.receivers
            .push((name.to_string(), ty.as_ref().is_some_and(is_array)));
    }

    fn undeclare(&mut self) {
        self.receivers.pop();
    }

    /// Whether `e` is the empty sequence — directly, or as a hoisted keyword
    /// argument bound to one.
    fn is_empty_sequence(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::ArrayLit(elems) => elems.is_empty(),
            ExprKind::Ident(name) => self.empty.iter().any(|n| n == name),
            _ => false,
        }
    }

    /// Record an immutable binding of a hoisted keyword argument to an empty
    /// sequence; the `bool` is whether anything was pushed.
    fn note_empty(&mut self, name: &str, value: &Expr) -> bool {
        let push = name.starts_with(aipl_syntax::KWARG_ARG_PREFIX)
            && matches!(&value.kind, ExprKind::ArrayLit(e) if e.is_empty());
        if push {
            self.empty.push(name.to_string());
        }
        push
    }

    fn forget_empty(&mut self, noted: bool) {
        if noted {
            self.empty.pop();
        }
    }
}

fn is_array(t: &Type) -> bool {
    matches!(aipl_syntax::unrefined(t), Type::Array(_))
}

/// `e` with every foldable `set v.extend(..)` inside it folded.
fn fold_expr(e: &Expr, arrays: &mut ArrayReceivers) -> Expr {
    if let Some(folded) = extend_statement(e).and_then(|stmt| appended(stmt, arrays)) {
        // The replacement is a chain of `extend`s of smaller sources, so it is
        // folded in turn — one visit follows the source all the way down.
        return fold_expr(&folded, arrays);
    }
    // A `mut` is in scope for its body and nothing else, so it is recorded for
    // exactly that descent.
    if let ExprKind::LetMut(name, ty, value, body) = &e.kind {
        let value = fold_expr(value, arrays);
        arrays.declare(name, ty);
        let body = fold_expr(body, arrays);
        arrays.undeclare();
        return Expr::rebuilt(
            ExprKind::LetMut(name.clone(), ty.clone(), Box::new(value), Box::new(body)),
            e,
        );
    }
    // An immutable `let` is in scope for its body and nothing else, which is
    // where a hoisted keyword default lives.
    if let ExprKind::Let(name, ty, value, body) = &e.kind {
        let value = fold_expr(value, arrays);
        let noted = arrays.note_empty(name, &value);
        let body = fold_expr(body, arrays);
        arrays.forget_empty(noted);
        return Expr::rebuilt(
            ExprKind::Let(name.clone(), ty.clone(), Box::new(value), Box::new(body)),
            e,
        );
    }
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = fold_expr(child, arrays);
    }
    out
}

/// Whether [`fold_extends`] would rewrite anything in `e` — what
/// [`crate::appending_variants`] asks before minting a variant whose whole
/// point is that this pass can take its body apart.
///
/// The receiver set is empty, so only the rows that emit an `extend` count. A
/// minted variant's receiver *is* declared an array (it carries the producer's
/// return type), so the push rows will fire once it is a real function — but
/// counting them here would need the body in its final shape, and the question
/// at mint time is only whether there is anything to do at all.
pub(crate) fn folds_anything(e: &Expr) -> bool {
    fold_expr(e, &mut ArrayReceivers::from_params(&[])) != *e
}

/// The `set v.extend(src); rest` statement `e` is, taken apart.
///
/// `set v.extend(src)` parses to `set v = v.extend(src)` — the in-place
/// writeback rather than a copy (see monomorphization's `Assign` arm), which is
/// the only `extend` form worth folding: an expression-position one yields a
/// fresh array and has no accumulator to append into.
struct ExtendStatement<'a> {
    receiver: &'a str,
    source: &'a Expr,
    rest: &'a Expr,
    whole: &'a Expr,
}

fn extend_statement(e: &Expr) -> Option<ExtendStatement<'_>> {
    let ExprKind::Assign(lhs, value, rest) = &e.kind else {
        return None;
    };
    let ExprKind::Ident(receiver) = &lhs.kind else {
        return None;
    };
    let ExprKind::Call(Callee::Extend, args, true) = &value.kind else {
        return None;
    };
    let [target, source] = args.as_slice() else {
        return None;
    };
    if !crate::is_receiver(target, receiver) {
        return None;
    }
    Some(ExtendStatement {
        receiver,
        source,
        rest,
        whole: e,
    })
}

/// The statement appending `src`'s elements straight into `v`, followed by what
/// came after the `extend` — or `None` when the source's shape is not one this
/// pass knows.
fn appended(stmt: ExtendStatement<'_>, arrays: &ArrayReceivers) -> Option<Expr> {
    // The appends interleave with evaluating the source, where the `extend`
    // evaluated it whole first. Only an expression that reads `v` can tell, so
    // one that does keeps the statement as written.
    if mentions_free(stmt.source, stmt.receiver) {
        return None;
    }
    let appends = appends(stmt.receiver, stmt.source, arrays)?;
    Some(Expr::rebuilt(
        ExprKind::Seq(Box::new(appends), Box::new(stmt.rest.clone())),
        stmt.whole,
    ))
}

/// `set v.extend(src);` as a unit-valued statement, for use as a continuation.
fn extend_stmt(v: &str, src: &Expr) -> Expr {
    writeback(v, Callee::Extend, src)
}

/// `set v.push(x);` likewise.
fn push_stmt(v: &str, x: &Expr) -> Expr {
    writeback(v, Callee::Push, x)
}

fn writeback(v: &str, callee: Callee, arg: &Expr) -> Expr {
    let span = || arg.span.clone();
    let target = || Expr::new(ExprKind::Ident(v.to_string()), span());
    let call = Expr::new(
        ExprKind::Call(callee, vec![target(), arg.clone()], true),
        span(),
    );
    Expr::new(
        ExprKind::Assign(
            Box::new(target()),
            Box::new(call),
            Box::new(Expr::new(ExprKind::Unit, span())),
        ),
        span(),
    )
}

/// A unit-valued statement appending every element `src` would produce into
/// `v`, without building `src`.
fn appends(v: &str, src: &Expr, arrays: &ArrayReceivers) -> Option<Expr> {
    let span = || src.span.clone();
    let unit = || Expr::new(ExprKind::Unit, span());
    let seq = |a: Expr, b: Expr| Expr::new(ExprKind::Seq(Box::new(a), Box::new(b)), span());
    let like = |kind| Expr::rebuilt(kind, src);
    // A whole sequence appended into `v` — `extend` either way, so these rows
    // need nothing of the receiver's type.
    let extend = |x: &Expr| extend_stmt(v, x);
    match &src.kind {
        // Nothing to append, and nothing left of the statement.
        ExprKind::ArrayLit(elems) if elems.is_empty() => Some(unit()),
        // `[a, b]` — one push each, and no array in between. A spread is the
        // accumulator case below, not this one: the loader rewrote any literal
        // that holds one.
        ExprKind::ArrayLit(elems) => {
            if !arrays.holds_array(v) || elems.iter().any(|x| matches!(x.kind, ExprKind::Spread(_)))
            {
                return None;
            }
            Some(
                elems
                    .iter()
                    .rev()
                    .fold(unit(), |rest, x| seq(push_stmt(v, x), rest)),
            )
        }
        // `[a, ..xs]` as the loader left it: a reserve-and-append block with an
        // accumulator of its own. Appending into `v` instead needs neither that
        // binding nor the size it reserved — `v` grows as it goes.
        ExprKind::LetMut(..) => {
            let acc = crate::fold_lengths::spread_accumulator(src)?;
            let mut out = unit();
            for piece in acc.appended.iter().rev() {
                let step = match piece.spread {
                    true => extend(piece.value),
                    // A single element, so a push — the row that needs the
                    // receiver's declared type (see the module docs).
                    false if arrays.holds_array(v) => push_stmt(v, piece.value),
                    false => return None,
                };
                out = seq(step, out);
            }
            Some(seq(extend(acc.seed), out))
        }
        // `xs.map(f).join()` — the fusion pass has already collapsed the pair,
        // so this is one call. Each mapped piece goes straight into `v` as it is
        // produced: the same order, and the same number of calls to `f`.
        ExprKind::Call(Callee::MapJoin, args, _) => {
            let [xs, f, seps @ ..] = args.as_slice() else {
                return None;
            };
            // A separator would have to land between the pieces; only a plain
            // join folds. The loader fills every omitted one, so `join()`
            // arrives with three empty arguments rather than none.
            if !seps.iter().all(|s| arrays.is_empty_sequence(s)) {
                return None;
            }
            let elem = format!("$ext{}_x", crate::next_inline_id());
            let piece = apply(f, Expr::new(ExprKind::Ident(elem.clone()), span()))?;
            let walk = Expr::new(
                ExprKind::For(elem, None, Box::new(xs.clone()), Box::new(extend(&piece))),
                span(),
            );
            // A loop's own value is an `i64`, so it is sequenced to unit.
            Some(seq(walk, unit()))
        }
        // `if`/`match`/`if let` — append in each branch instead of producing a
        // sequence from it. These are what let the rest of the table reach
        // anything: a function that assembles a sequence assembles it per
        // branch. Each construct becomes the *statement* form, which is where it
        // now sits.
        ExprKind::If(cond, then, els) => Some(like(ExprKind::If(
            cond.clone(),
            Box::new(extend(then)),
            Box::new(extend(els)),
        ))),
        ExprKind::Match(scrutinee, arms) => Some(like(ExprKind::Match(
            scrutinee.clone(),
            arms.iter()
                .map(|a| MatchArm {
                    pattern: a.pattern.clone(),
                    body: extend(&a.body),
                    span: a.span.clone(),
                })
                .collect(),
        ))),
        ExprKind::IfLet(matched, scrutinee, els) => Some(like(ExprKind::IfLet(
            Box::new(MatchArm {
                pattern: matched.pattern.clone(),
                body: extend(&matched.body),
                span: matched.span.clone(),
            }),
            scrutinee.clone(),
            Box::new(extend(els)),
        ))),
        // The statement forms: whatever ran before the value still runs, and the
        // value itself is what gets appended. A binding that shadows the
        // receiver would send the append somewhere else, so it keeps the
        // statement as written.
        ExprKind::Seq(first, rest) => {
            Some(like(ExprKind::Seq(first.clone(), Box::new(extend(rest)))))
        }
        ExprKind::Assign(lhs, value, rest) => Some(like(ExprKind::Assign(
            lhs.clone(),
            value.clone(),
            Box::new(extend(rest)),
        ))),
        ExprKind::Let(name, ty, value, body) if name != v => Some(like(ExprKind::Let(
            name.clone(),
            ty.clone(),
            value.clone(),
            Box::new(extend(body)),
        ))),
        _ => None,
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

    /// `set v.extend(src); ()` — the writeback form, as the parser leaves it.
    fn extend_of(src: Expr) -> Expr {
        e(ExprKind::Assign(
            Box::new(id("v")),
            Box::new(e(ExprKind::Call(Callee::Extend, vec![id("v"), src], true))),
            Box::new(e(ExprKind::Unit)),
        ))
    }

    fn receivers(v_is_array: bool) -> ArrayReceivers {
        ArrayReceivers {
            receivers: vec![("v".to_string(), v_is_array)],
            empty: Vec::new(),
        }
    }

    fn array_receiver() -> ArrayReceivers {
        receivers(true)
    }

    #[test]
    fn an_empty_source_leaves_only_what_followed() {
        let whole = extend_of(e(ExprKind::ArrayLit(vec![])));
        let stmt = extend_statement(&whole).expect("the writeback form");
        let out = appended(stmt, &array_receiver()).expect("`extend([])` appends nothing");
        assert!(
            matches!(&out.kind, ExprKind::Seq(first, _) if matches!(first.kind, ExprKind::Unit)),
            "the statement should be gone, leaving what followed it: {out:?}"
        );
    }

    /// The one row that may not be emitted for a receiver the declaration does
    /// not call an array: `push` refuses a `str` where `extend` accepts one, so
    /// rewriting would stop the program compiling rather than speed it up.
    #[test]
    fn a_literal_is_only_pushed_into_a_declared_array() {
        let literal = || e(ExprKind::ArrayLit(vec![id("a"), id("b")]));
        let whole = extend_of(literal());
        assert!(appended(extend_statement(&whole).expect("shape"), &array_receiver()).is_some());

        // Declared, but not as an array.
        let str_receiver = receivers(false);
        let whole = extend_of(literal());
        assert!(appended(extend_statement(&whole).expect("shape"), &str_receiver).is_none());

        // Not declared here at all, so nothing is known about it.
        let whole = extend_of(literal());
        assert!(appended(
            extend_statement(&whole).expect("shape"),
            &ArrayReceivers::from_params(&[])
        )
        .is_none());
    }

    #[test]
    fn a_source_that_reads_the_receiver_is_left_alone() {
        // `set v.extend([v[0]])` — the appends would interleave with reading `v`.
        let read = e(ExprKind::Index(
            Box::new(id("v")),
            Box::new(e(ExprKind::Num(0))),
        ));
        let whole = extend_of(e(ExprKind::ArrayLit(vec![read])));
        let stmt = extend_statement(&whole).expect("the writeback form");
        assert!(appended(stmt, &array_receiver()).is_none());
    }

    #[test]
    fn only_the_writeback_form_is_a_candidate() {
        // `extend(v, xs)` free-call form: a copy-and-modify, with no accumulator
        // to append into.
        let free = e(ExprKind::Call(
            Callee::Extend,
            vec![id("v"), id("xs")],
            false,
        ));
        let whole = e(ExprKind::Assign(
            Box::new(id("v")),
            Box::new(free),
            Box::new(e(ExprKind::Unit)),
        ));
        assert!(extend_statement(&whole).is_none());
        // A writeback into a *different* binding is not this shape either.
        let other = e(ExprKind::Assign(
            Box::new(id("w")),
            Box::new(e(ExprKind::Call(
                Callee::Extend,
                vec![id("v"), id("xs")],
                true,
            ))),
            Box::new(e(ExprKind::Unit)),
        ));
        assert!(extend_statement(&other).is_none());
    }

    /// The receiver may arrive handed over with `__move` — `move_last_use` wraps
    /// it where the callee owns it — and that is the same writeback.
    #[test]
    fn a_moved_receiver_is_still_the_writeback() {
        let moved = e(ExprKind::Call(Callee::Move, vec![id("v")], false));
        let whole = e(ExprKind::Assign(
            Box::new(id("v")),
            Box::new(e(ExprKind::Call(
                Callee::Extend,
                vec![moved, e(ExprKind::ArrayLit(vec![]))],
                true,
            ))),
            Box::new(e(ExprKind::Unit)),
        ));
        assert!(extend_statement(&whole).is_some());
    }
}
