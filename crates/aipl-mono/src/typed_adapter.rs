//! From the stamped `Expr` body monomorphization infers to the typed tree everything after it reads.
//!
//! [`to_typed`] runs once per instance, at monomorphization's output. Every node of the result
//! carries its type: the stamp monomorphization left on it, or, for a node a pass built without
//! one, what its form says (a name is what its binder is, a statement form is unit, a binding or a
//! sequence is its body, an `if` or `match` is a branch's type, a call is what its callee declares).
//! A node nothing types is a hole in the stamping, and the conversion names it rather than guessing.
//!
//! A stamp the concrete representation cannot say (a generic application left as written) counts
//! as no stamp: the node's type is then derived like any other's.

use std::cell::RefCell;
use std::collections::HashMap;

use aipl_syntax::ast::{
    Callee, ConcreteStructDecl, ConcreteType, ConcreteVariantDecl, Expr, ExprKind, MatchArm,
    Pattern, Primitive, Type,
};
use aipl_syntax::typed::{TExpr, TExprKind, TFieldInit, TLambdaParam, TMatchArm, TPattern, TyRepr};

use crate::binder_types::{bindings_typed, Decls as BinderDecls};
use crate::ConcreteFn;

type Typed = TExpr<ConcreteType>;

/// A body as monomorphization hands it on: the typed tree when the instance was typed from the
/// pinned program, the written tree with each expression stamped otherwise.
#[derive(Clone)]
pub(crate) enum Inferred {
    Written(Expr),
    Typed(Typed),
}

/// The declarations a body's types are read from: the program's structs and variants, and for each
/// instance whether it mutates its receiver and what it declares it returns.
pub(crate) struct Decls<'a> {
    pub structs: &'a [ConcreteStructDecl],
    pub variants: &'a [ConcreteVariantDecl],
    pub fns: HashMap<&'a str, (bool, ConcreteType)>,
}

impl<'a> Decls<'a> {
    pub(crate) fn of(
        structs: &'a [ConcreteStructDecl],
        variants: &'a [ConcreteVariantDecl],
        fns: &'a [ConcreteFn<Inferred>],
    ) -> Decls<'a> {
        Decls {
            structs,
            variants,
            fns: fns
                .iter()
                .map(|f| (f.name.as_str(), (f.is_mutating(), f.return_type())))
                .collect(),
        }
    }
}

/// A name in scope: its type if it has one, and whether that type was derived rather than stamped.
type Binding = (String, Option<ConcreteType>, bool);

/// What converting a body needs to know: the declarations, and the names in scope. A bare name
/// carries no stamp, so what it is comes from its binder.
struct Cx<'a> {
    decls: &'a Decls<'a>,
    scope: RefCell<Vec<Binding>>,
}

impl Cx<'_> {
    fn bind(&self, names: impl IntoIterator<Item = Binding>) -> usize {
        let mut scope = self.scope.borrow_mut();
        let depth = scope.len();
        scope.extend(names);
        depth
    }

    fn unbind(&self, depth: usize) {
        self.scope.borrow_mut().truncate(depth);
    }

    fn lookup(&self, name: &str) -> Option<(Option<ConcreteType>, bool)> {
        self.scope
            .borrow()
            .iter()
            .rev()
            .find(|(n, _, _)| n == name)
            .map(|(_, t, d)| (t.clone(), *d))
    }

    /// The names a pattern binds, typed from the scrutinee's type where it says.
    fn pattern_bindings(&self, pattern: &Pattern, scrutinee: &ConcreteType) -> Vec<Binding> {
        let decls = BinderDecls {
            variants: self.decls.variants,
        };
        bindings_typed(pattern, Some(&scrutinee.widen()), &decls)
            .into_iter()
            .map(|(n, t)| (n, t.and_then(|t| concrete(&t)), true))
            .collect()
    }
}

/// The concrete form of a stamp, or `None` for one the representation cannot say.
pub(crate) fn concrete(t: &Type) -> Option<ConcreteType> {
    t.to_concrete()
}

