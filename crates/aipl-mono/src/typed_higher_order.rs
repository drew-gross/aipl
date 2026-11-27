//! Functions that take functions, typed from the pinned program: a lambda lifted to a function of
//! its own, the loops `map`, `filter`, `filter_map` and `zip_with` are expanded into, and the loops
//! a `for` with a position, over a dict or over a range stands for.

use aipl_syntax::ast::{Arity, Callee, ConcreteType, Primitive, Type};
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
pub(crate) fn free_locals(
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
        // A call through a local reads it: its type is the function type the call has.
        TExprKind::Call {
            callee: Callee::Local(name),
            args,
            ..
        } => {
            if !bound.contains(name) && scope.contains(name) && !out.iter().any(|(n, _)| n == name)
            {
                let ty = ConcreteType::Fn(
                    args.iter().map(|a| a.ty.clone()).collect(),
                    Box::new(e.ty.clone()),
                    Vec::new(),
                    false,
                );
                out.push((name.clone(), ty));
            }
            for a in args {
                free_locals(a, scope, bound, out);
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

/// A function-typed argument, resolved to the function it calls: its name, what it returns, and the
/// locals it captures, which its callee is given as parameters.
pub(crate) struct Lifted {
    pub(crate) name: String,
    pub(crate) ret: ConcreteType,
    pub(crate) captures: Vec<(String, ConcreteType)>,
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

/// `set name = name + 1;` for a counter of type `ty`.
fn bump(name: &str, ty: &ConcreteType, span: &Span) -> Typed {
    let at = || ident(name, ty, span);
    node(
        TExprKind::Assign(
            Box::new(at()),
            Box::new(call(
                Callee::WrappingAdd,
                vec![at(), node(TExprKind::Int(1), ty.clone(), span)],
                ty.clone(),
                span,
            )),
            Box::new(unit(span)),
        ),
        ConcreteType::Unit,
        span,
    )
}

fn increment(name: &str, span: &Span) -> Typed {
    bump(name, &primitive(Primitive::I64), span)
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
    /// `filter_map` over an array (`map` also over an optional or a range) and `zip_with` over two
    /// arrays, each given lambdas or function names of the parameters they take.
    pub(crate) fn is_typed_expansion(
        &self,
        callee: &Callee,
        args: &[TExpr<aipl_syntax::typed::Ty>],
        concrete: &dyn Fn(&aipl_syntax::typed::Ty) -> Option<ConcreteType>,
    ) -> bool {
        let takes = |a: &TExpr<aipl_syntax::typed::Ty>, count: usize| match &a.kind {
            TExprKind::Lambda(p, _) => p.len() == count,
            TExprKind::Ident(_) => {
                self.is_function_argument(a)
                    && matches!(&a.ty, aipl_syntax::typed::Ty::Fn(p, _, _, _) if p.len() == count)
            }
            _ => false,
        };
        let one_parameter = |a: &TExpr<aipl_syntax::typed::Ty>| takes(a, 1);
        let receiver = |a: &TExpr<aipl_syntax::typed::Ty>, optional: bool| {
            concrete(&a.ty).is_some_and(|t| match t {
                ConcreteType::Array(_) => true,
                ConcreteType::Optional(_) => optional,
                _ => false,
            })
        };
        let range = |a: &TExpr<aipl_syntax::typed::Ty>| matches!(&a.ty, aipl_syntax::typed::Ty::Named(n) if n == aipl_syntax::SPAN_TYPE);
        match (callee, args) {
            (Callee::Map, [r, f]) => (receiver(r, true) || range(r)) && one_parameter(f),
            (Callee::Filter, [r, f]) => receiver(r, false) && one_parameter(f),
            (Callee::FilterMap, [r, p, f]) => {
                receiver(r, false) && one_parameter(p) && one_parameter(f)
            }
            (Callee::ZipWith, [a, b, f]) => receiver(a, false) && receiver(b, false) && takes(f, 2),
            _ => false,
        }
    }

    /// The function-typed parameters of the function being processed that `e` calls or forwards, by
    /// name: the ones a lambda lifted out of it must still be able to resolve. A lambda parameter of
    /// the same name hides the outer one.
    fn function_parameters_used(
        &self,
        e: &Typed,
        params: &[TLambdaParam<ConcreteType>],
        out: &mut Vec<String>,
    ) {
        if self.cur_lenv.is_empty() {
            return;
        }
        if let TExprKind::Call {
            callee: Callee::Local(n),
            ..
        }
        | TExprKind::Ident(n) = &e.kind
        {
            if self.cur_lenv.contains_key(n)
                && !params.iter().any(|p| &p.name == n)
                && !out.contains(n)
            {
                out.push(n.clone());
            }
        }
        crate::typed_util::each_child(e, &mut |c| self.function_parameters_used(c, params, out));
    }

    /// What a lambda captures from the scope around it: the locals its body reads, and the captures
    /// of each function-typed parameter it calls, which arrive as parameters of this function.
    pub(crate) fn typed_lambda_captures(
        &self,
        params: &[TLambdaParam<ConcreteType>],
        body: &Typed,
    ) -> Vec<(String, ConcreteType)> {
        let mut bound: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut captures = Vec::new();
        free_locals(body, &self.scope_names, &mut bound, &mut captures);
        let mut used = Vec::new();
        self.function_parameters_used(body, params, &mut used);
        for name in used {
            for capture in &self.cur_lenv[&name].captures {
                let aipl_syntax::ast::ExprKind::Ident(cn) = &capture.kind else {
                    continue;
                };
                if let (false, Some(t)) = (
                    captures.iter().any(|(n, _)| n == cn),
                    self.current_parameters.get(cn),
                ) {
                    captures.push((cn.clone(), t.clone()));
                }
            }
        }
        captures
    }

    /// A lambda lifted to a function of its own: its parameters, then one parameter per captured
    /// local. Returns the function's name; it is queued.
    pub(crate) fn lift_lambda(
        &mut self,
        params: &[TLambdaParam<ConcreteType>],
        body: &Typed,
        captures: &[(String, ConcreteType)],
        return_ty: Type,
        effects: Vec<String>,
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
        // The function parameters the body calls resolve to the same functions here, with the same
        // captures: those arrived as `captures` above, under the names this function knew them by.
        let mut used = Vec::new();
        self.function_parameters_used(body, params, &mut used);
        let mut lenv = crate::LambdaEnv::new();
        for n in used {
            let binding = &self.cur_lenv[&n];
            let carried = binding.captures.iter().all(
                |c| matches!(&c.kind, aipl_syntax::ast::ExprKind::Ident(cn) if captures.iter().any(|(n, _)| n == cn)),
            );
            if carried {
                lenv.insert(n, binding.clone());
            }
        }
        if !lenv.is_empty() {
            self.lambda_envs.insert(name.clone(), lenv);
        }
        self.concrete.insert(
            name.clone(),
            ConcreteTemplate {
                params: lifted,
                effects,
                return_ty: Some(return_ty),
                typed: Some(body.clone()),
            },
        );
        self.enqueue_concrete(&name);
        name
    }

    /// A function-typed argument resolved: a lambda lifted, a function parameter of this function
    /// looked up in its bindings, or a named function (instantiated for the parameter types its
    /// position gives it).
    pub(crate) fn lifted_argument(&mut self, argument: Typed) -> Lifted {
        let argument_ty = argument.ty.clone();
        match argument.kind {
            TExprKind::Lambda(params, body) => {
                let captures = self.typed_lambda_captures(&params, &body);
                let ret = body.ty.clone();
                let effects = match &argument_ty {
                    ConcreteType::Fn(_, _, effects, _) => effects.clone(),
                    _ => Vec::new(),
                };
                let name = self.lift_lambda(&params, &body, &captures, ret.to_type(), effects);
                Lifted {
                    name,
                    ret,
                    captures,
                }
            }
            TExprKind::Ident(g) => {
                let ConcreteType::Fn(parameter_tys, ret, _, _) = &argument.ty else {
                    unreachable!("a function argument has a function type");
                };
                if let Some(binding) = self.cur_lenv.get(&g).cloned() {
                    let captures = binding
                        .captures
                        .iter()
                        .filter_map(|c| match &c.kind {
                            aipl_syntax::ast::ExprKind::Ident(cn) => self
                                .current_parameters
                                .get(cn)
                                .map(|t| (cn.clone(), t.clone())),
                            _ => None,
                        })
                        .collect();
                    return Lifted {
                        name: binding.fn_name,
                        ret: (**ret).clone(),
                        captures,
                    };
                }
                let expected: Vec<Type> = parameter_tys.iter().map(TyRepr::to_type).collect();
                let (name, _) = self
                    .fn_value(&g, &expected, &argument.span)
                    .unwrap_or_else(|e| panic!("a function value is instantiable: {e:?}"));
                Lifted {
                    name,
                    ret: (**ret).clone(),
                    captures: Vec::new(),
                }
            }
            _ => unreachable!("a function argument is a lambda or a function name"),
        }
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
        let body = crate::typed_util::distinct_binders(&body, &mut self.names);
        let name = self.synth_name(kind);
        self.concrete.insert(
            name.clone(),
            ConcreteTemplate {
                params: expansion.params,
                effects: self.cur_effects.clone(),
                return_ty: Some(result_ty.to_type()),
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

    /// A `for` that names a position, walks a dict or walks a range, as the loop it stands for. The
    /// result is not yet lowered.
    pub(crate) fn desugar_for(&mut self, e: Typed) -> Typed {
        let Typed {
            kind: TExprKind::For(var, index, iterable, body),
            span,
            ..
        } = e
        else {
            unreachable!("desugar_for is given a `for`");
        };
        let u64_ty = primitive(Primitive::U64);
        let counter_at = |name: &str| ident(name, &u64_ty, &span);
        let less_than = |left: Typed, right: Typed| {
            call(
                Callee::LessThan,
                vec![left, right],
                primitive(Primitive::Bool),
                &span,
            )
        };
        // A counter says it is a `u64`, so a literal start is not taken for an `i64`.
        let counter_let = |name: &str, value: Typed, body: Typed| {
            let mut counted = let_mut(name, value, body, &span);
            counted.annotation = Some(u64_ty.clone());
            counted
        };
        let wrap_let = |name: &str, value: Typed, body: Typed| {
            let ty = body.ty.clone();
            node(
                TExprKind::Let(name.to_string(), Box::new(value), Box::new(body)),
                ty,
                &span,
            )
        };
        let while_loop = |condition: Typed, body: Typed| {
            node(
                TExprKind::While(Box::new(condition), Box::new(body)),
                ConcreteType::Unit,
                &span,
            )
        };
        match (&iterable.ty, index) {
            // A dict walks its pairs: the position binder is the key, the element binder the value.
            // The compiler's own position never escapes the loop.
            (ConcreteType::Dict(key, value), index) => {
                let (key, value) = ((**key).clone(), (**value).clone());
                let dict = self.names.temp("dict");
                let counter = self.names.temp("dict_i");
                let dict_ty = iterable.ty.clone();
                let at = |callee: Callee, ty: &ConcreteType| {
                    call(
                        callee,
                        vec![ident(&dict, &dict_ty, &span), counter_at(&counter)],
                        ty.clone(),
                        &span,
                    )
                };
                let each = wrap_let(
                    &var,
                    at(Callee::DictValueAt, &value),
                    seq(*body, bump(&counter, &u64_ty, &span), &span),
                );
                let each = match index {
                    None => each,
                    Some(i) => wrap_let(&i, at(Callee::DictKeyAt, &key), each),
                };
                let length = node(
                    TExprKind::Call {
                        callee: Callee::Len,
                        type_args: Vec::new(),
                        args: vec![ident(&dict, &dict_ty, &span)],
                        method_style: true,
                    },
                    u64_ty.clone(),
                    &span,
                );
                let looped = while_loop(less_than(counter_at(&counter), length), each);
                let counted = counter_let(
                    &counter,
                    node(TExprKind::Int(0), u64_ty.clone(), &span),
                    looped,
                );
                wrap_let(&dict, *iterable, counted)
            }
            // A position is a counter declared right at the loop, so a loop nested in another
            // starts it afresh; `index` is re-bound each iteration, so the body cannot move the
            // counter.
            (_, Some(position)) => {
                let counter = self.names.temp("idx");
                let counted = seq(*body, bump(&counter, &u64_ty, &span), &span);
                let bound = wrap_let(&position, counter_at(&counter), counted);
                let plain = node(
                    TExprKind::For(var, None, iterable, Box::new(bound)),
                    ConcreteType::Unit,
                    &span,
                );
                counter_let(
                    &counter,
                    node(TExprKind::Int(0), u64_ty.clone(), &span),
                    plain,
                )
            }
            // A range is the counted loop from its start to its end.
            (_, None) => {
                let range = self.names.temp("range");
                let counter = self.names.temp("range_i");
                let range_ty = iterable.ty.clone();
                let field = |f: &str| {
                    node(
                        TExprKind::Field(Box::new(ident(&range, &range_ty, &span)), f.to_string()),
                        u64_ty.clone(),
                        &span,
                    )
                };
                let each = wrap_let(&var, counter_at(&counter), *body);
                let looped = while_loop(
                    less_than(counter_at(&counter), field("end")),
                    seq(each, bump(&counter, &u64_ty, &span), &span),
                );
                let from_start = counter_let(&counter, field("start"), looped);
                wrap_let(&range, *iterable, from_start)
            }
        }
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
            Callee::ZipWith => {
                let mut second = args.remove(0);
                self.lower_in_place(&mut second);
                self.lower_zip_with(receiver, second, args.remove(0), span)
            }
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
        let (element, over_optional, over_range) = match &receiver_ty {
            ConcreteType::Array(t) => ((**t).clone(), false, false),
            ConcreteType::Optional(t) => ((**t).clone(), true, false),
            // Mapping a range hands the function each position.
            ConcreteType::Named(n) if n == aipl_syntax::SPAN_TYPE => {
                (primitive(Primitive::U64), false, true)
            }
            _ => unreachable!("a typed map is given an array, an optional or a range"),
        };
        let in_place_receiver = !over_range
            && crate::is_heap(&receiver_ty.to_type())
            && crate::typed_infer::fresh_tail(&receiver);
        let Lifted {
            name: fname,
            ret: u,
            captures,
        } = self.lifted_argument(lambda);
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
            // How many elements are coming: a sequence's `len`, or the width of a range; the
            // subtraction saturates, so a backwards range reserves nothing, which is also how many
            // iterations it runs for.
            let count = || {
                if over_range {
                    let field = |f: &str| {
                        node(
                            TExprKind::Field(Box::new(arr()), f.to_string()),
                            primitive(Primitive::U64),
                            &span,
                        )
                    };
                    call(
                        Callee::SaturatingSub,
                        vec![field("end"), field("start")],
                        primitive(Primitive::U64),
                        &span,
                    )
                } else {
                    call(Callee::Len, vec![arr()], primitive(Primitive::U64), &span)
                }
            };
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
        let Lifted {
            name: fname,
            captures,
            ..
        } = self.lifted_argument(predicate);
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
        let Lifted {
            name: predicate_fn,
            captures: predicate_captures,
            ..
        } = self.lifted_argument(predicate);
        let Lifted {
            name: mapping_fn,
            ret: u,
            captures: mapping_captures,
        } = self.lifted_argument(mapping);
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

    /// `a.zip_with(b, f)` as a call to a synthesized function that pairs the elements by position
    /// and applies `f`, up to the shorter input. A fresh input is moved in and its buffer reused
    /// for the result; with both fresh, the longer one is reused and the other released.
    fn lower_zip_with(&mut self, a: Typed, b: Typed, f: Typed, span: Span) -> Typed {
        let (ty_a, ty_b) = (a.ty.clone(), b.ty.clone());
        let (ConcreteType::Array(elem_a), ConcreteType::Array(elem_b)) = (&ty_a, &ty_b) else {
            unreachable!("a typed zip_with is given arrays");
        };
        let (elem_a, elem_b) = ((**elem_a).clone(), (**elem_b).clone());
        let fresh =
            |t: &Typed| crate::is_heap(&t.ty.to_type()) && crate::typed_infer::fresh_tail(t);
        let (fresh_a, fresh_b) = (fresh(&a), fresh(&b));
        let Lifted {
            name: function,
            ret: r,
            captures,
        } = self.lifted_argument(f);
        let own_a = fresh_a
            && self.reusable(&elem_a)
            && self.reusable(&r)
            && crate::slot_fits(&elem_a.to_type(), &r.to_type());
        let own_b = fresh_b
            && self.reusable(&elem_b)
            && self.reusable(&r)
            && crate::slot_fits(&elem_b.to_type(), &r.to_type());
        let mut expansion = Expansion {
            span: span.clone(),
            params: vec![parameter("$a", &ty_a), parameter("$b", &ty_b)],
            call_args: vec![a, b],
        };
        // `f($e, $v, captures..)`: `$e` is always the element of `a` and `$v` the element of `b`,
        // whichever of the two the loop walks.
        let mut function_args = vec![ident("$e", &elem_a, &span), ident("$v", &elem_b, &span)];
        function_args.extend(self.thread_typed_captures(&captures, &mut expansion));
        let applied = call(Callee::resolve(function), function_args, r.clone(), &span);
        let result_ty = array_of(&r);
        let i64_ty = primitive(Primitive::I64);
        let some_none = |some_body: Typed, binder: &str, none_body: Typed, scrutinee: Typed| {
            let arm = |pattern, body| TMatchArm {
                pattern,
                body,
                span: span.clone(),
            };
            node(
                TExprKind::Match(
                    Box::new(scrutinee),
                    vec![
                        arm(
                            TPattern::Ctor {
                                name: "some".to_string(),
                                bindings: vec![binder.to_string()],
                                ignore_payload: false,
                            },
                            some_body,
                        ),
                        arm(
                            TPattern::Ctor {
                                name: "none".to_string(),
                                bindings: Vec::new(),
                                ignore_payload: false,
                            },
                            none_body,
                        ),
                    ],
                ),
                ConcreteType::Unit,
                &span,
            )
        };
        // The element of `other` at `$i`, if it has one.
        let indexed = |other: &str, other_ty: &ConcreteType, other_elem: &ConcreteType| {
            node(
                TExprKind::Index(
                    Box::new(ident(other, other_ty, &span)),
                    Box::new(ident("$i", &i64_ty, &span)),
                ),
                ConcreteType::Optional(Box::new(other_elem.clone())),
                &span,
            )
        };
        // The loop over the reused input: each slot is read before it is overwritten, and the other
        // input is indexed, so the shorter-input cutoff works from either side.
        let in_place_body = |reuse_a: bool, drop_other: bool, applied: Typed| -> Typed {
            let (reused, other, reused_el, other_el) = if reuse_a {
                ("$a", "$b", "$e", "$v")
            } else {
                ("$b", "$a", "$v", "$e")
            };
            let (reused_ty, other_ty, reused_elem, other_elem) = if reuse_a {
                (&ty_a, &ty_b, &elem_a, &elem_b)
            } else {
                (&ty_b, &ty_a, &elem_b, &elem_a)
            };
            let z = || ident("$z", reused_ty, &span);
            let map_set = call(
                Callee::MapSet,
                vec![
                    z(),
                    ident("$i", &i64_ty, &span),
                    applied,
                    ident(reused_el, reused_elem, &span),
                ],
                ConcreteType::Unit,
                &span,
            );
            let some_body = seq(map_set, increment("$w", &span), &span);
            let none_body = call(
                Callee::FilterDrop,
                vec![ident(reused_el, reused_elem, &span)],
                ConcreteType::Unit,
                &span,
            );
            let matched = some_none(
                some_body,
                other_el,
                none_body,
                indexed(other, other_ty, other_elem),
            );
            let for_body = seq(matched, increment("$i", &span), &span);
            let looped = for_each(reused_el, z(), for_body, &span);
            let truncate = call(
                Callee::FilterTruncate,
                vec![z(), ident("$w", &i64_ty, &span)],
                ConcreteType::Unit,
                &span,
            );
            let result = call(Callee::MapResult, vec![z()], result_ty.clone(), &span);
            let mut after = seq(truncate, result, &span);
            if drop_other {
                // Both inputs were moved in: release the one not reused, whose elements `f` only
                // borrowed.
                let release = call(
                    Callee::FilterDrop,
                    vec![ident(other, other_ty, &span)],
                    ConcreteType::Unit,
                    &span,
                );
                after = seq(release, after, &span);
            }
            let counted = let_mut(
                "$i",
                node(TExprKind::Int(0), i64_ty.clone(), &span),
                let_mut(
                    "$w",
                    node(TExprKind::Int(0), i64_ty.clone(), &span),
                    seq(looped, after, &span),
                    &span,
                ),
                &span,
            );
            let_mut(
                "$z",
                call(
                    Callee::ArrWritable,
                    vec![ident(reused, reused_ty, &span)],
                    reused_ty.clone(),
                    &span,
                ),
                counted,
                &span,
            )
        };
        let body = if own_a && own_b {
            let len_of = |n: &str, t: &ConcreteType| {
                call(
                    Callee::Len,
                    vec![ident(n, t, &span)],
                    primitive(Primitive::U64),
                    &span,
                )
            };
            let condition = call(
                Callee::LessThan,
                vec![len_of("$a", &ty_a), len_of("$b", &ty_b)],
                primitive(Primitive::Bool),
                &span,
            );
            node(
                TExprKind::If(
                    Box::new(condition),
                    Box::new(in_place_body(false, true, applied.clone())),
                    Box::new(in_place_body(true, true, applied)),
                ),
                result_ty.clone(),
                &span,
            )
        } else if own_a {
            in_place_body(true, false, applied)
        } else if own_b {
            in_place_body(false, false, applied)
        } else {
            let push = set_push("$out", &result_ty, applied, &span);
            let matched = some_none(push, "$v", unit(&span), indexed("$b", &ty_b, &elem_b));
            let for_body = seq(matched, increment("$i", &span), &span);
            let looped = for_each("$e", ident("$a", &ty_a, &span), for_body, &span);
            let counted = let_mut(
                "$i",
                node(TExprKind::Int(0), i64_ty.clone(), &span),
                seq(looped, ident("$out", &result_ty, &span), &span),
                &span,
            );
            let_mut(
                "$out",
                node(TExprKind::ArrayLit(Vec::new()), result_ty.clone(), &span),
                counted,
                &span,
            )
        };
        let owned: Vec<usize> = [(own_a, 0), (own_b, 1)]
            .iter()
            .filter(|(own, _)| *own)
            .map(|&(_, i)| i)
            .collect();
        self.call_synthesized("zip", expansion, result_ty, body, &owned)
    }
}
