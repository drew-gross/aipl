//! `xs.with(i, v).value_or(xs)` as the one operation it is, `xs.try_with(i, v)`.
//!
//! `with` answers `none` for an index past the end, and the natural way to keep
//! the array in that case is to fall back to the receiver. Written out that reads
//! the receiver twice — once to give to `with`, once as the default — and the
//! first of those is the one that wants to be a *move*: `with` writes into an
//! array nobody else holds, and a receiver that is read again afterwards is held
//! by that second read, so the write copies. `try_with` reads it once.
//!
//! ```text
//! xs.with(i, v).value_or(xs)   →   xs.try_with(i, v)
//! ```
//!
//! The lint `with_value_or_self` steers source toward the short spelling. This
//! pass is for the occurrences it never sees, and for the ones that only come to
//! be after other passes have run:
//!
//! - the receiver and the default are **the same place** — a binding, or a field
//!   path off one — once the aliases an inlined call leaves behind are resolved
//!   (`let $inl_a = xs; let $inl_b = xs; value_or(with($inl_a, ..), $inl_b)`);
//! - after monomorphization the builtin `value_or` has been instantiated and
//!   inlined, and the call has become the `match` it stands for, which this
//!   recognises too.
//!
//! An alias is followed only through an immutable `let` whose body neither
//! reassigns nor rebinds what it names: anything else may no longer be the same
//! value when the default is read.
//!
//! It runs in the optimizer's rounds, before monomorphization where the call is
//! still a call, and again after it, where the call is a `match`.

use aipl_syntax::ast::{Callee, Expr, ExprKind, Item, Pattern, Program};

use crate::{children, children_mut, MonoProgram};

/// A binding, and the fields read off it in turn.
type Place = (String, Vec<String>);

/// `program` with each such shape written as a call to `try_with`.
pub fn fuse_try_with(program: &Program) -> Program {
    Program {
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => {
                    let mut f = f.clone();
                    fuse(&mut f.body, &mut Vec::new());
                    if let Some(t) = f.test_body.as_mut() {
                        fuse(t, &mut Vec::new());
                    }
                    Item::Fn(f)
                }
                other => other.clone(),
            })
            .collect(),
    }
}

/// [`fuse_try_with`] over monomorphized functions.
pub fn fuse_try_with_post_mono(program: &MonoProgram) -> MonoProgram {
    let mut out = program.clone();
    for f in &mut out.fns {
        fuse(&mut f.body, &mut Vec::new());
    }
    out
}

/// Rewrite `e` in place, children first. `aliases` are the immutable `let a = place`
/// bindings in scope, innermost last.
fn fuse(e: &mut Expr, aliases: &mut Vec<(String, Place)>) {
    if let ExprKind::Let(name, _, value, body) = &mut e.kind {
        fuse(value, aliases);
        let alias = place_of(value, aliases)
            .filter(|(root, _)| !disturbs(body, &[name.as_str(), root.as_str()]));
        let aliased = alias.is_some();
        if let Some(place) = alias {
            aliases.push((name.clone(), place));
        }
        fuse(body, aliases);
        if aliased {
            aliases.pop();
        }
    } else {
        for child in children_mut(e) {
            fuse(child, aliases);
        }
    }
    rewrite(e, aliases);
}

