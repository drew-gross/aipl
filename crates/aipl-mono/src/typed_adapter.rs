//! Between the stamped `Expr` bodies of a [`MonoProgram`] and `TExpr<ConcreteTy>`.
//!
//! A pass that works on the typed tree runs through [`on_typed`]: each body is typed (every name
//! from its binder, see [`stamp_idents`]), converted, handed to the pass, and converted back with
//! the type of every value-producing node stamped. A node nothing types is a hole in the
//! stamping, and the conversion names it rather than guessing.
//!
//! What a node's type is, when it carries no stamp: a statement form (`Unit`, an assignment, a
//! loop, a `return`) is unit; a binding, a sequence is its body; an `if` or `match` is a branch's
//! type; a call is its callee's declared return (a user instance) or unit (the intrinsics that
//! answer nothing).
//!
//! Unit-typed nodes carry no stamp going back, which is the convention `infer` already follows.

use aipl_syntax::ast::{
    Callee, Expr, ExprKind, FieldInit, LambdaParam, MatchArm, Pattern, Type, WrittenNames,
};
use aipl_syntax::typed::{
    ConcreteTy, TExpr, TExprKind, TFieldInit, TLambdaParam, TMatchArm, TPattern, Ty, TyRepr,
};

use std::cell::RefCell;

use crate::{ConcreteFn, MonoProgram};

/// A name in scope: its type if it has one, and whether that type is a default.
type Binding = (String, Option<ConcreteTy>, bool);

/// What converting a body needs to know: the program, and the names in scope. A bare name carries
/// no stamp, so what it is comes from its binder.
struct Cx<'a> {
    program: &'a MonoProgram,
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

    fn lookup(&self, name: &str) -> Option<(Option<ConcreteTy>, bool)> {
        self.scope
            .borrow()
            .iter()
            .rev()
            .find(|(n, _, _)| n == name)
            .map(|(_, t, d)| (t.clone(), *d))
    }

    /// The names a pattern binds, typed from the scrutinee's type where it says.
    fn pattern_bindings(&self, pattern: &Pattern, scrutinee: &ConcreteTy) -> Vec<Binding> {
        let decls = crate::stamp_idents::Decls {
            variants: &self.program.variants,
            structs: &self.program.structs,
        };
        crate::stamp_idents::bindings_typed(pattern, Some(&scrutinee.to_type()), &decls)
            .into_iter()
            .map(|(n, t)| (n, t.and_then(|t| concrete(&t).ok()), true))
            .collect()
    }
}

/// `program` with `pass` applied to every function body as a typed tree.
pub fn on_typed(program: &MonoProgram, pass: impl Fn(&mut TExpr<ConcreteTy>)) -> MonoProgram {
    on_typed_where(program, |_| true, pass)
}

/// [`on_typed`] for a pass that needs to know which function it is in.
pub fn on_typed_named(
    program: &MonoProgram,
    pass: impl Fn(&str, &mut TExpr<ConcreteTy>),
) -> MonoProgram {
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| {
                let mut typed = to_typed(f, program)
                    .unwrap_or_else(|why| panic!("{}: cannot type the body: {why}", f.name));
                pass(&f.name, &mut typed);
                ConcreteFn {
                    body: from_typed(&typed),
                    ..f.clone()
                }
            })
            .collect(),
        ..program.clone()
    }
}

/// [`on_typed`] for the functions `covers` names; the others come back as they were.
pub fn on_typed_where(
    program: &MonoProgram,
    covers: impl Fn(&str) -> bool,
    pass: impl Fn(&mut TExpr<ConcreteTy>),
) -> MonoProgram {
    MonoProgram {
        fns: program
            .fns
            .iter()
            .map(|f| {
                if !covers(&f.name) {
                    return f.clone();
                }
                let mut typed = to_typed(f, program)
                    .unwrap_or_else(|why| panic!("{}: cannot type the body: {why}", f.name));
                pass(&mut typed);
                ConcreteFn {
                    body: from_typed(&typed),
                    ..f.clone()
                }
            })
            .collect(),
        ..program.clone()
    }
}

