//! Mint a *counting variant* of a function whose result is only ever measured,
//! and call that instead.
//!
//! [`crate::fold_lengths`] can measure a sequence it can see the shape of, and
//! stops at a `len` of a call — `group_styles(r).len()` in `grammar.aipl` — because
//! the shape is inside another function's body. This pass is the half that gets
//! it there: it copies `group_styles` into `group_styles$len`, whose return type
//! is `u64` and whose every result position is its own result *measured* rather
//! than produced, and rewrites the call site to it.
//!
//! ```text
//! fn group_styles(r: Rule<K>) -> ListStyle[] {      fn group_styles$len(r: Rule<K>) -> u64 {
//!     match (r) {                                       match (r) {
//!         Then(rs) => rs.map(group_styles).join(),          len(rs.map(group_styles).join()),
//!         Maybe(sub) => group_styles(sub),         →        len(group_styles(sub)),
//!         Term(..) => [],                                   len([]),
//!     }                                                 }
//! }                                                 }
//! ```
//!
//! On its own that is not an improvement — it is the same work with a `len` on
//! the end. The improvement is that the folder now has the shapes in front of
//! it, so the next round turns `len([])` into `0`, `len(rs.map(..).join())`
//! into a sum, and each `len(group_styles(..))` into another
//! `group_styles$len(..)` — and the array that every level of the recursion used
//! to build is never built at all. The two passes alternate in the pass
//! manager's rounds until the family closes over itself.
//!
//! So the minting is deliberately dumb and the folding is deliberately general:
//! this pass knows nothing about array shapes, and the folder knows nothing
//! about functions. What this pass does know is the three ways a producer's
//! result reaches a `len`, which is also what decides the variant's return type:
//!
//! | call site | becomes | producer returns | variant returns |
//! |---|---|---|---|
//! | `g(a).len()` | `g$len(a)` | `T[]` | `u64` |
//! | `g(a).value_or([]).len()` | `g$len(a).value_or(0)` | `T[]?` | `u64?` |
//! | `g(a)?.len()` | `g$len(a)?` | `T[]!E` | `u64!E` |
//!
//! # Minting only when it pays
//!
//! A variant the folder cannot measure is strictly worse than the call it
//! replaced: the same array, built, measured and thrown away, now behind one
//! more call and one more function in the binary. So a candidate is minted only
//! if [`fold_lengths::folds_anything`] says the folder can push at least one of
//! the `len`s it just introduced inward — and if not, the call site is left
//! exactly as written. That is also what keeps the pass away from the functions
//! it has nothing to offer: one whose body hands back a sequence it was given,
//! or reads out of a binding, folds nowhere and is never copied.
//!
//! # What makes it safe
//!
//! The variant is a copy, so nothing about the original moves and no call site
//! other than the one rewritten is affected. What the copy changes is that its
//! result is measured — and measuring is where work gets dropped, so every
//! judgement about whether that is observable belongs to the folder, which is
//! the pass that actually drops it ([`crate::fold_lengths`] has the argument).
//! Two things are this pass's own:
//!
//! - **An early `return` is a result position wherever it sits**, including deep
//!   inside a branch run for effect, so every one of them is rewritten rather
//!   than only the body's tail. A `return` inside a *lambda* is the one this
//!   pass refuses to reason about — whose return it is depends on the lambda,
//!   not on this pass — so a body holding one is not minted at all.
//! - **An optional or result leaf is only recognized as a constructor.**
//!   `some(xs)` becomes `some(len(xs))` and `err(e)` stays `err(e)`, because
//!   those say what they wrap; a leaf that merely *has* the type — another
//!   function's result, a binding — would need the type to rewrite, and this
//!   pass runs before monomorphization. Such a body is not minted. A plain
//!   `T[]` producer has no such restriction: every leaf is the array, so every
//!   leaf is `len(leaf)`.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{
    Callee, Expr, ExprKind, Function, Item, MatchArm, Primitive, Program, Type,
};

use crate::fold_lengths;
use crate::fuse::{through_bindings, under_bindings};
use crate::sink::undeferrable_fns;

/// The suffix a counting variant's name carries. `$` is not an identifier
/// character, so no source function can collide with one.
const COUNTING_SUFFIX: &str = "$len";

/// What a producer's declared return type wraps its sequence in, and so what
/// shape its counting variant answers with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Counted {
    /// `T[]` → `u64`.
    Array,
    /// `T[]?` → `u64?`.
    Optional,
    /// `T[]!E` → `u64!E`.
    Result,
}

