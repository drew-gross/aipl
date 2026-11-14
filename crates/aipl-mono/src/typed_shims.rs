//! Shims, from the pinned program: the operations a shim stands in for are bound to functions, and
//! a closure bound to one is lowered to such a function whose captures live in cells for the
//! shim's extent (see `shim_closures`, which does the same for a written tree).

use std::collections::HashSet;

use aipl_syntax::ast::{Arity, Callee, ConcreteType, Primitive, Type};
use aipl_syntax::typed::{TExpr, TExprKind, TLambdaParam, TyRepr};

use crate::typed_adapter::Typed;
use crate::{ConcreteTemplate, Mono, Param};

/// The closures of a chain of shim-closure `let`s and the shim the chain ends in.
fn closure_chain<T: TyRepr>(e: &TExpr<T>) -> Option<(Vec<(&str, &TExpr<T>)>, &TExpr<T>)> {
    let mut closures = Vec::new();
    let mut at = e;
    while let TExprKind::Let(name, closure, rest) = &at.kind {
        if !aipl_syntax::is_shim_closure_name(name) {
            return None;
        }
        closures.push((name.as_str(), &**closure));
        at = rest;
    }
    match at.kind {
        TExprKind::Shim(..) if !closures.is_empty() => Some((closures, at)),
        _ => None,
    }
}

