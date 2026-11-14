//! [`crate::counting_variants`] on the pinned tree. A counting variant answers how long its
//! original's result is, so every node that carried the result carries a count instead: the type
//! of each rewritten node is the original's with the array replaced by `u64`.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Callee, Primitive};
use aipl_syntax::typed::{TExpr, TExprKind, TFunction, TItem, TMatchArm, TProgram, TSignature, Ty};

use crate::fold_lengths_pinned;
use crate::typed_util::{any_child, each_child_mut};

type P = TExpr<Ty>;

const COUNTING_SUFFIX: &str = "$len";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Counted {
    Array,
    Optional,
    Result,
}

fn u64_ty() -> Ty {
    Ty::Primitive(Primitive::U64)
}

/// `ty`, a type of the shape's result, with the array it holds replaced by its length's type.
fn counted_ty(shape: Counted, ty: &Ty) -> Option<Ty> {
    match (shape, ty) {
        (Counted::Array, Ty::Array(_)) => Some(u64_ty()),
        (Counted::Optional, Ty::Optional(inner)) if matches!(**inner, Ty::Array(_)) => {
            Some(Ty::Optional(Box::new(u64_ty())))
        }
        (Counted::Result, Ty::Result(ok, err)) if matches!(**ok, Ty::Array(_)) => {
            Some(Ty::Result(Box::new(u64_ty()), err.clone()))
        }
        _ => None,
    }
}

/// [`crate::counting_variants`] on the pinned program.
pub fn counting_variants_pinned(program: &TProgram, effectful: &HashSet<String>) -> TProgram {
    let producers: HashMap<&str, (Counted, &TFunction)> = program
        .items
        .iter()
        .filter_map(|item| match item {
            TItem::Fn(f) => Some((f.name.text.as_str(), (producer_shape(f)?, f))),
            _ => None,
        })
        .collect();
    if producers.is_empty() {
        return program.clone();
    }
    let blocked = crate::sink::undeferrable_fns_pinned(program, effectful);
    let mut cx = Cx {
        blocked: &blocked,
        existing: program
            .items
            .iter()
            .filter_map(|item| match item {
                TItem::Fn(f) => Some(f.name.text.clone()),
                _ => None,
            })
            .collect(),
        producers,
        minted: HashMap::new(),
    };
    let mut items: Vec<TItem> = program
        .items
        .iter()
        .map(|item| match item {
            TItem::Fn(f) => {
                let mut f = f.clone();
                f.body = rewrite(&f.body, &mut cx);
                f.test_body = f.test_body.as_ref().map(|t| rewrite(t, &mut cx));
                TItem::Fn(f)
            }
            other => other.clone(),
        })
        .collect();
    let mut fresh: Vec<TFunction> = cx.minted.into_values().flatten().collect();
    fresh.sort_by(|a, b| a.name.text.cmp(&b.name.text));
    items.extend(fresh.into_iter().map(TItem::Fn));
    TProgram {
        sources: program.sources.clone(),
        doc: program.doc.clone(),
        items,
    }
}

struct Cx<'a> {
    blocked: &'a HashSet<String>,
    existing: HashSet<String>,
    producers: HashMap<&'a str, (Counted, &'a TFunction)>,
    minted: HashMap<String, Option<TFunction>>,
}

fn producer_shape(f: &TFunction) -> Option<Counted> {
    if f.sig.erase().is_mutating() || f.name.text.ends_with(COUNTING_SUFFIX) {
        return None;
    }
    let returns_only = |tp: &aipl_syntax::typed::TTypeParam| {
        !f.sig
            .params
            .iter()
            .any(|p| crate::ty_mentions(&aipl_syntax::typed::TyRepr::to_type(&p.ty), &tp.name))
    };
    if f.sig.type_vars.iter().any(returns_only) {
        return None;
    }
    match f.sig.return_ty.as_ref()? {
        Ty::Array(_) => Some(Counted::Array),
        Ty::Optional(inner) if matches!(**inner, Ty::Array(_)) => Some(Counted::Optional),
        Ty::Result(ok, _) if matches!(**ok, Ty::Array(_)) => Some(Counted::Result),
        _ => None,
    }
}

