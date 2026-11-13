//! The pinned IR: the program after type checking, in which every expression carries its type and
//! no type is provisional.
//!
//! [`ast::Expr`](crate::ast::Expr) is what the parser, loader, formatter and tools share, and it
//! cannot say "this is fully typed": its [`Type`] has placeholders (`Unknown`, `Any`, ...), an
//! integer literal is a bare `Num` until a context fits it, and only a few expressions record a
//! type at all. A pass over it has to guess what an expression is, and guesses are where
//! optimizations stop.
//!
//! This module is the type a pass can rely on instead:
//!
//! - **Every node has a type** ([`TExpr::ty`]), not an `Option` of one.
//! - **The type language cannot say "unknown".** [`Ty`] and [`ConcreteTy`] have no `Unknown`,
//!   `Any`, `ConcatStr`, `Without`, `Tuple` or
//!   `Generic`: the first three are placeholders, the rest are already lowered or erased by the time a
//!   program is pinned. A set's order is a [`PinnedOrder`], which cannot be the checker's `Context`
//!   placeholder.
//! - **A literal has its width.** [`TExprKind::Int`] and [`TExprKind::Float`] are literals whose
//!   `ty` is a concrete numeric primitive; there is no flexible literal.
//! - **A generic call records how it was instantiated** ([`TExprKind::Call::type_args`]), so
//!   monomorphization substitutes rather than re-infers.
//! - **The forms that cannot exist after checking are not variants**: keyword arguments, array
//!   spreads, written type arguments and tuple literals are gone, so no pass has to write the
//!   `unreachable!` arm for them.
//!
//! The tree is generic over its type representation, [`TyRepr`]: [`Ty`] before monomorphization (it
//! may name a rigid type variable of the enclosing generic function) and [`ConcreteTy`] after (it
//! cannot). Substituting a function's type variables is then a map of one into the other
//! ([`TExpr::concretize`]), which the compiler checks is total.
//!
//! [`TExpr::erase`] goes back to the untyped AST, dropping the types. It exists to compare against
//! the pipeline this one replaces while it is being replaced, and for diagnostics that print source.

use crate::ast::{
    Arity, Bound, Callee, CaseParam, Expr, ExprKind, FieldDecl, FieldInit, FileSource, Function,
    Item, LambdaParam, MatchArm, NameSpan, Param, Pattern, Primitive, Program, SetOrder, Signature,
    StructDecl, Type, TypeParam, VariantCase, VariantDecl,
};
use crate::{Span, SpanStr};

/// What a typed tree needs of its type representation.
pub trait TyRepr: Clone + std::fmt::Debug + PartialEq + Eq {
    /// The same type in the untyped AST's language, for [`TExpr::erase`] and for printing.
    fn to_type(&self) -> Type;
}

/// The order a set's walk promises. [`SetOrder`] without `Context`, the checker's placeholder for
/// "the use site decides": by the time a program is pinned the use site has decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinnedOrder {
    Unordered,
    Desc,
    Asc,
}

impl PinnedOrder {
    /// The pinned order of `order`, or `None` for the placeholder.
    pub fn of(order: SetOrder) -> Option<PinnedOrder> {
        match order {
            SetOrder::Unordered => Some(PinnedOrder::Unordered),
            SetOrder::Desc => Some(PinnedOrder::Desc),
            SetOrder::Asc => Some(PinnedOrder::Asc),
            SetOrder::Context => None,
        }
    }

    pub fn to_set_order(self) -> SetOrder {
        match self {
            PinnedOrder::Unordered => SetOrder::Unordered,
            PinnedOrder::Desc => SetOrder::Desc,
            PinnedOrder::Asc => SetOrder::Asc,
        }
    }
}

/// A pinned type before monomorphization: fully known, but possibly in terms of the rigid type
/// variables of the generic function it sits in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    Unit,
    /// What an expression that never produces a value has when nothing around it asks for a type
    /// (`return x;` as a statement). Where context does ask — a branch of an `if` whose other
    /// branch yields a number — the expression has that type instead.
    Never,
    Primitive(Primitive),
    /// A struct or variant — always a monomorphic one: generic applications are lowered to
    /// synthetic named types before type checking.
    Named(String),
    /// `Case<V>`, one case of the variant `V` without its payload.
    Case(Box<Ty>),
    /// A rigid type variable: the `T` of the enclosing `fn f<T: any>(..)`. It stands for one
    /// unknown type, the same at every use in the body — not a placeholder to be filled in.
    Var(String),
    /// A generic struct or variant applied to arguments, at least one of which mentions a type
    /// variable (`Box<T>` inside a generic function). An application whose arguments are all
    /// concrete is the instance it names (`Named("Box$i64")`), as after lowering, so one type has
    /// one spelling; and [`ConcreteTy`] has no such case.
    Generic(String, Vec<Ty>),
    Optional(Box<Ty>),
    Array(Box<Ty>),
    Set(Box<Ty>, PinnedOrder),
    Dict(Box<Ty>, Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    /// A lambda's or function value's type; compile-time only.
    Fn(Vec<Ty>, Box<Ty>),
}

/// A pinned type after monomorphization: [`Ty`] without [`Ty::Var`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConcreteTy {
    Unit,
    /// See [`Ty::Never`].
    Never,
    Primitive(Primitive),
    Named(String),
    Case(Box<ConcreteTy>),
    Optional(Box<ConcreteTy>),
    Array(Box<ConcreteTy>),
    Set(Box<ConcreteTy>, PinnedOrder),
    Dict(Box<ConcreteTy>, Box<ConcreteTy>),
    Result(Box<ConcreteTy>, Box<ConcreteTy>),
    Fn(Vec<ConcreteTy>, Box<ConcreteTy>),
}

