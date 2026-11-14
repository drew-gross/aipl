//! [`crate::fuse_operations`] on the pinned tree. Every shape is the one the `Expr` families in
//! `fuse/` rewrite; what differs is that each node the rewrite builds says its type. A fused call
//! answers what the expression it replaces answered, so it takes that expression's type; the
//! helper values it introduces take the type of what they stand for (an element's, a function's
//! result).

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast::{BinOp, Callee, Item, Primitive, SeqShape};
use aipl_syntax::typed::{TExpr, TExprKind, TLambdaParam, TMatchArm, TPattern, TProgram, Ty};

use crate::passes::Scope;
use crate::typed_util::{any_child, assigned_names, each_child_mut, read_names};

type P = TExpr<Ty>;

/// [`crate::fuse_operations`] on the pinned program.
pub fn fuse_operations_pinned(
    program: &TProgram,
    effectful: &HashSet<String>,
    scope: &Scope,
) -> TProgram {
    let blocked = crate::sink::undeferrable_fns_pinned(program, effectful);
    let guards = Guards {
        effectful,
        blocked: &blocked,
    };
    crate::typed_util::map_pinned_bodies(
        program,
        |name| scope.covers(name),
        |_, body| fuse_expr(body, None, &guards),
    )
}

struct Guards<'a> {
    effectful: &'a HashSet<String>,
    blocked: &'a HashSet<String>,
}

fn fuse_expr(e: &mut P, outer: Option<Callee>, guards: &Guards) {
    match &mut e.kind {
        TExprKind::Call { callee, args, .. } => {
            let name = callee.clone();
            for (i, arg) in args.iter_mut().enumerate() {
                fuse_expr(arg, (i == 0).then(|| name.clone()), guards);
            }
        }
        TExprKind::Let(_, value, body) => {
            fuse_expr(value, None, guards);
            fuse_expr(body, outer.clone(), guards);
        }
        _ => each_child_mut(e, &mut |c| fuse_expr(c, None, guards)),
    }
    if let Some(fused) = try_fuse(e, outer.as_ref(), guards) {
        *e = fused;
    }
}

fn try_fuse(e: &P, outer: Option<&Callee>, guards: &Guards) -> Option<P> {
    let fused = match &e.kind {
        TExprKind::Call { callee, args, .. } => {
            match (aipl_syntax::binop_for_builtin(callee), args.as_slice()) {
                (Some(op), [l, r]) => comparison(l, op, r, e),
                _ => compose(e, guards.blocked)
                    .or_else(|| predicate(e, guards.blocked))
                    .or_else(|| predicate_end(e, guards.blocked))
                    .or_else(|| predicate_emptiness(e, guards.blocked))
                    .or_else(|| slice(e))
                    .or_else(|| chain(e, outer)),
            }
        }
        TExprKind::For(..) => return loops(e, guards.blocked),
        TExprKind::Index(..) => return index(e, guards.blocked),
        _ => None,
    }?;
    (!has_effect(e, guards.effectful)).then_some(fused)
}

fn has_effect(e: &P, effectful: &HashSet<String>) -> bool {
    let here = match &e.kind {
        TExprKind::Call { callee, .. } => effectful.contains(callee.name()),
        TExprKind::Ident(name) => effectful.contains(name),
        TExprKind::Shim(..) => true,
        _ => false,
    };
    here || any_child(e, |c| has_effect(c, effectful))
}

// ---- helpers ----

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

fn mentions_free(e: &P, name: &str) -> bool {
    crate::typed_util::mentions_free(e, name)
}

/// The element a `for` over a value of type `ty` binds.
fn element_of(ty: &Ty) -> Option<Ty> {
    match ty {
        Ty::Array(e) | Ty::Set(e, _) => Some((**e).clone()),
        Ty::Dict(_, v) => Some((**v).clone()),
        Ty::Primitive(Primitive::Str) => Some(Ty::Primitive(Primitive::Char)),
        Ty::Named(n) if n == aipl_syntax::SPAN_TYPE => Some(Ty::Primitive(Primitive::U64)),
        _ => None,
    }
}

fn fn_param(f: &P) -> Option<Ty> {
    match &f.ty {
        Ty::Fn(params, _) => params.first().cloned(),
        _ => None,
    }
}

fn fn_result(f: &P) -> Option<Ty> {
    match &f.ty {
        Ty::Fn(_, ret) => Some((**ret).clone()),
        _ => None,
    }
}

fn ident(name: &str, ty: Ty, like: &P) -> P {
    TExpr::new(TExprKind::Ident(name.to_string()), ty, like.span.clone())
}

