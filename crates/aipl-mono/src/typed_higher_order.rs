//! Functions that take functions, typed from the pinned program: a lambda lifted to a function of
//! its own and the loops `map` and its siblings are expanded into.

use aipl_syntax::ast::{Arity, Callee, ConcreteType, Primitive};
use aipl_syntax::typed::{TExpr, TExprKind, TLambdaParam, TMatchArm, TPattern, TyRepr};
use aipl_syntax::Span;

use crate::typed_adapter::Typed;
use crate::{ConcreteTemplate, Mono, Param};

fn node(kind: TExprKind<ConcreteType>, ty: ConcreteType, span: &Span) -> Typed {
    TExpr::new(kind, ty, span.clone())
}

fn ident(name: &str, ty: &ConcreteType, span: &Span) -> Typed {
    node(TExprKind::Ident(name.to_string()), ty.clone(), span)
}

fn call(callee: Callee, args: Vec<Typed>, ty: ConcreteType, span: &Span) -> Typed {
    node(
        TExprKind::Call {
            callee,
            type_args: Vec::new(),
            args,
            method_style: false,
        },
        ty,
        span,
    )
}

fn unit(span: &Span) -> Typed {
    node(TExprKind::Unit, ConcreteType::Unit, span)
}

fn array_of(element: &ConcreteType) -> ConcreteType {
    ConcreteType::Array(Box::new(element.clone()))
}

fn primitive(p: Primitive) -> ConcreteType {
    ConcreteType::Primitive(p)
}

/// `set target = target.push(value);` — the in-place push the array-building loops grow their
/// accumulator with.
fn set_push(target: &str, array: &ConcreteType, value: Typed, span: &Span) -> Typed {
    let push = node(
        TExprKind::Call {
            callee: Callee::Push,
            type_args: Vec::new(),
            args: vec![ident(target, array, span), value],
            method_style: true,
        },
        array.clone(),
        span,
    );
    node(
        TExprKind::Assign(
            Box::new(ident(target, array, span)),
            Box::new(push),
            Box::new(unit(span)),
        ),
        ConcreteType::Unit,
        span,
    )
}

