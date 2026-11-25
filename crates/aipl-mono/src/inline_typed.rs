//! Expanding a call into the body of the function it names, over the typed tree.
//!
//! The decisions (which function, which sites) are made on the program's `Expr` bodies; the
//! expansion itself is done here, on the bodies the call sits in and the body it expands to, so the
//! bindings it builds are typed. Only a body that holds a call to the function is converted.

use std::collections::HashMap;

use aipl_syntax::ast::ConcreteType;
use aipl_syntax::typed::{TExpr, TExprKind};
use aipl_syntax::Names;

use crate::early_exit::without_early_exits_typed;
use crate::typed_adapter::concrete;
use crate::typed_util::{each_child_mut, references_name, rename_scoped, Typed};
use crate::{ConcreteFn, InlineParam, MonoProgram};

/// How many call sites one expansion rewrites.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sites {
    First,
    All,
}

/// Expand the calls to `f` (parameters `fparams`) in the bodies of `program`, except `f`'s own when
/// `skip_itself`. Whether anything was rewritten.
pub(crate) fn expand_calls(
    program: &mut MonoProgram,
    f: &ConcreteFn,
    fparams: &[InlineParam],
    sites: Sites,
    skip_itself: bool,
) -> bool {
    let mut replaced = false;
    for g in &mut program.fns {
        if sites == Sites::First && replaced {
            break;
        }
        if skip_itself && g.name == f.name {
            continue;
        }
        replaced |= expand_calls_in(g, f, fparams, sites);
    }
    replaced
}

/// [`expand_calls`] in the one function `g`. Whether anything was rewritten.
pub(crate) fn expand_calls_in(
    g: &mut ConcreteFn,
    f: &ConcreteFn,
    fparams: &[InlineParam],
    sites: Sites,
) -> bool {
    if !references_name(&g.body, &f.name) {
        return false;
    }
    let mut replaced = false;
    // The binders each expansion introduces are named against everything `g` holds, the
    // expansions already made included.
    let mut names = Names::of_typed(&g.body);
    for p in &g.params {
        names.reserve(&p.name);
    }
    replace_call(
        &mut g.body,
        &f.name,
        fparams,
        &f.body,
        &mut replaced,
        sites,
        &mut names,
    );
    replaced
}

/// Replace, in `e` itself, the calls to `fname` — with matching arity — by the inlined body. The
/// body that replaces a call is not searched again, and under [`Sites::First`] nothing is searched
/// once a call has been replaced.
fn replace_call(
    e: &mut Typed,
    fname: &str,
    fparams: &[InlineParam],
    fbody: &Typed,
    replaced: &mut bool,
    sites: Sites,
    names: &mut Names,
) {
    if *replaced && sites == Sites::First {
        return;
    }
    if let TExprKind::Call { callee, args, .. } = &e.kind {
        if callee.name() == fname && args.len() == fparams.len() {
            *replaced = true;
            *e = build_inlined(fparams, fbody, args, e, names);
            return;
        }
    }
    each_child_mut(e, &mut |child| {
        replace_call(child, fname, fparams, fbody, replaced, sites, names);
    });
}

/// A node made here carries no stamp, as the `Expr` it replaces had none.
fn made(kind: TExprKind<ConcreteType>, ty: ConcreteType, like: &Typed) -> Typed {
    let mut out = TExpr::new(kind, ty, like.span.clone());
    out.derived = true;
    out
}

/// The expansion of `at`, a call whose arguments are `args`: each (freshly renamed) parameter bound
/// to its argument by a `let` of the parameter's declared type, around `fbody`.
fn build_inlined(
    fparams: &[InlineParam],
    fbody: &Typed,
    args: &[Typed],
    at: &Typed,
    names: &mut Names,
) -> Typed {
    let fbody = &without_early_exits_typed(fbody)
        .expect("the inline gate accepted a body whose early exits cannot be rewritten");
    let mut map: HashMap<String, String> = HashMap::new();
    let fresh: Vec<String> = fparams
        .iter()
        .map(|p| {
            let nm = names.fresh("inl", &p.name);
            map.insert(p.name.clone(), nm.clone());
            nm
        })
        .collect();
    let mut expr = rename_scoped(fbody, &map, Some(names));
    let declared = |p: &InlineParam| -> ConcreteType {
        concrete(&p.ty).expect("a parameter's declared type is concrete")
    };

    // A mutating method yields its receiver: the body ends having assigned to the binding.
    if fparams.first().is_some_and(|p| p.mutable) {
        let receiver = made(
            TExprKind::Ident(fresh[0].clone()),
            declared(&fparams[0]),
            at,
        );
        let ty = receiver.ty.clone();
        expr = made(TExprKind::Seq(Box::new(expr), Box::new(receiver)), ty, at);
    }

    for ((fp, p), arg) in fresh.iter().zip(fparams).zip(args).rev() {
        // The binding carries the *parameter's* declared type, not whatever the argument inferred:
        // a narrow integer changes the arithmetic. An argument with no type of its own takes it.
        let mut init = arg.clone();
        if init.derived {
            init.ty = declared(p);
        }
        let ty = expr.ty.clone();
        let (init, body) = (Box::new(init), Box::new(expr));
        let kind = if p.mutable {
            TExprKind::LetMut(fp.clone(), init, body)
        } else {
            TExprKind::Let(fp.clone(), init, body)
        };
        expr = made(kind, ty, at);
        expr.annotation = Some(declared(p));
    }
    expr
}