fn call(
    callee: Callee,
    type_args: Vec<Ty>,
    args: Vec<P>,
    method_style: bool,
    ty: Ty,
    like: &P,
) -> P {
    TExpr::rebuilt(
        TExprKind::Call {
            callee,
            type_args,
            args,
            method_style,
        },
        ty,
        like,
    )
}

/// `func` applied to the bare name `arg` (see `loop_fusions::apply`).
fn apply(func: &P, arg: P) -> Option<P> {
    let TExprKind::Ident(arg_name) = &arg.kind else {
        unreachable!("the fused loop variable is always a bare identifier");
    };
    match &func.kind {
        TExprKind::Lambda(params, body) => {
            let [param] = params.as_slice() else {
                return None;
            };
            let map = HashMap::from([(param.name.clone(), arg_name.clone())]);
            Some(crate::typed_util::rename_scoped(body, &map, false))
        }
        TExprKind::Ident(g) => {
            let result = fn_result(func)?;
            Some(TExpr::new(
                TExprKind::Call {
                    callee: Callee::resolve(g.clone()),
                    type_args: Vec::new(),
                    args: vec![arg],
                    method_style: false,
                },
                result,
                func.span.clone(),
            ))
        }
        _ => None,
    }
}

fn is_pure(func: &P, blocked: &HashSet<String>) -> bool {
    match &func.kind {
        TExprKind::Lambda(_, body) => crate::sink::can_defer_typed(body, blocked),
        TExprKind::Ident(g) => !blocked.contains(g),
        _ => false,
    }
}

fn lambda(param: TLambdaParam<Ty>, body: P, like: &P) -> P {
    let ty = Ty::Fn(vec![param.ty.clone()], Box::new(body.ty.clone()));
    TExpr::rebuilt(TExprKind::Lambda(vec![param], Box::new(body)), ty, like)
}

// ---- comparisons ----

const COMPARISONS: &[(Callee, BinOp, Callee)] = &[
    (Callee::Count, BinOp::Lt, Callee::CountIsLessThan),
    (Callee::Count, BinOp::Le, Callee::CountIsAtMost),
    (Callee::Count, BinOp::Gt, Callee::CountIsGreaterThan),
    (Callee::Count, BinOp::Ge, Callee::CountIsAtLeast),
    (Callee::Count, BinOp::Eq, Callee::CountIsEqual),
    (Callee::Count, BinOp::Ne, Callee::CountIsNotEqual),
    (Callee::CountIf, BinOp::Lt, Callee::CountIfIsLessThan),
    (Callee::CountIf, BinOp::Le, Callee::CountIfIsAtMost),
    (Callee::CountIf, BinOp::Gt, Callee::CountIfIsGreaterThan),
    (Callee::CountIf, BinOp::Ge, Callee::CountIfIsAtLeast),
    (Callee::CountIf, BinOp::Eq, Callee::CountIfIsEqual),
    (Callee::CountIf, BinOp::Ne, Callee::CountIfIsNotEqual),
];

fn comparison(l: &P, op: BinOp, r: &P, whole: &P) -> Option<P> {
    comparison_one(l, op, r, whole).or_else(|| comparison_one(r, flip(op), l, whole))
}

fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Gt => BinOp::Lt,
        BinOp::Le => BinOp::Ge,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

fn comparison_one(c: &P, op: BinOp, bound: &P, whole: &P) -> Option<P> {
    let TExprKind::Call {
        callee,
        type_args,
        args,
        method_style,
    } = &c.kind
    else {
        return None;
    };
    let (_, _, into) = COMPARISONS
        .iter()
        .find(|(call, o, _)| call == callee && *o == op)?;
    let mut fused = args.clone();
    fused.push(bound.clone());
    Some(call(
        into.clone(),
        type_args.clone(),
        fused,
        *method_style,
        whole.ty.clone(),
        whole,
    ))
}

// ---- slices ----

fn slice(whole: &P) -> Option<P> {
    let TExprKind::Call {
        callee,
        type_args,
        args,
        method_style,
    } = &whole.kind
    else {
        return None;
    };
    if *callee != Callee::StartsWith(SeqShape::Seq) {
        return None;
    }
    let (recv, rest) = args.split_first()?;
    let TExprKind::Slice(obj, start, None) = &recv.kind else {
        return None;
    };
    let mut fused = vec![(**obj).clone()];
    fused.extend(rest.iter().cloned());
    fused.push((**start).clone());
    Some(call(
        Callee::StartsWithAt(SeqShape::Seq),
        type_args.clone(),
        fused,
        *method_style,
        whole.ty.clone(),
        whole,
    ))
}

