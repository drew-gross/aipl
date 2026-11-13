//! Expanding a call into the body of the function it names, over the typed tree.
//!
//! The decisions (which function, which sites) are made on the program's `Expr` bodies; the
//! expansion itself is done here, on the bodies the call sits in and the body it expands to, so the
//! bindings it builds are typed. Only a body that holds a call to the function is converted.

use std::collections::HashMap;

use aipl_syntax::typed::{ConcreteTy, TExpr, TExprKind};

use crate::early_exit::without_early_exits_typed;
use crate::typed_adapter::{concrete, from_typed, to_typed};
use crate::typed_util::{each_child_mut, rename_scoped};
use crate::{ConcreteFn, InlineParam, MonoProgram};

type Typed = TExpr<ConcreteTy>;

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
    let mut callee_body: Option<Typed> = None;
    for i in 0..program.fns.len() {
        if sites == Sites::First && replaced {
            break;
        }
        let g = &program.fns[i];
        if (skip_itself && g.name == f.name) || !crate::references_name(&g.body, &f.name) {
            continue;
        }
        let fbody = callee_body.get_or_insert_with(|| {
            to_typed(f, program)
                .unwrap_or_else(|why| panic!("{}: cannot type the body: {why}", f.name))
        });
        let typed = to_typed(g, program)
            .unwrap_or_else(|why| panic!("{}: cannot type the body: {why}", g.name));
        let mut here = false;
        let body = replace_call(&typed, &f.name, fparams, fbody, &mut here, sites);
        if here {
            replaced = true;
            program.fns[i].body = from_typed(&body);
        }
    }
    replaced
}

/// `e` with the calls to `fname` — with matching arity — replaced by the inlined body.
fn replace_call(
    e: &Typed,
    fname: &str,
    fparams: &[InlineParam],
    fbody: &Typed,
    replaced: &mut bool,
    sites: Sites,
) -> Typed {
    if !*replaced || sites == Sites::All {
        if let TExprKind::Call { callee, args, .. } = &e.kind {
            if callee.name() == fname && args.len() == fparams.len() {
                *replaced = true;
                return build_inlined(fparams, fbody, args, e);
            }
        }
    }
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| {
        *child = replace_call(child, fname, fparams, fbody, replaced, sites);
    });
    out
}

/// A node made here carries no stamp, as the `Expr` it replaces had none.
fn made(kind: TExprKind<ConcreteTy>, ty: ConcreteTy, like: &Typed) -> Typed {
    let mut out = TExpr::new(kind, ty, like.span.clone());
    out.derived = true;
    out
}

/// The expansion of `at`, a call whose arguments are `args`: each (freshly renamed) parameter bound
/// to its argument by a `let` of the parameter's declared type, around `fbody`.
fn build_inlined(fparams: &[InlineParam], fbody: &Typed, args: &[Typed], at: &Typed) -> Typed {
    let fbody = &without_early_exits_typed(fbody)
        .expect("the inline gate accepted a body whose early exits cannot be rewritten");
    let mut map: HashMap<String, String> = HashMap::new();
    let fresh: Vec<String> = fparams
        .iter()
        .map(|p| {
            let nm = format!("$inl{}_{}", aipl_syntax::fresh_id(), p.name);
            map.insert(p.name.clone(), nm.clone());
            nm
        })
        .collect();
    let mut expr = rename_scoped(fbody, &map, true);
    let declared = |p: &InlineParam| -> ConcreteTy {
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