/// Whether a call to `callee` answers nothing.
fn answers_unit(callee: &Callee, decls: &Decls<'_>) -> bool {
    match callee {
        Callee::User(name) => decls
            .fns
            .get(name.as_str())
            .is_some_and(|(_, ret)| *ret == ConcreteType::Unit),
        Callee::ShimCellSet
        | Callee::RandomSeed
        | Callee::RandomSeedFromClock
        | Callee::MapSet
        | Callee::FilterKeep
        | Callee::FilterDrop
        | Callee::FilterTruncate => true,
        // A builtin declared to answer nothing (`print`, the test hooks).
        other => crate::builtin_sig(other.name())
            .is_some_and(|s| !s.is_mutating() && matches!(s.return_type(), Type::Unit)),
    }
}

/// `f` with its body as a typed tree, or why a node of it cannot be typed.
pub(crate) fn to_typed(f: &ConcreteFn<Inferred>, decls: &Decls<'_>) -> Result<ConcreteFn, String> {
    let cx = Cx {
        decls,
        scope: RefCell::new(
            f.params
                .iter()
                .map(|p| (p.name.clone(), Some(p.ty.clone()), false))
                .collect(),
        ),
    };
    Ok(ConcreteFn {
        name: f.name.clone(),
        params: f.params.clone(),
        effects: f.effects.clone(),
        return_ty: f.return_ty.clone(),
        body: match &f.body {
            Inferred::Written(body) => typed(body, &cx)?,
            Inferred::Typed(body) => body.clone(),
        },
    })
}