// ---- index ----

fn index(whole: &P, blocked: &HashSet<String>) -> Option<P> {
    let TExprKind::Index(recv, idx) = &whole.kind else {
        return None;
    };
    let (bindings, inner) = through_bindings(recv);
    let TExprKind::Call {
        callee: Callee::Map,
        type_args,
        args,
        method_style,
    } = &inner.kind
    else {
        return None;
    };
    let [xs, f] = args.as_slice() else {
        return None;
    };
    if !is_pure(f, blocked) {
        return None;
    }
    if bindings.iter().any(|b| mentions_free(idx, b.name)) {
        return None;
    }
    let element = element_of(&xs.ty)?;
    let indexed = TExpr::rebuilt(
        TExprKind::Index(Box::new(xs.clone()), idx.clone()),
        Ty::Optional(Box::new(element)),
        whole,
    );
    let mapped = call(
        Callee::Map,
        type_args.clone(),
        vec![indexed, f.clone()],
        *method_style,
        whole.ty.clone(),
        whole,
    );
    Some(under_bindings(bindings, whole, mapped))
}

// ---- compose ----

fn compose(whole: &P, blocked: &HashSet<String>) -> Option<P> {
    let TExprKind::Call {
        callee: Callee::Map,
        type_args,
        args,
        method_style,
    } = &whole.kind
    else {
        return None;
    };
    let [receiver, g] = args.as_slice() else {
        return None;
    };
    let (bindings, inner) = through_bindings(receiver);
    let TExprKind::Call {
        callee: Callee::Map,
        type_args: inner_type_args,
        args: inner_args,
        ..
    } = &inner.kind
    else {
        return None;
    };
    let [source, f] = inner_args.as_slice() else {
        return None;
    };
    if bindings.iter().any(|b| mentions_free(g, b.name)) {
        return None;
    }
    if !is_pure(f, blocked) || !is_pure(g, blocked) {
        return None;
    }
    if matches!(&f.kind, TExprKind::Lambda(_, body) if crate::typed_util::contains_context_literal(body))
    {
        return None;
    }
    let id = aipl_syntax::fresh_id();
    let (x, y) = (format!("$compose{id}_x"), format!("$compose{id}_y"));
    let x_ty = fn_param(f)?;
    let y_ty = fn_result(f)?;
    let f_of_x = apply(f, ident(&x, x_ty.clone(), whole))?;
    let g_of_y = apply(g, ident(&y, y_ty, whole))?;
    let param = TLambdaParam {
        name: x,
        ty: x_ty,
        span: f.span.clone(),
    };
    let g_ty = g_of_y.ty.clone();
    let body = TExpr::rebuilt(
        TExprKind::Let(y, Box::new(f_of_x), Box::new(g_of_y)),
        g_ty,
        g,
    );
    let composed = lambda(param, body, g);
    // `map<T, U>` over the source's element and the outer map's result.
    let type_args = match (inner_type_args.first(), type_args.get(1)) {
        (Some(a), Some(b)) => vec![a.clone(), b.clone()],
        _ => type_args.clone(),
    };
    let fused = call(
        Callee::Map,
        type_args,
        vec![source.clone(), composed],
        *method_style,
        whole.ty.clone(),
        whole,
    );
    Some(under_bindings(bindings, whole, fused))
}

// ---- predicates ----

enum Stage<'a> {
    Map { f: &'a P },
    Filter { p: &'a P },
    FilterMap { p: &'a P, f: &'a P },
}

impl<'a> Stage<'a> {
    fn of(callee: &Callee, args: &'a [P]) -> Option<(&'a P, Stage<'a>)> {
        match (callee, args) {
            (Callee::Map, [source, f]) => Some((source, Stage::Map { f })),
            (Callee::Filter, [source, p]) => Some((source, Stage::Filter { p })),
            (Callee::FilterMap, [source, p, f]) => Some((source, Stage::FilterMap { p, f })),
            _ => None,
        }
    }

    fn functions(&self) -> Vec<&'a P> {
        match self {
            Stage::Map { f } => vec![f],
            Stage::Filter { p } => vec![p],
            Stage::FilterMap { p, f } => vec![p, f],
        }
    }

    fn first_function(&self) -> &'a P {
        match self {
            Stage::Map { f } => f,
            Stage::Filter { p } | Stage::FilterMap { p, .. } => p,
        }
    }

    fn mapping(&self) -> Option<&'a P> {
        match self {
            Stage::Map { f } | Stage::FilterMap { f, .. } => Some(f),
            Stage::Filter { .. } => None,
        }
    }

    fn feeds(&self, consumer: &Callee) -> bool {
        match self {
            Stage::Map { .. } => matches!(
                consumer,
                Callee::Any | Callee::All | Callee::CountIf | Callee::FindMap | Callee::FindIndex
            ),
            Stage::FilterMap { .. } => matches!(
                consumer,
                Callee::Any | Callee::All | Callee::CountIf | Callee::FindMap
            ),
            Stage::Filter { .. } => matches!(
                consumer,
                Callee::Any
                    | Callee::All
                    | Callee::CountIf
                    | Callee::FindIf
                    | Callee::FindMap
                    | Callee::Filter
            ),
        }
    }
}