/// The names a pattern binds.
pub(crate) fn pattern_bindings<T: aipl_syntax::typed::TyRepr>(
    pattern: &TPattern<T>,
) -> Vec<String> {
    match pattern {
        TPattern::Ctor { bindings, .. } => bindings.clone(),
        TPattern::Array(elems) | TPattern::ArrayRest(elems) => elems
            .iter()
            .filter_map(|e| match &e.kind {
                TExprKind::Ident(n) => Some(n.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The locals of the enclosing scope that `e` reads and does not itself bind, with their types, in
/// first-occurrence order.
fn free_locals(
    e: &Typed,
    scope: &[String],
    bound: &mut Vec<String>,
    out: &mut Vec<(String, ConcreteType)>,
) {
    let depth = bound.len();
    match &e.kind {
        TExprKind::Ident(name) => {
            if !bound.contains(name) && scope.contains(name) && !out.iter().any(|(n, _)| n == name)
            {
                out.push((name.clone(), e.ty.clone()));
            }
        }
        TExprKind::Let(name, value, body) | TExprKind::LetMut(name, value, body) => {
            free_locals(value, scope, bound, out);
            bound.push(name.clone());
            free_locals(body, scope, bound, out);
        }
        TExprKind::Lambda(params, body) => {
            bound.extend(params.iter().map(|p| p.name.clone()));
            free_locals(body, scope, bound, out);
        }
        TExprKind::For(var, index, iterable, body) => {
            free_locals(iterable, scope, bound, out);
            bound.push(var.clone());
            bound.extend(index.clone());
            free_locals(body, scope, bound, out);
        }
        TExprKind::Match(scrutinee, arms) => {
            free_locals(scrutinee, scope, bound, out);
            for arm in arms {
                let arm_depth = bound.len();
                bound.extend(pattern_bindings(&arm.pattern));
                free_locals(&arm.body, scope, bound, out);
                bound.truncate(arm_depth);
            }
        }
        TExprKind::IfLet(arm, scrutinee, otherwise) => {
            free_locals(scrutinee, scope, bound, out);
            let arm_depth = bound.len();
            bound.extend(pattern_bindings(&arm.pattern));
            free_locals(&arm.body, scope, bound, out);
            bound.truncate(arm_depth);
            free_locals(otherwise, scope, bound, out);
        }
        _ => crate::typed_util::each_child(e, &mut |c| free_locals(c, scope, bound, out)),
    }
    bound.truncate(depth);
}

/// What is known about one expanded call before its loop is built: the receiver, lowered, and the
/// parameters and arguments of the synthesized function.
struct Expansion {
    span: Span,
    params: Vec<Param>,
    call_args: Vec<Typed>,
}

fn parameter(name: &str, ty: &ConcreteType) -> Param {
    Param {
        name_span: Default::default(),
        name: name.to_string(),
        ty: ty.to_type(),
        mutable: false,
        arity: Arity::One,
        default: None,
        implicit_some: false,
    }
}

/// `($arr, a captured local ..)`'s increment `set name = name + 1;`.
fn increment(name: &str, span: &Span) -> Typed {
    let i64_ty = primitive(Primitive::I64);
    let at = || ident(name, &i64_ty, span);
    node(
        TExprKind::Assign(
            Box::new(at()),
            Box::new(call(
                Callee::WrappingAdd,
                vec![at(), node(TExprKind::Int(1), i64_ty.clone(), span)],
                i64_ty.clone(),
                span,
            )),
            Box::new(unit(span)),
        ),
        ConcreteType::Unit,
        span,
    )
}

fn seq(first: Typed, rest: Typed, span: &Span) -> Typed {
    let ty = rest.ty.clone();
    node(TExprKind::Seq(Box::new(first), Box::new(rest)), ty, span)
}

fn let_mut(name: &str, value: Typed, body: Typed, span: &Span) -> Typed {
    let ty = body.ty.clone();
    node(
        TExprKind::LetMut(name.to_string(), Box::new(value), Box::new(body)),
        ty,
        span,
    )
}

fn for_each(var: &str, iterable: Typed, body: Typed, span: &Span) -> Typed {
    node(
        TExprKind::For(var.to_string(), None, Box::new(iterable), Box::new(body)),
        ConcreteType::Unit,
        span,
    )
}

/// `if (condition) { then } else { otherwise }`, as a statement.
fn if_else(condition: Typed, then: Typed, otherwise: Typed, span: &Span) -> Typed {
    node(
        TExprKind::If(Box::new(condition), Box::new(then), Box::new(otherwise)),
        ConcreteType::Unit,
        span,
    )
}

/// The initial, empty output of a loop that pushes `element`s: pre-sized to `count` for an
/// element a buffer can be sized for in advance, empty for an optional (16-byte) or `bool`
/// (bit-packed) one, whose first push sizes it for its representation.
fn output_start(element: &ConcreteType, count: impl FnOnce() -> Typed, span: &Span) -> Typed {
    let array = array_of(element);
    if matches!(element, ConcreteType::Optional(_))
        || matches!(element, ConcreteType::Primitive(Primitive::Bool))
    {
        node(TExprKind::ArrayLit(Vec::new()), array, span)
    } else {
        call(Callee::WithCapacity, vec![count()], array, span)
    }
}

impl Mono<'_> {
    /// Whether a call is one of the expansions made from the typed tree: `map`, `filter` and
    /// `filter_map` over an array (`map` also over an optional), each given lambdas of
    /// one parameter.
    pub(crate) fn is_typed_expansion(
        &self,
        callee: &Callee,
        args: &[TExpr<aipl_syntax::typed::Ty>],
        concrete: &dyn Fn(&aipl_syntax::typed::Ty) -> Option<ConcreteType>,
    ) -> bool {
        let one_parameter = |a: &TExpr<aipl_syntax::typed::Ty>| matches!(&a.kind, TExprKind::Lambda(p, _) if p.len() == 1);
        let receiver = |a: &TExpr<aipl_syntax::typed::Ty>, optional: bool| {
            concrete(&a.ty).is_some_and(|t| match t {
                ConcreteType::Array(_) => true,
                ConcreteType::Optional(_) => optional,
                _ => false,
            })
        };
        match (callee, args) {
            (Callee::Map, [r, f]) => receiver(r, true) && one_parameter(f),
            (Callee::Filter, [r, f]) => receiver(r, false) && one_parameter(f),
            (Callee::FilterMap, [r, p, f]) => {
                receiver(r, false) && one_parameter(p) && one_parameter(f)
            }
            _ => false,
        }
    }

    /// A lambda lifted to a function of its own: its parameters, then one parameter per captured
    /// local. Returns the function's name; it is queued.
    fn lift_lambda(
        &mut self,
        params: &[TLambdaParam<ConcreteType>],
        body: &Typed,
        captures: &[(String, ConcreteType)],
    ) -> String {
        let name = self.synth_name("lambda");
        let mut lifted: Vec<Param> = params
            .iter()
            .map(|p| Param {
                name_span: aipl_syntax::ast::NameSpan(p.span.clone()),
                ..parameter(&p.name, &p.ty)
            })
            .collect();
        lifted.extend(captures.iter().map(|(n, t)| parameter(n, t)));
        self.concrete.insert(
            name.clone(),
            ConcreteTemplate {
                params: lifted,
                effects: Vec::new(),
                return_ty: Some(body.ty.to_type()),
                body: body.erase(),
                typed: Some(body.clone()),
            },
        );
        self.enqueue_concrete(&name);
        name
    }

    /// A lambda argument lifted: the function's name, and the locals it captures, which the
    /// synthesized function is given as parameters and its call as arguments.
    fn lifted_argument(&mut self, lambda: Typed) -> (String, Typed, Vec<(String, ConcreteType)>) {
        let TExprKind::Lambda(params, body) = lambda.kind else {
            unreachable!("a typed expansion is given lambdas");
        };
        let mut bound: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut captures = Vec::new();
        free_locals(&body, &self.scope_names.clone(), &mut bound, &mut captures);
        let name = self.lift_lambda(&params, &body, &captures);
        (name, *body, captures)
    }

    /// Give the synthesized function one parameter per capture, under a name of its own, and
    /// forward the captured local at the call. Returns the names to pass on to the lifted lambda.
    fn thread_typed_captures(
        &self,
        captures: &[(String, ConcreteType)],
        expansion: &mut Expansion,
    ) -> Vec<Typed> {
        let mut idents = Vec::with_capacity(captures.len());
        for (name, ty) in captures {
            let cap = format!("$cap{}", expansion.params.len());
            expansion.params.push(parameter(&cap, ty));
            idents.push(ident(&cap, ty, &expansion.span));
            expansion.call_args.push(ident(name, ty, &expansion.span));
        }
        idents
    }

    /// Register the synthesized function and call it. An in-place body consumes its array
    /// parameter, so its owned instance is emitted (the fresh argument is moved in rather than
    /// retained); a copying one borrows it.
    fn call_synthesized(
        &mut self,
        kind: &str,
        expansion: Expansion,
        result_ty: ConcreteType,
        body: Typed,
        owned: &[usize],
    ) -> Typed {
        let body = crate::typed_util::distinct_binders(&body);
        let name = self.synth_name(kind);
        self.concrete.insert(
            name.clone(),
            ConcreteTemplate {
                params: expansion.params,
                effects: self.cur_effects.clone(),
                return_ty: Some(result_ty.to_type()),
                body: body.erase(),
                typed: Some(body),
            },
        );
        let mangled = if owned.is_empty() {
            self.enqueue_concrete(&name);
            name
        } else {
            self.enqueue(&name, &[], owned)
        };
        call(
            Callee::User(mangled),
            expansion.call_args,
            result_ty,
            &expansion.span,
        )
    }

    /// Whether a buffer holding `t` can be overwritten in place by a value of another type: not an
    /// optional (16 bytes) or struct or variant (composite), not a `bool` (bit-packed), not a
    /// `char` (which makes the array `str`-shaped, one byte a slot).
    fn reusable(&self, t: &ConcreteType) -> bool {
        !matches!(t, ConcreteType::Optional(_))
            && !matches!(
                t,
                ConcreteType::Primitive(Primitive::Bool | Primitive::Char)
            )
            && !matches!(t, ConcreteType::Named(n) if self.structs.contains_key(n)
                || self.syn_structs.contains_key(n)
                || self.variants.contains_key(n))
    }

    /// The call to an expansion made from the typed tree, lowered.
    pub(crate) fn lower_expansion(&mut self, e: Typed) -> Typed {
        let Typed {
            kind: TExprKind::Call {
                callee, mut args, ..
            },
            span,
            ..
        } = e
        else {
            unreachable!("lower_expansion is given a call");
        };
        let mut receiver = args.remove(0);
        self.lower_in_place(&mut receiver);
        match callee {
            Callee::Map => self.lower_map(receiver, args.remove(0), span),
            Callee::Filter => self.lower_filter(receiver, args.remove(0), span),
            _ => {
                let predicate = args.remove(0);
                self.lower_filter_map(receiver, predicate, args.remove(0), span)
            }
        }
    }

    /// `receiver.map(|x| body)` as a call to a synthesized function that maps it: the loop that
    /// builds the result (reusing the receiver's buffer when it is fresh and the slots fit), or
    /// the `match` that maps an optional.
    fn lower_map(&mut self, receiver: Typed, lambda: Typed, span: Span) -> Typed {
        let receiver_ty = receiver.ty.clone();
        let (element, over_optional) = match &receiver_ty {
            ConcreteType::Array(t) => ((**t).clone(), false),
            ConcreteType::Optional(t) => ((**t).clone(), true),
            _ => unreachable!("a typed map is given an array or an optional"),
        };
        let in_place_receiver =
            crate::is_heap(&receiver_ty.to_type()) && crate::typed_infer::fresh_tail(&receiver);
        let (fname, lambda_body, captures) = self.lifted_argument(lambda);
        let u = lambda_body.ty.clone();
        let in_place = !over_optional
            && in_place_receiver
            && self.reusable(&element)
            && self.reusable(&u)
            && crate::slot_fits(&element.to_type(), &u.to_type());
        let mut expansion = Expansion {
            span: span.clone(),
            params: vec![parameter("$arr", &receiver_ty)],
            call_args: vec![receiver],
        };
        let mut lambda_args = vec![ident("$e", &element, &span)];
        lambda_args.extend(self.thread_typed_captures(&captures, &mut expansion));
        let applied = call(Callee::resolve(fname), lambda_args, u.clone(), &span);
        let arr = || ident("$arr", &receiver_ty, &span);
        let result_ty = if over_optional {
            ConcreteType::Optional(Box::new(u.clone()))
        } else {
            array_of(&u)
        };
        let body = if over_optional {
            let arm = |pattern, body| TMatchArm {
                pattern,
                body,
                span: span.clone(),
            };
            let some = TPattern::Ctor {
                name: "some".to_string(),
                bindings: vec!["$e".to_string()],
                ignore_payload: false,
            };
            let none = TPattern::Ctor {
                name: "none".to_string(),
                bindings: Vec::new(),
                ignore_payload: false,
            };
            node(
                TExprKind::Match(
                    Box::new(arr()),
                    vec![
                        arm(
                            some,
                            call(Callee::Some, vec![applied], result_ty.clone(), &span),
                        ),
                        arm(none, node(TExprKind::None, result_ty.clone(), &span)),
                    ],
                ),
                result_ty.clone(),
                &span,
            )
        } else if in_place {
            // Overwrite each slot with its mapped value and hand the buffer back as the result.
            let a = || ident("$a", &receiver_ty, &span);
            let i64_ty = primitive(Primitive::I64);
            let map_set = call(
                Callee::MapSet,
                vec![
                    a(),
                    ident("$i", &i64_ty, &span),
                    applied,
                    ident("$e", &element, &span),
                ],
                ConcreteType::Unit,
                &span,
            );
            let each = for_each(
                "$e",
                a(),
                seq(map_set, increment("$i", &span), &span),
                &span,
            );
            let finished = call(Callee::MapResult, vec![a()], result_ty.clone(), &span);
            let counted = let_mut(
                "$i",
                node(TExprKind::Int(0), i64_ty, &span),
                seq(each, finished, &span),
                &span,
            );
            let_mut(
                "$a",
                call(Callee::ArrWritable, vec![arr()], receiver_ty.clone(), &span),
                counted,
                &span,
            )
        } else {
            let count = || call(Callee::Len, vec![arr()], primitive(Primitive::U64), &span);
            let each = for_each(
                "$e",
                arr(),
                set_push("$out", &result_ty, applied, &span),
                &span,
            );
            let_mut(
                "$out",
                output_start(&u, count, &span),
                seq(each, ident("$out", &result_ty, &span), &span),
                &span,
            )
        };
        let owned: &[usize] = if in_place { &[0] } else { &[] };
        self.call_synthesized("map", expansion, result_ty, body, owned)
    }

    /// `receiver.filter(|x| keep)` as a call to a synthesized function that keeps the elements the
    /// predicate holds for: compacted in the receiver's own buffer when it is fresh, copied into a
    /// new one otherwise.
    fn lower_filter(&mut self, receiver: Typed, predicate: Typed, span: Span) -> Typed {
        let receiver_ty = receiver.ty.clone();
        let ConcreteType::Array(element) = &receiver_ty else {
            unreachable!("a typed filter is given an array");
        };
        let element = (**element).clone();
        let in_place = !matches!(element, ConcreteType::Optional(_))
            && !matches!(
                element,
                ConcreteType::Primitive(Primitive::Bool | Primitive::Char)
            )
            && crate::is_heap(&receiver_ty.to_type())
            && crate::typed_infer::fresh_tail(&receiver);
        let (fname, _, captures) = self.lifted_argument(predicate);
        let mut expansion = Expansion {
            span: span.clone(),
            params: vec![parameter("$arr", &receiver_ty)],
            call_args: vec![receiver],
        };
        let mut predicate_args = vec![ident("$e", &element, &span)];
        predicate_args.extend(self.thread_typed_captures(&captures, &mut expansion));
        let keep = call(
            Callee::resolve(fname),
            predicate_args,
            primitive(Primitive::Bool),
            &span,
        );
        let arr = || ident("$arr", &receiver_ty, &span);
        let body = if in_place {
            // Two-pointer compaction: `$w` is the write cursor, `$e` the read one.
            let a = || ident("$a", &receiver_ty, &span);
            let i64_ty = primitive(Primitive::I64);
            let w = || ident("$w", &i64_ty, &span);
            let e = || ident("$e", &element, &span);
            let kept = seq(
                call(
                    Callee::FilterKeep,
                    vec![a(), w(), e()],
                    ConcreteType::Unit,
                    &span,
                ),
                increment("$w", &span),
                &span,
            );
            let dropped = call(Callee::FilterDrop, vec![e()], ConcreteType::Unit, &span);
            let each = for_each("$e", a(), if_else(keep, kept, dropped, &span), &span);
            let truncate = call(
                Callee::FilterTruncate,
                vec![a(), w()],
                ConcreteType::Unit,
                &span,
            );
            let after = seq(truncate, a(), &span);
            let_mut(
                "$a",
                call(Callee::ArrWritable, vec![arr()], receiver_ty.clone(), &span),
                let_mut(
                    "$w",
                    node(TExprKind::Int(0), i64_ty.clone(), &span),
                    seq(each, after, &span),
                    &span,
                ),
                &span,
            )
        } else {
            let push = set_push("$out", &receiver_ty, ident("$e", &element, &span), &span);
            let each = for_each("$e", arr(), if_else(keep, push, unit(&span), &span), &span);
            let count = || call(Callee::Len, vec![arr()], primitive(Primitive::U64), &span);
            let_mut(
                "$out",
                output_start(&element, count, &span),
                seq(each, ident("$out", &receiver_ty, &span), &span),
                &span,
            )
        };
        let owned: &[usize] = if in_place { &[0] } else { &[] };
        self.call_synthesized("filter", expansion, receiver_ty, body, owned)
    }

    /// `receiver.filter_map(|x| keep, |x| value)`: `filter` and `map` as one loop.
    fn lower_filter_map(
        &mut self,
        receiver: Typed,
        predicate: Typed,
        mapping: Typed,
        span: Span,
    ) -> Typed {
        let receiver_ty = receiver.ty.clone();
        let ConcreteType::Array(element) = &receiver_ty else {
            unreachable!("a typed filter_map is given an array");
        };
        let element = (**element).clone();
        let in_place_receiver =
            crate::is_heap(&receiver_ty.to_type()) && crate::typed_infer::fresh_tail(&receiver);
        let (predicate_fn, _, predicate_captures) = self.lifted_argument(predicate);
        let (mapping_fn, mapping_body, mapping_captures) = self.lifted_argument(mapping);
        let u = mapping_body.ty.clone();
        let in_place = in_place_receiver
            && self.reusable(&element)
            && self.reusable(&u)
            && crate::slot_fits(&element.to_type(), &u.to_type());
        let mut expansion = Expansion {
            span: span.clone(),
            params: vec![parameter("$arr", &receiver_ty)],
            call_args: vec![receiver],
        };
        let mut predicate_args = vec![ident("$e", &element, &span)];
        predicate_args.extend(self.thread_typed_captures(&predicate_captures, &mut expansion));
        let mut mapping_args = vec![ident("$e", &element, &span)];
        mapping_args.extend(self.thread_typed_captures(&mapping_captures, &mut expansion));
        let keep = call(
            Callee::resolve(predicate_fn),
            predicate_args,
            primitive(Primitive::Bool),
            &span,
        );
        let mapped = call(Callee::resolve(mapping_fn), mapping_args, u.clone(), &span);
        let result_ty = array_of(&u);
        let arr = || ident("$arr", &receiver_ty, &span);
        let body = if in_place {
            // `filter`'s compaction with `map`'s slot write.
            let a = || ident("$a", &receiver_ty, &span);
            let i64_ty = primitive(Primitive::I64);
            let w = || ident("$w", &i64_ty, &span);
            let e = || ident("$e", &element, &span);
            let set = call(
                Callee::MapSet,
                vec![a(), w(), mapped, e()],
                ConcreteType::Unit,
                &span,
            );
            let kept = seq(set, increment("$w", &span), &span);
            let dropped = call(Callee::FilterDrop, vec![e()], ConcreteType::Unit, &span);
            let each = for_each("$e", a(), if_else(keep, kept, dropped, &span), &span);
            let truncate = call(
                Callee::FilterTruncate,
                vec![a(), w()],
                ConcreteType::Unit,
                &span,
            );
            let finished = call(Callee::MapResult, vec![a()], result_ty.clone(), &span);
            let after = seq(truncate, finished, &span);
            let_mut(
                "$a",
                call(Callee::ArrWritable, vec![arr()], receiver_ty.clone(), &span),
                let_mut(
                    "$w",
                    node(TExprKind::Int(0), i64_ty.clone(), &span),
                    seq(each, after, &span),
                    &span,
                ),
                &span,
            )
        } else {
            let push = set_push("$out", &result_ty, mapped, &span);
            let each = for_each("$e", arr(), if_else(keep, push, unit(&span), &span), &span);
            let count = || call(Callee::Len, vec![arr()], primitive(Primitive::U64), &span);
            let_mut(
                "$out",
                output_start(&u, count, &span),
                seq(each, ident("$out", &result_ty, &span), &span),
                &span,
            )
        };
        let owned: &[usize] = if in_place { &[0] } else { &[] };
        self.call_synthesized("filter_map", expansion, result_ty, body, owned)
    }
}