impl Mono<'_> {
    /// Whether a shim-closure chain, or a plain shim, holds only forms that are typed from the pinned
    /// tree: each closure is a lambda over supported forms, and the shim's body is supported.
    pub(crate) fn shim_supported(
        &self,
        e: &TExpr<aipl_syntax::typed::Ty>,
        supported: &dyn Fn(&TExpr<aipl_syntax::typed::Ty>) -> bool,
        parameters: &dyn Fn(&[TLambdaParam<aipl_syntax::typed::Ty>]) -> bool,
    ) -> bool {
        if let TExprKind::Shim(_, _, body) = &e.kind {
            return supported(body);
        }
        let Some((closures, shim)) = closure_chain(e) else {
            return false;
        };
        let TExprKind::Shim(_, _, body) = &shim.kind else {
            return false;
        };
        closures.iter().all(|(_, closure)| {
            matches!(&closure.kind, TExprKind::Lambda(params, b) if parameters(params) && supported(b))
        }) && supported(body)
    }

    /// A shim, with the functions bound to its operations queued for emission and the operation's
    /// effect counted among those the function around it has while its body is lowered.
    pub(crate) fn lower_shim(&mut self, mut e: Typed) -> Typed {
        let TExprKind::Shim(effect, bindings, body) = &mut e.kind else {
            unreachable!("lower_shim is given a shim");
        };
        for (_, f) in bindings.iter() {
            self.enqueue_concrete(f);
        }
        let outer = self.cur_effects.clone();
        if !self.cur_effects.contains(effect) {
            self.cur_effects.push(effect.clone());
        }
        self.lower_in_place(body);
        self.cur_effects = outer;
        e.ty = match &e.kind {
            TExprKind::Shim(_, _, body) => body.ty.clone(),
            _ => unreachable!(),
        };
        e
    }

    /// A chain of shim closures around a shim: each closure becomes a function, each variable it
    /// captures a cell, filled before the shim and read back after it.
    pub(crate) fn lower_shim_closures(&mut self, e: Typed) -> Typed {
        let span = e.span.clone();
        let (closures, shim) = closure_chain(&e).expect("a chain of shim closures around a shim");
        let TExprKind::Shim(effect, bindings, shim_body) = &shim.kind else {
            unreachable!("a closure chain ends in a shim");
        };
        let int = |n: i64| {
            TExpr::new(
                TExprKind::Int(n),
                ConcreteType::Primitive(Primitive::I64),
                span.clone(),
            )
        };
        let ident = |name: &str, ty: &ConcreteType| {
            TExpr::new(TExprKind::Ident(name.to_string()), ty.clone(), span.clone())
        };
        let cell_get = |id: usize, ty: &ConcreteType| {
            TExpr::new(
                TExprKind::Call {
                    callee: Callee::ShimCellGet,
                    type_args: Vec::new(),
                    args: vec![int(id as i64)],
                    method_style: false,
                },
                ty.clone(),
                span.clone(),
            )
        };
        let cell_set = |id: usize, value: Typed| {
            TExpr::new(
                TExprKind::Call {
                    callee: Callee::ShimCellSet,
                    type_args: Vec::new(),
                    args: vec![int(id as i64), value],
                    method_style: false,
                },
                ConcreteType::Unit,
                span.clone(),
            )
        };
        let unit = || TExpr::new(TExprKind::Unit, ConcreteType::Unit, span.clone());
        let seq = |first: Typed, rest: Typed| {
            let ty = rest.ty.clone();
            TExpr::new(
                TExprKind::Seq(Box::new(first), Box::new(rest)),
                ty,
                span.clone(),
            )
        };

        // One cell per captured variable, shared by the closures that capture it.
        let mut cells: Vec<(String, usize, ConcreteType)> = Vec::new();
        let mut set_by_a_closure: Vec<String> = Vec::new();
        let mut installed = bindings.clone();
        let scope = self.scope_names.clone();

        for (closure_name, closure) in closures {
            let TExprKind::Lambda(params, closure_body) = &closure.kind else {
                unreachable!("a shim closure is a lambda");
            };
            let op = bindings
                .iter()
                .find(|(_, f)| f == closure_name)
                .map(|(op, _)| op.clone())
                .expect("the parser binds each closure it names");
            let sig = crate::builtin_sig(&format!("__builtin_{op}"))
                .expect("a shimmable operation is a declared builtin");
            let ptys = sig.param_types();
            let returned = sig.return_type();

            let mut bound: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
            let mut captured: Vec<(String, ConcreteType)> = Vec::new();
            crate::typed_higher_order::free_locals(closure_body, &scope, &mut bound, &mut captured);
            let mut assigned = HashSet::new();
            crate::typed_util::assigned_names(closure_body, &mut assigned);
            // A variable the closure only assigns is no use of it, but its copy is what the
            // assignment writes to, so it is captured too.
            closure_body.walk(&mut |n| {
                if let TExprKind::Assign(lhs, _, _) = &n.kind {
                    let mut root = &**lhs;
                    while let TExprKind::Field(object, _) = &root.kind {
                        root = object;
                    }
                    if let TExprKind::Ident(name) = &root.kind {
                        if scope.contains(name)
                            && !params.iter().any(|p| p.name == *name)
                            && !captured.iter().any(|(n, _)| n == name)
                        {
                            captured.push((name.clone(), root.ty.clone()));
                        }
                    }
                }
            });
            // The variables the closure sets, whose copy is what the assignment writes to.
            let mut writes: Vec<(String, usize, ConcreteType)> = Vec::new();
            let mut reads: Vec<(String, usize, ConcreteType, bool)> = Vec::new();
            for (name, ty) in &captured {
                let id = match cells.iter().find(|(n, _, _)| n == name) {
                    Some((_, id, _)) => *id,
                    None => {
                        let id = self.shim_cells.len();
                        self.shim_cells.push(crate::decay_concat(ty.to_type()));
                        cells.push((name.clone(), id, ty.clone()));
                        id
                    }
                };
                let sets = assigned.contains(name);
                if sets {
                    writes.push((name.clone(), id, ty.clone()));
                    if !set_by_a_closure.contains(name) {
                        set_by_a_closure.push(name.clone());
                    }
                }
                reads.push((name.clone(), id, ty.clone(), sets));
            }
            let finish = |value: Typed| {
                let mut tail = value;
                for (name, id, ty) in writes.iter().rev() {
                    tail = seq(cell_set(*id, ident(name, ty)), tail);
                }
                tail
            };
            let mut body = if writes.is_empty() {
                (**closure_body).clone()
            } else if returned == Type::Unit {
                seq((**closure_body).clone(), finish(unit()))
            } else {
                let result_ty = returned
                    .to_concrete()
                    .expect("an operation's result is concrete");
                let mut with_result = TExpr::new(
                    TExprKind::Let(
                        "$shim_result".to_string(),
                        Box::new((**closure_body).clone()),
                        Box::new(finish(ident("$shim_result", &result_ty))),
                    ),
                    result_ty.clone(),
                    span.clone(),
                );
                with_result.annotation = Some(result_ty);
                with_result
            };
            for (name, id, ty, sets) in reads.into_iter().rev() {
                let value = cell_get(id, &ty);
                let body_ty = body.ty.clone();
                body = TExpr::new(
                    if sets {
                        TExprKind::LetMut(name, Box::new(value), Box::new(body))
                    } else {
                        TExprKind::Let(name, Box::new(value), Box::new(body))
                    },
                    body_ty,
                    span.clone(),
                );
            }
            let function = self.synth_name("shim_closure");
            let function_params: Vec<Param> = params
                .iter()
                .zip(&ptys)
                .map(|(p, ty)| Param {
                    name_span: aipl_syntax::ast::NameSpan(p.span.clone()),
                    name: p.name.clone(),
                    ty: ty.clone(),
                    mutable: false,
                    arity: Arity::One,
                    default: None,
                    implicit_some: false,
                })
                .collect();
            self.concrete.insert(
                function.clone(),
                ConcreteTemplate {
                    params: function_params,
                    effects: self.cur_effects.clone(),
                    return_ty: Some(returned),
                    body: body.erase(),
                    typed: Some(body),
                },
            );
            for (bound_op, bound_fn) in installed.iter_mut() {
                if *bound_op == op {
                    *bound_fn = function.clone();
                }
            }
        }

        // The shim itself, with each closure's operation bound to its function.
        let shim_expr = TExpr::new(
            TExprKind::Shim(effect.clone(), installed, shim_body.clone()),
            shim.ty.clone(),
            span.clone(),
        );
        let installed_shim = self.lower_shim(shim_expr);
        let shim_ty = installed_shim.ty.clone();

        // Before it: each captured variable into its cell. After it: what the closures set, back
        // out.
        let after: Vec<Typed> = set_by_a_closure
            .iter()
            .map(|name| {
                let (_, id, ty) = cells.iter().find(|(n, _, _)| n == name).expect("a cell");
                TExpr::new(
                    TExprKind::Assign(
                        Box::new(ident(name, ty)),
                        Box::new(cell_get(*id, ty)),
                        Box::new(unit()),
                    ),
                    ConcreteType::Unit,
                    span.clone(),
                )
            })
            .collect();
        let mut lowered = if after.is_empty() {
            installed_shim
        } else {
            let mut tail = unit();
            for restore in after.into_iter().rev() {
                tail = seq(restore, tail);
            }
            seq(installed_shim, tail)
        };
        let _ = shim_ty;
        for (name, id, ty) in cells.iter().rev() {
            lowered = seq(cell_set(*id, ident(name, ty)), lowered);
        }
        lowered
    }
}