thread_local! {
    /// The stamps `ConcreteTy` cannot say (a concat string, a generic application), by the number
    /// their stand-in is named with.
    static OPAQUE: RefCell<Vec<Type>> = const { RefCell::new(Vec::new()) };
}

/// A stamp `ConcreteTy` cannot say travels through a typed tree as a named type of its own, so it
/// comes back as the stamp it was.
fn opaque(t: &Type) -> ConcreteTy {
    OPAQUE.with(|table| {
        let mut table = table.borrow_mut();
        let i = table.iter().position(|x| x == t).unwrap_or_else(|| {
            table.push(t.clone());
            table.len() - 1
        });
        ConcreteTy::Named(format!("$opaque#{i}"))
    })
}

/// `t` as a stamp: what it says, or the opaque type it stands in for.
fn stamp_of(t: &ConcreteTy) -> Type {
    if let ConcreteTy::Named(n) = t {
        if let Some(i) = n
            .strip_prefix("$opaque#")
            .and_then(|i| i.parse::<usize>().ok())
        {
            return OPAQUE.with(|table| table.borrow()[i].clone());
        }
    }
    t.to_type()
}

pub(crate) fn concrete(t: &Type) -> Result<ConcreteTy, String> {
    Ok(Ty::from_type(t)
        .and_then(|t| t.concretize(&|_| None))
        .unwrap_or_else(|| opaque(t)))
}

