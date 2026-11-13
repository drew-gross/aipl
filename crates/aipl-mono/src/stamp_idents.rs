//! Giving each bare name the type of what it names.
//!
//! The passes after monomorphization build names of their own (an inlined parameter's
//! binding, a reduction's operand, a moved receiver), and a name made without a type stands
//! for something whose type is on its binder: a parameter, or a `let` or `for` that names it.
//! This reads it from there, so a body leaves lowering with a type on every name whose binder
//! says one. A name bound by a pattern is left alone: its type comes from the case it
//! destructures, which is not a name's to derive.

use aipl_syntax::ast::{
    Callee, ConcreteStructDecl, ConcreteVariantDecl, Expr, ExprKind, Pattern, Primitive, Type,
};

use crate::{children_mut, ConcreteFn, MonoProgram};

/// `program` with the type of each untyped name filled in from its binder.
pub fn stamp_idents(program: &MonoProgram) -> MonoProgram {
    let decls = Decls {
        variants: &program.variants,
        structs: &program.structs,
    };
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| {
                let mut scope: Vec<(String, Option<Type>)> = f
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), Some(p.ty.widen())))
                    .collect();
                let mut body = f.body.clone();
                fill(&mut body, &mut scope, &decls);
                ConcreteFn { body, ..f.clone() }
            })
            .collect(),
        ..program.clone()
    }
}

/// What each element of a value of type `t` is, when a `for` can walk it.
pub(crate) fn element_of(t: &Type) -> Option<Type> {
    match t {
        Type::Array(inner) | Type::Set(inner, _) => Some((**inner).clone()),
        Type::Primitive(Primitive::Str) | Type::ConcatStr => Some(Type::Primitive(Primitive::Char)),
        _ => None,
    }
}

fn fill(e: &mut Expr, scope: &mut Vec<(String, Option<Type>)>, decls: &Decls<'_>) {
    match &mut e.kind {
        ExprKind::Ident(name) => {
            if e.ty.is_none() {
                let found = scope.iter().rev().find(|(n, _)| n == name);
                if let Some((_, Some(t))) = found {
                    if !matches!(t, Type::Unit) {
                        e.ty = Some(Box::new(t.clone()));
                    }
                }
            }
        }
        ExprKind::Let(name, declared, value, body)
        | ExprKind::LetMut(name, declared, value, body) => {
            fill(value, scope, decls);
            let ty = declared.clone().or_else(|| derived(value));
            scope.push((name.clone(), ty));
            fill(body, scope, decls);
            scope.pop();
        }
        ExprKind::For(item, index, iterable, body) => {
            fill(iterable, scope, decls);
            let element = iterable.ty.as_deref().and_then(element_of);
            scope.push((item.clone(), element));
            if let Some(index) = index {
                scope.push((index.clone(), Some(Type::Primitive(Primitive::U64))));
            }
            fill(body, scope, decls);
            if index.is_some() {
                scope.pop();
            }
            scope.pop();
        }
        ExprKind::Lambda(params, body) => {
            let depth = scope.len();
            scope.extend(params.iter().map(|p| (p.name.clone(), p.ty.clone())));
            fill(body, scope, decls);
            scope.truncate(depth);
        }
        ExprKind::IfLet(arm, scrutinee, otherwise) => {
            fill(scrutinee, scope, decls);
            let depth = scope.len();
            scope.extend(bindings_typed(&arm.pattern, scrutinee.ty.as_deref(), decls));
            fill(&mut arm.body, scope, decls);
            scope.truncate(depth);
            fill(otherwise, scope, decls);
        }
        ExprKind::Match(scrutinee, arms) => {
            fill(scrutinee, scope, decls);
            for arm in arms {
                let bound = bindings_typed(&arm.pattern, scrutinee.ty.as_deref(), decls);
                let depth = scope.len();
                scope.extend(bound);
                fill(&mut arm.body, scope, decls);
                scope.truncate(depth);
            }
        }
        _ => {
            for child in children_mut(e) {
                fill(child, scope, decls);
            }
            settle_built(e, decls);
        }
    }
}

/// The type `e` has: its stamp, or the one its form says from what it holds (a binding or a
/// sequence is its body, an `if` or `match` a branch). `None` for a unit value.
fn derived(e: &Expr) -> Option<Type> {
    if let Some(t) = &e.ty {
        return Some((**t).clone());
    }
    match &e.kind {
        ExprKind::Let(_, _, _, body)
        | ExprKind::LetMut(_, _, _, body)
        | ExprKind::Seq(_, body)
        | ExprKind::Shim(_, _, body) => derived(body),
        ExprKind::If(_, then, otherwise) => derived(then).or_else(|| derived(otherwise)),
        ExprKind::Match(_, arms) => arms.iter().find_map(|a| derived(&a.body)),
        ExprKind::IfLet(arm, _, otherwise) => derived(&arm.body).or_else(|| derived(otherwise)),
        _ => None,
    }
}

/// The declarations a name's type can be read from.
pub(crate) struct Decls<'a> {
    pub(crate) variants: &'a [ConcreteVariantDecl],
    pub(crate) structs: &'a [ConcreteStructDecl],
}

/// A node a pass built without a type, once its children have theirs: what it is follows from what
/// they are. A comparison is a `bool`, a literal one side of it takes the other side's type, a
/// string literal is a `str`, a field is the field's declared type, and a moved value is the value.
fn settle_built(e: &mut Expr, decls: &Decls<'_>) {
    let bool_ty = || Box::new(Type::Primitive(Primitive::Bool));
    match &mut e.kind {
        ExprKind::Str(_) if e.ty.is_none() => {
            e.ty = Some(Box::new(Type::Primitive(Primitive::Str)))
        }
        ExprKind::Field(object, field) if e.ty.is_none() => {
            if let Some(Type::Named(owner)) = object.ty.as_deref() {
                let declared = decls
                    .structs
                    .iter()
                    .find(|s| &s.name == owner)
                    .and_then(|s| s.fields.iter().find(|f| &f.name == field));
                if let Some(declared) = declared {
                    e.ty = Some(Box::new(declared.ty.widen()));
                }
            }
        }
        ExprKind::Call(Callee::Move, args, _) if e.ty.is_none() => {
            if let [only] = args.as_slice() {
                e.ty = only.ty.clone();
            }
        }
        ExprKind::Call(
            Callee::Equal
            | Callee::NotEqual
            | Callee::LessThan
            | Callee::GreaterThan
            | Callee::LessThanOrEqual
            | Callee::GreaterThanOrEqual,
            args,
            _,
        ) => {
            if e.ty.is_none() {
                e.ty = Some(bool_ty());
            }
            if let [a, b] = args.as_mut_slice() {
                let (first, second) = (a.ty.clone(), b.ty.clone());
                if a.ty.is_none() && matches!(a.kind, ExprKind::Num(_)) {
                    a.ty = second;
                }
                if b.ty.is_none() && matches!(b.kind, ExprKind::Num(_)) {
                    b.ty = first;
                }
            }
        }
        _ => {}
    }
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