fn typed(e: &Expr, cx: &Cx<'_>) -> Result<Typed, String> {
    let decls = cx.decls;
    // A stamp the concrete representation cannot say (a generic application left as written)
    // counts as none.
    let stamped = e.ty.as_deref().and_then(concrete);
    let mut annotation: Option<ConcreteType> = None;
    let sub = |x: &Expr| typed(x, cx);
    let boxed = |x: &Expr| typed(x, cx).map(Box::new);
    let primitive = |p: Primitive| Some(ConcreteType::Primitive(p));
    let (kind, derived) = match &e.kind {
        // A literal nothing fitted is the integer a literal is.
        ExprKind::Num(n) => (TExprKind::Int(*n), primitive(Primitive::I64)),
        ExprKind::Float(s) => (TExprKind::Float(s.clone()), primitive(Primitive::F64)),
        ExprKind::Bool(b) => (TExprKind::Bool(*b), primitive(Primitive::Bool)),
        ExprKind::Str(s) => (TExprKind::Str(s.clone()), primitive(Primitive::Str)),
        ExprKind::Char(c) => (TExprKind::Char(*c), primitive(Primitive::Char)),
        ExprKind::Ident(n) => {
            let from_binder = if stamped.is_none() {
                cx.lookup(n)
            } else {
                None
            };
            (
                TExprKind::Ident(n.clone()),
                from_binder.and_then(|(t, _)| t),
            )
        }
        ExprKind::Call(callee, args, method_style) => {
            let args: Vec<_> = args.iter().map(sub).collect::<Result<_, _>>()?;
            let derived = match (callee, args.as_slice()) {
                _ if answers_unit(callee, decls) => Some(ConcreteType::Unit),
                (Callee::Move, [only]) => Some(only.ty.clone()),
                (
                    Callee::Equal
                    | Callee::NotEqual
                    | Callee::LessThan
                    | Callee::GreaterThan
                    | Callee::LessThanOrEqual
                    | Callee::GreaterThanOrEqual,
                    _,
                ) => primitive(Primitive::Bool),
                // An instance answers what it declares.
                (Callee::User(name), _) => {
                    decls.fns.get(name.as_str()).and_then(|(mutating, ret)| {
                        // A mutating method is declared void; a call to it yields its receiver.
                        match (mutating, args.first()) {
                            (true, Some(receiver)) => Some(receiver.ty.clone()),
                            (true, None) => None,
                            (false, _) => Some(ret.clone()),
                        }
                    })
                }
                (Callee::CharToStr, _) => primitive(Primitive::Str),
                // A call through a function-valued name answers what the function does.
                (Callee::Local(name), _) => match cx.lookup(name) {
                    Some((Some(ConcreteType::Fn(_, ret)), _)) => Some(*ret),
                    _ => None,
                },
                // A builtin whose declared answer names no type variable.
                (other, _) => match crate::builtin_sig(other.name()) {
                    Some(sig) if sig.is_mutating() => args.first().map(|r| r.ty.clone()),
                    Some(sig) => concrete(&sig.return_type()),
                    None => None,
                },
            };
            (
                TExprKind::Call {
                    callee: callee.clone(),
                    type_args: Vec::new(),
                    args,
                    method_style: *method_style,
                },
                derived,
            )
        }
        ExprKind::Neg(x) => (TExprKind::Neg(boxed(x)?), None),
        ExprKind::If(c, t, o) => {
            let (c, t, o) = (boxed(c)?, boxed(t)?, boxed(o)?);
            let ty = first_valued([&*t, &*o]);
            (TExprKind::If(c, t, o), Some(ty))
        }
        ExprKind::Construct(name, inits) => (
            TExprKind::Construct(
                name.clone(),
                inits
                    .iter()
                    .map(|i| {
                        Ok(TFieldInit {
                            name: i.name.clone(),
                            value: sub(&i.value)?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            ),
            Some(ConcreteType::Named(name.clone())),
        ),
        ExprKind::Field(x, f) => {
            let x = boxed(x)?;
            let declared = match &x.ty {
                ConcreteType::Named(owner) => decls
                    .structs
                    .iter()
                    .find(|s| &s.name == owner)
                    .and_then(|s| s.fields.iter().find(|d| &d.name == f))
                    .map(|d| d.ty.clone()),
                _ => None,
            };
            (TExprKind::Field(x, f.clone()), declared)
        }
        ExprKind::Let(n, declared, v, b) => {
            let (v, b, declared_ty) = binding(n, declared, v, b, cx)?;
            let ty = b.ty.clone();
            annotation = declared_ty;
            (TExprKind::Let(n.clone(), v, b), Some(ty))
        }
        ExprKind::LetMut(n, declared, v, b) => {
            let (v, b, declared_ty) = binding(n, declared, v, b, cx)?;
            let ty = b.ty.clone();
            annotation = declared_ty;
            (TExprKind::LetMut(n.clone(), v, b), Some(ty))
        }
        ExprKind::Assign(l, v, r) => {
            let v = boxed(v)?;
            // The target is a bare name the stamping walk does not reach; the variable has the
            // type of what is stored in it.
            let l = match (&l.kind, l.ty.as_deref().and_then(concrete)) {
                (ExprKind::Ident(n), None) => {
                    let mut target =
                        TExpr::new(TExprKind::Ident(n.clone()), v.ty.clone(), l.span.clone());
                    target.name_span = l.name_span.clone();
                    target.derived = true;
                    Box::new(target)
                }
                _ => boxed(l)?,
            };
            (TExprKind::Assign(l, v, boxed(r)?), Some(ConcreteType::Unit))
        }
        ExprKind::For(n, i, it, b) => {
            let it = boxed(it)?;
            let depth = cx.bind(
                std::iter::once((n.clone(), element_type(&it.ty), true))
                    .chain(i.clone().map(|i| (i, primitive(Primitive::U64), true))),
            );
            let b = boxed(b);
            cx.unbind(depth);
            (
                TExprKind::For(n.clone(), i.clone(), it, b?),
                Some(ConcreteType::Unit),
            )
        }
        ExprKind::While(c, b) => (
            TExprKind::While(boxed(c)?, boxed(b)?),
            Some(ConcreteType::Unit),
        ),
        ExprKind::Shim(effect, bindings, body) => {
            let body = boxed(body)?;
            let ty = body.ty.clone();
            (
                TExprKind::Shim(effect.clone(), bindings.clone(), body),
                Some(ty),
            )
        }
        ExprKind::None => (TExprKind::None, None),
        ExprKind::Match(s, arms) => {
            let s = boxed(s)?;
            let arms = arms
                .iter()
                .map(|a| arm_to_typed(a, &s.ty, cx))
                .collect::<Result<Vec<_>, _>>()?;
            let ty = first_valued(arms.iter().map(|a| &a.body));
            (TExprKind::Match(s, arms), Some(ty))
        }
        ExprKind::IfLet(arm, s, o) => {
            let s = boxed(s)?;
            let arm = Box::new(arm_to_typed(arm, &s.ty, cx)?);
            let o = boxed(o)?;
            let ty = first_valued([&arm.body, &*o]);
            (TExprKind::IfLet(arm, s, o), Some(ty))
        }
        ExprKind::ArrayLit(xs) => (
            TExprKind::ArrayLit(xs.iter().map(sub).collect::<Result<_, _>>()?),
            None,
        ),
        ExprKind::TupleLit(xs) => (
            TExprKind::TupleLit(xs.iter().map(sub).collect::<Result<_, _>>()?),
            None,
        ),
        ExprKind::SetLit(xs, order) => (
            TExprKind::SetLit(
                xs.iter().map(sub).collect::<Result<_, _>>()?,
                aipl_syntax::typed::PinnedOrder::of(*order)
                    .ok_or("a set literal whose order is not settled")?,
            ),
            None,
        ),
        ExprKind::DictLit(pairs) => (
            TExprKind::DictLit(
                pairs
                    .iter()
                    .map(|(k, v)| Ok((sub(k)?, sub(v)?)))
                    .collect::<Result<_, String>>()?,
            ),
            None,
        ),
        ExprKind::Index(a, i) => (TExprKind::Index(boxed(a)?, boxed(i)?), None),
        ExprKind::Slice(a, s, end) => (
            TExprKind::Slice(
                boxed(a)?,
                boxed(s)?,
                end.as_ref().map(|x| boxed(x)).transpose()?,
            ),
            None,
        ),
        ExprKind::Try(x) => {
            let x = boxed(x)?;
            let payload = match &x.ty {
                ConcreteType::Optional(t) => Some((**t).clone()),
                ConcreteType::Result(ok, _) => Some((**ok).clone()),
                _ => None,
            };
            (TExprKind::Try(x), payload)
        }
        ExprKind::Unit => (TExprKind::Unit, Some(ConcreteType::Unit)),
        ExprKind::Seq(a, b) => {
            let (a, b) = (boxed(a)?, boxed(b)?);
            let ty = b.ty.clone();
            (TExprKind::Seq(a, b), Some(ty))
        }
        ExprKind::Return(x) => (TExprKind::Return(boxed(x)?), Some(ConcreteType::Unit)),
        ExprKind::Lambda(params, body) => {
            let from_stamp = match e.ty.as_deref() {
                Some(Type::Fn(ps, _)) => Some(ps.clone()),
                _ => None,
            };
            let params: Vec<TLambdaParam<ConcreteType>> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let ty = match (&p.ty, &from_stamp) {
                        (Some(t), _) => t.clone(),
                        (None, Some(ps)) if i < ps.len() => ps[i].clone(),
                        _ => return Err(format!("lambda parameter {:?} has no type", p.name)),
                    };
                    Ok(TLambdaParam {
                        name: p.name.clone(),
                        ty: concrete(&ty)
                            .ok_or_else(|| format!("lambda parameter {:?} is {ty:?}", p.name))?,
                        span: p.span.clone(),
                    })
                })
                .collect::<Result<_, String>>()?;
            let depth = cx.bind(
                params
                    .iter()
                    .map(|p| (p.name.clone(), Some(p.ty.clone()), false)),
            );
            let body = boxed(body);
            cx.unbind(depth);
            (TExprKind::Lambda(params, body?), None)
        }
        other => return Err(format!("{other:?} is not a post-monomorphization form")),
    };
    let ty = match (stamped.clone(), derived) {
        (Some(t), _) => t,
        (None, Some(d)) => d,
        (None, None) => {
            return Err(format!(
                "nothing types the {} at {:?}",
                kind_name(&kind),
                e.span
            ))
        }
    };
    let mut out = TExpr::new(kind, ty, e.span.clone());
    out.value_span = e.value_span.clone();
    out.name_span = e.name_span.clone();
    out.derived = stamped.is_none();
    out.annotation = annotation;
    Ok(out)
}

/// A binding's value and body, converted, with the type the binding declares. A literal nothing
/// fitted takes that type (`mut n: u64 = 0` is a `u64` literal).
#[allow(clippy::type_complexity)]
fn binding(
    name: &str,
    declared: &Option<Type>,
    value: &Expr,
    body: &Expr,
    cx: &Cx<'_>,
) -> Result<(Box<Typed>, Box<Typed>, Option<ConcreteType>), String> {
    let mut value = typed(value, cx)?;
    let declared = declared.as_ref().and_then(concrete);
    if let (Some(t), true) = (&declared, value.derived) {
        value.ty = t.clone();
    }
    let depth = cx.bind([(
        name.to_string(),
        (value.ty != ConcreteType::Unit).then(|| value.ty.clone()),
        value.derived,
    )]);
    let body = typed(body, cx);
    cx.unbind(depth);
    Ok((Box::new(value), Box::new(body?), declared))
}

/// What a `for` over a value of type `t` binds per iteration, when it says.
fn element_type(t: &ConcreteType) -> Option<ConcreteType> {
    match t {
        ConcreteType::Array(inner) | ConcreteType::Set(inner, _) => Some((**inner).clone()),
        ConcreteType::Primitive(Primitive::Str) | ConcreteType::ConcatStr => {
            Some(ConcreteType::Primitive(Primitive::Char))
        }
        _ => None,
    }
}

/// The type of the first of `branches` that is not unit (unit when all are).
fn first_valued<'a>(branches: impl IntoIterator<Item = &'a Typed>) -> ConcreteType {
    branches
        .into_iter()
        .find(|b| b.ty != ConcreteType::Unit)
        .map_or(ConcreteType::Unit, |b| b.ty.clone())
}

fn kind_name<T: TyRepr>(kind: &TExprKind<T>) -> String {
    match kind {
        TExprKind::Call { callee, .. } => format!("call to {}", callee.name()),
        TExprKind::Ident(n) => format!("name {n:?}"),
        other => format!("{:?}", std::mem::discriminant(other)),
    }
}

fn arm_to_typed(
    arm: &MatchArm,
    scrutinee: &ConcreteType,
    cx: &Cx<'_>,
) -> Result<TMatchArm<ConcreteType>, String> {
    let pattern = pattern_to_typed(&arm.pattern, cx)?;
    let depth = cx.bind(cx.pattern_bindings(&arm.pattern, scrutinee));
    let body = typed(&arm.body, cx);
    cx.unbind(depth);
    Ok(TMatchArm {
        pattern,
        body: body?,
        span: arm.span.clone(),
    })
}

/// The expressions in an array pattern are binders and literals, not values: a binder's type is
/// the element's, which the pattern does not carry, so an untyped one is unit here.
fn pattern_to_typed(p: &Pattern, cx: &Cx<'_>) -> Result<TPattern<ConcreteType>, String> {
    let element = |x: &Expr| match (&x.kind, typed(x, cx)) {
        (_, Ok(t)) => Ok(t),
        (ExprKind::Ident(n), Err(_)) => {
            let mut t = TExpr::new(
                TExprKind::Ident(n.clone()),
                ConcreteType::Unit,
                x.span.clone(),
            );
            t.name_span = x.name_span.clone();
            t.derived = true;
            Ok(t)
        }
        (_, Err(why)) => Err(why),
    };
    let nested = |ps: &[Pattern]| {
        ps.iter()
            .map(|p| pattern_to_typed(p, cx))
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match p {
        Pattern::Ctor {
            name,
            bindings,
            ignore_payload,
        } => TPattern::Ctor {
            name: name.clone(),
            bindings: bindings.clone(),
            ignore_payload: *ignore_payload,
        },
        Pattern::Str(s) => TPattern::Str(s.clone()),
        Pattern::Char(c) => TPattern::Char(*c),
        Pattern::ArrayNested(ps) => TPattern::ArrayNested(nested(ps)?),
        Pattern::Array(xs) => TPattern::Array(xs.iter().map(element).collect::<Result<_, _>>()?),
        Pattern::ArrayRest(xs) => {
            TPattern::ArrayRest(xs.iter().map(element).collect::<Result<_, _>>()?)
        }
        Pattern::Wildcard => TPattern::Wildcard,
        Pattern::Tuple(ps) => TPattern::Tuple(nested(ps)?),
        Pattern::Nested { name, args } => TPattern::Nested {
            name: name.clone(),
            args: nested(args)?,
        },
        Pattern::Int(n) => TPattern::Int(*n),
        Pattern::Bind(n) => TPattern::Bind(n.clone()),
    })
}