/// Rewrite every measured call to a sequence producer into a call to its
/// counting variant, and add the variants.
///
/// Whole-program: it reads the callee's body to copy it and adds items, so
/// there is no per-function scope to honor — the same reason the inliners are
/// whole-program (see [`crate::passes`]).
pub fn counting_variants(program: &Program, effectful: &HashSet<String>) -> Program {
    let producers: HashMap<&str, (Counted, &Function)> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) => Some((f.name.as_str(), (producer_shape(f)?, f))),
            _ => None,
        })
        .collect();
    if producers.is_empty() {
        return program.clone();
    }
    let blocked = undeferrable_fns(program, effectful);
    let mut cx = Cx {
        blocked: &blocked,
        existing: program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(f) => Some(f.name.text.clone()),
                _ => None,
            })
            .collect(),
        producers,
        minted: HashMap::new(),
    };
    let mut items: Vec<Item> = program
        .items
        .iter()
        .map(|item| match item {
            Item::Fn(f) => {
                let mut f = f.clone();
                f.body = rewrite(&f.body, &mut cx);
                f.test_body = f.test_body.as_ref().map(|t| rewrite(t, &mut cx));
                Item::Fn(f)
            }
            other => other.clone(),
        })
        .collect();
    // Deterministic order: the variants are named after the functions they were
    // minted from, and the program's own item order is what decides those.
    let mut fresh: Vec<Function> = cx.minted.into_values().flatten().collect();
    fresh.sort_by(|a, b| a.name.text.cmp(&b.name.text));
    items.extend(fresh.into_iter().map(Item::Fn));
    Program {
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items,
    }
}

/// What the pass carries while it rewrites: who the producers are, which
/// variants exist already, and the ones minted this round.
struct Cx<'a> {
    /// Functions whose call must not be deferred, for the folder's own
    /// judgement about whether a candidate body is worth minting.
    blocked: &'a HashSet<String>,
    /// Every function name the program already holds — so a variant minted in
    /// an earlier round is reused rather than minted again.
    existing: HashSet<String>,
    producers: HashMap<&'a str, (Counted, &'a Function)>,
    /// Memo per producer: the variant minted for it, or `None` for a candidate
    /// the folder had nothing to offer. Keyed by the *producer*'s name.
    minted: HashMap<String, Option<Function>>,
}

/// The sequence shape `f` produces, or `None` if it is not a candidate.
///
/// A mutating method is excluded: it is declared void and a call yields the
/// mutated receiver, so its declared return type says nothing about what a
/// caller measures. A variant is excluded so a second round cannot mint
/// `g$len$len`.
fn producer_shape(f: &Function) -> Option<Counted> {
    if f.sig.is_mutating() || f.name.text.ends_with(COUNTING_SUFFIX) {
        return None;
    }
    match aipl_syntax::unrefined(f.sig.return_ty.as_ref()?) {
        Type::Array(_) => Some(Counted::Array),
        Type::Optional(inner) if is_array(inner) => Some(Counted::Optional),
        Type::Result(ok, _) if is_array(ok) => Some(Counted::Result),
        _ => None,
    }
}

fn is_array(t: &Type) -> bool {
    matches!(aipl_syntax::unrefined(t), Type::Array(_))
}

/// `e` with every measured producer call rewritten, children included.
fn rewrite(e: &Expr, cx: &mut Cx) -> Expr {
    let mut out = counted_call(e, cx).unwrap_or_else(|| e.clone());
    for child in crate::children_mut(&mut out) {
        *child = rewrite(child, cx);
    }
    out
}

/// `len(..)` over a producer's result, as a call to that producer's counting
/// variant — one of the three rows in the module docs — or `None`.
///
/// The measured call may sit under the `let`s an inlined call leaves in front
/// of its body ([`through_bindings`]), so it is looked for beneath them and the
/// rewrite put back there.
fn counted_call(whole: &Expr, cx: &mut Cx) -> Option<Expr> {
    let ExprKind::Call(Callee::Len, args, _) = &whole.kind else {
        return None;
    };
    let [arg] = args.as_slice() else {
        return None;
    };
    let (bindings, measured) = through_bindings(arg);
    let counted = match &measured.kind {
        // `g(a).value_or([]).len()` — the default is what says the producer's
        // result is a sequence, and `0` is the count that answers it.
        ExprKind::Call(Callee::ValueOr, value_or_args, method_style) => {
            let [optional, default] = value_or_args.as_slice() else {
                return None;
            };
            if !matches!(&default.kind, ExprKind::ArrayLit(e) if e.is_empty()) {
                return None;
            }
            let count = variant_call(optional, Counted::Optional, cx)?;
            let zero = Expr::new(ExprKind::Num(0), measured.span.clone());
            Expr::rebuilt(
                ExprKind::Call(Callee::ValueOr, vec![count, zero], *method_style),
                measured,
            )
        }
        // `g(a)?.len()` — the propagation stays where it was, over the count.
        ExprKind::Try(result) => Expr::rebuilt(
            ExprKind::Try(Box::new(variant_call(result, Counted::Result, cx)?)),
            measured,
        ),
        // `g(a).len()`.
        _ => variant_call(measured, Counted::Array, cx)?,
    };
    Some(under_bindings(bindings, whole, counted))
}