fn predicate_end(whole: &P, blocked: &HashSet<String>) -> Option<P> {
    let TExprKind::Call {
        callee: end,
        type_args,
        args,
        method_style,
    } = &whole.kind
    else {
        return None;
    };
    let [receiver] = args.as_slice() else {
        return None;
    };
    if !matches!(end, Callee::First | Callee::Last) {
        return None;
    }
    let (bindings, inner) = through_bindings(receiver);
    let TExprKind::Call {
        callee: stage_callee,
        type_args: stage_type_args,
        args: inner_args,
        ..
    } = &inner.kind
    else {
        return None;
    };
    let (source, stage) = Stage::of(stage_callee, inner_args)?;
    if !stage.functions().into_iter().all(|f| is_pure(f, blocked)) {
        return None;
    }
    let walked = |source: &P| match end {
        Callee::First => source.clone(),
        _ => call(
            Callee::Reverse,
            type_args.first().cloned().into_iter().collect(),
            vec![source.clone()],
            false,
            source.ty.clone(),
            whole,
        ),
    };
    let found_ty = |source: &P| element_of(&source.ty).map(|e| Ty::Optional(Box::new(e)));
    let call_ty = match &stage {
        Stage::Map { .. } | Stage::FilterMap { .. } => whole.ty.clone(),
        Stage::Filter { .. } => whole.ty.clone(),
    };
    let fused = match stage {
        Stage::Map { f } => {
            let element = call(
                end.clone(),
                type_args.clone(),
                vec![source.clone()],
                false,
                found_ty(source)?,
                whole,
            );
            call(
                Callee::Map,
                stage_type_args.clone(),
                vec![element, f.clone()],
                *method_style,
                call_ty,
                whole,
            )
        }
        Stage::Filter { p } => call(
            Callee::FindIf,
            type_args.clone(),
            vec![walked(source), p.clone()],
            *method_style,
            call_ty,
            whole,
        ),
        Stage::FilterMap { p, f } => {
            let found = call(
                Callee::FindIf,
                type_args.clone(),
                vec![walked(source), p.clone()],
                *method_style,
                found_ty(source)?,
                whole,
            );
            call(
                Callee::Map,
                stage_type_args.clone(),
                vec![found, f.clone()],
                *method_style,
                call_ty,
                whole,
            )
        }
    };
    Some(under_bindings(bindings, whole, fused))
}

fn predicate_emptiness(whole: &P, blocked: &HashSet<String>) -> Option<P> {
    let TExprKind::Call {
        callee: asked,
        args,
        method_style,
        ..
    } = &whole.kind
    else {
        return None;
    };
    let [receiver] = args.as_slice() else {
        return None;
    };
    if !matches!(asked, Callee::IsEmpty | Callee::IsNonempty) {
        return None;
    }
    let (bindings, inner) = through_bindings(receiver);
    let TExprKind::Call {
        callee: stage_callee,
        type_args: stage_type_args,
        args: inner_args,
        ..
    } = &inner.kind
    else {
        return None;
    };
    let (source, stage) = Stage::of(stage_callee, inner_args)?;
    let (Stage::Filter { p } | Stage::FilterMap { p, .. }) = &stage else {
        return None;
    };
    if !stage.functions().into_iter().all(|f| is_pure(f, blocked)) {
        return None;
    }
    let any = call(
        Callee::Any,
        stage_type_args.iter().take(1).cloned().collect(),
        vec![source.clone(), (*p).clone()],
        *method_style,
        Ty::Primitive(Primitive::Bool),
        whole,
    );
    let answer = match asked {
        Callee::IsNonempty => any,
        _ => call(
            Callee::LogicalNot,
            Vec::new(),
            vec![any],
            false,
            Ty::Primitive(Primitive::Bool),
            whole,
        ),
    };
    Some(under_bindings(bindings, whole, answer))
}