impl Ty {
    /// The pinned form of `t`, or `None` when `t` mentions anything a pinned type cannot say: a
    /// placeholder, an unlowered application, a refinement, or a `Context`-ordered set.
    pub fn from_type(t: &Type) -> Option<Ty> {
        let boxed = |t: &Type| Ty::from_type(t).map(Box::new);
        Some(match t {
            Type::Unit => Ty::Unit,
            Type::Primitive(p) => Ty::Primitive(*p),
            Type::Named(n) => Ty::Named(n.clone()),
            Type::Case(v) => Ty::Case(boxed(v)?),
            Type::TypeVar(v) => Ty::Var(v.clone()),
            Type::Generic(name, args) => Ty::Generic(
                name.clone(),
                args.iter().map(Ty::from_type).collect::<Option<Vec<_>>>()?,
            ),
            Type::Optional(t) => Ty::Optional(boxed(t)?),
            Type::Array(t) => Ty::Array(boxed(t)?),
            Type::Set(t, order) => Ty::Set(boxed(t)?, PinnedOrder::of(*order)?),
            Type::Dict(k, v) => Ty::Dict(boxed(k)?, boxed(v)?),
            Type::Result(ok, err) => Ty::Result(boxed(ok)?, boxed(err)?),
            Type::Fn(params, ret) => Ty::Fn(
                params
                    .iter()
                    .map(Ty::from_type)
                    .collect::<Option<Vec<_>>>()?,
                boxed(ret)?,
            ),
            Type::Tuple(_) | Type::Any | Type::Unknown | Type::ConcatStr | Type::Without(..) => {
                return None
            }
        })
    }

    /// Whether the type names a type variable anywhere inside it.
    pub fn mentions_var(&self) -> bool {
        match self {
            Ty::Var(_) => true,
            Ty::Generic(_, args) => args.iter().any(Ty::mentions_var),
            Ty::Unit | Ty::Never | Ty::Primitive(_) | Ty::Named(_) => false,
            Ty::Case(t) | Ty::Optional(t) | Ty::Array(t) | Ty::Set(t, _) => t.mentions_var(),
            Ty::Dict(a, b) | Ty::Result(a, b) => a.mentions_var() || b.mentions_var(),
            Ty::Fn(params, ret) => params.iter().any(Ty::mentions_var) || ret.mentions_var(),
        }
    }

    /// This type with each type variable replaced by what `subst` binds it to, or `None` if `subst`
    /// leaves one unbound — the totality that makes a concrete tree concrete.
    pub fn concretize(&self, subst: &dyn Fn(&str) -> Option<ConcreteTy>) -> Option<ConcreteTy> {
        self.concretize_with(subst, &|_, _| None)
    }

    /// [`Ty::concretize`], also naming the instance a generic application with concrete arguments
    /// is (`instance(base, args)`), or `None` if there is no such instance.
    pub fn concretize_with(
        &self,
        subst: &dyn Fn(&str) -> Option<ConcreteTy>,
        instance: &dyn Fn(&str, &[ConcreteTy]) -> Option<String>,
    ) -> Option<ConcreteTy> {
        let boxed = |t: &Ty| t.concretize_with(subst, instance).map(Box::new);
        Some(match self {
            Ty::Unit => ConcreteTy::Unit,
            Ty::Never => ConcreteTy::Never,
            Ty::Primitive(p) => ConcreteTy::Primitive(*p),
            Ty::Named(n) => ConcreteTy::Named(n.clone()),
            Ty::Case(t) => ConcreteTy::Case(boxed(t)?),
            Ty::Var(v) => subst(v)?,
            Ty::Generic(base, args) => {
                let args = args
                    .iter()
                    .map(|a| a.concretize_with(subst, instance))
                    .collect::<Option<Vec<_>>>()?;
                ConcreteTy::Named(instance(base, &args)?)
            }
            Ty::Optional(t) => ConcreteTy::Optional(boxed(t)?),
            Ty::Array(t) => ConcreteTy::Array(boxed(t)?),
            Ty::Set(t, order) => ConcreteTy::Set(boxed(t)?, *order),
            Ty::Dict(k, v) => ConcreteTy::Dict(boxed(k)?, boxed(v)?),
            Ty::Result(ok, err) => ConcreteTy::Result(boxed(ok)?, boxed(err)?),
            Ty::Fn(params, ret) => ConcreteTy::Fn(
                params
                    .iter()
                    .map(|p| p.concretize_with(subst, instance))
                    .collect::<Option<Vec<_>>>()?,
                boxed(ret)?,
            ),
        })
    }
}

impl ConcreteTy {
    /// This type as a [`Ty`], for the places a concrete type is used where a pre-mono one is wanted.
    pub fn to_ty(&self) -> Ty {
        let boxed = |t: &ConcreteTy| Box::new(t.to_ty());
        match self {
            ConcreteTy::Unit => Ty::Unit,
            ConcreteTy::Never => Ty::Never,
            ConcreteTy::Primitive(p) => Ty::Primitive(*p),
            ConcreteTy::Named(n) => Ty::Named(n.clone()),
            ConcreteTy::Case(t) => Ty::Case(boxed(t)),
            ConcreteTy::Optional(t) => Ty::Optional(boxed(t)),
            ConcreteTy::Array(t) => Ty::Array(boxed(t)),
            ConcreteTy::Set(t, order) => Ty::Set(boxed(t), *order),
            ConcreteTy::Dict(k, v) => Ty::Dict(boxed(k), boxed(v)),
            ConcreteTy::Result(ok, err) => Ty::Result(boxed(ok), boxed(err)),
            ConcreteTy::Fn(params, ret) => {
                Ty::Fn(params.iter().map(ConcreteTy::to_ty).collect(), boxed(ret))
            }
        }
    }
}