/// `call` — a call to a producer of `shape` — as the same call to that
/// producer's counting variant, minting the variant if it does not exist yet.
///
/// `None` when `call` is not a call to such a producer, or when the variant
/// would not be worth having (see the module docs).
fn variant_call(call: &Expr, shape: Counted, cx: &mut Cx) -> Option<Expr> {
    let ExprKind::Call(callee, args, method_style) = &call.kind else {
        return None;
    };
    let name = callee.user()?;
    let (declared, f) = cx.producers.get(name)?;
    if *declared != shape {
        return None;
    }
    let counting = format!("{name}{COUNTING_SUFFIX}");
    if !cx.existing.contains(&counting) {
        let (f, name) = (*f, name.to_string());
        if !cx.minted.contains_key(&name) {
            let variant = mint(f, shape, &counting, cx.blocked);
            cx.minted.insert(name.clone(), variant);
        }
        // A candidate the folder had nothing to offer leaves the call as written.
        cx.minted[&name].as_ref()?;
    }
    Some(Expr::rebuilt(
        ExprKind::Call(Callee::User(counting), args.clone(), *method_style),
        call,
    ))
}

/// `f`'s counting variant, named `counting` — the same signature answering a
/// count, and the same body with every result position measured — or `None`
/// when the body cannot be measured or the folder would find nothing in it.
fn mint(
    f: &Function,
    shape: Counted,
    counting: &str,
    blocked: &HashSet<String>,
) -> Option<Function> {
    if lambda_returns(&f.body) {
        return None;
    }
    let body = counted_body(&f.body, shape)?;
    // A variant the folder cannot measure is worse than the call it replaced.
    if !fold_lengths::folds_anything(&body, blocked) {
        return None;
    }
    let u64_ty = || Type::Primitive(Primitive::U64);
    let return_ty = match (shape, aipl_syntax::unrefined(f.sig.return_ty.as_ref()?)) {
        (Counted::Array, _) => u64_ty(),
        (Counted::Optional, _) => Type::Optional(Box::new(u64_ty())),
        // The error side is untouched: only the ok side was ever a sequence.
        (Counted::Result, Type::Result(_, err)) => Type::Result(Box::new(u64_ty()), err.clone()),
        (Counted::Result, _) => return None,
    };
    Some(Function {
        // Spanned as the producer's own name, so a diagnostic about the variant
        // points at the function it was minted from — the only source there is.
        name: f.name.renamed(counting),
        // Private: nothing imports a synthesized function, and private is what
        // lets the single-use inliner fold it back into its one caller.
        is_pub: false,
        sig: aipl_syntax::ast::Signature {
            return_ty: Some(return_ty),
            ..f.sig.clone()
        },
        body,
        // The producer's `.test` block asserts on the elements, which this
        // variant does not produce.
        test_body: None,
        test_fns: Vec::new(),
        doc: Some(format!(
            "The counting variant of `{}`: how long its result is, measured \
             rather than built. Synthesized by the compiler for a call whose \
             result only reached `len` (see `aipl_mono::counting_variants`).",
            crate::display_fn(&f.name)
        )),
    })
}

/// Whether `e` holds a `return` inside a lambda — the one shape this pass will
/// not reason about (see the module docs).
fn lambda_returns(e: &Expr) -> bool {
    fn has_return(e: &Expr) -> bool {
        matches!(e.kind, ExprKind::Return(_)) || crate::children(e).iter().any(|c| has_return(c))
    }
    match &e.kind {
        ExprKind::Lambda(_, body) => has_return(body),
        _ => crate::children(e).iter().any(|c| lambda_returns(c)),
    }
}

