//! Mint an *appending variant* of a function whose result is only ever appended
//! to somebody else's array, and call that instead.
//!
//! [`crate::fold_extends`] can append a sequence it can see the shape of, and
//! stops at an `extend` of a call — `set groups.extend(group_styles(rule))` in
//! `grammar.aipl` — because the shape is inside another function's body. This is
//! the half that gets it there: it copies `group_styles` into
//! `group_styles$into`, whose receiver is the caller's array and whose every
//! result position *appends into* that receiver rather than producing an array
//! of its own, and rewrites the call site to it.
//!
//! ```text
//! fn group_styles(r: Rule<K>) -> ListStyle[] {   fn group_styles$into(mut self: ListStyle[], r: Rule<K>) {
//!     match (r) {                                    match (r) {
//!         Then(rs) => rs.map(group_styles)               Then(rs) => set self.extend(rs.map(group_styles).join()),
//!                       .join(),              →          Many(..) => set self.extend([style, ..group_styles(sub)]),
//!         Many(..) => [style, ..group_styles(sub)],       Term(..) => set self.extend([]),
//!         Term(..) => [],                            }
//!     }                                          }
//! }
//! ```
//!
//! On its own that is not an improvement — it is the same arrays, copied into
//! the receiver instead of returned. The improvement is that the folder now has
//! the shapes in front of it, so the next round turns `set self.extend([])` into
//! nothing, the spread literal into pushes, the `map(..).join()` into a loop,
//! and each inner `extend(group_styles(..))` into another
//! `group_styles$into(..)` — until one array grows in place and no level builds
//! anything. The two passes alternate across the pass manager's rounds, exactly
//! as the length pair does.
//!
//! So the minting is deliberately dumb and the folding deliberately general:
//! this pass knows nothing about sequence shapes, and the folder knows nothing
//! about functions.
//!
//! # What makes it fast rather than merely equivalent
//!
//! The variant's receiver is a `mut self` array, and such a receiver grows *in
//! place* only because monomorphization hands it over rather than lending it
//! (`mut_receiver_owned`): the caller's slot gives up its reference, the callee
//! makes the block unique — free at a refcount of one — and hands it back
//! through the writeback. Before that existed, every append inside a `mut self`
//! method copied the whole receiver, and a recursive one paid that per level, so
//! a variant like this would have been dramatically *worse* than the
//! build-and-copy it replaced. It is the foundation this pass stands on.
//!
//! # Minting only when it pays
//!
//! A variant the folder cannot take apart is strictly worse than the call it
//! replaced: the same array, built and then copied into the receiver, now behind
//! one more call and one more function in the binary. So a candidate is minted
//! only if [`fold_extends`] can fold at least one of the `extend`s it just
//! introduced — and if not, the call site is left exactly as written.
//!
//! # What makes it safe
//!
//! The variant is a copy, so nothing about the original moves and no call site
//! other than the one rewritten is affected. What the copy changes is *when*
//! elements reach the caller's array — one at a time as they are produced,
//! rather than all at once at the end — and that judgement belongs to the
//! folder, which is the pass that actually interleaves them
//! ([`crate::fold_extends`] has the argument). Two things are this pass's own:
//!
//! - **An early `return` is a result position wherever it sits**, including deep
//!   inside a branch run for effect, so every one is rewritten rather than only
//!   the body's tail. A `return` inside a *lambda* is the one this pass will not
//!   reason about — whose return it is depends on the lambda — so a body holding
//!   one is not minted.
//! - **Only a plain `T[]` producer is minted.** A `T[]?` or `T[]!E` one would
//!   need the wrapper taken apart before anything could be appended, and the
//!   call site then has somewhere to put the `none`/`err` that this rewrite does
//!   not: `set v.extend(..)` has no error channel. Those keep their call.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, Expr, ExprKind, Function, Item, Param, Program, Type};

use crate::fold_extends;

/// The suffix an appending variant's name carries. `$` is not an identifier
/// character, so no source function can collide with one.
const APPENDING_SUFFIX: &str = "$into";

/// The receiver an appending variant takes. `self`, so the variant is
/// method-callable and so `set v.f$into(..)` is the writeback form every
/// mutating method reaches codegen in.
const RECEIVER: &str = "self";