impl TyRepr for Ty {
    fn to_type(&self) -> Type {
        let boxed = |t: &Ty| Box::new(t.to_type());
        match self {
            // The written language has no type for it; a value that never arrives is as good as
            // unit wherever one is looked for.
            Ty::Unit | Ty::Never => Type::Unit,
            Ty::Primitive(p) => Type::Primitive(*p),
            Ty::Named(n) => Type::Named(n.clone()),
            Ty::Case(t) => Type::Case(boxed(t)),
            Ty::Var(v) => Type::TypeVar(v.clone()),
            Ty::Generic(name, args) => {
                Type::Generic(name.clone(), args.iter().map(Ty::to_type).collect())
            }
            Ty::Optional(t) => Type::Optional(boxed(t)),
            Ty::Array(t) => Type::Array(boxed(t)),
            Ty::Set(t, order) => Type::Set(boxed(t), order.to_set_order()),
            Ty::Dict(k, v) => Type::Dict(boxed(k), boxed(v)),
            Ty::Result(ok, err) => Type::Result(boxed(ok), boxed(err)),
            Ty::Fn(params, ret) => Type::Fn(params.iter().map(Ty::to_type).collect(), boxed(ret)),
        }
    }
}

impl TyRepr for ConcreteTy {
    fn to_type(&self) -> Type {
        self.to_ty().to_type()
    }
}

/// A typed expression: what [`Expr`] is, with its type always present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TExpr<T: TyRepr = Ty> {
    pub kind: TExprKind<T>,
    /// The type of the value this expression produces. Never provisional.
    pub ty: T,
    pub span: Span,
    /// See [`Expr::value_span`].
    pub value_span: Option<Box<Span>>,
    /// See [`Expr::name_span`].
    pub name_span: Option<Box<Span>>,
    /// Whether `ty` is, or holds, a generic application (`Rule<K>`) one of whose arguments nothing in
    /// the function decided, so pinning fell back to a default. Such a type is not a finding: the
    /// argument is for monomorphization to settle from the instances the program has.
    pub unsettled: bool,
    /// Whether `ty` was worked out from the node's form and its surroundings rather than recorded
    /// on it: the tree this one came from carried no stamp here. Going back to an `Expr` such a
    /// node is left unstamped, so a conversion round trip adds nothing a pass could trip over.
    pub derived: bool,
    /// For a `let` or `mut`: whether the binding was written with, or recorded as having, a type of
    /// its own (`mut n: u64 = 0`), which is then its value's `ty`.
    pub annotated: bool,
}

/// One arm of a typed `match`, or the arm of an `if let`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TMatchArm<T: TyRepr = Ty> {
    pub pattern: TPattern<T>,
    pub body: TExpr<T>,
    pub span: Span,
}

/// A pattern, with the expressions inside an array pattern typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TPattern<T: TyRepr = Ty> {
    Ctor {
        name: String,
        bindings: Vec<String>,
        ignore_payload: bool,
    },
    Str(String),
    Char(u8),
    ArrayNested(Vec<TPattern<T>>),
    Array(Vec<TExpr<T>>),
    ArrayRest(Vec<TExpr<T>>),
    Wildcard,
    Tuple(Vec<TPattern<T>>),
    Nested {
        name: String,
        args: Vec<TPattern<T>>,
    },
    Int(i64),
    Bind(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TFieldInit<T: TyRepr = Ty> {
    pub name: SpanStr,
    pub value: TExpr<T>,
}

/// A lambda parameter. Its type is always known: a lambda is checked against the function type it
/// is passed as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TLambdaParam<T: TyRepr = Ty> {
    pub name: String,
    pub ty: T,
    pub span: Span,
}

/// What an expression is. Mirrors [`ExprKind`] except where noted on the module: no keyword
/// arguments, spreads, written type arguments or tuple literals, and no binding annotations (the
/// bound value's own type is the binding's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TExprKind<T: TyRepr = Ty> {
    /// An integer literal; the node's `ty` is the integer primitive it was fitted to.
    Int(i64),
    /// A floating-point literal as written; the node's `ty` is `F32` or `F64`.
    Float(String),
    Bool(bool),
    Str(String),
    Char(u8),
    Ident(String),
    /// A call. `type_args` are the callee's declared type variables, in declaration order, as this
    /// call instantiates them (empty for a callee that is not generic).
    Call {
        callee: Callee,
        type_args: Vec<T>,
        args: Vec<TExpr<T>>,
        method_style: bool,
    },
    Neg(Box<TExpr<T>>),
    If(Box<TExpr<T>>, Box<TExpr<T>>, Box<TExpr<T>>),
    Construct(String, Vec<TFieldInit<T>>),
    Field(Box<TExpr<T>>, String),
    Let(String, Box<TExpr<T>>, Box<TExpr<T>>),
    LetMut(String, Box<TExpr<T>>, Box<TExpr<T>>),
    Assign(Box<TExpr<T>>, Box<TExpr<T>>, Box<TExpr<T>>),
    For(String, Option<String>, Box<TExpr<T>>, Box<TExpr<T>>),
    While(Box<TExpr<T>>, Box<TExpr<T>>),
    Shim(String, Vec<(String, String)>, Box<TExpr<T>>),
    /// `none`; the node's `ty` is the optional it is.
    None,
    Match(Box<TExpr<T>>, Vec<TMatchArm<T>>),
    /// `if let PATTERN = scrutinee { arm } else { otherwise }`: the arm, the scrutinee, the else.
    IfLet(Box<TMatchArm<T>>, Box<TExpr<T>>, Box<TExpr<T>>),
    /// `[a, b]`; the node's `ty` is the array, so an empty one still says its element.
    ArrayLit(Vec<TExpr<T>>),
    /// `(a, b)`; the node's `ty` is the tuple's struct (or its template applied to type variables).
    /// Monomorphization lowers it to that struct's construction.
    TupleLit(Vec<TExpr<T>>),
    SetLit(Vec<TExpr<T>>, PinnedOrder),
    DictLit(Vec<(TExpr<T>, TExpr<T>)>),
    Index(Box<TExpr<T>>, Box<TExpr<T>>),
    Slice(Box<TExpr<T>>, Box<TExpr<T>>, Option<Box<TExpr<T>>>),
    Try(Box<TExpr<T>>),
    Unit,
    Seq(Box<TExpr<T>>, Box<TExpr<T>>),
    Return(Box<TExpr<T>>),
    Lambda(Vec<TLambdaParam<T>>, Box<TExpr<T>>),
}

