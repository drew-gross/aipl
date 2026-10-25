//! Shims whose operations are bound to closures.
//!
//! `shim prints { print = |text| { set seen.push(text); } } { body }` installs a
//! stand-in for `print` at every call site below the shim, so the stand-in is a
//! plain function: an address in a runtime slot, with nowhere to carry what it
//! captured. This pass lowers the closure to such a function and moves what it
//! captures into *cells* for the shim's extent.
//!
//! ```text
//!     shim eff { op = |args| closure_body } { body }
//!
//!   becomes
//!
//!     __shim_cell_set(0, seen);                    // each captured variable, in
//!     let value = shim eff { op = closure } {     // a cell of its own
//!         body
//!     };
//!     set seen = __shim_cell_get(0);               // what the closure set, back
//!     value
//!
//!     fn closure(args) {
//!         mut seen = __shim_cell_get(0);           // a copy of each capture
//!         let result = closure_body;
//!         __shim_cell_set(0, seen);                // what it set, kept
//!         result
//!     }
//! ```
//!
//! A variable is a cell shared by every closure of the shim that captures it, so
//! two operations that both record into one array write to the same one. The
//! checker keeps the shim's own body from using a variable a closure sets
//! (`check_shim_closures`), which is what makes the copy at the end the whole
//! story: while the shim runs, the closures are the only code that sees it.

use super::*;

impl Mono<'_> {
    /// Lower the chain of closure `let`s around a shim, and infer the result. `expr`
    /// is that chain, which `aipl_syntax::shim_closure_chain` takes apart.
    pub(super) fn infer_shim_closures(
        &mut self,
        expr: &Expr,
        env: &Env,
    ) -> Result<(Expr, Type), Error> {
        let (closures, shim) = aipl_syntax::shim_closure_chain(expr)
            .expect("the checker accepted a chain of shim closures around a shim");
        let ExprKind::Shim(effect, bindings, shim_body) = &shim.kind else {
            unreachable!("a closure chain ends in a shim")
        };
        let span = expr.span.clone();
        let at = |kind: ExprKind| Expr::new(kind, span.clone());
        let ident = |name: &str| at(ExprKind::Ident(name.to_string()));
        let cell_number = |id: usize| at(ExprKind::Num(id as i64));
        let cell_get = |id: usize| {
            at(ExprKind::Call(
                Callee::ShimCellGet,
                vec![cell_number(id)],
                false,
            ))
        };
        let cell_set = |id: usize, value: Expr| {
            at(ExprKind::Call(
                Callee::ShimCellSet,
                vec![cell_number(id), value],
                false,
            ))
        };

        // One cell per captured variable, shared by the closures that capture it.
        let mut cells: Vec<(String, usize)> = Vec::new();
        let mut set_by_a_closure: Vec<String> = Vec::new();
        let mut installed = bindings.clone();

        for (closure_name, closure) in closures {
            let ExprKind::Lambda(params, closure_body) = &closure.kind else {
                unreachable!("the checker requires a shim closure to be a lambda")
            };
            let op = bindings
                .iter()
                .find(|(_, f)| f == closure_name)
                .map(|(op, _)| op.clone())
                .expect("the parser binds each closure it names");
            let sig = builtin_sig(&format!("__builtin_{op}"))
                .expect("a shimmable operation is a declared builtin");
            let ptys = sig.param_types();
            let returned = sig.return_type();

            let captured = free_vars(closure_body, params, env);
            let assigned = aipl_syntax::assigned_names(closure_body);
            // The closure's own body: a copy of what it captures, then its code,
            // then what it set put back.
            let mut writes: Vec<(String, usize)> = Vec::new();
            let mut reads: Vec<(String, usize, bool)> = Vec::new();
            for (name, ty) in &captured {
                let id = match cells.iter().find(|(n, _)| n == name) {
                    Some((_, id)) => *id,
                    None => {
                        let id = self.shim_cells.len();
                        self.shim_cells.push(decay_concat(ty.clone()));
                        cells.push((name.clone(), id));
                        id
                    }
                };
                let sets = assigned.contains(name);
                if sets {
                    writes.push((name.clone(), id));
                    if !set_by_a_closure.contains(name) {
                        set_by_a_closure.push(name.clone());
                    }
                }
                reads.push((name.clone(), id, sets));
            }
            let finish = |value: Expr| {
                let mut tail = value;
                for (name, id) in writes.iter().rev() {
                    tail = at(ExprKind::Seq(
                        Box::new(cell_set(*id, ident(name))),
                        Box::new(tail),
                    ));
                }
                tail
            };
            // With nothing to put back the body is the closure's own, which keeps a
            // bare literal in tail position at the operation's integer width; the
            // `let` that holds the value while it is put back says the width instead.
            let mut body = if writes.is_empty() {
                (**closure_body).clone()
            } else if returned == Type::Unit {
                at(ExprKind::Seq(
                    Box::new((**closure_body).clone()),
                    Box::new(finish(at(ExprKind::Unit))),
                ))
            } else {
                at(ExprKind::Let(
                    "$shim_result".to_string(),
                    Some(returned.clone()),
                    Box::new((**closure_body).clone()),
                    Box::new(finish(ident("$shim_result"))),
                ))
            };
            for (name, id, sets) in reads.into_iter().rev() {
                let value = Box::new(cell_get(id));
                body = at(if sets {
                    ExprKind::LetMut(name, None, value, Box::new(body))
                } else {
                    ExprKind::Let(name, None, value, Box::new(body))
                });
            }

            let function = self.synth_name("shim_closure");
            let function_params: Vec<Param> = params
                .iter()
                .zip(&ptys)
                .map(|(p, ty)| Param {
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
                    body,
                },
            );
            for (bound_op, bound_fn) in installed.iter_mut() {
                if *bound_op == op {
                    *bound_fn = function.clone();
                }
            }
        }

        // The shim itself, with each closure's operation bound to its function.
        let shim_expr = at(ExprKind::Shim(effect.clone(), installed, shim_body.clone()));
        let (installed_shim, shim_ty) = self.infer(&shim_expr, env)?;

        // Before it: each captured variable into its cell. After it: what the
        // closures set, back out.
        let mut before: Vec<Expr> = Vec::new();
        for (name, id) in &cells {
            before.push(self.infer(&cell_set(*id, ident(name)), env)?.0);
        }
        let mut after: Vec<Expr> = Vec::new();
        for name in &set_by_a_closure {
            let id = cells.iter().find(|(n, _)| n == name).expect("a cell").1;
            let restore = at(ExprKind::Assign(
                Box::new(ident(name)),
                Box::new(cell_get(id)),
                Box::new(at(ExprKind::Unit)),
            ));
            after.push(self.infer(&restore, env)?.0);
        }

        // A shim whose closures set nothing has nothing to put back, so it is its body's
        // value as it was. One that does is a statement (`check_shim_closures`), whose
        // value nobody reads.
        let (mut lowered, lowered_ty) = if after.is_empty() {
            (installed_shim, shim_ty)
        } else {
            let mut tail = at(ExprKind::Unit);
            for restore in after.into_iter().rev() {
                tail = at(ExprKind::Seq(Box::new(restore), Box::new(tail)));
            }
            (
                at(ExprKind::Seq(Box::new(installed_shim), Box::new(tail))),
                Type::Unit,
            )
        };
        for store in before.into_iter().rev() {
            lowered = at(ExprKind::Seq(Box::new(store), Box::new(lowered)));
        }
        Ok((lowered, lowered_ty))
    }
}
