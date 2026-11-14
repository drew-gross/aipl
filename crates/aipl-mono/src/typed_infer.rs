//! Typing an instance's body from the pinned program rather than inferring it.
//!
//! An instance whose source function the pinner typed, and whose body holds only forms
//! [`supported`] here, takes its tree from the pinned one: each node's type is the pinned type with
//! the instance's type arguments substituted. Every other instance is inferred from the written
//! tree. The forms covered grow until none is left to infer.

use std::collections::HashMap;

use aipl_syntax::ast::{Callee, ConcreteType, Type};
use aipl_syntax::typed::{ConcreteTy, TExpr, TExprKind, TMatchArm, TPattern, Ty, TyRepr};

use crate::{ArgShape, Mono, Param, ParamSpec};

impl Mono<'_> {
    /// Whether a node, and everything under it, is a form that is typed from the pinned tree.
    fn supported(&self, e: &TExpr<Ty>, concrete: &dyn Fn(&Ty) -> Option<ConcreteType>) -> bool {
        // A `return` has no value of its own; it is typed as unit.
        if e.unsettled
            || matches!(e.ty, Ty::Fn(..))
            || (matches!(e.ty, Ty::Never) && !matches!(e.kind, TExprKind::Return(_)))
        {
            return false;
        }
        let sub = |x: &TExpr<Ty>| self.supported(x, concrete);
        match &e.kind {
            TExprKind::Int(_)
            | TExprKind::Float(_)
            | TExprKind::Bool(_)
            | TExprKind::Char(_)
            | TExprKind::Str(_)
            | TExprKind::Unit
            | TExprKind::None => true,
            // A nullary constructor of a generic variant is settled from the program's instances, and a
            // function used as a value is queued for emission.
            TExprKind::Ident(name) => {
                !self.generic_ctors.contains_key(name) && !self.concrete.contains_key(name)
            }
            TExprKind::Let(name, value, body) | TExprKind::LetMut(name, value, body) => {
                !aipl_syntax::is_shim_closure_name(name) && sub(value) && sub(body)
            }
            TExprKind::If(cond, then, otherwise) => sub(cond) && sub(then) && sub(otherwise),
            TExprKind::Seq(first, rest) => sub(first) && sub(rest),
            TExprKind::Return(value) => sub(value),
            TExprKind::While(cond, body) => sub(cond) && sub(body),
            TExprKind::Neg(inner) => sub(inner),
            // A loop over an array, set or string with no position binder; a dict, a range and a
            // position are each a loop of their own, spelled out.
            TExprKind::For(_, None, iterable, body) => {
                concrete(&iterable.ty).is_some_and(|t| {
                    matches!(
                        aipl_syntax::unrefined(&t.to_type()),
                        Type::Array(_)
                            | Type::Set(..)
                            | Type::Primitive(aipl_syntax::ast::Primitive::Str)
                    )
                }) && sub(iterable)
                    && sub(body)
            }
            // A tuple is the struct of its elements.
            TExprKind::TupleLit(elems) => {
                let types: Option<Vec<Type>> = elems
                    .iter()
                    .map(|x| concrete(&x.ty).map(|t| crate::decay_concat(t.to_type())))
                    .collect();
                types.is_some_and(|types| {
                    concrete(&e.ty).is_some_and(|t| {
                        t.to_type() == Type::Named(crate::typing::tuple_struct_name(&types))
                    })
                }) && elems.iter().all(sub)
            }
            TExprKind::Try(inner) => sub(inner),
            TExprKind::ArrayLit(elems) | TExprKind::SetLit(elems, _) => elems.iter().all(sub),
            TExprKind::DictLit(pairs) => pairs.iter().all(|(k, v)| sub(k) && sub(v)),
            TExprKind::Slice(object, start, end) => {
                sub(object) && sub(start) && end.as_deref().map_or(true, sub)
            }
            // A grid is read by cell, which is a call to the grid's own function.
            TExprKind::Index(object, index) => {
                sub(object) && sub(index) && !self.is_grid_read(object, index, concrete)
            }
            TExprKind::Field(object, field) => {
                sub(object)
                    && concrete(&object.ty).is_some_and(|t| match t.to_type() {
                        Type::Named(n) => self
                            .structs
                            .get(&n)
                            .or_else(|| self.syn_structs.get(&n))
                            .is_some_and(|fields| fields.iter().any(|(f, _, _)| f == field)),
                        _ => false,
                    })
            }
            // Only the in-place update of the binding assigned: `set xs = xs.f(..)`.
            TExprKind::Assign(target, value, body) => {
                let TExprKind::Ident(name) = &target.kind else {
                    return false;
                };
                let in_place = matches!(
                    &value.kind,
                    TExprKind::Call { callee, args, method_style: true, .. }
                        if self.mutating.contains(callee.name())
                            && matches!(args.first().map(|a| &a.kind), Some(TExprKind::Ident(n)) if n == name)
                );
                (if in_place {
                    self.supported_call(value, true, concrete)
                } else {
                    sub(value)
                }) && sub(body)
            }
            TExprKind::Call { .. } => self.supported_call(e, false, concrete),
            TExprKind::Match(scrutinee, arms) => {
                sub(scrutinee)
                    && arms
                        .iter()
                        .all(|a| is_simple_pattern(&a.pattern) && sub(&a.body))
            }
            TExprKind::IfLet(arm, scrutinee, otherwise) => {
                is_simple_pattern(&arm.pattern)
                    && sub(scrutinee)
                    && sub(&arm.body)
                    && sub(otherwise)
            }
            // A construction that names every field; a generic struct's is settled from its
            // arguments, and a missing field is filled from its default.
            TExprKind::Construct(name, inits) => {
                !self.generic_structs.contains_key(name)
                    && self
                        .structs
                        .get(name)
                        .or_else(|| self.syn_structs.get(name))
                        .is_some_and(|fields| {
                            fields.len() == inits.len()
                                && fields
                                    .iter()
                                    .all(|(f, _, _)| inits.iter().any(|i| i.name.text == *f))
                        })
                    && inits.iter().all(|i| sub(&i.value))
            }
            _ => false,
        }
    }

    /// Whether `e`, a call, is of a kind that is typed from the pinned tree and has arguments that
    /// are. `in_place` is whether it is the update of a binding by one of its own methods.
    fn supported_call(
        &self,
        e: &TExpr<Ty>,
        in_place: bool,
        concrete: &dyn Fn(&Ty) -> Option<ConcreteType>,
    ) -> bool {
        let TExprKind::Call { callee, args, .. } = &e.kind else {
            return false;
        };
        (self.is_operator_call(callee, args.len())
            || self.is_plain_call(callee, args, in_place)
            || self.is_generic_call(callee, args, in_place)
            || self.is_native_call(callee, args.len(), in_place))
            && args.iter().all(|a| self.supported(a, concrete))
    }

    /// Whether a native builtin call is kept as written, with the result type its context gives it:
    /// the builtins that are not expanded, shaped by their arguments or specialized for a
    /// function they are handed.
    fn is_native_call(&self, callee: &Callee, arguments: usize, in_place: bool) -> bool {
        let name = callee.name();
        // The sequence forms are given the callee that implements their argument's shape.
        let shaped = (matches!(
            callee,
            Callee::StartsWith(_) | Callee::EndsWith(_) | Callee::Contains(_)
        ) && arguments == 2)
            || (matches!(callee, Callee::StartsWithAt(_)) && arguments == 3);
        (callee.is_builtin() || self.ctors.contains_key(name))
            && !self.generics.contains_key(name)
            && !self.concrete.contains_key(name)
            && !self.generic_ctors.contains_key(name)
            && (in_place || !self.mutating.contains(name))
            && (shaped
                || !matches!(
                    callee,
                    Callee::StartsWith(_)
                        | Callee::EndsWith(_)
                        | Callee::Contains(_)
                        | Callee::StartsWithAt(_)
                ))
            && !is_expanded(callee)
    }

    /// Whether a call to a generic function is typed from the pinned tree: it keeps its arguments
    /// as written, and the instance it calls follows from the type arguments it records.
    fn is_generic_call(&self, callee: &Callee, args: &[TExpr<Ty>], in_place: bool) -> bool {
        let name = callee.name();
        let Some(generic) = self.generics.get(name) else {
            return false;
        };
        !is_expanded(callee)
            && (in_place || !self.mutating.contains(name))
            && generic.sig.params.len() == args.len()
            && generic
                .sig
                .params
                .iter()
                .all(|p| !matches!(p.ty, Type::Fn(..)))
            && args
                .iter()
                .all(|a| !matches!(&a.ty, Ty::Named(n) if n == aipl_syntax::SPAN_TYPE))
    }

    /// Whether `object[index]` is a read of a grid's cell.
    fn is_grid_read(
        &self,
        object: &TExpr<Ty>,
        index: &TExpr<Ty>,
        concrete: &dyn Fn(&Ty) -> Option<ConcreteType>,
    ) -> bool {
        match (concrete(&object.ty), concrete(&index.ty)) {
            (Some(o), Some(i)) => self.is_grid_cell_index(&o.to_type(), &i.to_type()),
            _ => true,
        }
    }

    /// Whether a call is to a user function that is not generic, is mutating only when it updates
    /// its own receiver in place, and takes no function-typed or variadic parameter: the call keeps its arguments as written, and which
    /// instance it calls follows from what they are.
    fn is_plain_call(&self, callee: &Callee, args: &[TExpr<Ty>], in_place: bool) -> bool {
        let name = callee.name();
        let Some(function) = self.concrete.get(name) else {
            return false;
        };
        !self.generics.contains_key(name)
            && (in_place || !self.mutating.contains(name))
            && function.params.len() == args.len()
            && function
                .params
                .iter()
                .all(|p| !p.arity.is_variadic() && !matches!(p.ty, Type::Fn(..)))
    }

    /// Whether a call is an arithmetic, comparison or logical operator on two operands, which is a
    /// native builtin that needs no instance: the call stays as written.
    fn is_operator_call(&self, callee: &Callee, arguments: usize) -> bool {
        let name = callee.name();
        arguments == 2
            && !self.generics.contains_key(name)
            && !self.concrete.contains_key(name)
            && (aipl_syntax::float_operation_yields_bool(callee).is_some()
                || matches!(
                    callee,
                    Callee::WrappingAdd
                        | Callee::SaturatingAdd
                        | Callee::WrappingSub
                        | Callee::SaturatingSub
                        | Callee::WrappingMul
                )
                || aipl_syntax::binop_for_builtin(callee).is_some())
    }

    /// The body of an instance of `template`, typed from the pinned program, when it can be: the
    /// instance is the plain borrow one (no parameter is owned, kept as a `str` or set, joined as
    /// a concat string or variadic), it takes no function-typed argument, and its body is made of
    /// supported forms.
    pub(crate) fn typed_body(
        &mut self,
        template: &str,
        params: &[Param],
        specs: &[ParamSpec],
        type_args: &HashMap<String, Type>,
    ) -> Option<TExpr<ConcreteType>> {
        let source = *self.typed_fns.get(template)?;
        let plain = |s: &ParamSpec| {
            !s.owned
                && !s.str_kept
                && s.set_kept.is_none()
                && !s.concat
                && s.variadic == crate::VShape::Seq
        };
        let instance = |base: &str, args: &[ConcreteTy]| {
            let written: Vec<Type> = args.iter().map(|a| a.to_type()).collect();
            Some(crate::typing::generic_instance_name(base, &written))
        };
        let subst = |v: &str| {
            type_args
                .get(v)
                .and_then(|t| Ty::from_type(t)?.concretize(&|_| None))
        };
        let concrete = |t: &Ty| {
            t.concretize_with(&subst, &instance)?
                .to_type()
                .to_concrete()
        };
        if !specs.iter().all(plain)
            || !self.cur_lenv.is_empty()
            || params.len() != source.sig.params.len()
            || !self.supported(&source.body, &concrete)
        {
            return None;
        }
        let body = source.body.map_types(&mut |t| concrete(t))?;
        Some(self.lower(body))
    }

    /// `e` as monomorphization leaves it: each call to a user function points at the instance it
    /// calls, which is queued, and the last uses of a heap binding are marked for moving, before the
    /// scope they cover is lowered, so the calls in it see the marks. Parts are lowered in the order
    /// inference reaches them.
    fn lower(&mut self, mut e: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
        if let TExprKind::Let(name, value, body) | TExprKind::LetMut(name, value, body) =
            &mut e.kind
        {
            let marked = self.with_last_uses_moved_typed(name, e.annotation.as_ref(), value, body);
            **body = marked;
            let value_ty = e.annotation.clone().unwrap_or_else(|| value.ty.clone());
            if e.annotation.is_none() && self.struct_fields_of(&value_ty.to_type()).is_some() {
                e.annotation = Some(value_ty);
            }
        }
        match &mut e.kind {
            TExprKind::Let(_, value, body) | TExprKind::LetMut(_, value, body) => {
                self.lower_in_place(value);
                self.lower_in_place(body);
            }
            TExprKind::Match(scrutinee, arms) => {
                let scrutinee_ty = scrutinee.ty.to_type();
                for arm in arms.iter_mut() {
                    self.expand_ignored_payload(&mut arm.pattern, &scrutinee_ty);
                }
                self.lower_in_place(scrutinee);
                for arm in arms.iter_mut() {
                    self.lower_in_place(&mut arm.body);
                }
            }
            TExprKind::IfLet(..) => return self.lower_if_let(e),
            TExprKind::Construct(name, inits) => {
                // Every field, in the order the struct declares them.
                if let Some(fields) = self
                    .structs
                    .get(name)
                    .or_else(|| self.syn_structs.get(name))
                {
                    let order: Vec<&String> = fields.iter().map(|(f, _, _)| f).collect();
                    inits.sort_by_key(|i| order.iter().position(|f| **f == i.name.text));
                }
                for init in inits.iter_mut() {
                    self.lower_in_place(&mut init.value);
                }
            }
            _ => crate::typed_util::each_child_mut(&mut e, &mut |c| self.lower_in_place(c)),
        }
        if matches!(e.kind, TExprKind::Return(_)) {
            e.ty = ConcreteType::Unit;
        }
        if let TExprKind::TupleLit(elems) = &mut e.kind {
            let elems = std::mem::take(elems);
            let types: Vec<Type> = elems
                .iter()
                .map(|x| crate::decay_concat(x.ty.to_type()))
                .collect();
            let name = crate::typing::tuple_struct_name(&types);
            if !self.structs.contains_key(&name) && !self.syn_structs.contains_key(&name) {
                let fields = types
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (format!("_{i}"), t.clone(), None))
                    .collect();
                self.syn_structs.insert(name.clone(), fields);
            }
            let inits = elems
                .into_iter()
                .enumerate()
                .map(|(i, value)| aipl_syntax::typed::TFieldInit {
                    name: aipl_syntax::SpanStr::synthetic(format!("_{i}")),
                    value,
                })
                .collect();
            e.kind = TExprKind::Construct(name, inits);
        }
        if let TExprKind::Call { callee, args, .. } = &mut e.kind {
            if matches!(
                callee,
                Callee::StartsWith(_)
                    | Callee::EndsWith(_)
                    | Callee::Contains(_)
                    | Callee::StartsWithAt(_)
            ) {
                let first = args[0].ty.to_type();
                let seq = if crate::is_str_repr(&first) {
                    Type::Primitive(aipl_syntax::ast::Primitive::Str)
                } else {
                    first
                };
                *callee = callee.with_seq_shape(crate::variadic_shape(&args[1].ty.to_type(), &seq));
            }
        }
        self.point_at_instance(e)
    }

    /// `Ctor(..)` spelled out as one `_` binder per slot of the case, which the scrutinee's type
    /// says.
    fn expand_ignored_payload(&self, pattern: &mut TPattern<ConcreteType>, scrutinee: &Type) {
        if let TPattern::Ctor {
            name,
            bindings,
            ignore_payload: ignore_payload @ true,
        } = pattern
        {
            let slots = self
                .match_payload_tys(scrutinee, name, bindings.len())
                .len();
            *bindings = vec!["_".to_string(); slots];
            *ignore_payload = false;
        }
    }

    /// An `if let` as the exhaustive `match` it means: the named case, then every other case (or a
    /// wildcard, for a string, array or character) with the `else` as its body.
    fn lower_if_let(&mut self, e: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
        let TExpr {
            kind: TExprKind::IfLet(arm, scrutinee, otherwise),
            ty,
            span,
            value_span,
            name_span,
            unsettled,
            derived,
            annotation,
        } = e
        else {
            unreachable!("lower_if_let is given an `if let`");
        };
        let scrutinee_ty = scrutinee.ty.to_type();
        let mut arm = *arm;
        self.expand_ignored_payload(&mut arm.pattern, &scrutinee_ty);
        let (mut scrutinee, mut otherwise) = (*scrutinee, *otherwise);
        self.lower_in_place(&mut scrutinee);
        self.lower_in_place(&mut arm.body);
        self.lower_in_place(&mut otherwise);
        let else_span = otherwise.span.clone();
        let else_arms: Vec<TMatchArm<ConcreteType>> = if crate::is_str_repr(&scrutinee_ty)
            || matches!(
                &scrutinee_ty,
                Type::Array(_) | Type::Primitive(aipl_syntax::ast::Primitive::Char)
            ) {
            vec![TMatchArm {
                pattern: TPattern::Wildcard,
                body: otherwise,
                span: else_span,
            }]
        } else {
            let given = match &arm.pattern {
                TPattern::Ctor { name, .. } => name.split('@').next().unwrap_or(name).to_string(),
                _ => String::new(),
            };
            self.other_cases(&scrutinee_ty, &given)
                .into_iter()
                .map(|(case, payload)| TMatchArm {
                    pattern: TPattern::Ctor {
                        name: case,
                        bindings: vec!["_".to_string(); payload.len()],
                        ignore_payload: false,
                    },
                    body: crate::typed_util::distinct_binders(&otherwise),
                    span: else_span.clone(),
                })
                .collect()
        };
        let mut arms = vec![arm];
        arms.extend(else_arms);
        let mut out = TExpr::new(TExprKind::Match(Box::new(scrutinee), arms), ty, span);
        out.value_span = value_span;
        out.name_span = name_span;
        out.unsettled = unsettled;
        out.derived = derived;
        out.annotation = annotation;
        out
    }

    fn lower_in_place(&mut self, slot: &mut TExpr<ConcreteType>) {
        let taken = std::mem::replace(slot, unit_like(slot));
        *slot = self.lower(taken);
    }

    /// A binding's scope with the last uses of the binding marked for moving, when it holds a heap
    /// value.
    fn with_last_uses_moved_typed(
        &self,
        name: &str,
        annotation: Option<&ConcreteType>,
        value: &TExpr<ConcreteType>,
        body: &TExpr<ConcreteType>,
    ) -> TExpr<ConcreteType> {
        let ty = annotation.map_or_else(|| value.ty.to_type(), TyRepr::to_type);
        if self.owns_heap(&ty) {
            let heap_fields = self.heap_fields_of(&ty);
            let body = crate::move_last_use_typed::move_last_uses(
                name,
                value,
                body,
                &heap_fields,
                &self.mutating,
            );
            crate::move_last_use_typed::move_field_reads(name, value, &body, &heap_fields)
        } else {
            body.clone()
        }
    }

    /// A call to a user function pointed at the instance it calls, which is queued. A parameter is
    /// owned when the callee can take it and the argument is a fresh heap value.
    fn point_at_instance(&mut self, mut e: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
        let TExprKind::Call {
            callee,
            args,
            type_args,
            ..
        } = &e.kind
        else {
            return e;
        };
        let name = callee.name().to_string();
        if self.generics.contains_key(&name) {
            let wanted = self.generics[&name].sig.type_var_names().len();
            let type_args: Vec<Type> = type_args.iter().take(wanted).map(TyRepr::to_type).collect();
            assert_eq!(
                type_args.len(),
                wanted,
                "{name}: the call records its type arguments"
            );
            let shapes = arg_shapes_of(args);
            let atys: Vec<Type> = args.iter().map(|a| a.ty.to_type()).collect();
            let owned = self.owned_for_call(&name, &type_args, &shapes, &atys);
            let params = self.param_specs(&name, &type_args, &atys, &owned);
            let mangled = self.enqueue_full(&name, crate::ParamSpecs { type_args, params });
            if let TExprKind::Call { callee, .. } = &mut e.kind {
                *callee = Callee::User(mangled);
            }
            return e;
        }
        let Some(function) = self.concrete.get(&name) else {
            return e;
        };
        let shapes = arg_shapes_of(args);
        let atys: Vec<Type> = args.iter().map(|a| a.ty.to_type()).collect();
        let owned = self.owned_for_call(&name, &[], &shapes, &atys);
        let params: Vec<ParamSpec> = function
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| ParamSpec {
                owned: owned.contains(&i),
                concat: matches!(p.ty, Type::Primitive(aipl_syntax::ast::Primitive::Str))
                    && atys.get(i).is_some_and(crate::is_concat_str),
                ..ParamSpec::default()
            })
            .collect();
        let mangled = self.enqueue_full(
            &name,
            crate::ParamSpecs {
                type_args: Vec::new(),
                params,
            },
        );
        if let TExprKind::Call { callee, .. } = &mut e.kind {
            *callee = Callee::User(mangled);
        }
        e
    }
}