fn rewrite(e: &P, cx: &mut Cx) -> P {
    let mut out = counted_call(e, cx).unwrap_or_else(|| e.clone());
    each_child_mut(&mut out, &mut |child| {
        let done = rewrite(child, cx);
        *child = done;
    });
    out
}

struct Binding<'a> {
    name: &'a str,
    annotation: &'a Option<Ty>,
    value: &'a P,
}

fn through_bindings(e: &P) -> (Vec<Binding<'_>>, &P) {
    let mut bindings = Vec::new();
    let mut e = e;
    while let TExprKind::Let(name, value, body) = &e.kind {
        bindings.push(Binding {
            name,
            annotation: &e.annotation,
            value,
        });
        e = body;
    }
    (bindings, e)
}

fn under_bindings(bindings: Vec<Binding<'_>>, whole: &P, body: P) -> P {
    bindings.into_iter().rev().fold(body, |body, b| {
        let mut out = TExpr::rebuilt(
            TExprKind::Let(
                b.name.to_string(),
                Box::new(b.value.clone()),
                Box::new(body.clone()),
            ),
            body.ty.clone(),
            whole,
        );
        out.annotation = b.annotation.clone();
        out
    })
}

fn counted_call(whole: &P, cx: &mut Cx) -> Option<P> {
    let TExprKind::Call {
        callee: Callee::Len,
        args,
        ..
    } = &whole.kind
    else {
        return None;
    };
    let [arg] = args.as_slice() else {
        return None;
    };
    let (bindings, measured) = through_bindings(arg);
    let counted = match &measured.kind {
        TExprKind::Call {
            callee: Callee::ValueOr,
            type_args,
            args: value_or_args,
            method_style,
        } => {
            let [optional, default] = value_or_args.as_slice() else {
                return None;
            };
            if !matches!(&default.kind, TExprKind::ArrayLit(e) if e.is_empty()) {
                return None;
            }
            let count = variant_call(optional, Counted::Optional, cx)?;
            let zero = TExpr::new(TExprKind::Int(0), u64_ty(), measured.span.clone());
            TExpr::rebuilt(
                TExprKind::Call {
                    callee: Callee::ValueOr,
                    type_args: type_args.iter().take(1).map(|_| u64_ty()).collect(),
                    args: vec![count, zero],
                    method_style: *method_style,
                },
                u64_ty(),
                measured,
            )
        }
        TExprKind::Try(result) => TExpr::rebuilt(
            TExprKind::Try(Box::new(variant_call(result, Counted::Result, cx)?)),
            u64_ty(),
            measured,
        ),
        _ => variant_call(measured, Counted::Array, cx)?,
    };
    Some(under_bindings(bindings, whole, counted))
}

fn variant_call(call: &P, shape: Counted, cx: &mut Cx) -> Option<P> {
    let TExprKind::Call {
        callee,
        type_args,
        args,
        method_style,
    } = &call.kind
    else {
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
        cx.minted[&name].as_ref()?;
    }
    Some(TExpr::rebuilt(
        TExprKind::Call {
            callee: Callee::User(counting),
            type_args: type_args.clone(),
            args: args.clone(),
            method_style: *method_style,
        },
        counted_ty(shape, &call.ty)?,
        call,
    ))
}

fn mint(
    f: &TFunction,
    shape: Counted,
    counting: &str,
    blocked: &HashSet<String>,
) -> Option<TFunction> {
    if lambda_returns(&f.body) {
        return None;
    }
    let body = counted_body(&f.body, shape)?;
    if !fold_lengths_pinned::folds_anything(&body, blocked) {
        return None;
    }
    let return_ty = counted_ty(shape, f.sig.return_ty.as_ref()?)?;
    Some(TFunction {
        name: f.name.renamed(counting),
        is_pub: false,
        sig: TSignature {
            return_ty: Some(return_ty),
            ..f.sig.clone()
        },
        body,
        test_body: None,
        doc: Some(format!(
            "The counting variant of `{}`: how long its result is, measured \
             rather than built. Synthesized by the compiler for a call whose \
             result only reached `len` (see `aipl_mono::counting_variants`).",
            crate::display_fn(&f.name)
        )),
    })
}

fn lambda_returns(e: &P) -> bool {
    fn has_return(e: &P) -> bool {
        matches!(e.kind, TExprKind::Return(_)) || any_child(e, has_return)
    }
    match &e.kind {
        TExprKind::Lambda(_, body) => has_return(body),
        _ => any_child(e, lambda_returns),
    }
}

