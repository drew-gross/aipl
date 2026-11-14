//! [`crate::fold_lengths`] on the pinned tree: the same rewrites, each node the pass builds saying
//! its type. A rewritten `len`/`is_empty` answers what the call it replaces answered, and a
//! rewritten `[i]` what the index it replaces did, so a node built for either takes the replaced
//! node's type.

use std::collections::HashSet;

use aipl_syntax::ast::{Callee, Primitive};
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TPattern, TProgram, Ty};

use crate::passes::Scope;
use crate::sink::can_defer_typed;
use crate::typed_util::{any_child, each_child_mut, mentions_free};

type P = TExpr<Ty>;

/// [`crate::fold_lengths`] on the pinned program.
pub fn fold_lengths_pinned(
    program: &TProgram,
    effectful: &HashSet<String>,
    scope: &Scope,
) -> TProgram {
    let blocked = crate::sink::undeferrable_fns_pinned(program, effectful);
    crate::typed_util::map_pinned_bodies(
        program,
        |name| scope.covers(name),
        |_, body| *body = fold_expr(body, &blocked),
    )
}

pub(crate) fn fold_expr(e: &P, blocked: &HashSet<String>) -> P {
    fold_in(e, &mut Vec::new(), blocked)
}

fn fold_in(e: &P, empty: &mut Vec<String>, blocked: &HashSet<String>) -> P {
    if let TExprKind::Call { callee, args, .. } = &e.kind {
        if let (Some(ask), [arg]) = (Ask::of(callee), args.as_slice()) {
            if let Some(folded) = fold_length(e, arg, empty, blocked, ask) {
                return fold_in(&folded, empty, blocked);
            }
        }
    }
    if let TExprKind::Index(operand, index) = &e.kind {
        if let Some(folded) = fold_index(e, operand, index, blocked) {
            return fold_in(&folded, empty, blocked);
        }
    }
    if let TExprKind::Let(name, value, body) = &e.kind {
        let value = fold_in(value, empty, blocked);
        let noted = note_empty(name, &value, empty);
        let body = fold_in(body, empty, blocked);
        empty.truncate(empty.len() - noted as usize);
        let mut out = TExpr::rebuilt(
            TExprKind::Let(name.clone(), Box::new(value), Box::new(body)),
            e.ty.clone(),
            e,
        );
        out.annotation = e.annotation.clone();
        return out;
    }
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| {
        *child = fold_in(child, empty, blocked)
    });
    out
}

fn note_empty(name: &str, value: &P, empty: &mut Vec<String>) -> bool {
    let is_kwarg = name.starts_with(aipl_syntax::KWARG_ARG_PREFIX);
    let push = is_kwarg && matches!(&value.kind, TExprKind::ArrayLit(e) if e.is_empty());
    if push {
        empty.push(name.to_string());
    }
    push
}

pub(crate) fn folds_anything(e: &P, blocked: &HashSet<String>) -> bool {
    fold_expr(e, blocked) != *e
}

fn can_drop(e: &P, blocked: &HashSet<String>) -> bool {
    can_defer_typed(e, blocked) && !consumes_a_map(e)
}

fn consumes_a_map(e: &P) -> bool {
    matches!(
        &e.kind,
        TExprKind::Call {
            callee: Callee::Map,
            ..
        }
    ) || any_child(e, consumes_a_map)
}

fn is_pure(f: &P, blocked: &HashSet<String>) -> bool {
    match &f.kind {
        TExprKind::Lambda(_, body) => can_defer_typed(body, blocked),
        TExprKind::Ident(g) => !blocked.contains(g),
        _ => false,
    }
}

