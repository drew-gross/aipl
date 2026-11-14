//! Typing an instance's body from the pinned program rather than inferring it.
//!
//! An instance whose source function the pinner typed, and whose body holds only forms
//! [`supported`] here, takes its tree from the pinned one: each node's type is the pinned type with
//! the instance's type arguments substituted. Every other instance is inferred from the written
//! tree. The forms covered grow until none is left to infer.

use std::collections::HashMap;

use aipl_syntax::ast::{Callee, ConcreteType, Type};
use aipl_syntax::typed::{ConcreteTy, TExpr, TExprKind, Ty, TyRepr};

use crate::{Mono, Param, ParamSpec};

impl Mono<'_> {
    /// Whether a node, and everything under it, is a form that is typed from the pinned tree.
    fn supported(&self, e: &TExpr<Ty>) -> bool {
        if e.unsettled || matches!(e.ty, Ty::Never | Ty::Fn(..)) {
            return false;
        }
        let all = |xs: &[&TExpr<Ty>]| xs.iter().all(|x| self.supported(x));
        match &e.kind {
            TExprKind::Int(_)
            | TExprKind::Float(_)
            | TExprKind::Bool(_)
            | TExprKind::Char(_)
            | TExprKind::Str(_)
            | TExprKind::Ident(_)
            | TExprKind::Unit => true,
            TExprKind::Let(_, value, body) => all(&[value, body]),
            TExprKind::If(cond, then, otherwise) => all(&[cond, then, otherwise]),
            TExprKind::Seq(first, rest) => all(&[first, rest]),
            TExprKind::Call { callee, args, .. } => {
                self.is_operator_call(callee, args.len()) && args.iter().all(|a| self.supported(a))
            }
            _ => false,
        }
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
        &self,
        template: &str,
        params: &[Param],
        specs: &[ParamSpec],
        type_args: &HashMap<String, Type>,
    ) -> Option<TExpr<ConcreteType>> {
        let source = self.typed_fns.get(template)?;
        let plain = |s: &ParamSpec| {
            !s.owned
                && !s.str_kept
                && s.set_kept.is_none()
                && !s.concat
                && s.variadic == crate::VShape::Seq
        };
        if !specs.iter().all(plain)
            || !self.cur_lenv.is_empty()
            || params.len() != source.sig.params.len()
            || !self.supported(&source.body)
        {
            return None;
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
        source.body.map_types(&mut |t| {
            t.concretize_with(&subst, &instance)?
                .to_type()
                .to_concrete()
        })
    }
}