/// Whether a call to `callee` answers nothing.
fn answers_unit(callee: &Callee, program: &MonoProgram) -> bool {
    match callee {
        Callee::User(name) => program
            .fns
            .iter()
            .find(|f| &f.name == name)
            .is_some_and(|f| matches!(f.return_type(), aipl_syntax::ast::ConcreteType::Unit)),
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

/// `e` as a typed tree.
pub fn to_typed(f: &ConcreteFn, program: &MonoProgram) -> Result<TExpr<ConcreteTy>, String> {
    let cx = Cx {
        program,
        scope: RefCell::new(
            f.params
                .iter()
                .map(|p| Ok((p.name.clone(), Some(concrete(&p.ty.widen())?), false)))
                .collect::<Result<_, String>>()?,
        ),
    };
    typed(&f.body, &cx)
}

fn typed(e: &Expr, cx: &Cx<'_>) -> Result<TExpr<ConcreteTy>, String> {
    let program = cx.program;
    let mut annotation: Option<ConcreteTy> = None;
    let sub = |x: &Expr| typed(x, cx);
    let boxed = |x: &Expr| typed(x, cx).map(Box::new);
    let (kind, derived) = match &e.kind {
        // A literal nothing fitted is the integer a literal is.
        ExprKind::Num(n) => (
            TExprKind::Int(*n),
            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::I64)),
        ),
        ExprKind::Float(s) => (
            TExprKind::Float(s.clone()),
            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::F64)),
        ),
        ExprKind::Bool(b) => (
            TExprKind::Bool(*b),
            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Bool)),
        ),
        ExprKind::Str(s) => (
            TExprKind::Str(s.clone()),
            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Str)),
        ),
        ExprKind::Char(c) => (
            TExprKind::Char(*c),
            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Char)),
        ),
        ExprKind::Ident(n) => {
            let from_binder = if e.ty.is_none() { cx.lookup(n) } else { None };
            (
                TExprKind::Ident(n.clone()),
                from_binder.and_then(|(t, _)| t),
            )
        }
        ExprKind::Call(callee, args, method_style) => {
            let args: Vec<_> = args.iter().map(sub).collect::<Result<_, _>>()?;
            let derived = match (callee, args.as_slice()) {
                _ if answers_unit(callee, program) => Some(ConcreteTy::Unit),
                (Callee::Move, [only]) => Some(only.ty.clone()),
                (
                    Callee::Equal
                    | Callee::NotEqual
                    | Callee::LessThan
                    | Callee::GreaterThan
                    | Callee::LessThanOrEqual
                    | Callee::GreaterThanOrEqual,
                    _,
                ) => Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Bool)),
                // An instance answers what it declares.
                (Callee::User(name), _) => program
                    .fns
                    .iter()
                    .find(|f| &f.name == name && !f.is_mutating())
                    .and_then(|f| concrete(&f.return_type().widen()).ok()),
                (Callee::CharToStr, _) => {
                    Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Str))
                }
                // A call through a function-valued name answers what the function does.
                (Callee::Local(name), _) => match cx.lookup(name) {
                    Some((Some(ConcreteTy::Fn(_, ret)), _)) => Some(*ret),
                    _ => None,
                },
                // A builtin whose declared answer names no type variable.
                (other, _) => crate::builtin_sig(other.name())
                    .filter(|s| !s.is_mutating())
                    .and_then(|s| Ty::from_type(&s.return_type()))
                    .and_then(|t| t.concretize(&|_| None)),
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
            let (ty, _) = first_valued([&*t, &*o]);
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
            Some(ConcreteTy::Named(name.clone())),
        ),
        ExprKind::Field(x, f) => {
            let x = boxed(x)?;
            let declared = match &x.ty {
                ConcreteTy::Named(owner) => program
                    .structs
                    .iter()
                    .find(|s| &s.name == owner)
                    .and_then(|s| s.fields.iter().find(|d| &d.name == f))
                    .and_then(|d| concrete(&d.ty.widen()).ok()),
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
            let l = match (&l.kind, &l.ty) {
                (ExprKind::Ident(n), None) => {
                    let mut target =
                        TExpr::new(TExprKind::Ident(n.clone()), v.ty.clone(), l.span.clone());
                    target.name_span = l.name_span.clone();
                    target.derived = true;
                    Box::new(target)
                }
                _ => boxed(l)?,
            };
            (TExprKind::Assign(l, v, boxed(r)?), Some(ConcreteTy::Unit))
        }
        ExprKind::For(n, i, it, b) => {
            let it = boxed(it)?;
            let depth = cx.bind(
                std::iter::once((n.clone(), element_type(&it.ty), true)).chain(i.clone().map(
                    |i| {
                        (
                            i,
                            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::U64)),
                            true,
                        )
                    },
                )),
            );
            let b = boxed(b);
            cx.unbind(depth);
            (
                TExprKind::For(n.clone(), i.clone(), it, b?),
                Some(ConcreteTy::Unit),
            )
        }
        ExprKind::While(c, b) => (
            TExprKind::While(boxed(c)?, boxed(b)?),
            Some(ConcreteTy::Unit),
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
            let (ty, _) = first_valued(arms.iter().map(|a| &a.body));
            (TExprKind::Match(s, arms), Some(ty))
        }
        ExprKind::IfLet(arm, s, o) => {
            let s = boxed(s)?;
            let arm = Box::new(arm_to_typed(arm, &s.ty, cx)?);
            let o = boxed(o)?;
            let (ty, _) = first_valued([&arm.body, &*o]);
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
                ConcreteTy::Optional(t) => Some((**t).clone()),
                ConcreteTy::Result(ok, _) => Some((**ok).clone()),
                _ => None,
            };
            (TExprKind::Try(x), payload)
        }
        ExprKind::Unit => (TExprKind::Unit, Some(ConcreteTy::Unit)),
        ExprKind::Seq(a, b) => {
            let (a, b) = (boxed(a)?, boxed(b)?);
            let ty = b.ty.clone();
            (TExprKind::Seq(a, b), Some(ty))
        }
        ExprKind::Return(x) => (TExprKind::Return(boxed(x)?), Some(ConcreteTy::Unit)),
        ExprKind::Lambda(params, body) => {
            let from_stamp = match e.ty.as_deref() {
                Some(Type::Fn(ps, _)) => Some(ps.clone()),
                _ => None,
            };
            let params: Vec<TLambdaParam<ConcreteTy>> = params
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
                        ty: concrete(&ty)?,
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
    let ty = match (&e.ty, derived) {
        (Some(t), _) => concrete(t)?,
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
    out.derived = e.ty.is_none();
    out.annotation = annotation;
    Ok(out)
}

/// A binding's value and body, converted. A binding with a type of its own gives that type to its
/// value (`mut n: u64 = 0` is a `u64` literal), and says it had one.
#[allow(clippy::type_complexity)]
fn binding(
    name: &str,
    declared: &Option<Type>,
    value: &Expr,
    body: &Expr,
    cx: &Cx<'_>,
) -> Result<
    (
        Box<TExpr<ConcreteTy>>,
        Box<TExpr<ConcreteTy>>,
        Option<ConcreteTy>,
    ),
    String,
> {
    let mut value = typed(value, cx)?;
    let declared = declared.as_ref().map(concrete).transpose()?;
    // A literal nothing fitted takes the type the binding declares.
    if let (Some(t), true) = (&declared, value.derived) {
        value.ty = t.clone();
    }
    let depth = cx.bind([(
        name.to_string(),
        (value.ty != ConcreteTy::Unit).then(|| value.ty.clone()),
        value.derived,
    )]);
    let body = typed(body, cx);
    cx.unbind(depth);
    Ok((Box::new(value), Box::new(body?), declared))
}

/// What a `for` over a value of type `t` binds per iteration, when it says.
fn element_type(t: &ConcreteTy) -> Option<ConcreteTy> {
    match t {
        ConcreteTy::Array(inner) | ConcreteTy::Set(inner, _) => Some((**inner).clone()),
        ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Str) => {
            Some(ConcreteTy::Primitive(aipl_syntax::ast::Primitive::Char))
        }
        _ => None,
    }
}