/// Turn `e` into a `try_with` call if it is either shape.
fn rewrite(e: &mut Expr, aliases: &[(String, Place)]) {
    let args = match &e.kind {
        // `value_or(with(x, i, v), x)`.
        ExprKind::Call(Callee::ValueOr, args, _) => match args.as_slice() {
            [w, default] => with_args(w).filter(|a| same(&a[0], default, aliases)),
            _ => None,
        },
        // `let s = with(x, i, v); match s { some(p) => p, none => x }` — what the
        // call above becomes once `value_or` is inlined.
        ExprKind::Let(s, _, w, body) => match &body.kind {
            ExprKind::Match(scrutinee, arms) if matches!(&scrutinee.kind, ExprKind::Ident(n) if n == s) => {
                with_args(w).filter(|a| falls_back_to(arms, &a[0], aliases))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(args) = args {
        e.kind = ExprKind::Call(Callee::TryWith, args, false);
    }
}

/// The three arguments of `e` when it is a call to `with`.
fn with_args(e: &Expr) -> Option<Vec<Expr>> {
    match &e.kind {
        ExprKind::Call(Callee::With, args, _) if args.len() == 3 => Some(args.clone()),
        _ => None,
    }
}

/// Whether `arms` are `some(p) => p` and `none => <receiver>`, in either order,
/// the default possibly bound to a name first.
fn falls_back_to(
    arms: &[aipl_syntax::ast::MatchArm],
    receiver: &Expr,
    aliases: &[(String, Place)],
) -> bool {
    let [a, b] = arms else {
        return false;
    };
    fn ctor(arm: &aipl_syntax::ast::MatchArm) -> Option<(&str, &[String])> {
        match &arm.pattern {
            Pattern::Ctor { name, bindings, .. } => Some((name.as_str(), bindings.as_slice())),
            _ => None,
        }
    }
    let (some, none) = match (ctor(a), ctor(b)) {
        (Some(("some", _)), Some(("none", []))) => (a, b),
        (Some(("none", [])), Some(("some", _))) => (b, a),
        _ => return false,
    };
    let Some(("some", [bound])) = ctor(some) else {
        return false;
    };
    if !matches!(&some.body.kind, ExprKind::Ident(n) if n == bound) {
        return false;
    }
    match &none.body.kind {
        // `none => let $default = place; $default`.
        ExprKind::Let(d, _, value, rest) if matches!(&rest.kind, ExprKind::Ident(n) if n == d) => {
            same(receiver, value, aliases)
        }
        _ => same(receiver, &none.body, aliases),
    }
}

/// Whether `a` and `b` read the same place.
fn same(a: &Expr, b: &Expr, aliases: &[(String, Place)]) -> bool {
    match (place_of(a, aliases), place_of(b, aliases)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// The place `e` reads, with the aliases in scope resolved to what they name.
fn place_of(e: &Expr, aliases: &[(String, Place)]) -> Option<Place> {
    match &e.kind {
        ExprKind::Ident(n) => Some(
            aliases
                .iter()
                .rev()
                .find(|(a, _)| a == n)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| (n.clone(), Vec::new())),
        ),
        ExprKind::Field(inner, f) => {
            let (root, mut path) = place_of(inner, aliases)?;
            path.push(f.clone());
            Some((root, path))
        }
        // `let d = place; d`: what an inlined `fn f(xs) { xs }` leaves.
        ExprKind::Let(d, _, value, body) if matches!(&body.kind, ExprKind::Ident(n) if n == d) => {
            place_of(value, aliases)
        }
        _ => None,
    }
}

/// Whether `e` rebinds or assigns any of `names`, so that a name read before it
/// need not be the name read after.
fn disturbs(e: &Expr, names: &[&str]) -> bool {
    let named = |n: &String| names.contains(&n.as_str());
    let here = match &e.kind {
        ExprKind::Let(n, _, _, _) | ExprKind::LetMut(n, _, _, _) => named(n),
        ExprKind::Assign(lhs, _, _) => matches!(&lhs.kind, ExprKind::Ident(n) if named(n)),
        ExprKind::For(n, index, _, _) => names.iter().any(|x| aipl_syntax::for_binds(n, index, x)),
        ExprKind::Lambda(params, _) => params.iter().any(|p| named(&p.name)),
        ExprKind::Match(_, arms) => arms
            .iter()
            .any(|arm| arm.pattern.bindings().iter().any(named)),
        ExprKind::IfLet(arm, _, _) => arm.pattern.bindings().iter().any(named),
        _ => false,
    };
    here || children(e).into_iter().any(|c| disturbs(c, names))
}