impl<T: TyRepr> TExpr<T> {
    pub fn new(kind: TExprKind<T>, ty: T, span: Span) -> Self {
        TExpr {
            kind,
            ty,
            span,
            value_span: None,
            name_span: None,
            unsettled: false,
            derived: false,
            annotated: false,
        }
    }

    /// A node of `kind` and `ty` standing where `like` stood: its spans, nothing else. The builder
    /// for a pass that replaces one expression with another and has to say what the new one is.
    pub fn rebuilt(kind: TExprKind<T>, ty: T, like: &TExpr<T>) -> Self {
        TExpr {
            kind,
            ty,
            span: like.span.clone(),
            value_span: like.value_span.clone(),
            name_span: like.name_span.clone(),
            unsettled: false,
            derived: false,
            annotated: false,
        }
    }

    /// Call `f` on each immediate sub-expression, in evaluation order. A binder's name and a
    /// pattern's shape are not expressions; the expressions inside an array pattern are not
    /// visited either, matching [`Expr::children`].
    pub fn for_each_child<'a>(&'a self, f: &mut impl FnMut(&'a TExpr<T>)) {
        use TExprKind as K;
        match &self.kind {
            K::Int(_)
            | K::Float(_)
            | K::Bool(_)
            | K::Str(_)
            | K::Char(_)
            | K::Ident(_)
            | K::None
            | K::Unit => {}
            K::Shim(_, _, x)
            | K::Neg(x)
            | K::Field(x, _)
            | K::Try(x)
            | K::Return(x)
            | K::Lambda(_, x) => f(x),
            K::Seq(a, b)
            | K::Index(a, b)
            | K::Let(_, a, b)
            | K::LetMut(_, a, b)
            | K::For(_, _, a, b)
            | K::While(a, b) => {
                f(a);
                f(b);
            }
            K::Assign(a, b, c) | K::If(a, b, c) => {
                f(a);
                f(b);
                f(c);
            }
            K::Slice(a, b, c) => {
                f(a);
                f(b);
                if let Some(c) = c {
                    f(c);
                }
            }
            K::Call { args: items, .. }
            | K::ArrayLit(items)
            | K::TupleLit(items)
            | K::SetLit(items, _) => items.iter().for_each(f),
            K::DictLit(pairs) => {
                for (k, v) in pairs {
                    f(k);
                    f(v);
                }
            }
            K::Construct(_, inits) => inits.iter().for_each(|init| f(&init.value)),
            K::Match(scrutinee, arms) => {
                f(scrutinee);
                arms.iter().for_each(|arm| f(&arm.body));
            }
            K::IfLet(arm, scrutinee, otherwise) => {
                f(scrutinee);
                f(&arm.body);
                f(otherwise);
            }
        }
    }

    /// [`TExpr::for_each_child`], mutably.
    pub fn for_each_child_mut(&mut self, f: &mut impl FnMut(&mut TExpr<T>)) {
        use TExprKind as K;
        match &mut self.kind {
            K::Int(_)
            | K::Float(_)
            | K::Bool(_)
            | K::Str(_)
            | K::Char(_)
            | K::Ident(_)
            | K::None
            | K::Unit => {}
            K::Shim(_, _, x)
            | K::Neg(x)
            | K::Field(x, _)
            | K::Try(x)
            | K::Return(x)
            | K::Lambda(_, x) => f(x),
            K::Seq(a, b)
            | K::Index(a, b)
            | K::Let(_, a, b)
            | K::LetMut(_, a, b)
            | K::For(_, _, a, b)
            | K::While(a, b) => {
                f(a);
                f(b);
            }
            K::Assign(a, b, c) | K::If(a, b, c) => {
                f(a);
                f(b);
                f(c);
            }
            K::Slice(a, b, c) => {
                f(a);
                f(b);
                if let Some(c) = c {
                    f(c);
                }
            }
            K::Call { args: items, .. }
            | K::ArrayLit(items)
            | K::TupleLit(items)
            | K::SetLit(items, _) => items.iter_mut().for_each(f),
            K::DictLit(pairs) => {
                for (k, v) in pairs {
                    f(k);
                    f(v);
                }
            }
            K::Construct(_, inits) => inits.iter_mut().for_each(|init| f(&mut init.value)),
            K::Match(scrutinee, arms) => {
                f(scrutinee);
                arms.iter_mut().for_each(|arm| f(&mut arm.body));
            }
            K::IfLet(arm, scrutinee, otherwise) => {
                f(scrutinee);
                f(&mut arm.body);
                f(otherwise);
            }
        }
    }