fn predicate(whole: &P, blocked: &HashSet<String>) -> Option<P> {
    let TExprKind::Call {
        callee: consumer,
        type_args,
        args,
        method_style,
    } = &whole.kind
    else {
        return None;
    };
    let [receiver, g] = args.as_slice() else {
        return None;
    };
    let (bindings, inner) = through_bindings(receiver);
    let TExprKind::Call {
        callee: stage_callee,
        args: inner_args,
        ..
    } = &inner.kind
    else {
        return None;
    };
    let (source, stage) = Stage::of(stage_callee, inner_args)?;
    if !stage.feeds(consumer) {
        return None;
    }
    if bindings.iter().any(|b| mentions_free(g, b.name)) {
        return None;
    }
    if !is_pure(g, blocked) || !stage.functions().into_iter().all(|f| is_pure(f, blocked)) {
        return None;
    }
    if matches!(stage.mapping().map(|f| &f.kind), Some(TExprKind::Lambda(_, body)) if crate::typed_util::contains_context_literal(body))
    {
        return None;
    }
    let id = aipl_syntax::fresh_id();
    let (x, y) = (format!("$predicate{id}_x"), format!("$predicate{id}_y"));
    let x_ty = fn_param(stage.first_function())?;
    let mapped_then_g = |f: &P| {
        let f_of_x = apply(f, ident(&x, x_ty.clone(), whole))?;
        let y_ty = fn_result(f)?;
        let g_of_y = apply(g, ident(&y, y_ty, whole))?;
        let ty = g_of_y.ty.clone();
        Some(TExpr::rebuilt(
            TExprKind::Let(y.clone(), Box::new(f_of_x), Box::new(g_of_y)),
            ty,
            g,
        ))
    };
    let guarded = |p: &P, kept: P| {
        let p_of_x = apply(p, ident(&x, x_ty.clone(), whole))?;
        let ty = kept.ty.clone();
        let miss = match consumer {
            Callee::All => TExprKind::Bool(true),
            Callee::FindMap => TExprKind::None,
            _ => TExprKind::Bool(false),
        };
        let missed = TExpr::new(miss, ty.clone(), g.span.clone());
        Some(TExpr::rebuilt(
            TExprKind::If(Box::new(p_of_x), Box::new(kept), Box::new(missed)),
            ty,
            g,
        ))
    };
    let body = match &stage {
        Stage::Map { f } => mapped_then_g(f)?,
        Stage::Filter { p } => guarded(p, apply(g, ident(&x, x_ty.clone(), whole))?)?,
        Stage::FilterMap { p, f } => guarded(p, mapped_then_g(f)?)?,
    };
    let param = TLambdaParam {
        name: x,
        ty: x_ty.clone(),
        span: stage.first_function().span.clone(),
    };
    let composed = lambda(param, body, g);
    let fused = call(
        consumer.clone(),
        type_args.clone(),
        vec![source.clone(), composed],
        *method_style,
        whole.ty.clone(),
        whole,
    );
    Some(under_bindings(bindings, whole, fused))
}

// ---- chains ----

const CHAINS: &[(Callee, Callee, Callee)] = &[
    (Callee::Filter, Callee::Map, Callee::FilterMap),
    (Callee::Map, Callee::FindIf, Callee::MapFindIf),
    (Callee::Map, Callee::Join, Callee::MapJoin),
    (Callee::Map, Callee::ToDict, Callee::MapToDict),
    (Callee::Join, Callee::IsNonempty, Callee::JoinIsNonempty),
    (Callee::Join, Callee::IsEmpty, Callee::JoinIsEmpty),
    (Callee::Split, Callee::Map, Callee::SplitMap),
    (Callee::Split, Callee::Len, Callee::SplitLen),
    (Callee::ToStr, Callee::Len, Callee::ToStrLen),
    (Callee::Sort, Callee::First, Callee::Minimum),
    (Callee::Sort, Callee::Last, Callee::Maximum),
];

const STAGES: &[(Callee, Callee)] = &[
    (Callee::Map, Callee::MapReduce),
    (Callee::FilterMap, Callee::FilterMapReduce),
];

fn chain_row(inner: &Callee, outer: &Callee) -> Option<&'static Callee> {
    CHAINS
        .iter()
        .find(|(i, o, _)| i == inner && o == outer)
        .map(|(_, _, into)| into)
}

fn stage_row(stage: &Callee) -> Option<&'static Callee> {
    STAGES
        .iter()
        .find(|(s, _)| s == stage)
        .map(|(_, into)| into)
}

fn stands_aside_for(whole: &P, name: &Callee, outer: &Callee) -> bool {
    if chain_row(name, outer).is_some() {
        return true;
    }
    stage_row(name).is_some()
        && outer.is_reduction()
        && !(*name == Callee::Map && has_filter_receiver(whole))
}

