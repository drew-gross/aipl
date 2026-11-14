//! Typing an instance's body from the pinned program rather than inferring it.
//!
//! An instance whose source function the pinner typed, and whose body holds only forms
//! [`supported`] here, takes its tree from the pinned one: each node's type is the pinned type with
//! the instance's type arguments substituted. Every other instance is inferred from the written
//! tree. The forms covered grow until none is left to infer.

use std::collections::HashMap;

use aipl_syntax::ast::{ConcreteType, Type};
use aipl_syntax::typed::{ConcreteTy, TExpr, TExprKind, Ty, TyRepr};

use crate::{Mono, Param, ParamSpec};

/// Whether a node, and everything under it, is a form that is typed from the pinned tree.
fn supported(e: &TExpr<Ty>) -> bool {
    if e.unsettled || matches!(e.ty, Ty::Never | Ty::Fn(..)) {
        return false;
    }
    match &e.kind {
        TExprKind::Int(_) | TExprKind::Bool(_) | TExprKind::Ident(_) | TExprKind::Unit => true,
        TExprKind::Let(_, value, body) => supported(value) && supported(body),
        TExprKind::If(cond, then, otherwise) => {
            supported(cond) && supported(then) && supported(otherwise)
        }
        TExprKind::Seq(first, rest) => supported(first) && supported(rest),
        _ => false,
    }
}

impl Mono<'_> {
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
            || !supported(&source.body)
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
