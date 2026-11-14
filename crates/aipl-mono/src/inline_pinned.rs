//! The pre-monomorphization inliners on the pinned tree. A call replaced by a function's body
//! keeps the call's type; each parameter is bound by a `let` that records the parameter's declared
//! type (`TExpr::annotation`), since a narrow integer parameter changes the arithmetic.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::Callee;
use aipl_syntax::typed::{TExpr, TExprKind, TFunction, TItem, TProgram, Ty, TyRepr};

use crate::early_exit::without_early_exits_typed;
use crate::mentions_abstract_type;
use crate::typed_util::{
    any_child, body_size, collect_body_binders, contains_inplace_hof_intrinsic, count_uses,
    each_child_mut, references_name, rename_scoped,
};

type P = TExpr<Ty>;

/// Which call sites one expansion rewrites.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sites {
    First,
    All,
    Inspected,
}

struct Param {
    name: String,
    ty: Ty,
    mutable: bool,
}

fn params_of(f: &TFunction) -> Vec<Param> {
    f.sig
        .params
        .iter()
        .map(|p| Param {
            name: p.name.clone(),
            ty: p.ty.clone(),
            mutable: p.mutable,
        })
        .collect()
}

fn functions(program: &TProgram) -> impl Iterator<Item = &TFunction> {
    program.items.iter().filter_map(|item| match item {
        TItem::Fn(f) => Some(f),
        _ => None,
    })
}

fn use_counts(program: &TProgram) -> HashMap<&str, usize> {
    let mut counts: HashMap<&str, usize> = functions(program)
        .map(|f| (f.name.text.as_str(), 0))
        .collect();
    for f in functions(program) {
        let mut bound: HashSet<&str> = f.sig.params.iter().map(|p| p.name.as_str()).collect();
        count_uses(&f.body, &mut bound, &mut counts);
    }
    counts
}

fn collect_binders(program: &TProgram) -> HashSet<&str> {
    let mut set = HashSet::new();
    for f in functions(program) {
        for p in &f.sig.params {
            set.insert(p.name.as_str());
        }
        collect_body_binders(&f.body, &mut set);
    }
    set
}

fn is_inline_shape(higher_order_or_variadic: bool, body: &P) -> bool {
    !higher_order_or_variadic
        && without_early_exits_typed(body).is_some()
        && !contains_inplace_hof_intrinsic(body)
}

fn takes_a_function(f: &TFunction) -> bool {
    f.sig
        .params
        .iter()
        .any(|p| matches!(p.ty, Ty::Fn(..)) || p.arity.is_variadic())
}

fn abstract_params(f: &TFunction) -> bool {
    f.sig
        .params
        .iter()
        .any(|p| mentions_abstract_type(&p.ty.to_type()))
        || f.sig
            .return_ty
            .as_ref()
            .is_some_and(|t| mentions_abstract_type(&t.to_type()))
}

fn is_single_use_candidate(
    f: &TFunction,
    counts: &HashMap<&str, usize>,
    binders: &HashSet<&str>,
    skip: &HashSet<String>,
) -> bool {
    counts.get(f.name.text.as_str()) == Some(&1)
        && !f.is_pub
        && f.name.text != "main"
        && !aipl_syntax::is_constant_initializer(&f.name.text)
        && !skip.contains(f.name.text.as_str())
        && !binders.contains(f.name.text.as_str())
        && f.sig.type_vars.is_empty()
        && !abstract_params(f)
        && is_inline_shape(takes_a_function(f), &f.body)
        && !references_name(&f.body, &f.name.text)
}