/// The type of the first of `branches` that is not unit (unit when all are), and whether that
/// type is a default.
fn first_valued<'a>(
    branches: impl IntoIterator<Item = &'a TExpr<ConcreteTy>>,
) -> (ConcreteTy, bool) {
    branches
        .into_iter()
        .find(|b| b.ty != ConcreteTy::Unit)
        .map_or((ConcreteTy::Unit, false), |b| (b.ty.clone(), b.derived))
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
    scrutinee: &ConcreteTy,
    cx: &Cx<'_>,
) -> Result<TMatchArm<ConcreteTy>, String> {
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
/// the element's, which the pattern does not carry, so an untyped one is unit here and stays
/// unstamped going back.
fn pattern_to_typed(p: &Pattern, cx: &Cx<'_>) -> Result<TPattern<ConcreteTy>, String> {
    let element = |x: &Expr| match typed(x, cx) {
        Ok(t) => Ok(t),
        Err(_) if matches!(x.kind, ExprKind::Ident(_)) => {
            let mut t = TExpr::new(
                TExprKind::Ident(match &x.kind {
                    ExprKind::Ident(n) => n.clone(),
                    _ => unreachable!(),
                }),
                ConcreteTy::Unit,
                x.span.clone(),
            );
            t.name_span = x.name_span.clone();
            Ok(t)
        }
        Err(why) => Err(why),
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

/// `e` as an `Expr` again: every node of a value carries its type, a binding its value's.
pub fn from_typed(e: &TExpr<ConcreteTy>) -> Expr {
    use TExprKind as K;
    let sub = |x: &TExpr<ConcreteTy>| from_typed(x);
    let boxed = |x: &TExpr<ConcreteTy>| Box::new(from_typed(x));
    let kind = match &e.kind {
        K::Int(n) => ExprKind::Num(*n),
        K::Float(s) => ExprKind::Float(s.clone()),
        K::Bool(b) => ExprKind::Bool(*b),
        K::Str(s) => ExprKind::Str(s.clone()),
        K::Char(c) => ExprKind::Char(*c),
        K::Ident(n) => ExprKind::Ident(n.clone()),
        K::Call {
            callee,
            args,
            method_style,
            ..
        } => ExprKind::Call(
            callee.clone(),
            args.iter().map(sub).collect(),
            *method_style,
        ),
        K::Neg(x) => ExprKind::Neg(boxed(x)),
        K::If(c, t, o) => ExprKind::If(boxed(c), boxed(t), boxed(o)),
        K::Construct(name, inits) => ExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|i| FieldInit {
                    name: i.name.clone(),
                    value: sub(&i.value),
                })
                .collect(),
        ),
        K::Field(x, f) => ExprKind::Field(boxed(x), f.clone()),
        K::Let(n, v, b) => ExprKind::Let(n.clone(), annotation(e), boxed(v), boxed(b)),
        K::LetMut(n, v, b) => ExprKind::LetMut(n.clone(), annotation(e), boxed(v), boxed(b)),
        K::Assign(l, v, r) => ExprKind::Assign(boxed(l), boxed(v), boxed(r)),
        K::For(n, i, it, b) => ExprKind::For(n.clone(), i.clone(), boxed(it), boxed(b)),
        K::While(c, b) => ExprKind::While(boxed(c), boxed(b)),
        K::Shim(effect, bindings, body) => {
            ExprKind::Shim(effect.clone(), bindings.clone(), boxed(body))
        }
        K::None => ExprKind::None,
        K::Match(s, arms) => ExprKind::Match(boxed(s), arms.iter().map(arm_from_typed).collect()),
        K::IfLet(arm, s, o) => ExprKind::IfLet(Box::new(arm_from_typed(arm)), boxed(s), boxed(o)),
        K::ArrayLit(xs) => ExprKind::ArrayLit(xs.iter().map(sub).collect()),
        K::TupleLit(xs) => ExprKind::TupleLit(xs.iter().map(sub).collect()),
        K::SetLit(xs, order) => {
            ExprKind::SetLit(xs.iter().map(sub).collect(), order.to_set_order())
        }
        K::DictLit(pairs) => {
            ExprKind::DictLit(pairs.iter().map(|(k, v)| (sub(k), sub(v))).collect())
        }
        K::Index(a, i) => ExprKind::Index(boxed(a), boxed(i)),
        K::Slice(a, s, end) => ExprKind::Slice(boxed(a), boxed(s), end.as_ref().map(|x| boxed(x))),
        K::Try(x) => ExprKind::Try(boxed(x)),
        K::Unit => ExprKind::Unit,
        K::Seq(a, b) => ExprKind::Seq(boxed(a), boxed(b)),
        K::Return(x) => ExprKind::Return(boxed(x)),
        K::Lambda(params, body) => ExprKind::Lambda(
            params
                .iter()
                .map(|p| LambdaParam {
                    name: p.name.clone(),
                    ty: Some(p.ty.to_type()),
                    span: p.span.clone(),
                })
                .collect(),
            boxed(body),
        ),
    };
    let mut out = Expr::new(kind, e.span.clone());
    out.value_span = e.value_span.clone();
    out.name_span = e.name_span.clone();
    if e.ty != ConcreteTy::Unit && !e.derived {
        out.ty = Some(Box::new(stamp_of(&e.ty)));
    }
    out
}

/// What a binding was annotated with: its value's type, when it had one of its own.
fn annotation(binding: &TExpr<ConcreteTy>) -> Option<Type> {
    binding.annotation.as_ref().map(stamp_of)
}

fn arm_from_typed(arm: &TMatchArm<ConcreteTy>) -> MatchArm {
    MatchArm {
        pattern: pattern_from_typed(&arm.pattern),
        body: from_typed(&arm.body),
        span: arm.span.clone(),
        written: WrittenNames::default(),
    }
}

fn pattern_from_typed(p: &TPattern<ConcreteTy>) -> Pattern {
    match p {
        TPattern::Ctor {
            name,
            bindings,
            ignore_payload,
        } => Pattern::Ctor {
            name: name.clone(),
            bindings: bindings.clone(),
            ignore_payload: *ignore_payload,
        },
        TPattern::Str(s) => Pattern::Str(s.clone()),
        TPattern::Char(c) => Pattern::Char(*c),
        TPattern::ArrayNested(ps) => {
            Pattern::ArrayNested(ps.iter().map(pattern_from_typed).collect())
        }
        TPattern::Array(xs) => Pattern::Array(xs.iter().map(from_typed).collect()),
        TPattern::ArrayRest(xs) => Pattern::ArrayRest(xs.iter().map(from_typed).collect()),
        TPattern::Wildcard => Pattern::Wildcard,
        TPattern::Tuple(ps) => Pattern::Tuple(ps.iter().map(pattern_from_typed).collect()),
        TPattern::Nested { name, args } => Pattern::Nested {
            name: name.clone(),
            args: args.iter().map(pattern_from_typed).collect(),
        },
        TPattern::Int(n) => Pattern::Int(*n),
        TPattern::Bind(n) => Pattern::Bind(n.clone()),
    }
}