/// A pattern that names one case, one string or character, or an array's elements: not one that
/// nests another pattern.
/// The callees that monomorphization expands, resolves by the shape of an argument, or reads a
/// shim's cell for, rather than calling.
fn is_expanded(callee: &Callee) -> bool {
    matches!(
        callee,
        Callee::Map
            | Callee::Filter
            | Callee::FilterMap
            | Callee::ZipWith
            | Callee::IsInBoundsOf
            | Callee::ToArray
            | Callee::WithOrSelf
            | Callee::Any
            | Callee::Extend
            | Callee::RopeStr
            | Callee::ShimCellGet
    )
}

fn is_simple_pattern<T: TyRepr>(pattern: &TPattern<T>) -> bool {
    matches!(
        pattern,
        TPattern::Ctor { .. }
            | TPattern::Str(_)
            | TPattern::Char(_)
            | TPattern::Array(_)
            | TPattern::ArrayRest(_)
            | TPattern::Wildcard
    )
}

fn unit_like(like: &TExpr<ConcreteType>) -> TExpr<ConcreteType> {
    TExpr::new(TExprKind::Unit, ConcreteType::Unit, like.span.clone())
}

fn arg_shapes_of(args: &[TExpr<ConcreteType>]) -> Vec<ArgShape> {
    args.iter()
        .map(|a| ArgShape {
            fresh: fresh_tail(a),
            name: match &a.kind {
                TExprKind::Ident(n) => Some(n.clone()),
                _ => None,
            },
        })
        .collect()
}

/// Whether a value is a fresh heap value: built here or returned by a call, possibly at the end of
/// a chain of statements.
fn fresh_tail(e: &TExpr<ConcreteType>) -> bool {
    match &e.kind {
        TExprKind::ArrayLit(_)
        | TExprKind::SetLit(..)
        | TExprKind::DictLit(_)
        | TExprKind::Construct(..)
        | TExprKind::Call { .. } => true,
        TExprKind::Let(_, _, body)
        | TExprKind::LetMut(_, _, body)
        | TExprKind::Seq(_, body)
        | TExprKind::Assign(_, _, body) => fresh_tail(body),
        _ => false,
    }
}
