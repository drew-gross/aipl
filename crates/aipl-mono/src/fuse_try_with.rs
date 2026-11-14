//! `xs.with(i, v).value_or(xs)` as the one operation it is, `xs.try_set(i, v)`.
//!
//! `with` answers `none` for an index past the end, and the natural way to keep
//! the array in that case is to fall back to the receiver. Written out that reads
//! the receiver twice — once to give to `with`, once as the default — and the
//! first of those is the one that wants to be a *move*: `with` writes into an
//! array nobody else holds, and a receiver that is read again afterwards is held
//! by that second read, so the write copies. `try_set` reads it once.
//!
//! ```text
//! xs.with(i, v).value_or(xs)   →   xs.try_set(i, v)
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

use aipl_syntax::ast::Callee;

use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TPattern, TyRepr};

use crate::MonoProgram;

/// A binding, and the fields read off it in turn.
type Place = (String, Vec<String>);

/// [`fuse_try_with`] on the pinned program.
pub fn fuse_try_with_pinned(
    program: &aipl_syntax::typed::TProgram,
) -> aipl_syntax::typed::TProgram {
    crate::typed_util::map_pinned_bodies(
        program,
        |_| true,
        |_, body| fuse_typed(body, &mut Vec::new()),
    )
}

/// [`lower_try_set`] on the pinned program.
pub fn lower_try_set_pinned(
    program: &aipl_syntax::typed::TProgram,
) -> aipl_syntax::typed::TProgram {
    fn lower_pinned<T: TyRepr>(e: &mut TExpr<T>) {
        crate::typed_util::each_child_mut(e, &mut lower_pinned);
        if let TExprKind::Call {
            callee: callee @ Callee::TrySet,
            method_style,
            ..
        } = &mut e.kind
        {
            *callee = Callee::WithOrSelf;
            *method_style = false;
        }
    }
    crate::typed_util::map_pinned_bodies(program, |_| true, |_, body| lower_pinned(body))
}

/// [`fuse_try_with`] over monomorphized functions, on the typed tree.
pub fn fuse_try_with_post_mono(program: &MonoProgram) -> MonoProgram {
    crate::typed_util::map_bodies(
        program,
        |_| true,
        |_, body| fuse_typed(body, &mut Vec::new()),
    )
}

// The typed twin of `fuse` and what it calls, for the passes after monomorphization. The
// pre-monomorphization pass above still reads `Expr`; it goes when that side is typed.

fn fuse_typed<T: TyRepr>(e: &mut TExpr<T>, aliases: &mut Vec<(String, Place)>) {
    if let TExprKind::Let(name, value, body) = &mut e.kind {
        fuse_typed(value, aliases);
        let alias = place_of_typed(value, aliases)
            .filter(|(root, _)| !disturbs_typed(body, &[name.as_str(), root.as_str()]));
        let aliased = alias.is_some();
        if let Some(place) = alias {
            aliases.push((name.clone(), place));
        }
        fuse_typed(body, aliases);
        if aliased {
            aliases.pop();
        }
    } else {
        crate::typed_util::each_child_mut(e, &mut |child| fuse_typed(child, aliases));
    }
    rewrite_typed(e, aliases);
}

fn rewrite_typed<T: TyRepr>(e: &mut TExpr<T>, aliases: &[(String, Place)]) {
    let args = match &e.kind {
        TExprKind::Call {
            callee: Callee::ValueOr,
            args,
            ..
        } => match args.as_slice() {
            [w, default] => with_args_typed(w).filter(|a| same_typed(&a[0], default, aliases)),
            _ => None,
        },
        TExprKind::Let(s, w, body) => match &body.kind {
            TExprKind::Match(scrutinee, arms) if matches!(&scrutinee.kind, TExprKind::Ident(n) if n == s) => {
                with_args_typed(w).filter(|a| falls_back_to_typed(arms, &a[0], aliases))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(args) = args {
        e.kind = TExprKind::Call {
            callee: Callee::WithOrSelf,
            type_args: Vec::new(),
            args,
            method_style: false,
        };
    }
}

fn with_args_typed<T: TyRepr>(e: &TExpr<T>) -> Option<Vec<TExpr<T>>> {
    match &e.kind {
        TExprKind::Call {
            callee: Callee::With,
            args,
            ..
        } if args.len() == 3 => Some(args.clone()),
        _ => None,
    }
}

fn falls_back_to_typed<T: TyRepr>(
    arms: &[TMatchArm<T>],
    receiver: &TExpr<T>,
    aliases: &[(String, Place)],
) -> bool {
    let [a, b] = arms else {
        return false;
    };
    fn ctor<T: TyRepr>(arm: &TMatchArm<T>) -> Option<(&str, &[String])> {
        match &arm.pattern {
            TPattern::Ctor { name, bindings, .. } => Some((name.as_str(), bindings.as_slice())),
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
    if !matches!(&some.body.kind, TExprKind::Ident(n) if n == bound) {
        return false;
    }
    match &none.body.kind {
        TExprKind::Let(d, value, rest) if matches!(&rest.kind, TExprKind::Ident(n) if n == d) => {
            same_typed(receiver, value, aliases)
        }
        _ => same_typed(receiver, &none.body, aliases),
    }
}

fn same_typed<T: TyRepr>(a: &TExpr<T>, b: &TExpr<T>, aliases: &[(String, Place)]) -> bool {
    match (place_of_typed(a, aliases), place_of_typed(b, aliases)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn place_of_typed<T: TyRepr>(e: &TExpr<T>, aliases: &[(String, Place)]) -> Option<Place> {
    match &e.kind {
        TExprKind::Ident(n) => Some(
            aliases
                .iter()
                .rev()
                .find(|(a, _)| a == n)
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| (n.clone(), Vec::new())),
        ),
        TExprKind::Field(inner, f) => {
            let (root, mut path) = place_of_typed(inner, aliases)?;
            path.push(f.clone());
            Some((root, path))
        }
        TExprKind::Let(d, value, body) if matches!(&body.kind, TExprKind::Ident(n) if n == d) => {
            place_of_typed(value, aliases)
        }
        _ => None,
    }
}

fn disturbs_typed<T: TyRepr>(e: &TExpr<T>, names: &[&str]) -> bool {
    let named = |n: &String| names.contains(&n.as_str());
    let here = match &e.kind {
        TExprKind::Let(n, _, _) | TExprKind::LetMut(n, _, _) => named(n),
        TExprKind::Assign(lhs, _, _) => matches!(&lhs.kind, TExprKind::Ident(n) if named(n)),
        TExprKind::For(n, index, _, _) => names.iter().any(|x| aipl_syntax::for_binds(n, index, x)),
        TExprKind::Lambda(params, _) => params.iter().any(|p| named(&p.name)),
        TExprKind::Match(_, arms) => arms
            .iter()
            .any(|arm| arm.pattern.bindings().iter().any(named)),
        TExprKind::IfLet(arm, _, _) => arm.pattern.bindings().iter().any(named),
        _ => false,
    };
    here || crate::typed_util::any_child(e, |c| disturbs_typed(c, names))
}
