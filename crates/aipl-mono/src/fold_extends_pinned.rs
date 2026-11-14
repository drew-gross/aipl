//! [`crate::fold_extends`] on the pinned tree. What the pass builds are statements, so each node
//! it builds is unit; an `extend` writeback assigns its receiver, whose type is the receiver's.

use aipl_syntax::ast::Callee;
use aipl_syntax::typed::{TExpr, TExprKind, TMatchArm, TParam, TPattern, TProgram, Ty};

use crate::fold_lengths_pinned::spread_accumulator;
use crate::passes::Scope;
use crate::typed_util::{each_child_mut, mentions_free};

type P = TExpr<Ty>;

/// [`crate::fold_extends`] on the pinned program.
pub fn fold_extends_pinned(program: &TProgram, scope: &Scope) -> TProgram {
    use aipl_syntax::typed::TItem;
    let mut out = program.clone();
    for item in &mut out.items {
        if let TItem::Fn(f) = item {
            if scope.covers(&f.name.text) {
                f.body = fold_expr(&f.body, &mut ArrayReceivers::from_params(&f.sig.params));
                f.test_body = f
                    .test_body
                    .as_ref()
                    .map(|t| fold_expr(t, &mut ArrayReceivers::from_params(&f.sig.params)));
            }
        }
    }
    out
}

/// What is known, while walking a body, about the names an `extend` may write back to.
#[derive(Default)]
pub(crate) struct ArrayReceivers {
    receivers: Vec<(String, bool)>,
    empty: Vec<String>,
}

impl ArrayReceivers {
    fn from_params(params: &[TParam<Ty>]) -> ArrayReceivers {
        ArrayReceivers {
            receivers: params
                .iter()
                .filter(|p| p.mutable)
                .map(|p| (p.name.clone(), matches!(p.ty, Ty::Array(_))))
                .collect(),
            ..ArrayReceivers::default()
        }
    }

    fn known_array(&self, e: &P) -> bool {
        matches!(e.kind, TExprKind::ArrayLit(_)) || matches!(e.ty, Ty::Array(_))
    }

    fn holds_array(&self, name: &str) -> bool {
        self.receivers
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .is_some_and(|(_, yes)| *yes)
    }

    fn declare(&mut self, name: &str, annotation: &Option<Ty>, value: &P) {
        let holds = match annotation {
            Some(ty) => matches!(ty, Ty::Array(_)),
            None => matches!(
                value.kind,
                TExprKind::ArrayLit(_)
                    | TExprKind::Call {
                        callee: Callee::ArrReserve,
                        ..
                    }
            ),
        };
        self.receivers.push((name.to_string(), holds));
    }

    fn undeclare(&mut self) {
        self.receivers.pop();
    }

    fn is_empty_sequence(&self, e: &P) -> bool {
        match &e.kind {
            TExprKind::ArrayLit(elems) => elems.is_empty(),
            TExprKind::Ident(name) => self.empty.iter().any(|n| n == name),
            _ => false,
        }
    }

    fn note_empty(&mut self, name: &str, value: &P) -> bool {
        let push = name.starts_with(aipl_syntax::KWARG_ARG_PREFIX)
            && matches!(&value.kind, TExprKind::ArrayLit(e) if e.is_empty());
        if push {
            self.empty.push(name.to_string());
        }
        push
    }

    fn forget_empty(&mut self, noted: bool) {
        if noted {
            self.empty.pop();
        }
    }
}

fn fold_expr(e: &P, arrays: &mut ArrayReceivers) -> P {
    if let Some(folded) = extend_statement(e).and_then(|stmt| appended(stmt, arrays)) {
        return fold_expr(&folded, arrays);
    }
    if let TExprKind::LetMut(name, value, body) = &e.kind {
        let value = fold_expr(value, arrays);
        arrays.declare(name, &e.annotation, &value);
        let body = fold_expr(body, arrays);
        arrays.undeclare();
        let mut out = TExpr::rebuilt(
            TExprKind::LetMut(name.clone(), Box::new(value), Box::new(body)),
            e.ty.clone(),
            e,
        );
        out.annotation = e.annotation.clone();
        return out;
    }
    if let TExprKind::Let(name, value, body) = &e.kind {
        let value = fold_expr(value, arrays);
        let noted = arrays.note_empty(name, &value);
        let body = fold_expr(body, arrays);
        arrays.forget_empty(noted);
        let mut out = TExpr::rebuilt(
            TExprKind::Let(name.clone(), Box::new(value), Box::new(body)),
            e.ty.clone(),
            e,
        );
        out.annotation = e.annotation.clone();
        return out;
    }
    let mut out = e.clone();
    each_child_mut(&mut out, &mut |child| *child = fold_expr(child, arrays));
    out
}