fn has_filter_receiver(c: &P) -> bool {
    let TExprKind::Call { args, .. } = &c.kind else {
        return false;
    };
    let Some(receiver) = args.first() else {
        return false;
    };
    let (_, inner) = through_bindings(receiver);
    matches!(
        &inner.kind,
        TExprKind::Call {
            callee: Callee::Filter,
            ..
        }
    )
}

fn chain(whole: &P, outer: Option<&Callee>) -> Option<P> {
    let TExprKind::Call {
        callee: name,
        args,
        method_style,
        ..
    } = &whole.kind
    else {
        return None;
    };
    if outer.is_some_and(|o| stands_aside_for(whole, name, o)) {
        return None;
    }
    let (recv, rest) = args.split_first()?;
    let (bindings, inner) = through_bindings(recv);
    let TExprKind::Call {
        callee: inner_name,
        type_args: inner_type_args,
        args: inner_args,
        ..
    } = &inner.kind
    else {
        return None;
    };
    let (into, step) = match chain_row(inner_name, name) {
        Some(into) => (into.clone(), None),
        None => {
            let step = name.reduction_step()?;
            let into = stage_row(inner_name)?;
            (into.clone(), Some(step))
        }
    };
    if bindings
        .iter()
        .any(|b| rest.iter().any(|a| mentions_free(a, b.name)))
    {
        return None;
    }
    let mut fused = inner_args.clone();
    fused.extend(rest.iter().cloned());
    if let Some(step) = step {
        // The step combines two of what the stage produced.
        let produced = element_of(&inner.ty)?;
        fused.push(TExpr::new(
            TExprKind::Ident(step.name().to_string()),
            Ty::Fn(vec![produced.clone(), produced.clone()], Box::new(produced)),
            whole.span.clone(),
        ));
    }
    let fused = call(
        into,
        inner_type_args.clone(),
        fused,
        *method_style,
        whole.ty.clone(),
        whole,
    );
    Some(under_bindings(bindings, whole, fused))
}

// ---- loops ----

fn loops(whole: &P, blocked: &HashSet<String>) -> Option<P> {
    let TExprKind::For(var, index, iterable, body) = &whole.kind else {
        return None;
    };
    let (bindings, iterable) = through_bindings(iterable);
    if bindings
        .iter()
        .any(|b| b.name != var && mentions_free(body, b.name))
    {
        return None;
    }
    let rebuild = |fused: P| under_bindings(bindings, whole, fused);
    let TExprKind::Call {
        callee: name, args, ..
    } = &iterable.kind
    else {
        return None;
    };
    let for_over = |wrapped: P| {
        TExpr::rebuilt(
            TExprKind::For(var.clone(), index.clone(), Box::new(wrapped), body.clone()),
            whole.ty.clone(),
            whole,
        )
    };
    if *name == Callee::TupleWindows {
        let [recv] = args.as_slice() else {
            return None;
        };
        let fused = windows(whole, var, index, recv, iterable, body)?;
        return Some(rebuild(fused));
    }
    if *name == Callee::Split {
        let [recv, sep] = args.as_slice() else {
            return None;
        };
        let mut written = HashSet::new();
        assigned_names(body, &mut written);
        let mut read = HashSet::new();
        read_names(recv, &mut read);
        read_names(sep, &mut read);
        if read.iter().any(|n| written.contains(n)) {
            return None;
        }
        let wrapped = call(
            Callee::SplitIter,
            Vec::new(),
            vec![recv.clone(), sep.clone()],
            false,
            iterable.ty.clone(),
            iterable,
        );
        return Some(rebuild(for_over(wrapped)));
    }
    if *name == Callee::Reverse {
        let [recv] = args.as_slice() else {
            return None;
        };
        let wrapped = call(
            Callee::ReverseIter,
            Vec::new(),
            vec![recv.clone()],
            false,
            iterable.ty.clone(),
            iterable,
        );
        return Some(rebuild(for_over(wrapped)));
    }
    if *name == Callee::CartesianProduct {
        let [recv, other] = args.as_slice() else {
            return None;
        };
        if index.is_some() {
            return None;
        }
        let fused = product(whole, var, recv, other, iterable, body)?;
        return Some(rebuild(fused));
    }
    if index.is_some() {
        return None;
    }
    // (keep, map): which of the call's functions filter and which map.
    let (keep_at, map_at) = match name {
        Callee::Map => (None, Some(0)),
        Callee::Filter => (Some(0), None),
        Callee::FilterMap => (Some(0), Some(1)),
        _ => return None,
    };
    let (recv, fns) = args.split_first()?;
    if fns.len() != usize::from(keep_at.is_some()) + usize::from(map_at.is_some()) {
        return None;
    }
    let keep = keep_at.map(|i: usize| &fns[i]);
    let map = map_at.map(|i: usize| &fns[i]);
    let mut written = HashSet::new();
    assigned_names(body, &mut written);
    for func in [keep, map].into_iter().flatten() {
        if !is_pure(func, blocked) {
            return None;
        }
        let mut read = HashSet::new();
        read_names(func, &mut read);
        if read.iter().any(|n| written.contains(n)) {
            return None;
        }
    }
    let element = element_of(&recv.ty)?;
    let elem = format!("$fuse{}_{}", aipl_syntax::fresh_id(), var);
    let elem_ref = || {
        TExpr::new(
            TExprKind::Ident(elem.clone()),
            element.clone(),
            iterable.span.clone(),
        )
    };
    let mapped = match map {
        Some(func) => apply(func, elem_ref())?,
        None => elem_ref(),
    };
    let mut inner = TExpr::new(
        TExprKind::Let(var.clone(), Box::new(mapped), body.clone()),
        body.ty.clone(),
        iterable.span.clone(),
    );
    if let Some(pred) = keep {
        let unit = || TExpr::new(TExprKind::Unit, Ty::Unit, iterable.span.clone());
        inner = TExpr::new(
            TExprKind::If(
                Box::new(apply(pred, elem_ref())?),
                Box::new(TExpr::new(
                    TExprKind::Seq(Box::new(inner), Box::new(unit())),
                    Ty::Unit,
                    iterable.span.clone(),
                )),
                Box::new(unit()),
            ),
            Ty::Unit,
            iterable.span.clone(),
        );
    }
    Some(rebuild(TExpr::rebuilt(
        TExprKind::For(elem.clone(), None, Box::new(recv.clone()), Box::new(inner)),
        whole.ty.clone(),
        whole,
    )))
}