/// [`crate::inline_single_use`] on the pinned program.
pub fn inline_single_use_pinned(program: &TProgram) -> TProgram {
    let has_fn = |n: &str| functions(program).any(|f| f.name.text == n);
    if !has_fn("main") || has_fn("__test_main") {
        return program.clone();
    }
    let mut program = program.clone();
    let mut skip: HashSet<String> = HashSet::new();
    loop {
        let candidate = {
            let counts = use_counts(&program);
            let binders = collect_binders(&program);
            functions(&program)
                .find(|f| is_single_use_candidate(f, &counts, &binders, &skip))
                .cloned()
        };
        let Some(f) = candidate else { break };
        let fparams = params_of(&f);
        let mut replaced = false;
        for item in &mut program.items {
            if let TItem::Fn(g) = item {
                if references_name(&g.body, &f.name.text) {
                    replace_call(
                        &mut g.body,
                        &f.name.text,
                        &fparams,
                        &f.body,
                        &mut replaced,
                        Sites::First,
                    );
                }
            }
        }
        if replaced {
            program
                .items
                .retain(|item| !matches!(item, TItem::Fn(g) if g.name.text == f.name.text));
        } else {
            skip.insert(f.name.text.clone());
        }
    }
    program
}

fn inline_candidates(
    program: &TProgram,
    binders: &HashSet<&str>,
    max_exprs: usize,
    wanted: Option<&HashSet<String>>,
) -> Vec<TFunction> {
    functions(program)
        .filter(|f| {
            body_size(&f.body) <= max_exprs
                && wanted.is_none_or(|w| w.contains(f.name.text.as_str()))
                && f.name.text != "main"
                && !aipl_syntax::is_constant_initializer(&f.name.text)
                && f.sig.type_vars.is_empty()
                && !abstract_params(f)
                && !binders.contains(f.name.text.as_str())
                && is_inline_shape(takes_a_function(f), &f.body)
                && !references_name(&f.body, &f.name.text)
        })
        .cloned()
        .collect()
}

fn expand_everywhere(program: &mut TProgram, f: &TFunction, sites: Sites) {
    let fparams = params_of(f);
    let mut replaced = false;
    for item in &mut program.items {
        if let TItem::Fn(g) = item {
            if g.name.text == f.name.text {
                continue;
            }
            if references_name(&g.body, &f.name.text) {
                replace_call(
                    &mut g.body,
                    &f.name.text,
                    &fparams,
                    &f.body,
                    &mut replaced,
                    sites,
                );
            }
            if let Some(t) = g.test_body.as_mut() {
                if references_name(t, &f.name.text) {
                    replace_call(t, &f.name.text, &fparams, &f.body, &mut replaced, sites);
                }
            }
        }
    }
}

/// [`crate::inline_small`] on the pinned program.
pub fn inline_small_pinned(program: &TProgram, max_exprs: usize) -> TProgram {
    let mut program = program.clone();
    let candidates = {
        let binders = collect_binders(&program);
        inline_candidates(&program, &binders, max_exprs, None)
    };
    for f in candidates {
        expand_everywhere(&mut program, &f, Sites::All);
    }
    program
}

/// [`crate::inline_inspected`] on the pinned program.
pub fn inline_inspected_pinned(program: &TProgram, max_exprs: usize) -> TProgram {
    let mut program = program.clone();
    let mut inspected = HashSet::new();
    for f in functions(&program) {
        collect_inspected_callees(&f.body, &mut inspected);
        if let Some(t) = &f.test_body {
            collect_inspected_callees(t, &mut inspected);
        }
    }
    let candidates = {
        let binders = collect_binders(&program);
        inline_candidates(&program, &binders, max_exprs, Some(&inspected))
    };
    for f in candidates {
        expand_everywhere(&mut program, &f, Sites::Inspected);
    }
    program
}

fn is_emptiness_check(callee: &Callee) -> bool {
    matches!(callee, Callee::IsEmpty | Callee::IsNonempty)
}

fn collect_inspected_callees(e: &P, out: &mut HashSet<String>) {
    let operand = match &e.kind {
        TExprKind::Call { callee, args, .. } if is_emptiness_check(callee) && args.len() == 1 => {
            Some(&args[0])
        }
        TExprKind::Index(operand, _) => Some(&**operand),
        _ => None,
    };
    if let Some(TExpr {
        kind: TExprKind::Call { callee, .. },
        ..
    }) = operand
    {
        out.insert(callee.name().to_string());
    }
    let _ = any_child(e, |c| {
        collect_inspected_callees(c, out);
        false
    });
}