pub(crate) fn folds_anything(e: &P) -> bool {
    fold_expr(e, &mut ArrayReceivers::from_params(&[])) != *e
}

struct ExtendStatement<'a> {
    receiver: &'a str,
    /// The type of the receiver: what the writeback call answers.
    receiver_ty: &'a Ty,
    source: &'a P,
    rest: &'a P,
    whole: &'a P,
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

fn extend_statement(e: &P) -> Option<ExtendStatement<'_>> {
    let TExprKind::Assign(lhs, value, rest) = &e.kind else {
        return None;
    };
    let TExprKind::Ident(receiver) = &lhs.kind else {
        return None;
    };
    let TExprKind::Call {
        callee,
        args,
        method_style,
        ..
    } = &value.kind
    else {
        return None;
    };
    if !(matches!(callee, Callee::Extend) && *method_style || matches!(callee, Callee::ArrConcat)) {
        return None;
    }
    let [target, source] = args.as_slice() else {
        return None;
    };
    if !is_receiver(target, receiver) {
        return None;
    }
    Some(ExtendStatement {
        receiver,
        receiver_ty: &lhs.ty,
        source,
        rest,
        whole: e,
    })
}

fn appended(stmt: ExtendStatement<'_>, arrays: &ArrayReceivers) -> Option<P> {
    if mentions_free(stmt.source, stmt.receiver) {
        return None;
    }
    let appends = appends(stmt.receiver, stmt.receiver_ty, stmt.source, arrays)?;
    Some(TExpr::rebuilt(
        TExprKind::Seq(Box::new(appends), Box::new(stmt.rest.clone())),
        stmt.whole.ty.clone(),
        stmt.whole,
    ))
}