fn apply(func: &P, arg: P) -> Option<P> {
    let TExprKind::Ident(arg_name) = &arg.kind else {
        unreachable!("the applied name is always a bare identifier");
    };
    match &func.kind {
        TExprKind::Lambda(params, body) => {
            let [param] = params.as_slice() else {
                return None;
            };
            let map = std::collections::HashMap::from([(param.name.clone(), arg_name.clone())]);
            Some(crate::typed_util::rename_scoped(body, &map, false))
        }
        TExprKind::Ident(g) => {
            let Ty::Fn(_, result) = &func.ty else {
                return None;
            };
            Some(TExpr::new(
                TExprKind::Call {
                    callee: Callee::resolve(g.clone()),
                    type_args: Vec::new(),
                    args: vec![arg],
                    method_style: false,
                },
                (**result).clone(),
                func.span.clone(),
            ))
        }
        _ => None,
    }
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

fn fold_length(
    whole: &P,
    arg: &P,
    outer: &[String],
    blocked: &HashSet<String>,
    ask: Ask,
) -> Option<P> {
    let (bindings, inner) = through_bindings(arg);
    let counted = {
        let mut empty: Vec<String> = outer.to_vec();
        for b in &bindings {
            note_empty(b.name, b.value, &mut empty);
        }
        count_of(whole, inner, &empty, blocked, ask)?
    };
    Some(under_bindings(bindings, whole, counted))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ask {
    Len,
    IsEmpty,
    IsNonempty,
}

impl Ask {
    fn of(callee: &Callee) -> Option<Ask> {
        match callee {
            Callee::Len => Some(Ask::Len),
            Callee::IsEmpty => Some(Ask::IsEmpty),
            Callee::IsNonempty => Some(Ask::IsNonempty),
            _ => None,
        }
    }

    fn callee(self) -> Callee {
        match self {
            Ask::Len => Callee::Len,
            Ask::IsEmpty => Callee::IsEmpty,
            Ask::IsNonempty => Callee::IsNonempty,
        }
    }

    fn answer(self, n: usize, whole: &P) -> P {
        let kind = match self {
            Ask::Len => TExprKind::Int(n as i64),
            Ask::IsEmpty => TExprKind::Bool(n == 0),
            Ask::IsNonempty => TExprKind::Bool(n != 0),
        };
        TExpr::new(kind, whole.ty.clone(), whole.span.clone())
    }
}

fn count_of(whole: &P, e: &P, empty: &[String], blocked: &HashSet<String>, ask: Ask) -> Option<P> {
    let span = || whole.span.clone();
    let measure = |x: &P| {
        TExpr::new(
            TExprKind::Call {
                callee: ask.callee(),
                type_args: Vec::new(),
                args: vec![x.clone()],
                method_style: true,
            },
            whole.ty.clone(),
            span(),
        )
    };
    let like = |kind| TExpr::rebuilt(kind, whole.ty.clone(), whole);
    match &e.kind {
        TExprKind::ArrayLit(elems) => {
            if elems.iter().any(|x| !can_drop(x, blocked)) {
                return None;
            }
            Some(ask.answer(elems.len(), whole))
        }
        TExprKind::Str(text) if ask != Ask::Len => Some(ask.answer(text.len(), whole)),
        TExprKind::LetMut(..) if ask == Ask::Len => {
            let acc = spread_accumulator(e)?;
            if !acc.appended.iter().all(|p| can_drop(p.value, blocked)) {
                return None;
            }
            Some(add(measure(acc.seed), acc.extra.clone(), whole))
        }
        TExprKind::If(c, a, b) => Some(like(TExprKind::If(
            c.clone(),
            Box::new(measure(a)),
            Box::new(measure(b)),
        ))),
        TExprKind::Match(scrutinee, arms) => Some(like(TExprKind::Match(
            scrutinee.clone(),
            arms.iter().map(|a| measured_arm(a, &measure)).collect(),
        ))),
        TExprKind::IfLet(arm, scrutinee, els) => Some(like(TExprKind::IfLet(
            Box::new(measured_arm(arm, &measure)),
            scrutinee.clone(),
            Box::new(measure(els)),
        ))),
        TExprKind::Seq(first, rest) => {
            Some(like(TExprKind::Seq(first.clone(), Box::new(measure(rest)))))
        }
        TExprKind::Assign(lhs, value, rest) => Some(like(TExprKind::Assign(
            lhs.clone(),
            value.clone(),
            Box::new(measure(rest)),
        ))),
        TExprKind::Call {
            callee,
            type_args,
            args,
            method_style,
        } => match callee {
            Callee::Filter | Callee::FilterMap | Callee::MapJoin | Callee::ValueOr
                if ask != Ask::Len =>
            {
                None
            }
            Callee::Filter | Callee::FilterMap => {
                let [xs, keep, rest @ ..] = args.as_slice() else {
                    return None;
                };
                if *callee == Callee::FilterMap {
                    let [map] = rest else { return None };
                    if !is_pure(map, blocked) {
                        return None;
                    }
                } else if !rest.is_empty() {
                    return None;
                }
                Some(like(TExprKind::Call {
                    callee: Callee::CountIf,
                    type_args: type_args.iter().take(1).cloned().collect(),
                    args: vec![xs.clone(), keep.clone()],
                    method_style: *method_style,
                }))
            }
            Callee::MapJoin => sum_of_parts(whole, args, empty, blocked),
            Callee::ValueOr => optional_count(whole, args, empty, blocked),
            _ if callee.preserves_length() && *callee != Callee::Map => {
                let [xs, fns @ ..] = args.as_slice() else {
                    return None;
                };
                if !fns.iter().all(|f| is_pure(f, blocked)) {
                    return None;
                }
                Some(measure(xs))
            }
            _ => None,
        },
        _ => None,
    }
}

fn fold_index(whole: &P, operand: &P, index: &P, blocked: &HashSet<String>) -> Option<P> {
    if !matches!(index.kind, TExprKind::Int(_) | TExprKind::Ident(_)) {
        return None;
    }
    let (bindings, inner) = through_bindings(operand);
    if let TExprKind::Ident(name) = &index.kind {
        if bindings.iter().any(|b| b.name == name) {
            return None;
        }
    }
    let picked = index_of(whole, inner, index, blocked)?;
    Some(under_bindings(bindings, whole, picked))
}

fn index_of(whole: &P, e: &P, index: &P, blocked: &HashSet<String>) -> Option<P> {
    let like = |kind| TExpr::rebuilt(kind, whole.ty.clone(), whole);
    let at = |x: &P| {
        TExpr::rebuilt(
            TExprKind::Index(Box::new(x.clone()), Box::new(index.clone())),
            whole.ty.clone(),
            whole,
        )
    };
    match &e.kind {
        TExprKind::ArrayLit(elems) => {
            let droppable = |skip: Option<usize>| {
                elems
                    .iter()
                    .enumerate()
                    .all(|(i, x)| Some(i) == skip || can_drop(x, blocked))
            };
            match &index.kind {
                TExprKind::Int(k) => {
                    let k = usize::try_from(*k).ok()?;
                    match elems.get(k) {
                        Some(picked) => {
                            if !droppable(Some(k)) {
                                return None;
                            }
                            Some(like(TExprKind::Call {
                                callee: Callee::Some,
                                type_args: Vec::new(),
                                args: vec![picked.clone()],
                                method_style: false,
                            }))
                        }
                        None if droppable(None) => none_for(whole, e),
                        None => None,
                    }
                }
                _ if elems.is_empty() => none_for(whole, e),
                _ => None,
            }
        }
        TExprKind::If(c, a, b) => Some(like(TExprKind::If(
            c.clone(),
            Box::new(at(a)),
            Box::new(at(b)),
        ))),
        TExprKind::Match(scrutinee, arms) => Some(like(TExprKind::Match(
            scrutinee.clone(),
            arms.iter()
                .map(|a| TMatchArm {
                    pattern: a.pattern.clone(),
                    body: at(&a.body),
                    span: a.span.clone(),
                })
                .collect(),
        ))),
        TExprKind::IfLet(arm, scrutinee, els) => Some(like(TExprKind::IfLet(
            Box::new(TMatchArm {
                pattern: arm.pattern.clone(),
                body: at(&arm.body),
                span: arm.span.clone(),
            }),
            scrutinee.clone(),
            Box::new(at(els)),
        ))),
        _ => None,
    }
}

fn none_for(whole: &P, e: &P) -> Option<P> {
    let Ty::Array(element) = &e.ty else {
        return None;
    };
    if element.mentions_var() {
        return None;
    }
    Some(TExpr::new(
        TExprKind::None,
        Ty::Optional(element.clone()),
        whole.span.clone(),
    ))
}

fn measured_arm(arm: &TMatchArm<Ty>, measure: &dyn Fn(&P) -> P) -> TMatchArm<Ty> {
    TMatchArm {
        pattern: arm.pattern.clone(),
        body: measure(&arm.body),
        span: arm.span.clone(),
    }
}

fn add(a: P, b: P, whole: &P) -> P {
    match (&a.kind, &b.kind) {
        (TExprKind::Int(0), _) => b,
        (_, TExprKind::Int(0)) => a,
        _ => TExpr::new(
            TExprKind::Call {
                callee: Callee::WrappingAdd,
                type_args: Vec::new(),
                args: vec![a, b],
                method_style: false,
            },
            whole.ty.clone(),
            whole.span.clone(),
        ),
    }
}

fn sum_of_parts(whole: &P, args: &[P], empty: &[String], blocked: &HashSet<String>) -> Option<P> {
    let [xs, f, seps @ ..] = args else {
        return None;
    };
    if !seps.iter().all(|s| is_empty_sequence(s, empty)) {
        return None;
    }
    if !is_pure(f, blocked) {
        return None;
    }
    let id = aipl_syntax::fresh_id();
    let u64_ty = Ty::Primitive(Primitive::U64);
    let element = match &xs.ty {
        Ty::Array(e) => (**e).clone(),
        _ => return None,
    };
    let span = || whole.span.clone();
    let (total, elem) = (format!("$len{id}_n"), format!("$len{id}_x"));
    let at = |n: &str, ty: &Ty| TExpr::new(TExprKind::Ident(n.to_string()), ty.clone(), span());
    let unit = || TExpr::new(TExprKind::Unit, Ty::Unit, span());
    let piece = TExpr::new(
        TExprKind::Call {
            callee: Callee::Len,
            type_args: Vec::new(),
            args: vec![apply(f, at(&elem, &element))?],
            method_style: true,
        },
        u64_ty.clone(),
        span(),
    );
    let sum = TExpr::new(
        TExprKind::Call {
            callee: Callee::WrappingAdd,
            type_args: Vec::new(),
            args: vec![at(&total, &u64_ty), piece],
            method_style: false,
        },
        u64_ty.clone(),
        span(),
    );
    let step = TExpr::new(
        TExprKind::Assign(
            Box::new(at(&total, &u64_ty)),
            Box::new(sum),
            Box::new(unit()),
        ),
        Ty::Unit,
        span(),
    );
    let walk = TExpr::new(
        TExprKind::Seq(
            Box::new(TExpr::new(
                TExprKind::For(elem, None, Box::new(xs.clone()), Box::new(step)),
                Ty::Primitive(Primitive::I64),
                span(),
            )),
            Box::new(at(&total, &u64_ty)),
        ),
        u64_ty.clone(),
        span(),
    );
    let mut out = TExpr::rebuilt(
        TExprKind::LetMut(
            total,
            Box::new(TExpr::new(TExprKind::Int(0), u64_ty.clone(), span())),
            Box::new(walk),
        ),
        u64_ty.clone(),
        whole,
    );
    out.annotation = Some(u64_ty);
    Some(out)
}

fn optional_count(whole: &P, args: &[P], empty: &[String], blocked: &HashSet<String>) -> Option<P> {
    let [mapped, default] = args else {
        return None;
    };
    if !is_empty_sequence(default, empty) {
        return None;
    }
    let TExprKind::Call {
        callee: Callee::Map,
        args: map_args,
        ..
    } = &mapped.kind
    else {
        return None;
    };
    let [opt, f] = map_args.as_slice() else {
        return None;
    };
    if !is_pure(f, blocked) {
        return None;
    }
    let Ty::Optional(payload) = &opt.ty else {
        return None;
    };
    let span = || whole.span.clone();
    let u64_ty = Ty::Primitive(Primitive::U64);
    let bound = format!("$len{}_v", aipl_syntax::fresh_id());
    let some = TMatchArm {
        pattern: TPattern::Ctor {
            name: "some".to_string(),
            bindings: vec![bound.clone()],
            ignore_payload: false,
        },
        body: TExpr::new(
            TExprKind::Call {
                callee: Callee::Len,
                type_args: Vec::new(),
                args: vec![apply(
                    f,
                    TExpr::new(TExprKind::Ident(bound), (**payload).clone(), span()),
                )?],
                method_style: true,
            },
            u64_ty.clone(),
            span(),
        ),
        span: span(),
    };
    let none = TMatchArm {
        pattern: TPattern::Ctor {
            name: "none".to_string(),
            bindings: Vec::new(),
            ignore_payload: false,
        },
        body: TExpr::new(TExprKind::Int(0), u64_ty, span()),
        span: span(),
    };
    Some(TExpr::rebuilt(
        TExprKind::Match(Box::new(opt.clone()), vec![some, none]),
        whole.ty.clone(),
        whole,
    ))
}

fn is_empty_sequence(e: &P, empty: &[String]) -> bool {
    match &e.kind {
        TExprKind::ArrayLit(elems) => elems.is_empty(),
        TExprKind::Ident(name) => empty.iter().any(|n| n == name),
        _ => false,
    }
}

pub(crate) struct SpreadAccumulator<'a> {
    pub(crate) seed: &'a P,
    pub(crate) extra: &'a P,
    pub(crate) appended: Vec<AppendedPiece<'a>>,
}

pub(crate) struct AppendedPiece<'a> {
    pub(crate) spread: bool,
    pub(crate) value: &'a P,
}