fn counted_body(e: &P, shape: Counted) -> Option<P> {
    counted_value(&counted_returns(e, shape)?, shape)
}

fn counted_returns(e: &P, shape: Counted) -> Option<P> {
    if let TExprKind::Return(value) = &e.kind {
        return Some(TExpr::rebuilt(
            TExprKind::Return(Box::new(counted_value(value, shape)?)),
            e.ty.clone(),
            e,
        ));
    }
    if matches!(e.kind, TExprKind::Lambda(..)) {
        return Some(e.clone());
    }
    let mut out = e.clone();
    let mut failed = false;
    each_child_mut(&mut out, &mut |child| match counted_returns(child, shape) {
        Some(done) => *child = done,
        None => failed = true,
    });
    (!failed).then_some(out)
}

fn counted_value(e: &P, shape: Counted) -> Option<P> {
    let sub = |x: &P| counted_value(x, shape);
    let arm = |a: &TMatchArm<Ty>| {
        Some(TMatchArm {
            pattern: a.pattern.clone(),
            body: counted_value(&a.body, shape)?,
            span: a.span.clone(),
        })
    };
    let ty = || counted_ty(shape, &e.ty);
    let kind = match &e.kind {
        TExprKind::If(cond, then, els) => {
            TExprKind::If(cond.clone(), Box::new(sub(then)?), Box::new(sub(els)?))
        }
        TExprKind::Match(scrutinee, arms) => TExprKind::Match(
            scrutinee.clone(),
            arms.iter().map(arm).collect::<Option<Vec<_>>>()?,
        ),
        TExprKind::IfLet(matched, scrutinee, els) => TExprKind::IfLet(
            Box::new(arm(matched)?),
            scrutinee.clone(),
            Box::new(sub(els)?),
        ),
        TExprKind::Let(name, value, body) => {
            let body = sub(body)?;
            let mut out = TExpr::rebuilt(
                TExprKind::Let(name.clone(), value.clone(), Box::new(body.clone())),
                body.ty.clone(),
                e,
            );
            out.annotation = e.annotation.clone();
            return Some(out);
        }
        TExprKind::Seq(first, rest) => {
            let rest = sub(rest)?;
            let ty = rest.ty.clone();
            return Some(TExpr::rebuilt(
                TExprKind::Seq(first.clone(), Box::new(rest)),
                ty,
                e,
            ));
        }
        _ => return counted_leaf(e, shape),
    };
    Some(TExpr::rebuilt(kind, ty()?, e))
}

fn counted_leaf(e: &P, shape: Counted) -> Option<P> {
    let measure = |x: &P| {
        TExpr::new(
            TExprKind::Call {
                callee: Callee::Len,
                type_args: Vec::new(),
                args: vec![x.clone()],
                method_style: true,
            },
            u64_ty(),
            e.span.clone(),
        )
    };
    match (shape, &e.kind) {
        (_, TExprKind::Return(_)) => Some(e.clone()),
        (_, TExprKind::Unit) => None,
        (Counted::Array, _) => Some(measure(e)),
        (
            Counted::Optional,
            TExprKind::Call {
                callee: Callee::Some,
                type_args,
                args,
                method_style,
            },
        )
        | (
            Counted::Result,
            TExprKind::Call {
                callee: Callee::Ok,
                type_args,
                args,
                method_style,
            },
        ) => {
            let [inner] = args.as_slice() else {
                return None;
            };
            let callee = match shape {
                Counted::Optional => Callee::Some,
                _ => Callee::Ok,
            };
            Some(TExpr::rebuilt(
                TExprKind::Call {
                    callee,
                    type_args: type_args.iter().take(1).map(|_| u64_ty()).collect(),
                    args: vec![measure(inner)],
                    method_style: *method_style,
                },
                counted_ty(shape, &e.ty)?,
                e,
            ))
        }
        (Counted::Optional, TExprKind::None)
        | (
            Counted::Result,
            TExprKind::Call {
                callee: Callee::Err,
                ..
            },
        ) => {
            let mut retyped = e.clone();
            retyped.ty = counted_ty(shape, &e.ty)?;
            Some(retyped)
        }
        _ => None,
    }
}