fn inspected_call<'a>(e: &'a P, fname: &str, arity: usize) -> Option<&'a P> {
    let operand = match &e.kind {
        TExprKind::Call { callee, args, .. } if is_emptiness_check(callee) && args.len() == 1 => {
            &args[0]
        }
        TExprKind::Index(operand, _) => &**operand,
        _ => return None,
    };
    match &operand.kind {
        TExprKind::Call { callee, args, .. } if callee.name() == fname && args.len() == arity => {
            Some(operand)
        }
        _ => None,
    }
}

fn replace_inspected_call(
    e: &P,
    fname: &str,
    fparams: &[Param],
    fbody: &P,
    replaced: &mut bool,
) -> Option<P> {
    let call = inspected_call(e, fname, fparams.len())?;
    let TExprKind::Call {
        args: call_args, ..
    } = &call.kind
    else {
        return None;
    };
    let rc = |x: &P, replaced: &mut bool| {
        let mut x = x.clone();
        replace_call(&mut x, fname, fparams, fbody, replaced, Sites::Inspected);
        x
    };
    let call_args: Vec<P> = call_args.iter().map(|a| rc(a, replaced)).collect();
    *replaced = true;
    let inlined = build_inlined(fparams, fbody, &call_args, call);
    let kind = match &e.kind {
        TExprKind::Call {
            callee,
            type_args,
            method_style,
            ..
        } => TExprKind::Call {
            callee: callee.clone(),
            type_args: type_args.clone(),
            args: vec![inlined],
            method_style: *method_style,
        },
        TExprKind::Index(_, index) => {
            TExprKind::Index(Box::new(inlined), Box::new(rc(index, replaced)))
        }
        _ => return None,
    };
    Some(TExpr::rebuilt(kind, e.ty.clone(), e))
}

fn replace_call(
    e: &mut P,
    fname: &str,
    fparams: &[Param],
    fbody: &P,
    replaced: &mut bool,
    sites: Sites,
) {
    // One site is all `First` rewrites, so once it is done nothing below needs a look.
    if sites == Sites::First && *replaced {
        return;
    }
    if sites == Sites::Inspected {
        if let Some(inlined) = replace_inspected_call(e, fname, fparams, fbody, replaced) {
            *e = inlined;
            return;
        }
    }
    if sites != Sites::Inspected && (!*replaced || sites == Sites::All) {
        if let TExprKind::Call { callee, args, .. } = &e.kind {
            if callee.name() == fname && args.len() == fparams.len() {
                *replaced = true;
                *e = build_inlined(fparams, fbody, args, e);
                return;
            }
        }
    }
    each_child_mut(e, &mut |child| {
        replace_call(child, fname, fparams, fbody, replaced, sites);
    });
}

/// The expansion of `at`, a call whose arguments are `args`: each (freshly renamed) parameter bound
/// to its argument around `fbody`.
fn build_inlined(fparams: &[Param], fbody: &P, args: &[P], at: &P) -> P {
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
    let made = |kind: TExprKind<Ty>, ty: Ty| TExpr::new(kind, ty, at.span.clone());
    // A mutating method yields its receiver: the body ends having assigned to the binding.
    if fparams.first().is_some_and(|p| p.mutable) {
        let receiver = made(TExprKind::Ident(fresh[0].clone()), fparams[0].ty.clone());
        let ty = receiver.ty.clone();
        expr = made(TExprKind::Seq(Box::new(expr), Box::new(receiver)), ty);
    }
    for ((fp, p), arg) in fresh.iter().zip(fparams).zip(args).rev() {
        let ty = expr.ty.clone();
        let (init, body) = (Box::new(arg.clone()), Box::new(expr));
        let kind = if p.mutable {
            TExprKind::LetMut(fp.clone(), init, body)
        } else {
            TExprKind::Let(fp.clone(), init, body)
        };
        expr = made(kind, ty);
        expr.annotation = Some(p.ty.clone());
    }
    // The expansion stands where the call stood, so it answers what the call did.
    expr.ty = at.ty.clone();
    expr
}
