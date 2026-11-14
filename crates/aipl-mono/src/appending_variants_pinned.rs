//! [`crate::appending_variants`] on the pinned tree. An appending variant has no result: it writes
//! into its receiver, so what was a value in tail position becomes a statement and every node on
//! the way there is unit.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{Arity, Callee};
use aipl_syntax::typed::{
    TExpr, TExprKind, TFunction, TItem, TMatchArm, TParam, TProgram, TSignature, Ty,
};

use crate::fold_extends_pinned;
use crate::typed_util::{any_child, each_child_mut, mentions_free, references_name};

type P = TExpr<Ty>;

const APPENDING_SUFFIX: &str = "$into";

const RECEIVER: &str = "self";

/// [`crate::appending_variants`] on the pinned program.
pub fn appending_variants_pinned(program: &TProgram) -> TProgram {
    let producers: HashMap<&str, &TFunction> = program
        .items
        .iter()
        .filter_map(|item| match item {
            TItem::Fn(f) => Some((f.name.text.as_str(), f)).filter(|_| is_producer(f)),
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
    existing: HashSet<String>,
    producers: HashMap<&'a str, &'a TFunction>,
    minted: HashMap<String, Option<TFunction>>,
}

fn is_producer(f: &TFunction) -> bool {
    !f.sig.erase().is_mutating()
        && !f.name.text.ends_with(APPENDING_SUFFIX)
        && matches!(f.sig.return_ty, Some(Ty::Array(_)))
}

fn rewrite(e: &P, cx: &mut Cx) -> P {
    let mut out = appending_call(e, cx).unwrap_or_else(|| e.clone());
    each_child_mut(&mut out, &mut |child| {
        let done = rewrite(child, cx);
        *child = done;
    });
    out
}

fn is_receiver(e: &P, name: &str) -> bool {
    match &e.kind {
        TExprKind::Ident(n) => n == name,
        TExprKind::Call {
            callee: Callee::Move,
            args,
            ..
        } => matches!(args.as_slice(), [a] if matches!(&a.kind, TExprKind::Ident(n) if n == name)),
        _ => false,
    }
}

fn appending_call(whole: &P, cx: &mut Cx) -> Option<P> {
    let TExprKind::Assign(lhs, value, rest) = &whole.kind else {
        return None;
    };
    let TExprKind::Ident(receiver) = &lhs.kind else {
        return None;
    };
    let TExprKind::Call {
        callee: Callee::Extend | Callee::ArrConcat,
        args: extend_args,
        ..
    } = &value.kind
    else {
        return None;
    };
    let [target, source] = extend_args.as_slice() else {
        return None;
    };
    if !is_receiver(target, receiver) {
        return None;
    }
    let TExprKind::Call {
        callee,
        type_args,
        args,
        ..
    } = &source.kind
    else {
        return None;
    };
    if mentions_free(source, receiver) {
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
        cx.minted[&key].as_ref()?;
    }
    let mut call_args = vec![target.clone()];
    call_args.extend(args.iter().cloned());
    // A mutating call answers its receiver.
    let call = TExpr::rebuilt(
        TExprKind::Call {
            callee: Callee::User(variant),
            type_args: type_args.clone(),
            args: call_args,
            method_style: true,
        },
        target.ty.clone(),
        source,
    );
    Some(TExpr::rebuilt(
        TExprKind::Assign(lhs.clone(), Box::new(call), rest.clone()),
        whole.ty.clone(),
        whole,
    ))
}

fn mint(f: &TFunction, variant: &str) -> Option<TFunction> {
    if lambda_returns(&f.body) || mentions_receiver(&f.body) {
        return None;
    }
    let array_ty = f.sig.return_ty.clone()?;
    let body = match promote_accumulator(&f.body, &array_ty) {
        Some(promoted) => promoted,
        None => {
            let body = appended_body(&f.body, &array_ty)?;
            if !fold_extends_pinned::folds_anything(&body) {
                return None;
            }
            body
        }
    };
    let receiver = TParam {
        name_span: Default::default(),
        name: RECEIVER.to_string(),
        ty: array_ty,
        mutable: true,
        arity: Arity::One,
        default: None,
        implicit_some: false,
    };
    let mut params = vec![receiver];
    params.extend(f.sig.params.iter().cloned());
    Some(TFunction {
        name: f.name.renamed(variant),
        is_pub: false,
        sig: TSignature {
            params,
            return_ty: None,
            ..f.sig.clone()
        },
        body,
        test_body: None,
        doc: Some(format!(
            "The appending variant of `{}`: its elements, appended into the \
             caller's array instead of collected into one of its own. \
             Synthesized by the compiler for a call whose result only reached \
             `extend` (see `aipl_mono::appending_variants`).",
            crate::display_fn(&f.name)
        )),
    })
}

fn mentions_receiver(e: &P) -> bool {
    mentions_free(e, RECEIVER) || references_name(e, RECEIVER)
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

fn appended_body(e: &P, array_ty: &Ty) -> Option<P> {
    let with_returns = appended_returns(e, array_ty)?;
    Some(append_stmt(&with_returns, array_ty))
}

fn appended_returns(e: &P, array_ty: &Ty) -> Option<P> {
    if let TExprKind::Return(value) = &e.kind {
        let appended = append_stmt(value, array_ty);
        let bare = TExpr::rebuilt(
            TExprKind::Return(Box::new(TExpr::new(
                TExprKind::Unit,
                Ty::Unit,
                e.span.clone(),
            ))),
            e.ty.clone(),
            e,
        );
        return Some(TExpr::rebuilt(
            TExprKind::Seq(Box::new(appended), Box::new(bare)),
            e.ty.clone(),
            e,
        ));
    }
    if matches!(e.kind, TExprKind::Lambda(..)) {
        return Some(e.clone());
    }
    let mut out = e.clone();
    let mut failed = false;
    each_child_mut(
        &mut out,
        &mut |child| match appended_returns(child, array_ty) {
            Some(done) => *child = done,
            None => failed = true,
        },
    );
    (!failed).then_some(out)
}

fn append_stmt(value: &P, array_ty: &Ty) -> P {
    let span = || value.span.clone();
    let target = || {
        TExpr::new(
            TExprKind::Ident(RECEIVER.to_string()),
            array_ty.clone(),
            span(),
        )
    };
    let call = TExpr::new(
        TExprKind::Call {
            callee: Callee::Extend,
            type_args: Vec::new(),
            args: vec![target(), value.clone()],
            method_style: true,
        },
        array_ty.clone(),
        span(),
    );
    TExpr::new(
        TExprKind::Assign(
            Box::new(target()),
            Box::new(call),
            Box::new(TExpr::new(TExprKind::Unit, Ty::Unit, span())),
        ),
        Ty::Unit,
        span(),
    )
}

fn is_appender(callee: &Callee) -> bool {
    matches!(
        callee,
        Callee::Push
            | Callee::Extend
            | Callee::ExtendMap
            | Callee::ExtendFilter
            | Callee::ExtendFilterMap
            | Callee::ArrAppend
            | Callee::ArrConcat
    )
}

fn promote_accumulator(body: &P, array_ty: &Ty) -> Option<P> {
    match &body.kind {
        TExprKind::LetMut(acc, seed, rest) if matches!(&seed.kind, TExprKind::ArrayLit(elems) if elems.is_empty()) => {
            promote(rest, acc, true, array_ty)
        }
        TExprKind::Let(name, value, rest) => {
            let rest = promote_accumulator(rest, array_ty)?;
            let mut out = TExpr::rebuilt(
                TExprKind::Let(name.clone(), value.clone(), Box::new(rest.clone())),
                rest.ty.clone(),
                body,
            );
            out.annotation = body.annotation.clone();
            Some(out)
        }
        TExprKind::Seq(first, rest) => {
            let rest = promote_accumulator(rest, array_ty)?;
            Some(TExpr::rebuilt(
                TExprKind::Seq(first.clone(), Box::new(rest.clone())),
                rest.ty.clone(),
                body,
            ))
        }
        _ => None,
    }
}

/// `e` with `acc` promoted to the receiver; `tail` says a use of `acc` here is the function's
/// result, which a variant without one leaves as a statement.
fn promote(e: &P, acc: &str, tail: bool, array_ty: &Ty) -> Option<P> {
    let unit = || TExpr::rebuilt(TExprKind::Unit, Ty::Unit, e);
    // A node on the way to the tail ends in a statement now.
    let ty = if tail { Ty::Unit } else { e.ty.clone() };
    let rebuilt = |kind| Some(TExpr::rebuilt(kind, ty.clone(), e));
    match &e.kind {
        TExprKind::Ident(name) if name == acc => tail.then(unit),
        TExprKind::Return(value) => match &value.kind {
            TExprKind::Ident(name) if name == acc => Some(TExpr::rebuilt(
                TExprKind::Return(Box::new(unit())),
                e.ty.clone(),
                e,
            )),
            _ => None,
        },
        TExprKind::Assign(lhs, value, rest) if matches!(&lhs.kind, TExprKind::Ident(n) if n == acc) =>
        {
            let TExprKind::Call {
                callee,
                type_args,
                args,
                method_style,
            } = &value.kind
            else {
                return None;
            };
            let [target, operands @ ..] = args.as_slice() else {
                return None;
            };
            if !is_appender(callee)
                || !is_receiver(target, acc)
                || operands.iter().any(|o| mentions_free(o, acc))
            {
                return None;
            }
            let receiver = || {
                TExpr::rebuilt(
                    TExprKind::Ident(RECEIVER.to_string()),
                    array_ty.clone(),
                    lhs,
                )
            };
            let mut call_args = vec![receiver()];
            call_args.extend(operands.iter().cloned());
            let call = TExpr::rebuilt(
                TExprKind::Call {
                    callee: callee.clone(),
                    type_args: type_args.clone(),
                    args: call_args,
                    method_style: *method_style,
                },
                value.ty.clone(),
                value,
            );
            rebuilt(TExprKind::Assign(
                Box::new(receiver()),
                Box::new(call),
                Box::new(promote(rest, acc, tail, array_ty)?),
            ))
        }
        TExprKind::Let(name, value, rest) => {
            if name == acc {
                return None;
            }
            let mut out = TExpr::rebuilt(
                TExprKind::Let(
                    name.clone(),
                    Box::new(promote(value, acc, false, array_ty)?),
                    Box::new(promote(rest, acc, tail, array_ty)?),
                ),
                ty,
                e,
            );
            out.annotation = e.annotation.clone();
            Some(out)
        }
        TExprKind::LetMut(name, value, rest) => {
            if name == acc {
                return None;
            }
            let mut out = TExpr::rebuilt(
                TExprKind::LetMut(
                    name.clone(),
                    Box::new(promote(value, acc, false, array_ty)?),
                    Box::new(promote(rest, acc, tail, array_ty)?),
                ),
                ty,
                e,
            );
            out.annotation = e.annotation.clone();
            Some(out)
        }
        TExprKind::Seq(first, rest) => rebuilt(TExprKind::Seq(
            Box::new(promote(first, acc, false, array_ty)?),
            Box::new(promote(rest, acc, tail, array_ty)?),
        )),
        TExprKind::If(cond, then, otherwise) => rebuilt(TExprKind::If(
            Box::new(promote(cond, acc, false, array_ty)?),
            Box::new(promote(then, acc, tail, array_ty)?),
            Box::new(promote(otherwise, acc, tail, array_ty)?),
        )),
        TExprKind::Match(scrutinee, arms) => {
            let mut promoted = Vec::with_capacity(arms.len());
            for arm in arms {
                if arm.pattern.bindings().iter().any(|b| b == acc) {
                    return None;
                }
                promoted.push(TMatchArm {
                    body: promote(&arm.body, acc, tail, array_ty)?,
                    ..arm.clone()
                });
            }
            rebuilt(TExprKind::Match(
                Box::new(promote(scrutinee, acc, false, array_ty)?),
                promoted,
            ))
        }
        TExprKind::Lambda(..) => (!mentions_free(e, acc)).then(|| e.clone()),
        TExprKind::For(name, index, ..) if aipl_syntax::for_binds(name, index, acc) => None,
        _ => {
            let mut out = e.clone();
            let mut failed = false;
            each_child_mut(
                &mut out,
                &mut |child| match promote(child, acc, false, array_ty) {
                    Some(done) => *child = done,
                    None => failed = true,
                },
            );
            (!failed).then_some(out)
        }
    }
}