fn windows(
    whole: &P,
    var: &str,
    index: &Option<String>,
    recv: &P,
    iterable: &P,
    body: &P,
) -> Option<P> {
    let id = aipl_syntax::fresh_id();
    let element = element_of(&recv.ty)?;
    let pair_ty = element_of(&iterable.ty)?;
    let bool_ty = Ty::Primitive(Primitive::Bool);
    let u64_ty = Ty::Primitive(Primitive::U64);
    let sp = || recv.span.clone();
    let name = |what: &str| format!("$fuse{id}_{what}");
    let at = |n: &str, ty: &Ty| TExpr::new(TExprKind::Ident(n.to_string()), ty.clone(), sp());
    let unit = || TExpr::new(TExprKind::Unit, Ty::Unit, sp());
    let node = |kind: TExprKind<Ty>, ty: Ty| TExpr::new(kind, ty, sp());
    let (src, first, prev, skip, cur, widx) = (
        name("src"),
        name("first"),
        name("prev"),
        name("skip"),
        name("cur"),
        name("widx"),
    );
    let unskip = node(
        TExprKind::Assign(
            Box::new(at(&skip, &bool_ty)),
            Box::new(node(TExprKind::Bool(false), bool_ty.clone())),
            Box::new(unit()),
        ),
        Ty::Unit,
    );
    let pair = node(
        TExprKind::TupleLit(vec![at(&prev, &element), at(&cur, &element)]),
        pair_ty,
    );
    let bump = match index {
        None => unit(),
        Some(_) => node(
            TExprKind::Assign(
                Box::new(at(&widx, &u64_ty)),
                Box::new(node(
                    TExprKind::Call {
                        callee: Callee::WrappingAdd,
                        type_args: Vec::new(),
                        args: vec![at(&widx, &u64_ty), node(TExprKind::Int(1), u64_ty.clone())],
                        method_style: false,
                    },
                    u64_ty.clone(),
                )),
                Box::new(unit()),
            ),
            Ty::Unit,
        ),
    };
    let advance = node(
        TExprKind::Assign(
            Box::new(at(&prev, &element)),
            Box::new(at(&cur, &element)),
            Box::new(bump),
        ),
        Ty::Unit,
    );
    let run_ty = advance.ty.clone();
    let run = node(
        TExprKind::Seq(Box::new(body.clone()), Box::new(advance)),
        run_ty,
    );
    let run = match index {
        None => run,
        Some(i) => {
            let ty = run.ty.clone();
            node(
                TExprKind::Let(i.clone(), Box::new(at(&widx, &u64_ty)), Box::new(run)),
                ty,
            )
        }
    };
    let ty = run.ty.clone();
    let paired = node(
        TExprKind::Let(var.to_string(), Box::new(pair), Box::new(run)),
        ty,
    );
    let ty = paired.ty.clone();
    let step = node(
        TExprKind::If(
            Box::new(at(&skip, &bool_ty)),
            Box::new(unskip),
            Box::new(paired),
        ),
        ty,
    );
    let loop_ = node(
        TExprKind::Seq(
            Box::new(TExpr::rebuilt(
                TExprKind::For(cur, None, Box::new(at(&src, &recv.ty)), Box::new(step)),
                whole.ty.clone(),
                whole,
            )),
            Box::new(unit()),
        ),
        Ty::Unit,
    );
    let ty = loop_.ty.clone();
    let skip_binding = node(
        TExprKind::LetMut(
            skip,
            Box::new(node(TExprKind::Bool(true), bool_ty.clone())),
            Box::new(loop_),
        ),
        ty.clone(),
    );
    let carried = node(
        TExprKind::LetMut(prev, Box::new(at(&first, &element)), Box::new(skip_binding)),
        ty,
    );
    let ty = carried.ty.clone();
    let peeled = node(
        TExprKind::IfLet(
            Box::new(TMatchArm {
                pattern: TPattern::Ctor {
                    name: "some".to_string(),
                    bindings: vec![first],
                    ignore_payload: false,
                },
                body: carried,
                span: sp(),
            }),
            Box::new(node(
                TExprKind::Index(
                    Box::new(at(&src, &recv.ty)),
                    Box::new(node(TExprKind::Int(0), u64_ty.clone())),
                ),
                Ty::Optional(Box::new(element.clone())),
            )),
            Box::new(unit()),
        ),
        ty,
    );
    let counted = match index {
        None => peeled,
        Some(_) => {
            let ty = peeled.ty.clone();
            let mut counter = node(
                TExprKind::LetMut(
                    widx,
                    Box::new(node(TExprKind::Int(0), u64_ty.clone())),
                    Box::new(peeled),
                ),
                ty,
            );
            counter.annotation = Some(u64_ty);
            counter
        }
    };
    let ty = counted.ty.clone();
    Some(TExpr::rebuilt(
        TExprKind::Let(src, Box::new(recv.clone()), Box::new(counted)),
        ty,
        whole,
    ))
}