/// [`crate::fold_lengths::spread_accumulator`] over the pinned tree.
pub(crate) fn spread_accumulator(e: &P) -> Option<SpreadAccumulator<'_>> {
    let TExprKind::LetMut(acc, reserved, body) = &e.kind else {
        return None;
    };
    let TExprKind::Call {
        callee: Callee::ArrReserve,
        args: reserve_args,
        ..
    } = &reserved.kind
    else {
        return None;
    };
    let [seed, extra] = reserve_args.as_slice() else {
        return None;
    };
    let reads_acc = |x: &P| mentions_free(x, acc);
    if reads_acc(seed) || reads_acc(extra) {
        return None;
    }
    let mut appended: Vec<AppendedPiece<'_>> = Vec::new();
    let mut rest = &**body;
    loop {
        match &rest.kind {
            TExprKind::Ident(name) if name == acc => {
                return Some(SpreadAccumulator {
                    seed,
                    extra,
                    appended,
                })
            }
            TExprKind::Assign(lhs, value, next) => {
                if !matches!(&lhs.kind, TExprKind::Ident(name) if name == acc) {
                    return None;
                }
                let TExprKind::Call {
                    callee: intrinsic @ (Callee::ArrAppend | Callee::ArrConcat),
                    args: call_args,
                    ..
                } = &value.kind
                else {
                    return None;
                };
                let [receiver, element] = call_args.as_slice() else {
                    return None;
                };
                if !matches!(&receiver.kind, TExprKind::Ident(name) if name == acc)
                    || reads_acc(element)
                {
                    return None;
                }
                appended.push(AppendedPiece {
                    spread: *intrinsic == Callee::ArrConcat,
                    value: element,
                });
                rest = next;
            }
            _ => return None,
        }
    }
}
