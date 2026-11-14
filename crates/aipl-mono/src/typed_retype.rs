//! A pinned body for an instance that keeps a parameter in another representation than the
//! declared one: a `str` where the signature says `char[]`, or a set where it says an array. The
//! pinned types name the declared representation, so the parameter's uses and what is derived from
//! them are retyped.

use std::collections::HashMap;

use aipl_syntax::ast::{ConcreteType, Primitive};
use aipl_syntax::typed::TExprKind;

use crate::typed_adapter::Typed;

fn chars() -> ConcreteType {
    ConcreteType::Array(Box::new(ConcreteType::Primitive(Primitive::Char)))
}

fn text() -> ConcreteType {
    ConcreteType::Primitive(Primitive::Str)
}

/// Whether a parameter declared as `declared` may be kept as `kept`: a `char[]` as a `str`, an
/// array as a set of its elements.
pub(crate) fn is_kept_representation(declared: &ConcreteType, kept: &ConcreteType) -> bool {
    match (declared, kept) {
        (d, k) if d == k => true,
        (d, ConcreteType::Primitive(Primitive::Str)) => *d == chars(),
        (ConcreteType::Array(a), ConcreteType::Set(s, _)) => a == s,
        _ => false,
    }
}

/// `e` with each use of a name in `kept` given the type it is kept as, and the types derived from
/// those uses brought along: a slice or generic result that was a `char[]` is a `str` once an
/// argument of it is.
pub(crate) fn retype(e: &mut Typed, kept: &mut HashMap<String, ConcreteType>) {
    use TExprKind as K;
    let before = e.ty.clone();
    match &mut e.kind {
        K::Ident(name) => {
            if let Some(t) = kept.get(name) {
                e.ty = t.clone();
            }
            return;
        }
        K::Let(name, value, body) | K::LetMut(name, value, body) => {
            let was = value.ty.clone();
            retype(value, kept);
            let shadowed = kept.get(name.as_str()).cloned();
            if value.ty != was {
                kept.insert(name.clone(), value.ty.clone());
            } else {
                kept.remove(name.as_str());
            }
            retype(body, kept);
            match shadowed {
                Some(t) => kept.insert(name.clone(), t),
                None => kept.remove(name.as_str()),
            };
            e.ty = body.ty.clone();
            return;
        }
        K::For(var, index, iterable, body) => {
            retype(iterable, kept);
            let hidden: Vec<_> = [Some(var.clone()), index.clone()]
                .into_iter()
                .flatten()
                .filter_map(|n| kept.remove(&n).map(|t| (n, t)))
                .collect();
            retype(body, kept);
            kept.extend(hidden);
            return;
        }
        K::Match(scrutinee, arms) => {
            retype(scrutinee, kept);
            let mut result: Option<ConcreteType> = None;
            for arm in arms.iter_mut() {
                let hidden: Vec<_> = crate::typed_higher_order::pattern_bindings(&arm.pattern)
                    .into_iter()
                    .filter_map(|n| kept.remove(&n).map(|t| (n, t)))
                    .collect();
                retype(&mut arm.body, kept);
                kept.extend(hidden);
                if result.is_none() || arm.body.ty == text() {
                    result = Some(arm.body.ty.clone());
                }
            }
            if before == chars() && result.as_ref() == Some(&text()) {
                e.ty = text();
            }
            return;
        }
        K::Lambda(..) => return,
        _ => {}
    }
    let mut changed = false;
    crate::typed_util::each_child_mut(e, &mut |c| {
        let was = c.ty.clone();
        retype(c, kept);
        changed |= c.ty != was;
    });
    if !changed {
        return;
    }
    match &e.kind {
        // A sub-range of a `str` is a `str`.
        K::Slice(object, ..) if object.ty == text() && before == chars() => e.ty = text(),
        K::If(_, then, otherwise) if before == chars() => {
            if then.ty == text() || otherwise.ty == text() {
                e.ty = text();
            }
        }
        K::Seq(_, rest) | K::Assign(_, _, rest) => e.ty = rest.ty.clone(),
        K::Call { args, .. } if before == chars() && args.iter().any(|a| a.ty == text()) => {
            e.ty = text()
        }
        _ => {}
    }
}
