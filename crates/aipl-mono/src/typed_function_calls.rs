//! Calls to functions that take functions, from the pinned program: each is a call to a copy of
//! the callee specialized for the functions it is handed.

use std::collections::HashMap;

use aipl_syntax::ast::{Arity, Callee, ConcreteType, Primitive, Type};
use aipl_syntax::typed::{TExpr, TExprKind, Ty, TyRepr};
use aipl_syntax::Span;

use crate::typed_adapter::Typed;
use crate::typed_higher_order::Lifted;
use crate::{
    ConcreteTemplate, Generic, LambdaBinding, LambdaEnv, Mono, Param, ParamSpec, ParamSpecs, VShape,
};

/// What a specialized copy of a function was made from, for typing its body from the pinned tree.
#[derive(Clone)]
pub(crate) struct SpecializedFrom {
    /// The function whose body the copy has.
    pub(crate) source: String,
    /// What that function's type variables are, for a generic one.
    pub(crate) type_args: HashMap<String, Type>,
}

impl Mono<'_> {
    /// Whether a node stands for a function: a lambda, a function parameter of the function being
    /// processed, or a function used by name.
    pub(crate) fn is_function_argument(&self, a: &TExpr<Ty>) -> bool {
        match &a.kind {
            TExprKind::Lambda(..) => true,
            TExprKind::Ident(g) => {
                matches!(a.ty, Ty::Fn(..))
                    && (self.cur_lenv.contains_key(g)
                        || self.concrete.contains_key(g)
                        || self.generics.contains_key(g)
                        || Callee::from_canonical(g).is_some_and(|c| c.is_builtin()))
            }
            _ => false,
        }
    }

    /// `e` with each function that a call wants and cannot name written as the lambda that calls
    /// it: a local `g` as `|a| g(a)`, and any other expression of function type (a call that
    /// returns one) bound to a local first. The callee is specialized for the function it is
    /// handed, which has to be a lambda or a name, and the lambda captures what it calls.
    pub(crate) fn expand_local_functions(&self, e: &mut TExpr<Ty>, fresh: &mut usize) {
        e.for_each_child_mut(&mut |child| self.expand_local_functions(child, fresh));
        let TExprKind::Call { callee, args, .. } = &mut e.kind else {
            return;
        };
        let declared = self.declared_parameters(callee.name());
        let native = matches!(
            callee,
            Callee::Map | Callee::Filter | Callee::FilterMap | Callee::ZipWith
        );
        let mut bound: Vec<(String, TExpr<Ty>)> = Vec::new();
        for (i, arg) in args.iter_mut().enumerate() {
            let wanted = native
                || declared
                    .as_ref()
                    .is_some_and(|d| d.get(i).is_some_and(|p| matches!(p, Type::Fn(..))));
            if !wanted || self.is_function_argument(arg) {
                continue;
            }
            let Ty::Fn(parameter_tys, ret) = arg.ty.clone() else {
                continue;
            };
            *fresh += 1;
            let local = match &arg.kind {
                TExprKind::Ident(local) => local.clone(),
                _ => {
                    let local = format!("$fn$eta{fresh}");
                    let value = std::mem::replace(
                        arg,
                        TExpr::new(
                            TExprKind::Ident(local.clone()),
                            arg.ty.clone(),
                            arg.span.clone(),
                        ),
                    );
                    bound.push((local.clone(), value));
                    local
                }
            };
            let params: Vec<aipl_syntax::typed::TLambdaParam<Ty>> = parameter_tys
                .iter()
                .enumerate()
                .map(|(n, ty)| aipl_syntax::typed::TLambdaParam {
                    name: format!("{local}$a{n}"),
                    ty: ty.clone(),
                    span: arg.span.clone(),
                })
                .collect();
            let call = TExpr::new(
                TExprKind::Call {
                    callee: Callee::Local(local),
                    type_args: Vec::new(),
                    args: params
                        .iter()
                        .map(|p| {
                            TExpr::new(
                                TExprKind::Ident(p.name.clone()),
                                p.ty.clone(),
                                arg.span.clone(),
                            )
                        })
                        .collect(),
                    method_style: false,
                },
                (*ret).clone(),
                arg.span.clone(),
            );
            let ty = arg.ty.clone();
            let span = arg.span.clone();
            *arg = TExpr::new(TExprKind::Lambda(params, Box::new(call)), ty, span);
        }
        for (name, value) in bound.into_iter().rev() {
            let (ty, span) = (e.ty.clone(), e.span.clone());
            let call = std::mem::replace(e, TExpr::new(TExprKind::Unit, ty.clone(), span.clone()));
            *e = TExpr::new(
                TExprKind::Let(name, Box::new(value), Box::new(call)),
                ty,
                span,
            );
        }
    }

    /// The declared parameter types of a user function, concrete or generic.
    pub(crate) fn declared_parameters(&self, name: &str) -> Option<Vec<Type>> {
        if let Some(generic) = self.generics.get(name) {
            return Some(generic.sig.params.iter().map(|p| p.ty.clone()).collect());
        }
        self.concrete
            .get(name)
            .map(|f| f.params.iter().map(|p| p.ty.clone()).collect())
    }

    /// Whether a call is to a user function with function-typed parameters, handed lambdas, function
    /// parameters or function names for each of them. `in_place` is whether it is the update of a
    /// binding by one of its own methods.
    pub(crate) fn is_function_call(
        &self,
        callee: &Callee,
        args: &[TExpr<Ty>],
        in_place: bool,
    ) -> bool {
        let name = callee.name();
        if matches!(
            callee,
            Callee::Map | Callee::Filter | Callee::FilterMap | Callee::ZipWith
        ) || (!in_place && self.mutating.contains(name))
        {
            return false;
        }
        let Some(params) = self.declared_parameters(name) else {
            return false;
        };
        params.len() == args.len()
            && params.iter().any(|p| matches!(p, Type::Fn(..)))
            && params.iter().zip(args).all(|(p, a)| match p {
                Type::Fn(..) => self.is_function_argument(a),
                // A grid given to an array parameter is walked by its cells.
                _ => !matches!(&a.ty, Ty::Named(n) if aipl_syntax::is_grid_instance(n)),
            })
    }

    /// Whether a typed call hands a function to one that takes it.
    pub(crate) fn hands_functions(&self, callee: &Callee, args: &[Typed]) -> bool {
        let name = callee.name();
        !matches!(
            callee,
            Callee::Map | Callee::Filter | Callee::FilterMap | Callee::ZipWith
        ) && self.declared_parameters(name).is_some_and(|params| {
            params.len() == args.len()
                && params.iter().zip(args).any(|(p, a)| {
                    matches!(p, Type::Fn(..))
                        && (matches!(a.kind, TExprKind::Lambda(..))
                            || matches!(&a.kind, TExprKind::Ident(g)
                                if matches!(a.ty, ConcreteType::Fn(..))
                                    && (self.cur_lenv.contains_key(g)
                                        || self.concrete.contains_key(g)
                                        || self.generics.contains_key(g)
                                        || Callee::from_canonical(g).is_some_and(|c| c.is_builtin()))))
                })
        })
    }

    /// A call through a function parameter of the function being processed: a call to the function it
    /// is bound to, given the captures it was bound with.
    pub(crate) fn lower_parameter_call(&mut self, mut e: Typed) -> Typed {
        let TExprKind::Call { callee, args, .. } = &mut e.kind else {
            unreachable!("lower_parameter_call is given a call");
        };
        for a in args.iter_mut() {
            self.lower_in_place(a);
        }
        let binding = self.cur_lenv[callee.name()].clone();
        for capture in &binding.captures {
            if let aipl_syntax::ast::ExprKind::Ident(cn) = &capture.kind {
                let ty = self.current_parameters[cn].clone();
                args.push(TExpr::new(TExprKind::Ident(cn.clone()), ty, e.span.clone()));
            }
        }
        *callee = Callee::resolve(binding.fn_name);
        if let TExprKind::Call { method_style, .. } = &mut e.kind {
            *method_style = false;
        }
        e
    }

    /// A call that hands functions to a function: its arguments lowered, each function resolved to
    /// the function it calls, and the call pointed at the copy of the callee specialized for them.
    pub(crate) fn lower_function_call(&mut self, e: Typed) -> Typed {
        let Typed {
            kind:
                TExprKind::Call {
                    callee,
                    type_args,
                    args,
                    method_style,
                },
            ty,
            span,
            ..
        } = e
        else {
            unreachable!("lower_function_call is given a call");
        };
        let name = callee.name().to_string();
        let declared: Vec<Type> = self.declared_parameters(&name).expect("a user function");
        let mut args = args;
        for (a, p) in args.iter_mut().zip(&declared) {
            if !matches!(p, Type::Fn(..)) {
                self.lower_in_place(a);
            }
        }
        let ret = ty.to_type();
        let (base, template, from) = if self.generics.contains_key(&name) {
            self.generic_template(&name, &type_args, &args, &ret)
        } else {
            let template = self.concrete[&name].clone();
            (
                name.clone(),
                template,
                SpecializedFrom {
                    source: name.clone(),
                    type_args: HashMap::new(),
                },
            )
        };
        self.specialize_for_functions(&base, &template, from, ret, args, ty, method_style, span)
    }

    /// The copy of generic `name` at the call's type arguments, as a function with none left: its
    /// parameter and return types substituted, a `str` or set handed to an array parameter kept as
    /// it is, and the name the copy goes by.
    fn generic_template(
        &mut self,
        name: &str,
        type_args: &[ConcreteType],
        args: &[Typed],
        ret: &Type,
    ) -> (String, ConcreteTemplate, SpecializedFrom) {
        let Generic { sig } = self.generics[name].clone();
        let wanted = sig.type_var_names().len();
        let type_args: Vec<Type> = type_args.iter().take(wanted).map(TyRepr::to_type).collect();
        assert_eq!(
            type_args.len(),
            wanted,
            "{name}: the call records its type arguments"
        );
        let map: HashMap<String, Type> = sig
            .type_var_names()
            .into_iter()
            .zip(type_args.iter().cloned())
            .collect();
        let chars = Type::Array(Box::new(Type::Primitive(Primitive::Char)));
        let mut str_arg = vec![false; sig.params.len()];
        let mut set_arg: Vec<Option<aipl_syntax::ast::SetOrder>> = vec![None; sig.params.len()];
        let mut params: Vec<Param> = Vec::with_capacity(sig.params.len());
        for (i, p) in sig.params.iter().enumerate() {
            if !matches!(p.ty, Type::Fn(..)) {
                let aty = args[i].ty.to_type();
                str_arg[i] = aty == Type::Primitive(Primitive::Str);
                if let (Type::Set(_, order), Type::Array(_)) = (&aty, &p.ty) {
                    set_arg[i] = Some(*order);
                }
            }
            let ty = crate::subst_vars(&p.ty, &map);
            let ty = match (str_arg[i], set_arg[i], ty) {
                (true, _, t) if t == chars => Type::Primitive(Primitive::Str),
                (_, Some(order), Type::Array(e)) => Type::Set(e, order),
                (_, _, t) => t,
            };
            let ty = self
                .resolve_generic_ty(&ty)
                .unwrap_or_else(|e| panic!("a parameter type is settled: {e:?}"));
            params.push(Param {
                name_span: p.name_span.clone(),
                name: p.name.clone(),
                ty,
                mutable: p.mutable,
                arity: p.arity,
                default: p.default.clone(),
                implicit_some: p.implicit_some,
            });
        }
        // A `T[]` result the call is a `str` for is the `str` itself; the copy is marked so it never
        // collides with the one a real `char[]` would reach.
        let str_ret = *ret == Type::Primitive(Primitive::Str)
            && sig
                .return_ty
                .as_ref()
                .is_some_and(|t| crate::subst_vars(t, &map) == chars);
        // A function that updates its receiver in place is typed, as a call, by that receiver; what
        // it returns is what it declares.
        let declared_return = match &sig.return_ty {
            Some(_) if str_ret => Type::Primitive(Primitive::Str),
            Some(t) => self
                .resolve_generic_ty(&crate::subst_vars(t, &map))
                .unwrap_or_else(|e| panic!("a return type is settled: {e:?}")),
            None => Type::Unit,
        };
        let template = ConcreteTemplate {
            typed: None,
            params,
            effects: sig.effects.clone(),
            return_ty: Some(declared_return),
        };
        let mut base = name.to_string();
        for t in &type_args {
            base.push('$');
            base.push_str(&aipl_syntax::raw_type_name(t));
        }
        for (i, is_str) in str_arg.iter().enumerate() {
            if *is_str {
                base.push_str(&format!("$s{i}"));
            }
        }
        for (i, kept) in set_arg.iter().enumerate() {
            if kept.is_some() {
                base.push_str(&format!("$k{i}"));
            }
        }
        if str_ret {
            base.push_str("$rs");
        }
        (
            base,
            template,
            SpecializedFrom {
                source: name.to_string(),
                type_args: map,
            },
        )
    }

    /// The call to `template` specialized for the functions among `args`: each function-typed
    /// parameter dropped, a parameter added for each local the functions capture, and the functions
    /// bound in the copy's environment. One copy serves every call that binds the same functions.
    #[allow(clippy::too_many_arguments)]
    fn specialize_for_functions(
        &mut self,
        name: &str,
        template: &ConcreteTemplate,
        from: SpecializedFrom,
        _ret: Type,
        args: Vec<Typed>,
        ty: ConcreteType,
        _method_style: bool,
        span: Span,
    ) -> Typed {
        // The shape each forwarded argument gives its parameter: a variadic handed one element or an
        // optional takes the instance that rebuilds the sequence.
        let mut shapes: Vec<VShape> = Vec::new();
        let mut new_args: Vec<Typed> = Vec::new();
        // The type of each forwarded argument, for deciding which the callee may own.
        let mut new_tys: Vec<Type> = Vec::new();
        let mut targets: Vec<Option<(String, Vec<Type>)>> = Vec::new();
        for (arg, param) in args.into_iter().zip(&template.params) {
            if let Type::Fn(..) = &param.ty {
                let Lifted { name, captures, .. } = self.lifted_argument(arg);
                let mut cap_types = Vec::with_capacity(captures.len());
                for (cn, ct) in captures {
                    cap_types.push(ct.to_type());
                    new_tys.push(ct.to_type());
                    new_args.push(TExpr::new(TExprKind::Ident(cn), ct, span.clone()));
                    shapes.push(VShape::Seq);
                }
                targets.push(Some((name, cap_types)));
            } else {
                let aty = arg.ty.to_type();
                shapes.push(if param.arity.is_variadic() {
                    crate::variadic_shape(&aty, &param.ty)
                } else {
                    VShape::Seq
                });
                new_tys.push(aty);
                new_args.push(arg);
                targets.push(None);
            }
        }
        // A `mut self` receiver it may own is handed over like any call's, so the callee grows it in
        // place instead of copying a lent one.
        let mutating_receiver = template.params.first().is_some_and(|p| p.mutable);
        let instance = |m: &mut Self, spec: &str, new_args: &[Typed]| {
            let owned = if mutating_receiver {
                m.owned_for_call(
                    spec,
                    &[],
                    &crate::typed_infer::arg_shapes_of(new_args),
                    &new_tys,
                )
            } else {
                Vec::new()
            };
            m.enqueue_full(
                spec,
                ParamSpecs {
                    type_args: Vec::new(),
                    params: shapes
                        .iter()
                        .enumerate()
                        .map(|(i, v)| ParamSpec {
                            variadic: *v,
                            owned: owned.contains(&i),
                            ..ParamSpec::default()
                        })
                        .collect(),
                },
            )
        };
        let finish = |mangled: String, new_args: Vec<Typed>| {
            TExpr::new(
                TExprKind::Call {
                    callee: Callee::User(mangled),
                    type_args: Vec::new(),
                    args: new_args,
                    method_style: mutating_receiver,
                },
                ty.clone(),
                span.clone(),
            )
        };
        let key = (
            name.to_string(),
            targets
                .iter()
                .filter_map(|t| t.as_ref().map(|(f, _)| f.clone()))
                .collect::<Vec<_>>(),
        );
        if let Some(spec) = self.spec_memo.get(&key).cloned() {
            let mangled = instance(self, &spec, &new_args);
            return finish(mangled, new_args);
        }
        let mut new_params: Vec<Param> = Vec::new();
        let mut lenv: LambdaEnv = HashMap::new();
        for (param, target) in template.params.iter().zip(&targets) {
            match target {
                Some((fn_name, cap_types)) => {
                    let mut cap_idents = Vec::with_capacity(cap_types.len());
                    for ct in cap_types {
                        // A parameter name only has to be unique within this callee's own list.
                        let cap = format!("$cap{}", new_params.len());
                        new_params.push(Param {
                            name_span: Default::default(),
                            name: cap.clone(),
                            ty: ct.clone(),
                            mutable: false,
                            arity: Arity::One,
                            default: None,
                            implicit_some: false,
                        });
                        cap_idents.push(aipl_syntax::ast::Expr::new(
                            aipl_syntax::ast::ExprKind::Ident(cap),
                            span.clone(),
                        ));
                    }
                    lenv.insert(
                        param.name.clone(),
                        LambdaBinding {
                            fn_name: fn_name.clone(),
                            captures: cap_idents,
                        },
                    );
                }
                None => new_params.push(param.clone()),
            }
        }
        // Named by what specializes it — the callee and the functions bound to its function-typed
        // parameters, which is exactly the memo key.
        let spec_name = key
            .1
            .iter()
            .fold(format!("{name}$lam"), |acc, f| format!("{acc}${f}"));
        let effects = self.specialization_effects(&template.effects, &key.1);
        self.concrete.insert(
            spec_name.clone(),
            ConcreteTemplate {
                typed: None,
                params: new_params,
                effects,
                return_ty: template.return_ty.clone(),
            },
        );
        self.lambda_envs.insert(spec_name.clone(), lenv);
        self.specialized_from.insert(spec_name.clone(), from);
        self.spec_memo.insert(key, spec_name.clone());
        let mangled = instance(self, &spec_name, &new_args);
        finish(mangled, new_args)
    }
}