    /// Every expression in the tree, this one first, in evaluation order.
    pub fn walk<'a>(&'a self, f: &mut impl FnMut(&'a TExpr<T>)) {
        f(self);
        self.for_each_child(&mut |child| child.walk(f));
    }

    /// The untyped expression this one stands for: the same tree with the types dropped.
    pub fn erase(&self) -> Expr {
        use TExprKind as K;
        let e = |x: &TExpr<T>| Box::new(x.erase());
        let kind = match &self.kind {
            K::Int(n) => ExprKind::Num(*n),
            K::Float(text) => ExprKind::Float(text.clone()),
            K::Bool(b) => ExprKind::Bool(*b),
            K::Str(s) => ExprKind::Str(s.clone()),
            K::Char(c) => ExprKind::Char(*c),
            K::Ident(name) => ExprKind::Ident(name.clone()),
            K::Call {
                callee,
                args,
                method_style,
                ..
            } => ExprKind::Call(
                callee.clone(),
                args.iter().map(TExpr::erase).collect(),
                *method_style,
            ),
            K::Neg(x) => ExprKind::Neg(e(x)),
            K::If(c, t, o) => ExprKind::If(e(c), e(t), e(o)),
            K::Construct(name, inits) => ExprKind::Construct(
                name.clone(),
                inits
                    .iter()
                    .map(|i| FieldInit {
                        name: i.name.clone(),
                        value: i.value.erase(),
                    })
                    .collect(),
            ),
            K::Field(x, name) => ExprKind::Field(e(x), name.clone()),
            K::Let(name, v, b) => ExprKind::Let(name.clone(), None, e(v), e(b)),
            K::LetMut(name, v, b) => ExprKind::LetMut(name.clone(), None, e(v), e(b)),
            K::Assign(l, v, r) => ExprKind::Assign(e(l), e(v), e(r)),
            K::For(name, index, it, body) => {
                ExprKind::For(name.clone(), index.clone(), e(it), e(body))
            }
            K::While(c, b) => ExprKind::While(e(c), e(b)),
            K::Shim(effect, bindings, body) => {
                ExprKind::Shim(effect.clone(), bindings.clone(), e(body))
            }
            K::None => ExprKind::None,
            K::Match(s, arms) => ExprKind::Match(e(s), arms.iter().map(TMatchArm::erase).collect()),
            K::IfLet(arm, s, o) => ExprKind::IfLet(Box::new(arm.erase()), e(s), e(o)),
            K::ArrayLit(items) => ExprKind::ArrayLit(items.iter().map(TExpr::erase).collect()),
            K::TupleLit(items) => ExprKind::TupleLit(items.iter().map(TExpr::erase).collect()),
            K::SetLit(items, order) => ExprKind::SetLit(
                items.iter().map(TExpr::erase).collect(),
                order.to_set_order(),
            ),
            K::DictLit(pairs) => {
                ExprKind::DictLit(pairs.iter().map(|(k, v)| (k.erase(), v.erase())).collect())
            }
            K::Index(a, i) => ExprKind::Index(e(a), e(i)),
            K::Slice(a, s, end) => ExprKind::Slice(e(a), e(s), end.as_ref().map(|x| e(x))),
            K::Try(x) => ExprKind::Try(e(x)),
            K::Unit => ExprKind::Unit,
            K::Seq(a, b) => ExprKind::Seq(e(a), e(b)),
            K::Return(x) => ExprKind::Return(e(x)),
            K::Lambda(params, body) => ExprKind::Lambda(
                params
                    .iter()
                    .map(|p| LambdaParam {
                        name: p.name.clone(),
                        ty: None,
                        span: p.span.clone(),
                    })
                    .collect(),
                e(body),
            ),
        };
        let mut out = Expr::new(kind, self.span.clone());
        out.value_span = self.value_span.clone();
        out.name_span = self.name_span.clone();
        out
    }
}

fn map_all<T: TyRepr, U: TyRepr>(
    xs: &[TExpr<T>],
    f: &mut dyn FnMut(&T) -> Option<U>,
) -> Option<Vec<TExpr<U>>> {
    xs.iter().map(|x| x.map_types(&mut *f)).collect()
}

fn map_one<T: TyRepr, U: TyRepr>(
    x: &TExpr<T>,
    f: &mut dyn FnMut(&T) -> Option<U>,
) -> Option<Box<TExpr<U>>> {
    x.map_types(f).map(Box::new)
}