/// `v = callee(v, arg)`, for `v` of type `receiver_ty`.
fn writeback(v: &str, receiver_ty: &Ty, callee: Callee, type_args: Vec<Ty>, arg: &P) -> P {
    let span = || arg.span.clone();
    let target = || TExpr::new(TExprKind::Ident(v.to_string()), receiver_ty.clone(), span());
    let call = TExpr::new(
        TExprKind::Call {
            callee,
            type_args,
            args: vec![target(), arg.clone()],
            method_style: true,
        },
        receiver_ty.clone(),
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

fn appends(v: &str, receiver_ty: &Ty, src: &P, arrays: &ArrayReceivers) -> Option<P> {
    let span = || src.span.clone();
    let unit = || TExpr::new(TExprKind::Unit, Ty::Unit, span());
    let seq = |a: P, b: P| TExpr::new(TExprKind::Seq(Box::new(a), Box::new(b)), Ty::Unit, span());
    let like = |kind| TExpr::rebuilt(kind, Ty::Unit, src);
    let extend = |x: &P| writeback(v, receiver_ty, Callee::Extend, Vec::new(), x);
    let push = |x: &P| writeback(v, receiver_ty, Callee::Push, Vec::new(), x);
    match &src.kind {
        TExprKind::ArrayLit(elems) if elems.is_empty() => Some(unit()),
        TExprKind::ArrayLit(elems) => {
            if !arrays.holds_array(v) {
                return None;
            }
            Some(
                elems
                    .iter()
                    .rev()
                    .fold(unit(), |rest, x| seq(push(x), rest)),
            )
        }
        TExprKind::LetMut(..) => {
            let acc = spread_accumulator(src)?;
            let mut out = unit();
            for piece in acc.appended.iter().rev() {
                let step = match piece.spread {
                    true => extend(piece.value),
                    false if arrays.holds_array(v) => push(piece.value),
                    false => return None,
                };
                out = seq(step, out);
            }
            Some(seq(extend(acc.seed), out))
        }
        TExprKind::Call {
            callee: callee @ (Callee::MapJoin | Callee::Join),
            args,
            ..
        } => {
            let (xs, f, seps) = match (callee, args.as_slice()) {
                (Callee::MapJoin, [xs, f, seps @ ..]) => (xs, Some(f), seps),
                (Callee::Join, [xs, seps @ ..]) => (xs, None, seps),
                _ => return None,
            };
            if !seps.iter().all(|s| arrays.is_empty_sequence(s)) {
                return None;
            }
            let Ty::Array(element_ty) = &xs.ty else {
                return None;
            };
            let elem = format!("$ext{}_x", aipl_syntax::fresh_id());
            let element = TExpr::new(
                TExprKind::Ident(elem.clone()),
                (**element_ty).clone(),
                span(),
            );
            let piece = match f {
                Some(f) => apply(f, element)?,
                None => element,
            };
            let walk = TExpr::new(
                TExprKind::For(elem, None, Box::new(xs.clone()), Box::new(extend(&piece))),
                Ty::Primitive(aipl_syntax::ast::Primitive::I64),
                span(),
            );
            Some(seq(walk, unit()))
        }
        TExprKind::Call {
            callee: callee @ (Callee::Map | Callee::Filter | Callee::FilterMap),
            type_args,
            args,
            ..
        } => {
            let extending = match (callee, args.len()) {
                (Callee::Map, 2) => Callee::ExtendMap,
                (Callee::Filter, 2) => Callee::ExtendFilter,
                (Callee::FilterMap, 3) => Callee::ExtendFilterMap,
                _ => return None,
            };
            if !(arrays.holds_array(v) && arrays.known_array(&args[0])) {
                return None;
            }
            let target =
                || TExpr::new(TExprKind::Ident(v.to_string()), receiver_ty.clone(), span());
            let mut call_args = vec![target()];
            call_args.extend(args.iter().cloned());
            Some(TExpr::new(
                TExprKind::Assign(
                    Box::new(target()),
                    Box::new(TExpr::new(
                        TExprKind::Call {
                            callee: extending,
                            type_args: type_args.clone(),
                            args: call_args,
                            method_style: true,
                        },
                        receiver_ty.clone(),
                        span(),
                    )),
                    Box::new(unit()),
                ),
                Ty::Unit,
                span(),
            ))
        }
        TExprKind::Call {
            callee: Callee::ValueOr,
            args,
            ..
        } => {
            let [mapped, default] = args.as_slice() else {
                return None;
            };
            if !arrays.is_empty_sequence(default) {
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
            let Ty::Optional(payload) = &opt.ty else {
                return None;
            };
            let bound = format!("$ext{}_v", aipl_syntax::fresh_id());
            let piece = apply(
                f,
                TExpr::new(TExprKind::Ident(bound.clone()), (**payload).clone(), span()),
            )?;
            Some(like(TExprKind::Match(
                Box::new(opt.clone()),
                vec![
                    arm("some", vec![bound], extend(&piece), span()),
                    arm("none", Vec::new(), unit(), span()),
                ],
            )))
        }
        TExprKind::If(cond, then, els) => Some(like(TExprKind::If(
            cond.clone(),
            Box::new(extend(then)),
            Box::new(extend(els)),
        ))),
        TExprKind::Match(scrutinee, arms) => Some(like(TExprKind::Match(
            scrutinee.clone(),
            arms.iter()
                .map(|a| TMatchArm {
                    pattern: a.pattern.clone(),
                    body: extend(&a.body),
                    span: a.span.clone(),
                })
                .collect(),
        ))),
        TExprKind::IfLet(matched, scrutinee, els) => Some(like(TExprKind::IfLet(
            Box::new(TMatchArm {
                pattern: matched.pattern.clone(),
                body: extend(&matched.body),
                span: matched.span.clone(),
            }),
            scrutinee.clone(),
            Box::new(extend(els)),
        ))),
        TExprKind::Seq(first, rest) => {
            Some(like(TExprKind::Seq(first.clone(), Box::new(extend(rest)))))
        }
        TExprKind::Assign(lhs, value, rest) => Some(like(TExprKind::Assign(
            lhs.clone(),
            value.clone(),
            Box::new(extend(rest)),
        ))),
        TExprKind::Let(name, value, body) if name != v => {
            let mut out = like(TExprKind::Let(
                name.clone(),
                value.clone(),
                Box::new(extend(body)),
            ));
            out.annotation = src.annotation.clone();
            Some(out)
        }
        _ => None,
    }
}

fn arm(name: &str, bindings: Vec<String>, body: P, span: aipl_syntax::Span) -> TMatchArm<Ty> {
    TMatchArm {
        pattern: TPattern::Ctor {
            name: name.to_string(),
            bindings,
            ignore_payload: false,
        },
        body,
        span,
    }
}

/// `func` applied to the bare name `arg` (see `fuse::loop_fusions::apply`).
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
