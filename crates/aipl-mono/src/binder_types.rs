//! The type of each name a pattern binds, read from the scrutinee's type and the declarations.
//! What the conversion to the typed tree (`typed_adapter`) gives a match arm's binders.

use aipl_syntax::ast::{ConcreteVariantDecl, ExprKind, Pattern, Primitive, Type};

/// What each element of a value of type `t` is, when a `for` can walk it.
pub(crate) fn element_of(t: &Type) -> Option<Type> {
    match t {
        Type::Array(inner) | Type::Set(inner, _) => Some((**inner).clone()),
        Type::Primitive(Primitive::Str) | Type::ConcatStr => Some(Type::Primitive(Primitive::Char)),
        _ => None,
    }
}

/// The declarations a name's type can be read from.
pub(crate) struct Decls<'a> {
    pub(crate) variants: &'a [ConcreteVariantDecl],
}

/// The names `pattern` binds, with the type each has when the scrutinee's type says: the payload of
/// an optional, a result or a variant case, or the element of an array.
pub(crate) fn bindings_typed(
    pattern: &Pattern,
    scrutinee: Option<&Type>,
    decls: &Decls<'_>,
) -> Vec<(String, Option<Type>)> {
    let untyped = || {
        pattern
            .bindings()
            .into_iter()
            .map(|b| (b, None))
            .collect::<Vec<_>>()
    };
    let Some(scrutinee) = scrutinee else {
        return untyped();
    };
    match pattern {
        Pattern::Ctor { name, bindings, .. } => {
            let bare = name.split('@').next().unwrap_or(name);
            let payload: Option<Vec<Type>> = match (bare, scrutinee) {
                ("some", Type::Optional(t)) => Some(vec![(**t).clone()]),
                ("ok", Type::Result(t, _)) => Some(vec![(**t).clone()]),
                ("err", Type::Result(_, t)) => Some(vec![(**t).clone()]),
                (_, Type::Named(variant)) => decls
                    .variants
                    .iter()
                    .find(|v| &v.name == variant)
                    .and_then(|v| v.cases.iter().find(|c| c.name == bare))
                    .map(|c| c.payload.iter().map(|t| t.widen()).collect()),
                _ => None,
            };
            match payload {
                Some(payload) if payload.len() == bindings.len() => bindings
                    .iter()
                    .zip(payload)
                    .map(|(b, t)| (b.clone(), Some(t)))
                    .collect(),
                _ => untyped(),
            }
        }
        Pattern::Array(elems) | Pattern::ArrayRest(elems) => match element_of(scrutinee) {
            Some(t) => elems
                .iter()
                .filter_map(|e| match &e.kind {
                    ExprKind::Ident(n) => Some((n.clone(), Some(t.clone()))),
                    _ => None,
                })
                .collect(),
            None => untyped(),
        },
        _ => untyped(),
    }
}