fn product(whole: &P, var: &str, recv: &P, other: &P, iterable: &P, body: &P) -> Option<P> {
    let id = aipl_syntax::fresh_id();
    let left_ty = element_of(&recv.ty)?;
    let right_ty = element_of(&other.ty)?;
    let pair_ty = element_of(&iterable.ty)?;
    let sp = || recv.span.clone();
    let name = |what: &str| format!("$fuse{id}_{what}");
    let at = |n: &str, ty: &Ty| TExpr::new(TExprKind::Ident(n.to_string()), ty.clone(), sp());
    let node = |kind: TExprKind<Ty>, ty: Ty| TExpr::new(kind, ty, sp());
    let (left, right, a, b) = (name("left"), name("right"), name("a"), name("b"));
    let pair = node(
        TExprKind::TupleLit(vec![at(&a, &left_ty), at(&b, &right_ty)]),
        pair_ty,
    );
    let paired = node(
        TExprKind::Let(var.to_string(), Box::new(pair), Box::new(body.clone())),
        body.ty.clone(),
    );
    let inner = node(
        TExprKind::For(b, None, Box::new(at(&right, &other.ty)), Box::new(paired)),
        whole.ty.clone(),
    );
    let outer_body = node(
        TExprKind::Seq(Box::new(inner), Box::new(node(TExprKind::Unit, Ty::Unit))),
        Ty::Unit,
    );
    let outer = TExpr::rebuilt(
        TExprKind::For(a, None, Box::new(at(&left, &recv.ty)), Box::new(outer_body)),
        whole.ty.clone(),
        whole,
    );
    let ty = outer.ty.clone();
    let right_bound = node(
        TExprKind::Let(right, Box::new(other.clone()), Box::new(outer)),
        ty.clone(),
    );
    Some(node(
        TExprKind::Let(left, Box::new(recv.clone()), Box::new(right_bound)),
        ty,
    ))
}

/// The functions in `items` whose signature declares an effect — what
/// [`fuse_operations_pinned`] wants for its `effectful` argument. Pass the declarations
/// the checker saw, so builtin signatures (`print` is `!prints`) are included.
pub fn effectful_fns(items: &[Item]) -> HashSet<String> {
    items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) if !f.sig.effects.is_empty() => Some(f.name.text.clone()),
            _ => None,
        })
        .collect()
}