/// Rewrite every `set v.extend(g(..))` into a call to `g`'s appending variant,
/// and add the variants.
///
/// Whole-program: it reads the callee's body to copy it and adds items, so there
/// is no per-function scope to honor — the same reason the inliners are
/// whole-program (see [`crate::passes`]).
pub fn appending_variants(program: &Program) -> Program {
    let producers: HashMap<&str, &Function> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) => Some((f.name.as_str(), f)).filter(|_| is_producer(f)),
            _ => None,
        })
        .collect();
    if producers.is_empty() {
        return program.clone();
    }
    let mut cx = Cx {
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
    // minted from.
    let mut fresh: Vec<Function> = cx.minted.into_values().flatten().collect();
    fresh.sort_by(|a, b| a.name.text.cmp(&b.name.text));
    items.extend(fresh.into_iter().map(Item::Fn));
    Program {
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items,
    }
}

/// What the pass carries while it rewrites.
struct Cx<'a> {
    /// Every function name the program already holds — so a variant minted in
    /// an earlier round is reused rather than minted again.
    existing: HashSet<String>,
    producers: HashMap<&'a str, &'a Function>,
    /// Memo per producer: the variant minted for it, or `None` for a candidate
    /// the folder had nothing to offer.
    minted: HashMap<String, Option<Function>>,
}

/// Whether `f` is a candidate: it hands back a plain array, and is not already
/// a variant (so a second round cannot mint `g$into$into`).
///
/// A mutating method is excluded — it is declared void and a call yields the
/// mutated receiver, so its declared return type says nothing about what a
/// caller appends. An appending variant *is* one, which the suffix check also
/// covers.
fn is_producer(f: &Function) -> bool {
    !f.sig.is_mutating()
        && !f.name.text.ends_with(APPENDING_SUFFIX)
        && f.sig
            .return_ty
            .as_ref()
            .is_some_and(|t| matches!(aipl_syntax::unrefined(t), Type::Array(_)))
}

/// `e` with every appended producer call rewritten, children included.
fn rewrite(e: &Expr, cx: &mut Cx) -> Expr {
    let mut out = appending_call(e, cx).unwrap_or_else(|| e.clone());
    for child in crate::children_mut(&mut out) {
        *child = rewrite(child, cx);
    }
    out
}

/// `set v.extend(g(a..)); rest` as `set v.g$into(a..); rest`, or `None`.
fn appending_call(whole: &Expr, cx: &mut Cx) -> Option<Expr> {
    let ExprKind::Assign(lhs, value, rest) = &whole.kind else {
        return None;
    };
    let ExprKind::Ident(receiver) = &lhs.kind else {
        return None;
    };
    let ExprKind::Call(Callee::Extend, extend_args, _) = &value.kind else {
        return None;
    };
    let [target, source] = extend_args.as_slice() else {
        return None;
    };
    if !crate::is_receiver(target, receiver) {
        return None;
    }
    // The source must be a call to a producer, and must not read the receiver:
    // its elements now land in `v` as they are produced rather than after it has
    // been built, which only something reading `v` can tell.
    let ExprKind::Call(callee, args, _) = &source.kind else {
        return None;
    };
    if crate::sink::mentions_free(source, receiver) {
        return None;
    }
    let name = callee.user()?;
    let f = *cx.producers.get(name)?;
    let variant = format!("{name}{APPENDING_SUFFIX}");
    if !cx.existing.contains(&variant) {
        let key = name.to_string();
        if !cx.minted.contains_key(&key) {
            let fresh = mint(f, &variant);
            cx.minted.insert(key.clone(), fresh);
        }
        // A candidate the folder had nothing to offer leaves the call as written.
        cx.minted[&key].as_ref()?;
    }
    // `set v.g$into(a..)`, which is `set v = v.g$into(a..)` — the receiver goes
    // in front of the producer's own arguments, where `self` is.
    let mut call_args = vec![target.clone()];
    call_args.extend(args.iter().cloned());
    let call = Expr::rebuilt(
        ExprKind::Call(Callee::User(variant), call_args, true),
        source,
    );
    Some(Expr::rebuilt(
        ExprKind::Assign(lhs.clone(), Box::new(call), rest.clone()),
        whole,
    ))
}

