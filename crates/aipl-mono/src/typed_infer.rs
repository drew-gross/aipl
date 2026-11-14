//! Typing an instance's body from the pinned program rather than inferring it.
//!
//! An instance whose source function the pinner typed, and whose body holds only forms
//! [`supported`] here, takes its tree from the pinned one: each node's type is the pinned type with
//! the instance's type arguments substituted. Every other instance is inferred from the written
//! tree: one that keeps a parameter as a `str` or set where the pinned type says an array, joins a
//! concat string or a variadic, or whose body holds a shim, a grid form, or a function used as a
//! value that is not handed to a function.

use std::collections::HashMap;

use aipl_syntax::ast::{Callee, ConcreteType, Type};
use aipl_syntax::typed::{ConcreteTy, TExpr, TExprKind, TMatchArm, TPattern, Ty, TyRepr};

use crate::{ArgShape, Mono, Param, ParamSpec};

impl Mono<'_> {
    /// Whether a node, and everything under it, is a form that is typed from the pinned tree.
    fn supported(&self, e: &TExpr<Ty>, concrete: &dyn Fn(&Ty) -> Option<ConcreteType>) -> bool {
        // A lambda used as a value becomes a function of its own that captures nothing.
        if let (TExprKind::Lambda(params, body), Ty::Fn(..)) = (&e.kind, &e.ty) {
            return !e.unsettled
                && concrete(&e.ty).is_some()
                && params.iter().all(|p| concrete(&p.ty).is_some())
                && self.supported(body, concrete);
        }
        // A function used as a value is queued for emission.
        if let (TExprKind::Ident(name), Ty::Fn(..)) = (&e.kind, &e.ty) {
            return !e.unsettled
                && self.concrete.contains_key(name)
                && !self.generics.contains_key(name)
                && !self.cur_lenv.contains_key(name)
                && concrete(&e.ty).is_some();
        }
        if e.unsettled || matches!(e.ty, Ty::Fn(..)) {
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
            // A constructor reference where a `Case<V>` is expected.
            TExprKind::Lambda(params, body) => {
                matches!(e.ty, Ty::Case(_))
                    && concrete(&e.ty)
                        .is_some_and(|t| self.lambda_ctor_tag(params, body, &t).is_some())
            }
            TExprKind::Ident(name) => {
                if matches!(e.ty, Ty::Case(_))
                    && concrete(&e.ty).is_some_and(|t| self.ctor_reference_tag(name, &t).is_some())
                {
                    return true;
                }
                // A nullary constructor of a generic variant is built as the instance its type names.
                if self.generic_ctors.contains_key(name) {
                    return self
                        .generic_ctor_instance(name, concrete(&e.ty).as_ref())
                        .is_some();
                }
                // A function used as a value has a function type; a plain name is a local.
                true
            }
            TExprKind::Let(name, ..) if aipl_syntax::is_shim_closure_name(name) => self
                .shim_supported(e, &|x| self.supported(x, concrete), &|params| {
                    params.iter().all(|p| concrete(&p.ty).is_some())
                }),
            TExprKind::Shim(..) => {
                self.shim_supported(e, &|x| self.supported(x, concrete), &|_| true)
            }
            TExprKind::Let(name, value, body) | TExprKind::LetMut(name, value, body) => {
                !aipl_syntax::is_shim_closure_name(name) && sub(value) && sub(body)
            }
            TExprKind::If(cond, then, otherwise) => sub(cond) && sub(then) && sub(otherwise),
            TExprKind::Seq(first, rest) => sub(first) && sub(rest),
            TExprKind::Return(value) => sub(value),
            TExprKind::While(cond, body) => sub(cond) && sub(body),
            TExprKind::Neg(inner) => sub(inner),
            // A loop over an array, set or string; a position, a dict and a range are each a loop of
            // their own, spelled out.
            TExprKind::For(_, _, iterable, body) => {
                concrete(&iterable.ty).is_some_and(|t| {
                    matches!(
                        aipl_syntax::unrefined(&t.to_type()),
                        Type::Array(_)
                            | Type::Set(..)
                            | Type::Dict(..)
                            | Type::Primitive(aipl_syntax::ast::Primitive::Str)
                    ) || matches!(t, ConcreteType::Named(ref n) if n == aipl_syntax::SPAN_TYPE)
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
                sub(object)
                    && sub(index)
                    && (!self.is_grid_read(object, index, concrete)
                        || self.is_known_grid(&object.ty, concrete))
            }
            TExprKind::Field(object, field) => {
                sub(object)
                    && concrete(&object.ty).is_some_and(|t| match t.to_type() {
                        Type::Named(n) => self
                            .structs
                            .get(&n)
                            .or_else(|| self.syn_structs.get(&n))
                            .map_or_else(
                                // A tuple's struct is made where the tuple is built.
                                || {
                                    crate::tuple_instance_arity(&n).is_some()
                                        && field.starts_with('_')
                                },
                                |fields| fields.iter().any(|(f, _, _)| f == field),
                            ),
                        _ => false,
                    })
            }
            // Only the in-place update of the binding assigned: `set xs = xs.f(..)`.
            TExprKind::Assign(target, value, body)
                if !matches!(target.kind, TExprKind::Ident(_)) =>
            {
                // `set a.b.c = v`: a functional update of the structs along the path.
                let mut at = &**target;
                while let TExprKind::Field(object, field) = &at.kind {
                    let known = concrete(&object.ty).is_some_and(|t| match t.to_type() {
                        Type::Named(n) => self
                            .structs
                            .get(&n)
                            .or_else(|| self.syn_structs.get(&n))
                            .is_some_and(|fields| fields.iter().any(|(f, _, _)| f == field)),
                        _ => false,
                    });
                    if !known {
                        return false;
                    }
                    at = object;
                }
                matches!(at.kind, TExprKind::Ident(_)) && sub(value) && sub(body)
            }
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
                sub(scrutinee) && arms.iter().all(|a| sub(&a.body))
            }
            TExprKind::IfLet(arm, scrutinee, otherwise) => {
                sub(scrutinee) && sub(&arm.body) && sub(otherwise)
            }
            // A construction that names every field; a generic struct's is settled from its
            // arguments, and a missing field is filled from its default.
            TExprKind::Construct(name, inits) => {
                // A generic struct is built as the instance its type names.
                let name = if self.generic_structs.contains_key(name) {
                    match concrete(&e.ty) {
                        Some(ConcreteType::Named(instance)) => instance,
                        _ => return false,
                    }
                } else {
                    name.clone()
                };
                self.structs
                    .get(&name)
                    .or_else(|| self.syn_structs.get(&name))
                    .is_some_and(|fields| {
                        inits
                            .iter()
                            .all(|i| fields.iter().any(|(f, _, _)| *f == i.name.text))
                            && fields.iter().all(|(f, ty, default)| {
                                inits.iter().any(|i| i.name.text == *f)
                                    || default.as_ref().is_some_and(|d| {
                                        self.default_literal(d, ty, &e.span).is_some()
                                    })
                            })
                    })
                    && inits.iter().all(|i| sub(&i.value))
            }
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
        // A constructor of a generic variant is built as the instance the call's type names.
        if self.generic_ctors.contains_key(callee.name()) {
            return !matches!(
                e.kind,
                TExprKind::Call {
                    method_style: true,
                    ..
                }
            ) && self
                .generic_ctor_instance(callee.name(), concrete(&e.ty).as_ref())
                .is_some()
                && args.iter().all(|a| self.supported(a, concrete));
        }
        // `rope_str`, a tuple's `to_array` and a grid's `with_or_self` are rewritten, or kept as the
        // native call, by what their arguments are.
        if (matches!(callee, Callee::RopeStr) && args.len() == 2)
            || (matches!(callee, Callee::ToArray) && args.len() == 1)
            || (matches!(callee, Callee::WithOrSelf) && args.len() == 3)
        {
            return (!matches!(callee, Callee::WithOrSelf)
                || !self.is_grid_cell_write(&args[0].ty, &args[1].ty, concrete)
                || concrete(&args[0].ty).is_some_and(
                    |t| matches!(&t, ConcreteType::Named(n) if self.instance_args(n).is_some()),
                ))
                && args.iter().all(|a| self.supported(a, concrete));
        }
        // `recv.f(rest)` where `f` is a field of `recv` holding a function.
        if self.is_function_field_call(callee, args.first().map(|a| &a.ty), concrete) {
            return args.iter().all(|a| self.supported(a, concrete));
        }
        // A call through a function parameter of the function being processed.
        if self.cur_lenv.contains_key(callee.name()) {
            return args.iter().all(|a| self.supported(a, concrete));
        }
        // `extend` is the sequence form, the optional form or the single-element form, by what its
        // source is.
        if matches!(callee, Callee::Extend) && args.len() == 2 {
            return concrete(&args[0].ty)
                .zip(concrete(&args[1].ty))
                .is_some_and(|(r, s)| self.extend_shape(&r.to_type(), &s.to_type()).is_some())
                && args.iter().all(|a| self.supported(a, concrete));
        }
        // A call through a function value held in a local.
        if matches!(callee, Callee::Local(n) if !self.concrete.contains_key(n) && !self.generics.contains_key(n) && !self.ctors.contains_key(n))
        {
            return args.iter().all(|a| self.supported(a, concrete));
        }
        let function_argument = |a: &TExpr<Ty>| match &a.kind {
            TExprKind::Lambda(params, body) => {
                params.iter().all(|p| concrete(&p.ty).is_some()) && self.supported(body, concrete)
            }
            _ => self.is_function_argument(a),
        };
        // A cell pair given to a grid, and a grid walked by its cells.
        if matches!(callee, Callee::IsInBoundsOf)
            && args.len() == 2
            && self.is_grid_cell_write(&args[1].ty, &args[0].ty, concrete)
        {
            return self.is_known_grid(&args[1].ty, concrete)
                && args.iter().all(|a| self.supported(a, concrete));
        }
        if matches!(callee, Callee::Any)
            && args.len() == 2
            && matches!(concrete(&args[0].ty), Some(ConcreteType::Named(n)) if aipl_syntax::is_grid_instance(&n))
        {
            return concrete(&args[0].ty).is_some_and(|t| {
                let ConcreteType::Named(n) = t else {
                    return false;
                };
                self.structs
                    .get(&n)
                    .or_else(|| self.syn_structs.get(&n))
                    .is_some_and(|fs| fs.iter().any(|(f, _, _)| f == "cells"))
            }) && (self.is_function_argument(&args[1]) && function_argument(&args[1]))
                && self.supported(&args[0], concrete);
        }
        if self.is_typed_expansion(callee, args, concrete) {
            return args.iter().all(|a| match &a.kind {
                TExprKind::Lambda(..) | TExprKind::Ident(_) if self.is_function_argument(a) => {
                    function_argument(a)
                }
                _ => self.supported(a, concrete),
            });
        }
        if self.is_function_call(callee, args, in_place) {
            return args.iter().all(|a| {
                if self.is_function_argument(a) {
                    function_argument(a)
                } else {
                    self.supported(a, concrete)
                }
            });
        }
        (self.is_operator_call(callee, args.len())
            || self.is_plain_call(callee, args)
            || self.is_generic_call(callee, args)
            || self.is_native_call(callee, args.len()))
            && args.iter().all(|a| self.supported(a, concrete))
    }

    /// Whether a native builtin call is kept as written, with the result type its context gives it:
    /// the builtins that are not expanded, shaped by their arguments or specialized for a
    /// function they are handed.
    fn is_native_call(&self, callee: &Callee, arguments: usize) -> bool {
        let name = callee.name();
        // The sequence forms are given the callee that implements their argument's shape.
        let shaped = (matches!(
            callee,
            Callee::StartsWith(_) | Callee::EndsWith(_) | Callee::Contains(_)
        ) && arguments == 2)
            || (matches!(callee, Callee::StartsWithAt(_)) && arguments == 3);
        (!matches!(callee, Callee::User(_) | Callee::Local(_)) || self.ctors.contains_key(name))
            && !self.generics.contains_key(name)
            && !self.concrete.contains_key(name)
            && !self.generic_ctors.contains_key(name)
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
    fn is_generic_call(&self, callee: &Callee, args: &[TExpr<Ty>]) -> bool {
        let name = callee.name();
        let Some(generic) = self.generics.get(name) else {
            return false;
        };
        !is_expanded(callee)
            && generic.sig.params.len() == args.len()
            && generic
                .sig
                .params
                .iter()
                .all(|p| !matches!(p.ty, Type::Fn(..)))
    }

    /// A field's default, when it is a literal, a nullary constructor or a construction of such
    /// values: the typed node it stands for, with the field's type.
    pub(crate) fn default_literal(
        &self,
        default: &aipl_syntax::ast::Expr,
        ty: &Type,
        span: &aipl_syntax::Span,
    ) -> Option<TExpr<ConcreteType>> {
        use aipl_syntax::ast::ExprKind as K;
        let concrete = ty.to_concrete()?;
        let kind = match (&default.kind, ty) {
            (K::Num(n), Type::Primitive(p)) if p.is_int() => TExprKind::Int(*n),
            (K::Float(f), Type::Primitive(p)) if p.is_float() => TExprKind::Float(f.clone()),
            (K::Bool(b), Type::Primitive(aipl_syntax::ast::Primitive::Bool)) => TExprKind::Bool(*b),
            (K::Str(text), Type::Primitive(aipl_syntax::ast::Primitive::Str)) => {
                TExprKind::Str(text.clone())
            }
            (K::Char(c), Type::Primitive(aipl_syntax::ast::Primitive::Char)) => TExprKind::Char(*c),
            (K::None, Type::Optional(_)) => TExprKind::None,
            (K::ArrayLit(elems), Type::Array(_)) if elems.is_empty() => {
                TExprKind::ArrayLit(Vec::new())
            }
            (K::SetLit(elems, _), Type::Set(_, order)) if elems.is_empty() => {
                TExprKind::SetLit(Vec::new(), aipl_syntax::typed::PinnedOrder::of(*order)?)
            }
            (K::DictLit(pairs), Type::Dict(..)) if pairs.is_empty() => {
                TExprKind::DictLit(Vec::new())
            }
            // A nullary constructor of the variant the field holds.
            (K::Ident(n), Type::Named(variant))
                if self.ctors.get(n).is_some_and(|v| v == variant)
                    && !self.generic_ctors.contains_key(n) =>
            {
                TExprKind::Ident(n.clone())
            }
            // A struct whose every field is such a value.
            (K::Construct(name, inits), Type::Named(sn)) if name == sn => {
                let fields = self.structs.get(sn).or_else(|| self.syn_structs.get(sn))?;
                let mut out = Vec::with_capacity(fields.len());
                for (f, fty, fdefault) in fields {
                    let value = match inits.iter().find(|i| i.name.text == *f) {
                        Some(i) => self.default_literal(&i.value, fty, span)?,
                        None => self.default_literal(fdefault.as_ref()?, fty, span)?,
                    };
                    out.push(aipl_syntax::typed::TFieldInit {
                        name: aipl_syntax::SpanStr::synthetic(f.clone()),
                        value,
                    });
                }
                TExprKind::Construct(sn.clone(), out)
            }
            _ => return None,
        };
        Some(TExpr::new(kind, concrete, span.clone()))
    }

    /// `set a.b.c = v;` as the functional update it means: `set a = A { b: B { c: v, .. }, .. }`,
    /// every other field copied from the current value.
    pub(crate) fn desugar_field_assign(&self, e: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
        let TExpr {
            kind: TExprKind::Assign(target, value, body),
            ty,
            span,
            ..
        } = e
        else {
            unreachable!("desugar_field_assign is given an assignment");
        };
        // The root binding and the fields down to the one written.
        let mut path = Vec::new();
        let mut at = &*target;
        while let TExprKind::Field(object, field) = &at.kind {
            path.push(field.clone());
            at = object;
        }
        path.reverse();
        let base = at.clone();
        let update = self.field_update(&base, &path, *value, &span);
        TExpr::new(
            TExprKind::Assign(Box::new(base), Box::new(update), body),
            ty,
            span,
        )
    }

    /// A construction of `base`'s struct with the field at `path` set to `value` and the others
    /// read from `base`.
    fn field_update(
        &self,
        base: &TExpr<ConcreteType>,
        path: &[String],
        value: TExpr<ConcreteType>,
        span: &aipl_syntax::Span,
    ) -> TExpr<ConcreteType> {
        let ConcreteType::Named(sn) = &base.ty else {
            unreachable!("a field assignment target is a struct");
        };
        let fields = self
            .structs
            .get(sn)
            .or_else(|| self.syn_structs.get(sn))
            .expect("the struct of an assignment target is known");
        let mut value = Some(value);
        let inits = fields
            .iter()
            .map(|(fname, fty, _)| {
                let field_ty = fty.to_concrete().expect("a field type is concrete");
                let read = TExpr::new(
                    TExprKind::Field(Box::new(base.clone()), fname.clone()),
                    field_ty,
                    span.clone(),
                );
                let init = if *fname == path[0] {
                    let v = value.take().expect("the written field is visited once");
                    if path.len() == 1 {
                        v
                    } else {
                        self.field_update(&read, &path[1..], v, span)
                    }
                } else {
                    read
                };
                aipl_syntax::typed::TFieldInit {
                    name: aipl_syntax::SpanStr::synthetic(fname.clone()),
                    value: init,
                }
            })
            .collect();
        TExpr::new(
            TExprKind::Construct(sn.clone(), inits),
            base.ty.clone(),
            span.clone(),
        )
    }

    /// The form of `extend` a source of type `source` takes onto a receiver of type `receiver`: the
    /// sequence itself, an optional element, or one element. Where the source fits more than one,
    /// the sequence wins. A string receiver is extended only by a sequence.
    pub(crate) fn extend_shape(&self, receiver: &Type, source: &Type) -> Option<crate::VShape> {
        let str_receiver = crate::is_str_repr(receiver);
        let (sequence, element) = if str_receiver {
            (
                Type::Primitive(aipl_syntax::ast::Primitive::Str),
                Type::Primitive(aipl_syntax::ast::Primitive::Char),
            )
        } else if let Type::Array(e) = aipl_syntax::unrefined(receiver) {
            (receiver.clone(), (**e).clone())
        } else {
            return None;
        };
        if crate::typing::coerce(source, &sequence).is_ok() {
            return Some(crate::VShape::Seq);
        }
        if str_receiver {
            return None;
        }
        if matches!(aipl_syntax::unrefined(source), Type::Optional(_))
            && crate::typing::coerce(source, &Type::Optional(Box::new(element.clone()))).is_ok()
        {
            Some(crate::VShape::Opt)
        } else if crate::typing::coerce(source, &element).is_ok() {
            Some(crate::VShape::Elem)
        } else {
            None
        }
    }

    /// Whether `t` is a grid whose element type is recoverable from its instance.
    fn is_known_grid(&self, t: &Ty, concrete: &dyn Fn(&Ty) -> Option<ConcreteType>) -> bool {
        matches!(concrete(t), Some(ConcreteType::Named(n))
            if aipl_syntax::is_grid_instance(&n) && self.instance_args(&n).is_some())
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
    fn is_plain_call(&self, callee: &Callee, args: &[TExpr<Ty>]) -> bool {
        let name = callee.name();
        let Some(function) = self.concrete.get(name) else {
            return false;
        };
        !self.generics.contains_key(name)
            && function.params.len() == args.len()
            && function
                .params
                .iter()
                .all(|p| !matches!(p.ty, Type::Fn(..)))
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

    /// The body of an instance of `template`, typed from the pinned program, when it can be: no
    /// parameter is kept as a `str` or set, joined as a concat string or variadic, the parameters
    /// and result are the types the pinned function declares, and its body is made of supported
    /// forms. A copy specialized for the functions it is handed has the body of the function it
    /// copies, its function parameters being calls through the functions they are bound to.
    pub(crate) fn typed_body(
        &mut self,
        template: &str,
        params: &[Param],
        specs: &[ParamSpec],
        type_args: &HashMap<String, Type>,
        synthesized: Option<&TExpr<ConcreteType>>,
    ) -> Option<TExpr<ConcreteType>> {
        self.current_parameters = params
            .iter()
            .filter_map(|p| Some((p.name.clone(), p.ty.to_concrete()?)))
            .collect();
        // A function synthesized from typed code carries its tree.
        if let Some(body) = synthesized {
            self.scope_names = params.iter().map(|p| p.name.clone()).collect();
            let body = self.mark_owned_parameters(params, specs, body.clone());
            return Some(self.lower(body));
        }
        // A copy specialized for the functions it is handed has the body of the function it copies.
        let specialized = self.specialized_from.get(template).cloned();
        let (source_name, type_args) = match &specialized {
            Some(from) => (from.source.as_str(), &from.type_args),
            None => (template, type_args),
        };
        let source = *self.typed_fns.get(source_name)?;
        // The generic types the body names are instantiated before it is checked, so a struct or
        // variant built from a generic one is known.
        let named = std::cell::RefCell::new(Vec::<(String, Vec<Type>)>::new());
        {
            let record = |base: &str, args: &[ConcreteTy]| {
                let written: Vec<Type> = args.iter().map(|a| a.to_type()).collect();
                named.borrow_mut().push((base.to_string(), written.clone()));
                Some(crate::typing::generic_instance_name(base, &written))
            };
            let subst = |v: &str| {
                type_args
                    .get(v)
                    .and_then(|t| Ty::from_type(t)?.concretize(&|_| None))
            };
            let _ = source.body.map_types(&mut |t: &Ty| {
                t.concretize_with(&subst, &record)?.to_type().to_concrete()
            });
        }
        for (base, args) in named.into_inner() {
            let _ = self.instantiate_generic(&base, &args);
        }
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
        if (specialized.is_none() && params.len() != source.sig.params.len())
            || !self.supported(&source.body, &concrete)
        {
            return None;
        }
        // The pinned body is typed for the declared parameters and result; an instance that keeps a
        // parameter in another representation (a `str` for a `char[]`) or answers in one is not.
        let returns = source
            .sig
            .return_ty
            .as_ref()
            .map_or(Some(Type::Unit), |t| Some(concrete(t)?.to_type()));
        // A `T[]` result the instance answers as a `str` is the `str` itself.
        let returns_str = returns
            == Some(Type::Array(Box::new(Type::Primitive(
                aipl_syntax::ast::Primitive::Char,
            ))))
            && self.cur_ret == Type::Primitive(aipl_syntax::ast::Primitive::Str);
        if returns.as_ref() != Some(&self.cur_ret) && !returns_str {
            return None;
        }
        let mut prologues: Vec<(String, TExpr<ConcreteType>)> = Vec::new();
        let mut next = 0;
        let mut kept: HashMap<String, ConcreteType> = HashMap::new();
        for declared in &source.sig.params {
            if matches!(declared.ty, Ty::Fn(..)) {
                while params.get(next).is_some_and(|p| p.name.starts_with("$cap")) {
                    next += 1;
                }
                continue;
            }
            let instance_param = params.get(next)?;
            let declared_ty = concrete(&declared.ty)?;
            let shape = specs.get(next).map_or(crate::VShape::Seq, |s| s.variadic);
            if shape != crate::VShape::Seq {
                // A variadic handed one element or an optional arrives as that, and the body
                // starts by rebuilding the sequence it was written against.
                let ConcreteType::Array(element) = &declared_ty else {
                    return None;
                };
                let original = declared.name.clone();
                let given = instance_param.name.clone();
                let span = source.body.span.clone();
                let (sequence, convert) = variadic_sequence(
                    &given,
                    element,
                    shape == crate::VShape::Opt,
                    &format!("{original}$x"),
                    &span,
                );
                kept.insert(original.clone(), sequence);
                prologues.push((original, convert));
                next += 1;
                continue;
            }
            if instance_param.ty != declared_ty.to_type() {
                let kept_ty = instance_param.ty.to_concrete()?;
                if !crate::typed_retype::is_kept_representation(&declared_ty, &kept_ty) {
                    return None;
                }
                kept.insert(instance_param.name.clone(), kept_ty);
            }
            next += 1;
        }
        let mut body = source.body.map_types(&mut |t| concrete(t))?;
        let mut has_rope = false;
        body.walk(&mut |n| {
            has_rope |= matches!(&n.kind, TExprKind::Call { callee: Callee::RopeStr, args, .. } if args.len() == 2);
        });
        if !kept.is_empty() || has_rope {
            crate::typed_retype::retype(&mut body, &mut kept);
        }
        for (original, convert) in prologues.into_iter().rev() {
            let ty = body.ty.clone();
            let span = body.span.clone();
            body = TExpr::new(
                TExprKind::Let(original, Box::new(convert), Box::new(body)),
                ty,
                span,
            );
        }
        self.scope_names = params.iter().map(|p| p.name.clone()).collect();
        let body = self.mark_owned_parameters(params, specs, body);
        let lowered = self.lower(body);
        if returns_str {
            // `{ let $ret: str = <body>; $ret }`: the body computes a `char[]`, the same bytes.
            let span = lowered.span.clone();
            let text = ConcreteType::Primitive(aipl_syntax::ast::Primitive::Str);
            let mut wrapped = TExpr::new(
                TExprKind::Let(
                    "$ret".to_string(),
                    Box::new(lowered),
                    Box::new(TExpr::new(
                        TExprKind::Ident("$ret".to_string()),
                        text.clone(),
                        span.clone(),
                    )),
                ),
                text.clone(),
                span,
            );
            wrapped.annotation = Some(text);
            return Some(wrapped);
        }
        Some(lowered)
    }

    /// A struct parameter this instance owns is marked like a binding, before the body is lowered,
    /// so that `owned_for_call` sees its last-use arguments as fresh when it picks each callee's
    /// instance.
    fn mark_owned_parameters(
        &self,
        params: &[Param],
        specs: &[ParamSpec],
        body: TExpr<ConcreteType>,
    ) -> TExpr<ConcreteType> {
        params
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                specs.get(*i).is_some_and(|s| s.owned)
                    && !p.mutable
                    && self.struct_fields_of(&p.ty).is_some()
            })
            .fold(body, |b, (_, p)| {
                crate::move_last_use_typed::move_last_uses_of_param(
                    &p.name,
                    &b,
                    &self.heap_fields_of(&p.ty),
                    &self.mutating,
                )
            })
    }

    /// The instance of a generic variant that constructor `ctor` (`Case@Template`) builds when the
    /// result has type `ty`.
    pub(crate) fn generic_ctor_instance(
        &self,
        ctor: &str,
        ty: Option<&ConcreteType>,
    ) -> Option<String> {
        let base = self.generic_ctors.get(ctor)?;
        match ty? {
            ConcreteType::Named(instance) if instance.starts_with(&format!("{base}$")) => {
                Some(instance.clone())
            }
            _ => None,
        }
    }

    /// `recv.f(rest..)` used as a value, where `f` updates its receiver: `{ mut $copy = recv; set
    /// $copy.f(rest..); $copy }`.
    fn desugar_mutating_call(&self, e: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
        let TExpr {
            kind:
                TExprKind::Call {
                    callee,
                    type_args,
                    mut args,
                    ..
                },
            span,
            ty,
            ..
        } = e
        else {
            unreachable!("desugar_mutating_call is given a call");
        };
        let tmp = aipl_syntax::fresh_temp("mut_copy");
        let receiver = args.remove(0);
        let receiver_ty = receiver.ty.clone();
        let at = || {
            TExpr::new(
                TExprKind::Ident(tmp.clone()),
                receiver_ty.clone(),
                span.clone(),
            )
        };
        let mut call_args = vec![at()];
        call_args.extend(args);
        let method = TExpr::new(
            TExprKind::Call {
                callee,
                type_args,
                args: call_args,
                method_style: true,
            },
            receiver_ty.clone(),
            span.clone(),
        );
        let set = TExpr::new(
            TExprKind::Assign(Box::new(at()), Box::new(method), Box::new(at())),
            receiver_ty,
            span.clone(),
        );
        TExpr::new(
            TExprKind::LetMut(tmp.clone(), Box::new(receiver), Box::new(set)),
            ty,
            span,
        )
    }

    /// Whether `receiver.with_or_self(index, ..)` writes a grid's cell named by a pair.
    fn is_grid_cell_write(
        &self,
        receiver: &Ty,
        index: &Ty,
        concrete: &dyn Fn(&Ty) -> Option<ConcreteType>,
    ) -> bool {
        match (concrete(receiver), concrete(index)) {
            (Some(r), Some(i)) => self.is_grid_cell_index(&r.to_type(), &i.to_type()),
            _ => false,
        }
    }

    /// `(a, b).to_array()` as `[a, b]`; any other tuple is bound once and its fields are the
    /// elements. `grid.with_or_self((r, c), v)` as the grid's `grid_try_set` of the two cells.
    /// `None` for a call that is neither.
    fn rewrite_expanded_call(&self, e: &TExpr<ConcreteType>) -> Option<TExpr<ConcreteType>> {
        // `grid[(row, column)]` is the grid's `grid_get` of the two cells.
        if let TExprKind::Index(grid, pair) = &e.kind {
            return self.grid_call(Callee::GridGet, grid, pair, Vec::new(), e);
        }
        let TExprKind::Call { callee, args, .. } = &e.kind else {
            return None;
        };
        let span = e.span.clone();
        let field = |name: &str, ty: &ConcreteType, i: usize| {
            TExpr::new(
                TExprKind::Field(
                    Box::new(TExpr::new(
                        TExprKind::Ident(name.to_string()),
                        ty.clone(),
                        span.clone(),
                    )),
                    format!("_{i}"),
                ),
                match &e.ty {
                    ConcreteType::Array(elem) if matches!(callee, Callee::ToArray) => {
                        (**elem).clone()
                    }
                    _ => ConcreteType::Primitive(aipl_syntax::ast::Primitive::U64),
                },
                span.clone(),
            )
        };
        match (callee, args.as_slice()) {
            (Callee::ToArray, [tuple]) => {
                if let TExprKind::TupleLit(elems) = &tuple.kind {
                    return Some(TExpr::new(
                        TExprKind::ArrayLit(elems.clone()),
                        e.ty.clone(),
                        span,
                    ));
                }
                let arity = match &tuple.ty {
                    ConcreteType::Named(n) => crate::tuple_instance_arity(n)?,
                    _ => return None,
                };
                let tmp = aipl_syntax::fresh_temp("tuple_array");
                let elems = (0..arity).map(|i| field(&tmp, &tuple.ty, i)).collect();
                let array = TExpr::new(TExprKind::ArrayLit(elems), e.ty.clone(), span.clone());
                Some(TExpr::new(
                    TExprKind::Let(tmp, Box::new(tuple.clone()), Box::new(array)),
                    e.ty.clone(),
                    span,
                ))
            }
            (Callee::WithOrSelf, [grid, pair, value]) => {
                self.grid_call(Callee::GridTrySet, grid, pair, vec![value.clone()], e)
            }
            // The pair comes first: `(row, column).is_in_bounds_of(grid)`.
            (Callee::IsInBoundsOf, [pair, grid]) => {
                self.grid_call(Callee::GridIsInBoundsOf, grid, pair, Vec::new(), e)
            }
            // A grid is walked by its cells, row by row.
            (Callee::Any, [grid, predicate]) => {
                let ConcreteType::Named(instance) = &grid.ty else {
                    return None;
                };
                if !aipl_syntax::is_grid_instance(instance) {
                    return None;
                }
                let cells_ty = self
                    .structs
                    .get(instance)
                    .or_else(|| self.syn_structs.get(instance))?
                    .iter()
                    .find(|(n, _, _)| n == "cells")?
                    .1
                    .to_concrete()?;
                let cells = TExpr::new(
                    TExprKind::Field(Box::new(grid.clone()), "cells".to_string()),
                    cells_ty,
                    span.clone(),
                );
                let TExprKind::Call {
                    callee,
                    type_args,
                    method_style,
                    ..
                } = &e.kind
                else {
                    return None;
                };
                Some(TExpr::new(
                    TExprKind::Call {
                        callee: callee.clone(),
                        type_args: type_args.clone(),
                        args: vec![cells, predicate.clone()],
                        method_style: *method_style,
                    },
                    e.ty.clone(),
                    span,
                ))
            }
            _ => None,
        }
    }

    /// The call to `callee` on `grid` with the two cells of `pair` and then `rest`, when `pair`
    /// addresses a cell of `grid`. A pair written out is its two cells already; a bound one is read
    /// by its fields, and any other is bound first so it is evaluated once.
    fn grid_call(
        &self,
        callee: Callee,
        grid: &TExpr<ConcreteType>,
        pair: &TExpr<ConcreteType>,
        rest: Vec<TExpr<ConcreteType>>,
        like: &TExpr<ConcreteType>,
    ) -> Option<TExpr<ConcreteType>> {
        if !self.is_grid_cell_index(&grid.ty.to_type(), &pair.ty.to_type()) {
            return None;
        }
        let span = like.span.clone();
        let ConcreteType::Named(instance) = &grid.ty else {
            return None;
        };
        let (_, type_args) = self.instance_args(instance)?;
        let type_args = type_args
            .iter()
            .map(|t| t.to_concrete())
            .collect::<Option<Vec<_>>>()?;
        let field = |name: &str, i: usize| {
            TExpr::new(
                TExprKind::Field(
                    Box::new(TExpr::new(
                        TExprKind::Ident(name.to_string()),
                        pair.ty.clone(),
                        span.clone(),
                    )),
                    format!("_{i}"),
                ),
                ConcreteType::Primitive(aipl_syntax::ast::Primitive::U64),
                span.clone(),
            )
        };
        let (cells, binding) = match &pair.kind {
            TExprKind::TupleLit(cells) => (cells.clone(), None),
            TExprKind::Ident(name) => (vec![field(name, 0), field(name, 1)], None),
            _ => {
                let tmp = aipl_syntax::fresh_temp("cell");
                (vec![field(&tmp, 0), field(&tmp, 1)], Some(tmp))
            }
        };
        let mut args = vec![grid.clone()];
        args.extend(cells);
        args.extend(rest);
        let call = TExpr::new(
            TExprKind::Call {
                callee,
                type_args,
                args,
                method_style: false,
            },
            like.ty.clone(),
            span.clone(),
        );
        Some(match binding {
            Some(tmp) => TExpr::new(
                TExprKind::Let(tmp, Box::new(pair.clone()), Box::new(call)),
                like.ty.clone(),
                span,
            ),
            None => call,
        })
    }

    /// Whether a call is `recv.f(rest)` with no function of that name, where `recv` is a struct
    /// whose field `f` holds a function: the field is read and the function it holds is called.
    fn is_function_field_call(
        &self,
        callee: &Callee,
        receiver: Option<&Ty>,
        concrete: &dyn Fn(&Ty) -> Option<ConcreteType>,
    ) -> bool {
        let name = callee.name();
        !callee.is_builtin()
            && !self.concrete.contains_key(name)
            && !self.generics.contains_key(name)
            && !self.ctors.contains_key(name)
            && !self.generic_ctors.contains_key(name)
            && !self.cur_lenv.contains_key(name)
            && receiver
                .and_then(|t| concrete(t))
                .is_some_and(|t| self.fn_field_ret(&t.to_type(), name).is_some())
    }

    /// The tag of the case a constructor reference names (`__ctor$V$C` or `C@V`), when `ty` is the
    /// `Case<V>` of its variant.
    fn ctor_reference_tag(&self, name: &str, ty: &ConcreteType) -> Option<usize> {
        let (variant, case) = match name.strip_prefix(aipl_syntax::CTOR_WRAPPER_PREFIX) {
            Some(rest) => rest.split_once('$')?,
            None => name
                .split_once('@')
                .map(|(case, variant)| (variant, case))?,
        };
        if !matches!(ty, ConcreteType::Case(v) if v == variant) {
            return None;
        }
        self.variants
            .get(variant)?
            .iter()
            .position(|(c, _)| c == case)
    }

    /// The tag of the case a generic variant's constructor lambda names, when `ty` is its
    /// `Case<V>`: the lambda `lower_ctor_refs` builds passes its `__ctor{i}` parameters straight
    /// through to the constructor.
    fn lambda_ctor_tag<T: TyRepr>(
        &self,
        params: &[aipl_syntax::typed::TLambdaParam<T>],
        body: &TExpr<T>,
        ty: &ConcreteType,
    ) -> Option<usize> {
        let TExprKind::Call { callee, args, .. } = &body.kind else {
            return None;
        };
        let shaped = params.len() == args.len()
            && params.iter().enumerate().all(|(i, p)| {
                p.name == format!("{}{i}", aipl_syntax::CTOR_PARAM_PREFIX)
                    && matches!(&args[i].kind, TExprKind::Ident(a) if *a == p.name)
            });
        if !shaped {
            return None;
        }
        self.ctor_reference_tag(callee.name(), ty)
    }

    /// `e` as monomorphization leaves it: each call to a user function points at the instance it
    /// calls, which is queued, and the last uses of a heap binding are marked for moving, before the
    /// scope they cover is lowered, so the calls in it see the marks. Parts are lowered in the order
    /// inference reaches them.
    fn lower(&mut self, mut e: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
        // Whether this node is the call a `set recv = recv.f(..)` updates in place.
        let in_place = std::mem::take(&mut self.in_place_call);
        // A mutating call used as a value is copy-and-modify: copy the receiver, update the copy
        // in place, and yield it, leaving the original untouched.
        if !in_place {
            if let TExprKind::Call { callee, args, .. } = &e.kind {
                if self.mutating.contains(callee.name())
                    && !args.is_empty()
                    && !self.cur_lenv.contains_key(callee.name())
                {
                    let desugared = self.desugar_mutating_call(e);
                    return self.lower(desugared);
                }
            }
        }
        if let TExprKind::Lambda(params, body) = &e.kind {
            if matches!(e.ty, ConcreteType::Fn(..)) {
                let ConcreteType::Fn(_, ret) = &e.ty else {
                    unreachable!()
                };
                let name = self.lift_lambda(params, body, &[], ret.to_type());
                e.kind = TExprKind::Ident(name);
                return e;
            }
        }
        if let TExprKind::Call {
            callee,
            args,
            method_style,
            ..
        } = &e.kind
        {
            let receiver_ty = args.first().map(|a| a.ty.clone());
            let name = callee.name().to_string();
            let is_field_call = !callee.is_builtin()
                && !self.concrete.contains_key(&name)
                && !self.generics.contains_key(&name)
                && !self.ctors.contains_key(&name)
                && !self.generic_ctors.contains_key(&name)
                && !self.cur_lenv.contains_key(&name)
                && receiver_ty
                    .as_ref()
                    .is_some_and(|t| self.fn_field_ret(&t.to_type(), &name).is_some());
            let _ = method_style;
            if is_field_call {
                let TExprKind::Call { mut args, .. } =
                    std::mem::replace(&mut e.kind, TExprKind::Unit)
                else {
                    unreachable!("matched above");
                };
                let receiver = args.remove(0);
                let Type::Named(sn) = receiver.ty.to_type() else {
                    unreachable!()
                };
                let field_ty = self
                    .structs
                    .get(&sn)
                    .or_else(|| self.syn_structs.get(&sn))
                    .and_then(|fs| {
                        fs.iter()
                            .find(|(n, _, _)| *n == name)
                            .map(|(_, t, _)| t.clone())
                    })
                    .and_then(|t| t.to_concrete())
                    .expect("a function field is concrete");
                let tmp = aipl_syntax::fresh_temp("fldcall");
                let span = e.span.clone();
                let field = TExpr::new(
                    TExprKind::Field(Box::new(receiver), name.clone()),
                    field_ty,
                    span.clone(),
                );
                let call = TExpr::new(
                    TExprKind::Call {
                        callee: Callee::Local(tmp.clone()),
                        type_args: Vec::new(),
                        args,
                        method_style: false,
                    },
                    e.ty.clone(),
                    span.clone(),
                );
                let wrapped = TExpr::new(
                    TExprKind::Let(tmp, Box::new(field), Box::new(call)),
                    e.ty.clone(),
                    span,
                );
                return self.lower(wrapped);
            }
        }
        // A range handed to an array parameter is the array of its positions: `range.map(|i| i)`.
        if let TExprKind::Call { callee, args, .. } = &mut e.kind {
            if let Some(params) = self.declared_parameters(callee.name()) {
                for (arg, param) in args.iter_mut().zip(params) {
                    if matches!(param, Type::Array(_))
                        && matches!(&arg.ty, ConcreteType::Named(n) if n == aipl_syntax::SPAN_TYPE)
                    {
                        let range = std::mem::replace(arg, unit_like(arg));
                        *arg = positions_of(range);
                    }
                }
            }
        }
        if let Some(rewritten) = self.rewrite_expanded_call(&e) {
            return self.lower(rewritten);
        }
        if let TExprKind::Let(name, ..) = &e.kind {
            if aipl_syntax::is_shim_closure_name(name) {
                return self.lower_shim_closures(e);
            }
        }
        if matches!(e.kind, TExprKind::Shim(..)) {
            return self.lower_shim(e);
        }
        if let TExprKind::Ident(name) = &e.kind {
            if matches!(e.ty, ConcreteType::Fn(..))
                && self.concrete.contains_key(name)
                && !self.scope_names.contains(name)
            {
                let name = name.clone();
                self.enqueue_concrete(&name);
                return e;
            }
        }
        if let TExprKind::Assign(target, value, _) = &e.kind {
            if let TExprKind::Ident(name) = &target.kind {
                let updates_in_place = matches!(
                    &value.kind,
                    TExprKind::Call { callee, args, method_style: true, .. }
                        if self.mutating.contains(callee.name())
                            && matches!(args.first().map(|a| &a.kind), Some(TExprKind::Ident(n)) if n == name)
                );
                if updates_in_place {
                    let TExprKind::Assign(_, value, body) = &mut e.kind else {
                        unreachable!("matched above");
                    };
                    self.in_place_call = true;
                    self.lower_in_place(value);
                    self.in_place_call = false;
                    self.lower_in_place(body);
                    return e;
                }
            }
        }
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
        if matches!(
            &e.kind,
            TExprKind::Call {
                callee: Callee::Map | Callee::Filter | Callee::FilterMap | Callee::ZipWith,
                ..
            }
        ) {
            return self.lower_expansion(e);
        }
        if let TExprKind::Assign(target, ..) = &e.kind {
            if !matches!(target.kind, TExprKind::Ident(_)) {
                let desugared = self.desugar_field_assign(e);
                return self.lower(desugared);
            }
        }
        if let TExprKind::For(_, index, iterable, _) = &e.kind {
            let range =
                matches!(&iterable.ty, ConcreteType::Named(n) if n == aipl_syntax::SPAN_TYPE);
            if index.is_some() || range || matches!(iterable.ty, ConcreteType::Dict(..)) {
                let desugared = self.desugar_for(e);
                return self.lower(desugared);
            }
        }
        if let TExprKind::Call { callee, args, .. } = &e.kind {
            if self.cur_lenv.contains_key(callee.name()) {
                return self.lower_parameter_call(e);
            }
            if self.hands_functions(callee, args) {
                return self.lower_function_call(e);
            }
        }
        if let TExprKind::Ident(name) = &e.kind {
            if let Some(instance) = self.generic_ctor_instance(name, Some(&e.ty)) {
                let bare = name.split('@').next().unwrap_or(name);
                e.kind = TExprKind::Call {
                    callee: Callee::User(format!("{bare}@{instance}")),
                    type_args: Vec::new(),
                    args: Vec::new(),
                    method_style: false,
                };
                return e;
            }
        }
        if let TExprKind::Call { callee, args, .. } = &mut e.kind {
            if let Some(instance) = self.generic_ctor_instance(callee.name(), Some(&e.ty)) {
                let bare = callee
                    .name()
                    .split('@')
                    .next()
                    .unwrap_or(callee.name())
                    .to_string();
                for a in args.iter_mut() {
                    self.lower_in_place(a);
                }
                *callee = Callee::User(format!("{bare}@{instance}"));
                return e;
            }
        }
        if let TExprKind::Ident(name) = &e.kind {
            if let Some(tag) = self.ctor_reference_tag(name, &e.ty) {
                e.kind = TExprKind::Int(tag as i64);
                return e;
            }
        }
        if let TExprKind::Lambda(params, body) = &e.kind {
            if let Some(tag) = self.lambda_ctor_tag(params, body, &e.ty) {
                e.kind = TExprKind::Int(tag as i64);
                return e;
            }
        }
        match &mut e.kind {
            TExprKind::Let(name, value, body) | TExprKind::LetMut(name, value, body) => {
                self.lower_in_place(value);
                let depth = self.scope_names.len();
                self.scope_names.push(name.clone());
                self.lower_in_place(body);
                self.scope_names.truncate(depth);
            }
            TExprKind::For(var, index, iterable, body) => {
                self.lower_in_place(iterable);
                let depth = self.scope_names.len();
                self.scope_names.push(var.clone());
                self.scope_names.extend(index.clone());
                self.lower_in_place(body);
                self.scope_names.truncate(depth);
            }
            TExprKind::Match(scrutinee, arms) if Mono::is_nested_match(scrutinee, arms) => {
                let TExprKind::Match(scrutinee, arms) =
                    std::mem::replace(&mut e.kind, TExprKind::Unit)
                else {
                    unreachable!("matched above");
                };
                return self.lower_nested_match(*scrutinee, arms, e.ty, e.span);
            }
            TExprKind::Match(scrutinee, arms) => {
                let scrutinee_ty = scrutinee.ty.to_type();
                for arm in arms.iter_mut() {
                    self.expand_ignored_payload(&mut arm.pattern, &scrutinee_ty);
                }
                self.lower_in_place(scrutinee);
                for arm in arms.iter_mut() {
                    let depth = self.scope_names.len();
                    self.scope_names
                        .extend(crate::typed_higher_order::pattern_bindings(&arm.pattern));
                    self.lower_in_place(&mut arm.body);
                    self.scope_names.truncate(depth);
                }
            }
            TExprKind::IfLet(arm, ..) if is_nested_pattern(&arm.pattern) => {
                let TExprKind::IfLet(arm, scrutinee, otherwise) =
                    std::mem::replace(&mut e.kind, TExprKind::Unit)
                else {
                    unreachable!("matched above");
                };
                let else_span = otherwise.span.clone();
                let arms = vec![
                    *arm,
                    TMatchArm {
                        pattern: TPattern::Wildcard,
                        body: *otherwise,
                        span: else_span,
                    },
                ];
                return self.lower_nested_match(*scrutinee, arms, e.ty, e.span);
            }
            TExprKind::IfLet(..) => return self.lower_if_let(e),
            TExprKind::Construct(name, inits) => {
                if self.generic_structs.contains_key(name.as_str()) {
                    if let ConcreteType::Named(instance) = &e.ty {
                        *name = instance.clone();
                    }
                }
                // Every field, in the order the struct declares them.
                if let Some(fields) = self
                    .structs
                    .get(name)
                    .or_else(|| self.syn_structs.get(name))
                {
                    let order: Vec<&String> = fields.iter().map(|(f, _, _)| f).collect();
                    for (f, ty, default) in fields {
                        if !inits.iter().any(|i| i.name.text == *f) {
                            let value = default
                                .as_ref()
                                .and_then(|d| self.default_literal(d, ty, &e.span))
                                .expect("a missing field has a literal default");
                            inits.push(aipl_syntax::typed::TFieldInit {
                                name: aipl_syntax::SpanStr::synthetic(f.clone()),
                                value,
                            });
                        }
                    }
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
            if matches!(callee, Callee::Extend) && args.len() == 2 {
                match self.extend_shape(&args[0].ty.to_type(), &args[1].ty.to_type()) {
                    Some(crate::VShape::Opt) => *callee = Callee::ExtendOptional,
                    Some(crate::VShape::Elem) => *callee = Callee::Push,
                    _ => {}
                }
            }
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
        let depth = self.scope_names.len();
        self.scope_names
            .extend(crate::typed_higher_order::pattern_bindings(&arm.pattern));
        self.lower_in_place(&mut arm.body);
        self.scope_names.truncate(depth);
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

    pub(crate) fn lower_in_place(&mut self, slot: &mut TExpr<ConcreteType>) {
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
                variadic: if p.arity.is_variadic() {
                    atys.get(i)
                        .map_or(crate::VShape::Seq, |a| crate::variadic_shape(a, &p.ty))
                } else {
                    crate::VShape::Seq
                },
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

/// The callees that monomorphization expands, resolves by the shape of an argument, or reads a
/// shim's cell for, rather than calling.
fn is_expanded(callee: &Callee) -> bool {
    matches!(
        callee,
        Callee::Map
            | Callee::Filter
            | Callee::FilterMap
            | Callee::ZipWith
            | Callee::ToArray
            | Callee::WithOrSelf
            | Callee::Any
            | Callee::Extend
            | Callee::RopeStr
            | Callee::ShimCellGet
    )
}

/// The sequence a variadic parameter handed one element (`optional` false) or an optional stands
/// for, with its type: a `char` sequence is a `str`, any other an array.
fn variadic_sequence(
    given: &str,
    element: &ConcreteType,
    optional: bool,
    binder: &str,
    span: &aipl_syntax::Span,
) -> (ConcreteType, TExpr<ConcreteType>) {
    let is_char = *element == ConcreteType::Primitive(aipl_syntax::ast::Primitive::Char);
    let sequence = if is_char {
        ConcreteType::Primitive(aipl_syntax::ast::Primitive::Str)
    } else {
        ConcreteType::Array(Box::new(element.clone()))
    };
    let node = |kind, ty: &ConcreteType| TExpr::new(kind, ty.clone(), span.clone());
    let one = |x: TExpr<ConcreteType>| {
        if is_char {
            node(
                TExprKind::Call {
                    callee: Callee::CharToStr,
                    type_args: Vec::new(),
                    args: vec![x],
                    method_style: false,
                },
                &sequence,
            )
        } else {
            node(TExprKind::ArrayLit(vec![x]), &sequence)
        }
    };
    if !optional {
        return (
            sequence.clone(),
            one(node(TExprKind::Ident(given.to_string()), element)),
        );
    }
    let optional_ty = ConcreteType::Optional(Box::new(element.clone()));
    let empty = if is_char {
        node(TExprKind::Str(String::new()), &sequence)
    } else {
        node(TExprKind::ArrayLit(Vec::new()), &sequence)
    };
    let arm = |pattern, body| TMatchArm {
        pattern,
        body,
        span: span.clone(),
    };
    let matched = node(
        TExprKind::Match(
            Box::new(node(TExprKind::Ident(given.to_string()), &optional_ty)),
            vec![
                arm(
                    TPattern::Ctor {
                        name: "some".to_string(),
                        bindings: vec![binder.to_string()],
                        ignore_payload: false,
                    },
                    one(node(TExprKind::Ident(binder.to_string()), element)),
                ),
                arm(
                    TPattern::Ctor {
                        name: "none".to_string(),
                        bindings: Vec::new(),
                        ignore_payload: false,
                    },
                    empty,
                ),
            ],
        ),
        &sequence,
    );
    (sequence, matched)
}

fn is_nested_pattern<T: TyRepr>(pattern: &TPattern<T>) -> bool {
    crate::typed_match_tree::is_nested(pattern)
}

/// The array of a range's positions: `range.map(|i| i)`.
fn positions_of(range: TExpr<ConcreteType>) -> TExpr<ConcreteType> {
    let span = range.span.clone();
    let index = ConcreteType::Primitive(aipl_syntax::ast::Primitive::U64);
    let identity = TExpr::new(
        TExprKind::Lambda(
            vec![aipl_syntax::typed::TLambdaParam {
                name: "$span_index".to_string(),
                ty: index.clone(),
                span: span.clone(),
            }],
            Box::new(TExpr::new(
                TExprKind::Ident("$span_index".to_string()),
                index.clone(),
                span.clone(),
            )),
        ),
        ConcreteType::Fn(vec![index.clone()], Box::new(index.clone())),
        span.clone(),
    );
    TExpr::new(
        TExprKind::Call {
            callee: Callee::Map,
            type_args: Vec::new(),
            args: vec![range, identity],
            method_style: false,
        },
        ConcreteType::Array(Box::new(index)),
        span,
    )
}

fn unit_like(like: &TExpr<ConcreteType>) -> TExpr<ConcreteType> {
    TExpr::new(TExprKind::Unit, ConcreteType::Unit, like.span.clone())
}

pub(crate) fn arg_shapes_of(args: &[TExpr<ConcreteType>]) -> Vec<ArgShape> {
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
pub(crate) fn fresh_tail(e: &TExpr<ConcreteType>) -> bool {
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