impl<T: TyRepr> TExpr<T> {
    /// This tree with every type (the node's own, a call's `type_args`, a lambda parameter's) replaced
    /// by what `f` makes of it, or `None` if `f` refuses one. The one way a tree changes its type
    /// representation: [`TExpr::concretize`] is this with a substitution, and the pinner's
    /// resolution of inference variables is this with a lookup.
    pub fn map_types<U: TyRepr>(&self, f: &mut dyn FnMut(&T) -> Option<U>) -> Option<TExpr<U>> {
        use TExprKind as K;
        let kind = match &self.kind {
            K::Int(n) => K::Int(*n),
            K::Float(text) => K::Float(text.clone()),
            K::Bool(b) => K::Bool(*b),
            K::Str(s) => K::Str(s.clone()),
            K::Char(ch) => K::Char(*ch),
            K::Ident(name) => K::Ident(name.clone()),
            K::Call {
                callee,
                type_args,
                args,
                method_style,
            } => K::Call {
                callee: callee.clone(),
                type_args: type_args.iter().map(|t| f(t)).collect::<Option<Vec<_>>>()?,
                args: map_all(args, f)?,
                method_style: *method_style,
            },
            K::Neg(x) => K::Neg(map_one(x, f)?),
            K::If(a, b, d) => K::If(map_one(a, f)?, map_one(b, f)?, map_one(d, f)?),
            K::Construct(name, inits) => K::Construct(
                name.clone(),
                inits
                    .iter()
                    .map(|i| {
                        Some(TFieldInit {
                            name: i.name.clone(),
                            value: i.value.map_types(&mut *f)?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            ),
            K::Field(x, name) => K::Field(map_one(x, f)?, name.clone()),
            K::Let(name, v, b) => K::Let(name.clone(), map_one(v, f)?, map_one(b, f)?),
            K::LetMut(name, v, b) => K::LetMut(name.clone(), map_one(v, f)?, map_one(b, f)?),
            K::Assign(l, v, r) => K::Assign(map_one(l, f)?, map_one(v, f)?, map_one(r, f)?),
            K::For(name, index, it, body) => K::For(
                name.clone(),
                index.clone(),
                map_one(it, f)?,
                map_one(body, f)?,
            ),
            K::While(cond, body) => K::While(map_one(cond, f)?, map_one(body, f)?),
            K::Shim(effect, bindings, body) => {
                K::Shim(effect.clone(), bindings.clone(), map_one(body, f)?)
            }
            K::None => K::None,
            K::Match(s, arms) => K::Match(
                map_one(s, f)?,
                arms.iter()
                    .map(|a| a.map_types(&mut *f))
                    .collect::<Option<Vec<_>>>()?,
            ),
            K::IfLet(arm, s, o) => K::IfLet(
                Box::new(arm.map_types(&mut *f)?),
                map_one(s, f)?,
                map_one(o, f)?,
            ),
            K::ArrayLit(items) => K::ArrayLit(map_all(items, f)?),
            K::TupleLit(items) => K::TupleLit(map_all(items, f)?),
            K::SetLit(items, order) => K::SetLit(map_all(items, f)?, *order),
            K::DictLit(pairs) => K::DictLit(
                pairs
                    .iter()
                    .map(|(k, v)| Some((k.map_types(&mut *f)?, v.map_types(&mut *f)?)))
                    .collect::<Option<Vec<_>>>()?,
            ),
            K::Index(a, i) => K::Index(map_one(a, f)?, map_one(i, f)?),
            K::Slice(a, s, end) => K::Slice(
                map_one(a, f)?,
                map_one(s, f)?,
                match end {
                    Some(end) => Some(map_one(end, f)?),
                    None => None,
                },
            ),
            K::Try(x) => K::Try(map_one(x, f)?),
            K::Unit => K::Unit,
            K::Seq(a, b) => K::Seq(map_one(a, f)?, map_one(b, f)?),
            K::Return(x) => K::Return(map_one(x, f)?),
            K::Lambda(params, body) => K::Lambda(
                params
                    .iter()
                    .map(|p| {
                        Some(TLambdaParam {
                            name: p.name.clone(),
                            ty: f(&p.ty)?,
                            span: p.span.clone(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
                map_one(body, f)?,
            ),
        };
        Some(TExpr {
            kind,
            ty: f(&self.ty)?,
            span: self.span.clone(),
            value_span: self.value_span.clone(),
            name_span: self.name_span.clone(),
            unsettled: self.unsettled,
            derived: self.derived,
            annotated: self.annotated,
        })
    }
}

impl TExpr<Ty> {
    /// This tree with every type variable replaced through `subst`, or `None` if one is unbound.
    /// The one place a pre-mono tree becomes a post-mono one.
    pub fn concretize(
        &self,
        subst: &dyn Fn(&str) -> Option<ConcreteTy>,
    ) -> Option<TExpr<ConcreteTy>> {
        self.map_types(&mut |t| t.concretize(subst))
    }
}

impl<T: TyRepr> TMatchArm<T> {
    /// See [`TExpr::map_types`].
    pub fn map_types<U: TyRepr>(&self, f: &mut dyn FnMut(&T) -> Option<U>) -> Option<TMatchArm<U>> {
        Some(TMatchArm {
            pattern: self.pattern.map_types(f)?,
            body: self.body.map_types(f)?,
            span: self.span.clone(),
        })
    }
}

impl<T: TyRepr> TPattern<T> {
    /// See [`TExpr::map_types`].
    pub fn map_types<U: TyRepr>(&self, f: &mut dyn FnMut(&T) -> Option<U>) -> Option<TPattern<U>> {
        fn pats<T: TyRepr, U: TyRepr>(
            ps: &[TPattern<T>],
            f: &mut dyn FnMut(&T) -> Option<U>,
        ) -> Option<Vec<TPattern<U>>> {
            ps.iter().map(|p| p.map_types(&mut *f)).collect()
        }
        Some(match self {
            TPattern::Ctor {
                name,
                bindings,
                ignore_payload,
            } => TPattern::Ctor {
                name: name.clone(),
                bindings: bindings.clone(),
                ignore_payload: *ignore_payload,
            },
            TPattern::Str(s) => TPattern::Str(s.clone()),
            TPattern::Char(c) => TPattern::Char(*c),
            TPattern::ArrayNested(ps) => TPattern::ArrayNested(pats(ps, f)?),
            TPattern::Array(xs) => TPattern::Array(map_all(xs, f)?),
            TPattern::ArrayRest(xs) => TPattern::ArrayRest(map_all(xs, f)?),
            TPattern::Wildcard => TPattern::Wildcard,
            TPattern::Tuple(ps) => TPattern::Tuple(pats(ps, f)?),
            TPattern::Nested { name, args } => TPattern::Nested {
                name: name.clone(),
                args: pats(args, f)?,
            },
            TPattern::Int(n) => TPattern::Int(*n),
            TPattern::Bind(name) => TPattern::Bind(name.clone()),
        })
    }
}

impl<T: TyRepr> TMatchArm<T> {
    fn erase(&self) -> MatchArm {
        MatchArm {
            pattern: self.pattern.erase(),
            body: self.body.erase(),
            span: self.span.clone(),
            written: Default::default(),
        }
    }
}

impl<T: TyRepr> TPattern<T> {
    /// The names this pattern binds, as [`Pattern::bindings`] says them.
    pub fn bindings(&self) -> Vec<String> {
        self.erase().bindings().to_vec()
    }

    fn erase(&self) -> Pattern {
        match self {
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
            TPattern::ArrayNested(ps) => Pattern::ArrayNested(ps.iter().map(Self::erase).collect()),
            TPattern::Array(xs) => Pattern::Array(xs.iter().map(TExpr::erase).collect()),
            TPattern::ArrayRest(xs) => Pattern::ArrayRest(xs.iter().map(TExpr::erase).collect()),
            TPattern::Wildcard => Pattern::Wildcard,
            TPattern::Tuple(ps) => Pattern::Tuple(ps.iter().map(Self::erase).collect()),
            TPattern::Nested { name, args } => Pattern::Nested {
                name: name.clone(),
                args: args.iter().map(Self::erase).collect(),
            },
            TPattern::Int(n) => Pattern::Int(*n),
            TPattern::Bind(name) => Pattern::Bind(name.clone()),
        }
    }
}

/// A function parameter, typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TParam<T: TyRepr = Ty> {
    pub name: String,
    pub name_span: NameSpan,
    pub ty: T,
    pub mutable: bool,
    pub arity: Arity,
    pub default: Option<TExpr<T>>,
    pub implicit_some: bool,
}

/// A declared type variable: its bound and, for a function's, the type it takes when a call leaves
/// nothing to infer it from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TTypeParam {
    pub name: String,
    pub bound: Bound,
    pub default: Option<Ty>,
    pub name_span: NameSpan,
}

/// A function's shape: type variables, parameters, effects, return type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TSignature<T: TyRepr = Ty> {
    pub type_vars: Vec<TTypeParam>,
    pub params: Vec<TParam<T>>,
    pub effects: Vec<String>,
    pub return_ty: Option<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TFunction {
    pub name: SpanStr,
    pub is_pub: bool,
    pub sig: TSignature<Ty>,
    pub body: TExpr<Ty>,
    pub test_body: Option<TExpr<Ty>>,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TFieldDecl {
    pub name: SpanStr,
    pub ty: Ty,
    pub default: Option<TExpr<Ty>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TStructDecl {
    pub name: SpanStr,
    pub doc: Option<String>,
    pub type_vars: Vec<TTypeParam>,
    pub fields: Vec<TFieldDecl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TCaseParam {
    pub name: Option<String>,
    pub ty: Ty,
    pub default: Option<TExpr<Ty>>,
    pub implicit_some: bool,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TVariantCase {
    pub name: SpanStr,
    pub doc: Option<String>,
    pub payload: Vec<TCaseParam>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TVariantDecl {
    pub name: SpanStr,
    pub doc: Option<String>,
    pub type_vars: Vec<TTypeParam>,
    pub cases: Vec<TVariantCase>,
}

/// A declaration that survives to type checking's output. Imports, constants and refinement
/// declarations were resolved away before it, so they are not items here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TItem {
    Fn(TFunction),
    Struct(TStructDecl),
    Variant(TVariantDecl),
}

/// The pinned program: what type checking hands to the optimizer and to monomorphization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TProgram {
    pub items: Vec<TItem>,
    /// See [`Program::sources`].
    pub sources: Vec<FileSource>,
    pub doc: Option<String>,
}

impl<T: TyRepr> TParam<T> {
    fn erase(&self) -> Param {
        Param {
            name: self.name.clone(),
            name_span: self.name_span.clone(),
            ty: self.ty.to_type(),
            mutable: self.mutable,
            arity: self.arity,
            default: self.default.as_ref().map(TExpr::erase),
            implicit_some: self.implicit_some,
        }
    }
}

impl TTypeParam {
    fn erase(&self) -> TypeParam {
        TypeParam {
            name: self.name.clone(),
            bound: self.bound,
            default: self.default.as_ref().map(TyRepr::to_type),
            name_span: self.name_span.clone(),
        }
    }
}

impl<T: TyRepr> TSignature<T> {
    pub fn erase(&self) -> Signature {
        Signature {
            type_vars: self.type_vars.iter().map(TTypeParam::erase).collect(),
            params: self.params.iter().map(TParam::erase).collect(),
            effects: self.effects.clone(),
            return_ty: self.return_ty.as_ref().map(TyRepr::to_type),
        }
    }
}

impl TProgram {
    /// The untyped program this one stands for.
    pub fn erase(&self) -> Program {
        Program {
            items: self
                .items
                .iter()
                .map(|item| match item {
                    TItem::Fn(f) => Item::Fn(Function {
                        name: f.name.clone(),
                        is_pub: f.is_pub,
                        sig: f.sig.erase(),
                        body: f.body.erase(),
                        test_body: f.test_body.as_ref().map(TExpr::erase),
                        test_fns: Vec::new(),
                        doc: f.doc.clone(),
                    }),
                    TItem::Struct(s) => Item::Struct(StructDecl {
                        name: s.name.clone(),
                        doc: s.doc.clone(),
                        type_vars: s.type_vars.iter().map(TTypeParam::erase).collect(),
                        fields: s
                            .fields
                            .iter()
                            .map(|field| FieldDecl {
                                name: field.name.clone(),
                                ty: field.ty.to_type(),
                                default: field.default.as_ref().map(TExpr::erase),
                            })
                            .collect(),
                    }),
                    TItem::Variant(v) => Item::Variant(VariantDecl {
                        name: v.name.clone(),
                        doc: v.doc.clone(),
                        type_vars: v.type_vars.iter().map(TTypeParam::erase).collect(),
                        cases: v
                            .cases
                            .iter()
                            .map(|case| VariantCase {
                                name: case.name.clone(),
                                doc: case.doc.clone(),
                                payload: case
                                    .payload
                                    .iter()
                                    .map(|p| CaseParam {
                                        name: p.name.clone(),
                                        ty: p.ty.to_type(),
                                        default: p.default.as_ref().map(TExpr::erase),
                                        implicit_some: p.implicit_some,
                                        doc: p.doc.clone(),
                                    })
                                    .collect(),
                            })
                            .collect(),
                    }),
                })
                .collect(),
            sources: self.sources.clone(),
            doc: self.doc.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int() -> Ty {
        Ty::Primitive(Primitive::I64)
    }

    fn node(kind: TExprKind<Ty>, ty: Ty) -> TExpr<Ty> {
        TExpr::new(kind, ty, 0..0)
    }

    #[test]
    fn from_type_refuses_every_placeholder() {
        for placeholder in [
            Type::Unknown,
            Type::Any,
            Type::ConcatStr,
            Type::Tuple(vec![]),
            Type::Generic("Box".into(), vec![Type::Unknown]),
            Type::Set(Box::new(Type::Primitive(Primitive::I64)), SetOrder::Context),
        ] {
            assert_eq!(Ty::from_type(&placeholder), None, "{placeholder:?}");
        }
    }

    #[test]
    fn from_type_round_trips_what_it_accepts() {
        let t = Type::Result(
            Box::new(Type::Array(Box::new(Type::Optional(Box::new(
                Type::TypeVar("T".into()),
            ))))),
            Box::new(Type::Named("Error".into())),
        );
        assert_eq!(Ty::from_type(&t).unwrap().to_type(), t);
    }

    #[test]
    fn concretize_is_total_or_nothing() {
        let t = Ty::Array(Box::new(Ty::Var("T".into())));
        let bound = |name: &str| (name == "T").then(|| ConcreteTy::Primitive(Primitive::U8));
        assert_eq!(
            t.concretize(&bound),
            Some(ConcreteTy::Array(Box::new(ConcreteTy::Primitive(
                Primitive::U8
            ))))
        );
        assert_eq!(t.concretize(&|_| None), None);
        assert!(t.mentions_var());
        assert!(!int().mentions_var());
    }

    #[test]
    fn a_tree_concretizes_only_when_every_type_in_it_does() {
        let x = node(TExprKind::Ident("x".into()), Ty::Var("T".into()));
        let call = node(
            TExprKind::Call {
                callee: Callee::User("id".into()),
                type_args: vec![Ty::Var("T".into())],
                args: vec![x],
                method_style: false,
            },
            Ty::Var("T".into()),
        );
        let concrete = call
            .concretize(&|_| Some(ConcreteTy::Primitive(Primitive::I64)))
            .unwrap();
        assert_eq!(concrete.ty, ConcreteTy::Primitive(Primitive::I64));
        assert!(call.concretize(&|_| None).is_none());
    }

    #[test]
    fn erase_drops_types_and_keeps_the_tree() {
        let tree = node(
            TExprKind::Let(
                "n".into(),
                Box::new(node(TExprKind::Int(3), int())),
                Box::new(node(
                    TExprKind::ArrayLit(vec![node(TExprKind::Ident("n".into()), int())]),
                    Ty::Array(Box::new(int())),
                )),
            ),
            Ty::Array(Box::new(int())),
        );
        let erased = tree.erase();
        assert_eq!(
            erased.kind,
            ExprKind::Let(
                "n".into(),
                None,
                Box::new(Expr::new(ExprKind::Num(3), 0..0)),
                Box::new(Expr::new(
                    ExprKind::ArrayLit(vec![Expr::new(ExprKind::Ident("n".into()), 0..0)]),
                    0..0
                )),
            )
        );
        assert!(erased.ty.is_none());
    }

    #[test]
    fn children_come_in_evaluation_order_and_match_the_untyped_walk() {
        let tree = node(
            TExprKind::IfLet(
                Box::new(TMatchArm {
                    pattern: TPattern::Wildcard,
                    body: node(TExprKind::Int(2), int()),
                    span: 0..0,
                }),
                Box::new(node(TExprKind::Int(1), int())),
                Box::new(node(TExprKind::Int(3), int())),
            ),
            int(),
        );
        let mut typed = Vec::new();
        tree.for_each_child(&mut |c| typed.push(c.erase()));
        let untyped: Vec<Expr> = tree.erase().children().cloned().collect();
        assert_eq!(typed, untyped);
    }
}