/// `f`'s appending variant, named `variant` — the same parameters behind a `mut
/// self` receiver, and the same body with every result position appended into
/// that receiver — or `None` when the body cannot be rewritten or the folder
/// would find nothing in it.
fn mint(f: &Function, variant: &str) -> Option<Function> {
    if lambda_returns(&f.body) || mentions_receiver(&f.body) {
        return None;
    }
    let body = appended_body(&f.body)?;
    // A variant the folder cannot take apart is worse than the call it replaced.
    if !fold_extends::folds_anything(&body) {
        return None;
    }
    let receiver = Param {
        name: RECEIVER.to_string(),
        ty: f.sig.return_ty.clone()?,
        mutable: true,
        arity: aipl_syntax::ast::Arity::One,
        default: None,
        implicit_some: false,
    };
    let mut params = vec![receiver];
    params.extend(f.sig.params.iter().cloned());
    Some(Function {
        // Spanned as the producer's own name, so a diagnostic about the variant
        // points at the function it was minted from — the only source there is.
        name: f.name.renamed(variant),
        // Private: nothing imports a synthesized function.
        is_pub: false,
        sig: aipl_syntax::ast::Signature {
            params,
            // A mutating method is declared void; what it hands back is the
            // receiver, through the writeback.
            return_ty: None,
            ..f.sig.clone()
        },
        body,
        // The producer's `.test` block asserts on the array it returns, which
        // this variant does not return.
        test_body: None,
        test_fns: Vec::new(),
        doc: Some(format!(
            "The appending variant of `{}`: its elements, appended into the \
             caller's array instead of collected into one of its own. \
             Synthesized by the compiler for a call whose result only reached \
             `extend` (see `aipl_mono::appending_variants`).",
            crate::display_fn(&f.name)
        )),
    })
}

/// Whether `e` already binds or reads the name the variant's receiver will take.
///
/// The receiver is introduced *around* the producer's body, so a body with a
/// `self` of its own would have it shadowed or captured. Rather than rename
/// anything, such a body is not minted — a producer is a plain function, so this
/// is the rare case.
fn mentions_receiver(e: &Expr) -> bool {
    crate::sink::mentions_free(e, RECEIVER) || crate::references_name(e, RECEIVER)
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

/// `e` with every result position appended into the receiver instead of
/// produced: the value the body yields, and the value of every `return` in it.
fn appended_body(e: &Expr) -> Option<Expr> {
    let with_returns = appended_returns(e)?;
    Some(append_stmt(&with_returns))
}

/// `e` with every `return`'s value appended and the `return` left valueless.
///
/// A `return` leaves the function from wherever it sits — inside a branch run
/// for effect, inside a loop — so it is a result position no matter what
/// encloses it, and the whole body has to be walked for them rather than just
/// its tail. A lambda is stepped over: whose `return` it is depends on the
/// lambda, and [`lambda_returns`] has already refused a body that has one.
fn appended_returns(e: &Expr) -> Option<Expr> {
    if let ExprKind::Return(value) = &e.kind {
        // `set self.extend(v); return;` — the variant is void, so the `return`
        // carries unit once its value has been appended.
        let appended = append_stmt(value);
        let bare = Expr::rebuilt(
            ExprKind::Return(Box::new(Expr::new(ExprKind::Unit, e.span.clone()))),
            e,
        );
        return Some(Expr::rebuilt(
            ExprKind::Seq(Box::new(appended), Box::new(bare)),
            e,
        ));
    }
    if matches!(e.kind, ExprKind::Lambda(..)) {
        return Some(e.clone());
    }
    let mut out = e.clone();
    for child in crate::children_mut(&mut out) {
        *child = appended_returns(child)?;
    }
    Some(out)
}

/// `set self.extend(value);` — one result position appended into the receiver.
///
/// Deliberately the whole value, with no attempt to look inside it: pushing the
/// `extend` through the branches is [`fold_extends`]'s work, and leaving it all
/// to that pass is what keeps the two from disagreeing about a shape.
fn append_stmt(value: &Expr) -> Expr {
    let span = || value.span.clone();
    let target = || Expr::new(ExprKind::Ident(RECEIVER.to_string()), span());
    let call = Expr::new(
        ExprKind::Call(Callee::Extend, vec![target(), value.clone()], true),
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