/// `e` with every result position measured: the value the body produces, and
/// the value of every `return` inside it.
fn counted_body(e: &Expr, shape: Counted) -> Option<Expr> {
    counted_value(&counted_returns(e, shape)?, shape)
}

/// `e` with every `return`'s value measured.
///
/// A `return` leaves the function from wherever it sits — inside a branch run
/// for effect, inside a loop — so it is a result position no matter what
/// encloses it, and the whole body has to be walked for them rather than just
/// its tail. A lambda is stepped over: whose `return` it is depends on the
/// lambda, and [`lambda_returns`] has already refused a body that has one.
fn counted_returns(e: &Expr, shape: Counted) -> Option<Expr> {
    if let ExprKind::Return(value) = &e.kind {
        return Some(Expr::rebuilt(
            ExprKind::Return(Box::new(counted_value(value, shape)?)),
            e,
        ));
    }
    if matches!(e.kind, ExprKind::Lambda(..)) {
        return Some(e.clone());
    }
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = counted_returns(child, shape)?;
    }
    Some(out)
}

/// The count that `e` — a result position — answers with.
///
/// Recurses through the forms that *branch* to a value, because an optional or
/// result leaf has to be found to be rewritten: `some(xs)` is where the count
/// goes inside. It deliberately does **not** step through `mut`/`set`, which
/// accumulate into a value rather than branching to one — and which is exactly
/// what the loader leaves an array literal's spread as. Measuring that block
/// whole is what lets [`crate::fold_lengths`] recognize it; measuring the
/// binding it ends in would have taken the shape apart first.
fn counted_value(e: &Expr, shape: Counted) -> Option<Expr> {
    let sub = |x: &Expr| counted_value(x, shape);
    let arm = |a: &MatchArm| {
        Some(MatchArm {
            pattern: a.pattern.clone(),
            body: counted_value(&a.body, shape)?,
            span: a.span.clone(),
        })
    };
    let kind = match &e.kind {
        ExprKind::If(cond, then, els) => {
            ExprKind::If(cond.clone(), Box::new(sub(then)?), Box::new(sub(els)?))
        }
        ExprKind::Match(scrutinee, arms) => ExprKind::Match(
            scrutinee.clone(),
            arms.iter().map(arm).collect::<Option<Vec<_>>>()?,
        ),
        ExprKind::IfLet(matched, scrutinee, els) => ExprKind::IfLet(
            Box::new(arm(matched)?),
            scrutinee.clone(),
            Box::new(sub(els)?),
        ),
        ExprKind::Let(name, ty, value, body) => ExprKind::Let(
            name.clone(),
            ty.clone(),
            value.clone(),
            Box::new(sub(body)?),
        ),
        ExprKind::Seq(first, rest) => ExprKind::Seq(first.clone(), Box::new(sub(rest)?)),
        _ => return counted_leaf(e, shape),
    };
    Some(Expr::rebuilt(kind, e))
}

/// The count `e` — a result position that produces the sequence rather than
/// branching to it — answers with.
fn counted_leaf(e: &Expr, shape: Counted) -> Option<Expr> {
    let measure = |x: &Expr| {
        Expr::new(
            ExprKind::Call(Callee::Len, vec![x.clone()], true),
            e.span.clone(),
        )
    };
    match (shape, &e.kind) {
        // A `return` here has already been measured by `counted_returns`, and
        // the unit after one is the value of a block that cannot reach its end
        // — there is nothing to measure, so the variant is not worth minting.
        (_, ExprKind::Return(_)) => Some(e.clone()),
        (_, ExprKind::Unit) => None,
        (Counted::Array, _) => Some(measure(e)),
        // `some(xs)` / `ok(xs)` say what they wrap, so the count goes inside.
        (Counted::Optional, ExprKind::Call(Callee::Some, args, method_style))
        | (Counted::Result, ExprKind::Call(Callee::Ok, args, method_style)) => {
            let [inner] = args.as_slice() else {
                return None;
            };
            let callee = match shape {
                Counted::Optional => Callee::Some,
                _ => Callee::Ok,
            };
            Some(Expr::rebuilt(
                ExprKind::Call(callee, vec![measure(inner)], *method_style),
                e,
            ))
        }
        // The empty cases carry nothing to measure, and carry through as they are.
        (Counted::Optional, ExprKind::None) => Some(e.clone()),
        (Counted::Result, ExprKind::Call(Callee::Err, ..)) => Some(e.clone()),
        // Anything else has the type without saying so, and this pass runs
        // before the types are known (see the module docs).
        _ => None,
    }
}
