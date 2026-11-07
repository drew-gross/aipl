//! The type pinner: types a lowered program by unification and hands back a [`TProgram`], a tree
//! in which every node carries a final type and no placeholder can be written down.
//!
//! This is the first stage of the typed-IR project (`TYPED_IR.md`). It runs *beside* the checker in
//! `check.rs`, which stays the authority on what compiles; nothing consumes the pinner's output
//! yet. The pinner is held to the checker by the oracle test: over every program the checker
//! accepts it must pin, and every program it rejects it must reject.
//!
//! **How a type is found.** Every expression gets an inference type ([`It`]) whose leaves may be
//! variables. A variable stands for what is not yet known: an integer literal's width, the element
//! of an empty `[]`, the inner type of a `none`, the type argument of a generic call. Unification
//! binds them. At the end of a function every variable is resolved ([`Table::resolve`]); one
//! nothing constrained takes the default for its kind (`i64` for an integer and for everything else,
//! `f64` for a float), so `[].len()` is an `i64[]` and codegen is the same as for the annotated
//! spelling.
//!
//! **What is not unification.** The language coerces at a few places (an integer widens to a wider
//! declared one, a `str` is a `char[]`, a set is an array to a function that only reads its walk,
//! a value fits an optional). [`Pinner::coerce`] is where a value meets the type its position
//! declares, and the only place those are allowed.
//!
//! **Gaps.** A form the pinner does not type yet is a [`Failure::Gap`] naming it, never a guess, so
//! the oracle reports exactly how much of the corpus is covered.

use std::collections::{HashMap, HashSet};

use crate::check::{optional_map_signature, span_map_signature};
use aipl_syntax::ast::{
    Callee, Expr, ExprKind, Function, Item, Pattern, Primitive, Program, Signature, Type,
};
use aipl_syntax::typed::{
    PinnedOrder, TExpr, TExprKind, TFieldDecl, TFunction, TItem, TLambdaParam, TMatchArm, TParam,
    TPattern, TProgram, TSignature, TStructDecl, TTypeParam, Ty, TyRepr,
};
use aipl_syntax::{Error, Span};

/// An inference type: a [`Ty`] whose leaves may be variables. Never leaves this module.
#[derive(Debug, Clone, PartialEq, Eq)]
enum It {
    Unit,
    Prim(Primitive),
    Named(String),
    Case(Box<It>),
    /// A type parameter of the function being typed: it equals only itself.
    Rigid(String),
    Opt(Box<It>),
    Arr(Box<It>),
    Set(Box<It>, Ord),
    Dict(Box<It>, Box<It>),
    Res(Box<It>, Box<It>),
    Fn(Vec<It>, Box<It>),
    /// A generic struct or variant applied to arguments (`Box<T>`).
    Gen(String, Vec<It>),
    Var(usize),
}

/// What a set's walk promises, possibly not yet known: an empty `#{}` and a `to_set` take theirs
/// from where they sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ord {
    Known(PinnedOrder),
    Var(usize),
}

impl TyRepr for It {
    fn to_type(&self) -> Type {
        // Inference types only live inside the pinner, which resolves them before anything prints.
        Type::Unknown
    }
}

/// What may a variable become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Any,
    Int,
    Float,
    /// The type of an empty `#{}`: a set or a dict, whichever its position asks for.
    Brace,
}

#[derive(Debug)]
struct Slot {
    bound: Option<It>,
    kind: Kind,
}

/// The variables of one function body.
#[derive(Debug, Default)]
struct Table {
    slots: Vec<Slot>,
    orders: Vec<OrdSlot>,
}

#[derive(Debug, Clone, Copy)]
enum OrdSlot {
    Free,
    Known(PinnedOrder),
    Link(usize),
}

impl Table {
    fn fresh_order(&mut self) -> Ord {
        self.orders.push(OrdSlot::Free);
        Ord::Var(self.orders.len() - 1)
    }

    /// `o` with its variable chain followed.
    fn order(&self, o: Ord) -> Ord {
        match o {
            Ord::Var(v) => match self.orders[v] {
                OrdSlot::Free => o,
                OrdSlot::Known(k) => Ord::Known(k),
                OrdSlot::Link(w) => self.order(Ord::Var(w)),
            },
            known => known,
        }
    }

    fn unify_order(&mut self, a: Ord, b: Ord) -> Result<(), String> {
        match (self.order(a), self.order(b)) {
            (Ord::Known(x), Ord::Known(y)) if x == y => Ok(()),
            (Ord::Known(_), Ord::Known(_)) => Err("the sets promise different orders".into()),
            (Ord::Var(x), Ord::Var(y)) if x == y => Ok(()),
            (Ord::Var(x), Ord::Var(y)) => {
                self.orders[x] = OrdSlot::Link(y);
                Ok(())
            }
            (Ord::Var(x), Ord::Known(k)) | (Ord::Known(k), Ord::Var(x)) => {
                self.orders[x] = OrdSlot::Known(k);
                Ok(())
            }
        }
    }

    fn fresh(&mut self, kind: Kind) -> It {
        self.slots.push(Slot { bound: None, kind });
        It::Var(self.slots.len() - 1)
    }

    /// `t` with its outermost variable chain followed.
    fn shallow(&self, t: &It) -> It {
        let mut cur = t.clone();
        while let It::Var(v) = cur {
            match &self.slots[v].bound {
                Some(next) => cur = next.clone(),
                None => return It::Var(v),
            }
        }
        cur
    }

    fn occurs(&self, v: usize, t: &It) -> bool {
        match self.shallow(t) {
            It::Var(w) => v == w,
            It::Unit | It::Prim(_) | It::Named(_) | It::Rigid(_) => false,
            It::Case(t) | It::Opt(t) | It::Arr(t) | It::Set(t, _) => self.occurs(v, &t),
            It::Dict(a, b) | It::Res(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            It::Gen(_, args) => args.iter().any(|a| self.occurs(v, a)),
            It::Fn(ps, r) => ps.iter().any(|p| self.occurs(v, p)) || self.occurs(v, &r),
        }
    }

    fn bind(&mut self, v: usize, t: It) -> Result<(), String> {
        let kind = self.slots[v].kind;
        match (&t, kind) {
            (It::Var(w), _) => {
                // Two variables: the more specific kind survives.
                let other = self.slots[*w].kind;
                if kind != Kind::Any && other != Kind::Any && kind != other {
                    return Err("these two kinds of value cannot be the same type".into());
                }
                if kind != Kind::Any {
                    self.slots[*w].kind = kind;
                }
            }
            (It::Prim(p), Kind::Int) if !p.is_int() => {
                return Err(format!("{} is not an integer type", prim_name(*p)));
            }
            (It::Prim(p), Kind::Float) if !matches!(p, Primitive::F32 | Primitive::F64) => {
                return Err(format!("{} is not a float type", prim_name(*p)));
            }
            (_, Kind::Int | Kind::Float) if !matches!(t, It::Prim(_)) => {
                return Err("a number cannot be this type".into());
            }
            (It::Set(..) | It::Dict(..), Kind::Brace) => {}
            (_, Kind::Brace) => return Err("`#{}` is a set or a dict".into()),
            _ => {}
        }
        if self.occurs(v, &t) {
            return Err("a type cannot contain itself".into());
        }
        self.slots[v].bound = Some(t);
        Ok(())
    }

    fn unify(&mut self, a: &It, b: &It) -> Result<(), String> {
        let (a, b) = (self.shallow(a), self.shallow(b));
        match (&a, &b) {
            (It::Var(x), It::Var(y)) if x == y => Ok(()),
            (It::Var(x), _) => self.bind(*x, b),
            (_, It::Var(y)) => self.bind(*y, a),
            (It::Unit, It::Unit) => Ok(()),
            (It::Prim(x), It::Prim(y)) if x == y => Ok(()),
            (It::Named(x), It::Named(y)) | (It::Rigid(x), It::Rigid(y)) if x == y => Ok(()),
            (It::Case(x), It::Case(y)) | (It::Opt(x), It::Opt(y)) | (It::Arr(x), It::Arr(y)) => {
                self.unify(x, y)
            }
            (It::Set(x, ox), It::Set(y, oy)) => {
                self.unify_order(*ox, *oy)?;
                self.unify(x, y)
            }
            (It::Gen(n1, a1), It::Gen(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
                for (x, y) in a1.iter().zip(a2) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (It::Dict(k1, v1), It::Dict(k2, v2)) | (It::Res(k1, v1), It::Res(k2, v2)) => {
                self.unify(k1, k2)?;
                self.unify(v1, v2)
            }
            (It::Fn(p1, r1), It::Fn(p2, r2)) if p1.len() == p2.len() => {
                for (x, y) in p1.iter().zip(p2) {
                    self.unify(x, y)?;
                }
                self.unify(r1, r2)
            }
            _ => Err(format!("{} is not {}", self.show(&a), self.show(&b))),
        }
    }

    /// `t` as a pinned type. A variable nothing constrained takes its kind's default.
    fn resolve(&self, t: &It) -> Ty {
        let boxed = |t: &It| Box::new(self.resolve(t));
        match self.shallow(t) {
            It::Unit => Ty::Unit,
            It::Prim(p) => Ty::Primitive(p),
            It::Named(n) => Ty::Named(n),
            It::Case(t) => Ty::Case(boxed(&t)),
            It::Rigid(v) => Ty::Var(v),
            It::Opt(t) => Ty::Optional(boxed(&t)),
            It::Arr(t) => Ty::Array(boxed(&t)),
            It::Set(t, o) => Ty::Set(
                boxed(&t),
                match self.order(o) {
                    Ord::Known(k) => k,
                    Ord::Var(_) => PinnedOrder::Unordered,
                },
            ),
            It::Gen(base, args) => {
                let args: Vec<Ty> = args.iter().map(|a| self.resolve(a)).collect();
                if args.iter().any(Ty::mentions_var) {
                    Ty::Generic(base, args)
                } else {
                    // Every argument is known: the application is the instance it names.
                    let written: Vec<Type> = args.iter().map(TyRepr::to_type).collect();
                    Ty::Named(crate::check::generic_instance_name(&base, &written))
                }
            }
            It::Dict(k, v) => Ty::Dict(boxed(&k), boxed(&v)),
            It::Res(a, b) => Ty::Result(boxed(&a), boxed(&b)),
            It::Fn(ps, r) => Ty::Fn(ps.iter().map(|p| self.resolve(p)).collect(), boxed(&r)),
            It::Var(v) => match self.slots[v].kind {
                Kind::Float => Ty::Primitive(Primitive::F64),
                Kind::Int | Kind::Any => Ty::Primitive(Primitive::I64),
                Kind::Brace => Ty::Set(
                    Box::new(Ty::Primitive(Primitive::I64)),
                    PinnedOrder::Unordered,
                ),
            },
        }
    }

    fn show(&self, t: &It) -> String {
        aipl_syntax::type_name(&self.resolve(t).to_type())
    }
}

fn prim_name(p: Primitive) -> String {
    aipl_syntax::type_name(&Type::Primitive(p))
}

/// Why a function was not pinned.
#[derive(Debug)]
pub enum Failure {
    /// The program is wrong.
    Rejected(Error),
    /// The program may be fine, but the pinner does not type this form yet.
    Gap(String, Span),
}

type Res<T> = Result<T, Failure>;

fn reject<T>(message: impl Into<String>, span: &Span) -> Res<T> {
    Err(Failure::Rejected(Error::at(message, span.clone())))
}

fn gap<T>(what: impl Into<String>, span: &Span) -> Res<T> {
    Err(Failure::Gap(what.into(), span.clone()))
}

/// A function's signature as inference types, with its type variables left abstract.
#[derive(Clone)]
struct Sig {
    sig: Signature,
}

/// The declarations the bodies are typed against.
struct Decls {
    sigs: HashMap<String, Sig>,
    /// Field name, type and whether it has a default, in declaration order.
    structs: HashMap<String, Vec<(String, Type, bool)>>,
    /// Variant name -> its cases and their payload types.
    variants: HashMap<String, Vec<(String, Vec<Type>)>>,
    /// A constructor's loader-qualified name (`Case@Variant`) -> its variant.
    ctors: HashMap<String, String>,
    generic_templates: HashSet<String>,
    generic_structs: HashMap<String, aipl_syntax::ast::StructDecl>,
    generic_variants: HashMap<String, aipl_syntax::ast::VariantDecl>,
    /// A generic variant's constructor (`Case@Variant`) -> the template it belongs to.
    generic_ctors: HashMap<String, String>,
}

#[derive(Clone)]
struct Binding {
    ty: It,
    mutable: bool,
}

struct Pinner<'a> {
    decls: &'a Decls,
    table: Table,
    ret: It,
    rigid: HashSet<String>,
    env: HashMap<String, Binding>,
    /// Whether a type being lowered is a callee's, whose anonymous `any`s are fresh variables at
    /// each use, rather than this function's own, which are rigid.
    instantiating: bool,
    anonymous: usize,
    /// The fresh variables standing for anonymous `any`s in the signature being instantiated.
    anonymous_vars: Vec<It>,
    /// The instance links under way, see [`Pinner::link_instance`].
    linking: HashSet<String>,
}

/// What pinning a program produced.
pub struct Pinned {
    /// Every function that pinned, with every other item carried over.
    pub program: TProgram,
    /// Functions that did not pin: its name and why.
    pub failures: Vec<(String, Failure)>,
}

/// Pin every function of `program`. Functions are independent: one that fails is reported and
/// left out of [`Pinned::program`], and the rest are still pinned.
pub fn pin(program: &Program) -> Pinned {
    let mut decls = Decls {
        sigs: HashMap::new(),
        structs: HashMap::new(),
        variants: HashMap::new(),
        ctors: HashMap::new(),
        generic_templates: HashSet::new(),
        generic_structs: HashMap::new(),
        generic_variants: HashMap::new(),
        generic_ctors: HashMap::new(),
    };
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                decls
                    .sigs
                    .insert(f.name.text.clone(), Sig { sig: f.sig.clone() });
            }
            Item::Struct(s) if s.is_generic() => {
                decls.generic_templates.insert(s.name.text.clone());
                decls.generic_structs.insert(s.name.text.clone(), s.clone());
            }
            Item::Struct(s) => {
                decls.structs.insert(
                    s.name.text.clone(),
                    s.fields
                        .iter()
                        .map(|f| (f.name.text.clone(), f.ty.clone(), f.default.is_some()))
                        .collect(),
                );
            }
            Item::Variant(v) if v.is_generic() => {
                decls.generic_templates.insert(v.name.text.clone());
                for c in &v.cases {
                    decls.generic_ctors.insert(
                        format!("{}@{}", c.name.text, v.name.text),
                        v.name.text.clone(),
                    );
                }
                decls
                    .generic_variants
                    .insert(v.name.text.clone(), v.clone());
            }
            Item::Variant(v) => {
                // An instance of a generic variant (`Opt$i64`) shares its constructors' names
                // with the template and its siblings, so only a constructor of a plain variant
                // is resolvable by name.
                if !v.name.text.contains('$') {
                    for c in &v.cases {
                        decls.ctors.insert(
                            format!("{}@{}", c.name.text, v.name.text),
                            v.name.text.clone(),
                        );
                    }
                }
                decls.variants.insert(
                    v.name.text.clone(),
                    v.cases
                        .iter()
                        .map(|c| (c.name.text.clone(), c.payload_tys()))
                        .collect(),
                );
            }
            _ => {}
        }
    }
    let mut items = Vec::new();
    let mut failures = Vec::new();
    for item in &program.items {
        match item {
            Item::Fn(f) => match pin_function(&decls, f) {
                Ok(t) => items.push(TItem::Fn(t)),
                Err(e) => failures.push((f.name.text.clone(), e)),
            },
            Item::Struct(s) => {
                // Field defaults are expressions; a struct without any carries over whole.
                if s.fields.iter().all(|f| f.default.is_none()) {
                    if let Some(t) = pin_struct(s) {
                        items.push(TItem::Struct(t));
                    }
                }
            }
            _ => {}
        }
    }
    Pinned {
        program: TProgram {
            items,
            sources: program.sources.clone(),
            doc: program.doc.clone(),
        },
        failures,
    }
}

fn pin_struct(s: &aipl_syntax::ast::StructDecl) -> Option<TStructDecl> {
    Some(TStructDecl {
        name: s.name.clone(),
        doc: s.doc.clone(),
        type_vars: s
            .type_vars
            .iter()
            .map(pin_type_param)
            .collect::<Option<_>>()?,
        fields: s
            .fields
            .iter()
            .map(|f| {
                Some(TFieldDecl {
                    name: f.name.clone(),
                    ty: declared(&f.ty)?,
                    default: None,
                })
            })
            .collect::<Option<_>>()?,
    })
}

fn pin_type_param(tp: &aipl_syntax::ast::TypeParam) -> Option<TTypeParam> {
    Some(TTypeParam {
        name: tp.name.clone(),
        bound: tp.bound,
        default: match &tp.default {
            Some(d) => Some(declared(d)?),
            None => None,
        },
        name_span: tp.name_span.clone(),
    })
}

/// A written type as a pinned one, with a refinement dropped to its base.
fn declared(t: &Type) -> Option<Ty> {
    Ty::from_type(&peel(t))
}

/// `t` with every refinement dropped to its base: a `T[] without []` is a `T[]` once checked.
fn peel(t: &Type) -> Type {
    let b = |t: &Type| Box::new(peel(t));
    match t {
        Type::Without(t, _) => peel(t),
        Type::Case(t) => Type::Case(b(t)),
        Type::Optional(t) => Type::Optional(b(t)),
        Type::Array(t) => Type::Array(b(t)),
        Type::Set(t, o) => Type::Set(b(t), *o),
        Type::Dict(k, v) => Type::Dict(b(k), b(v)),
        Type::Result(a, c) => Type::Result(b(a), b(c)),
        Type::Fn(ps, r) => Type::Fn(ps.iter().map(peel).collect(), b(r)),
        other => other.clone(),
    }
}

fn pin_function(decls: &Decls, f: &Function) -> Res<TFunction> {
    let mut p = Pinner {
        decls,
        table: Table::default(),
        ret: It::Unit,
        rigid: f.sig.type_vars.iter().map(|t| t.name.clone()).collect(),
        env: HashMap::new(),
        instantiating: false,
        anonymous: 0,
        anonymous_vars: Vec::new(),
        linking: HashSet::new(),
    };
    let none = HashMap::new();
    let mut param_tys = Vec::new();
    for param in &f.sig.params {
        let ty = p.lower(&param.ty, &none, &f.name.span)?;
        param_tys.push(ty.clone());
        p.env.insert(
            param.name.clone(),
            Binding {
                ty,
                mutable: param.mutable,
            },
        );
    }
    p.ret = match &f.sig.return_ty {
        Some(t) => p.lower(t, &none, &f.name.span)?,
        None => It::Unit,
    };
    let body = p.expr(&f.body)?;
    let ret = p.ret.clone();
    p.coerce(&body.ty, &ret, &f.body.span, "the function's result")?;
    let body = p.finish(body, &f.body.span)?;
    let mut type_vars = f
        .sig
        .type_vars
        .iter()
        .map(pin_type_param)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Failure::Gap("type parameter default".into(), f.name.span.clone()))?;
    // Each anonymous `any` is a type variable of its own.
    for n in 1..=p.anonymous {
        type_vars.push(TTypeParam {
            name: format!("any${n}"),
            bound: aipl_syntax::ast::Bound::Any,
            default: None,
            name_span: Default::default(),
        });
    }
    let sig = TSignature {
        type_vars,
        params: f
            .sig
            .params
            .iter()
            .zip(&param_tys)
            .map(|(param, ty)| TParam {
                name: param.name.clone(),
                name_span: param.name_span.clone(),
                ty: p.table.resolve(ty),
                mutable: param.mutable,
                arity: param.arity,
                default: None,
                implicit_some: param.implicit_some,
            })
            .collect(),
        effects: f.sig.effects.clone(),
        return_ty: f.sig.return_ty.as_ref().map(|_| p.table.resolve(&ret)),
    };
    Ok(TFunction {
        name: f.name.clone(),
        is_pub: f.is_pub,
        sig,
        body,
        test_body: None,
        doc: f.doc.clone(),
    })
}

impl Pinner<'_> {
    /// Resolve every inference type in `body`.
    fn finish(&self, body: TExpr<It>, span: &Span) -> Res<TExpr<Ty>> {
        body.map_types(&mut |t| Some(self.table.resolve(t)))
            .ok_or_else(|| Failure::Gap("unresolved type".into(), span.clone()))
    }

    fn fresh(&mut self) -> It {
        self.table.fresh(Kind::Any)
    }

    /// A written type as an inference type; `args` says what each of a callee's type variables is
    /// at this use, and any other variable of the enclosing function stays rigid.
    fn lower(&mut self, t: &Type, args: &HashMap<String, It>, span: &Span) -> Res<It> {
        let go = |p: &mut Self, t: &Type| p.lower(t, args, span);
        Ok(match t {
            Type::Unit => It::Unit,
            Type::Primitive(p) => It::Prim(*p),
            Type::ConcatStr => It::Prim(Primitive::Str),
            Type::Named(n) => {
                if self.decls.generic_templates.contains(n) {
                    return gap("a generic template used as a type", span);
                }
                It::Named(n.clone())
            }
            Type::Case(v) => It::Case(Box::new(go(self, v)?)),
            Type::TypeVar(v) => match args.get(v) {
                Some(t) => t.clone(),
                None if self.rigid.contains(v) => It::Rigid(v.clone()),
                None => return gap(format!("unbound type variable {v}"), span),
            },
            Type::Optional(t) => It::Opt(Box::new(go(self, t)?)),
            Type::Array(t) => It::Arr(Box::new(go(self, t)?)),
            Type::Set(t, order) => {
                let elem = Box::new(go(self, t)?);
                let order = match PinnedOrder::of(*order) {
                    Some(k) => Ord::Known(k),
                    None => self.table.fresh_order(),
                };
                It::Set(elem, order)
            }
            Type::Generic(name, targs) => It::Gen(
                name.clone(),
                targs.iter().map(|a| go(self, a)).collect::<Res<Vec<_>>>()?,
            ),
            Type::Any => {
                if self.instantiating {
                    let v = self.fresh();
                    self.anonymous_vars.push(v.clone());
                    v
                } else {
                    self.anonymous += 1;
                    It::Rigid(format!("any${}", self.anonymous))
                }
            }
            Type::Dict(k, v) => It::Dict(Box::new(go(self, k)?), Box::new(go(self, v)?)),
            Type::Result(a, b) => It::Res(Box::new(go(self, a)?), Box::new(go(self, b)?)),
            Type::Fn(ps, r) => It::Fn(
                ps.iter().map(|p| go(self, p)).collect::<Res<Vec<_>>>()?,
                Box::new(go(self, r)?),
            ),
            Type::Without(t, _) => go(self, t)?,
            Type::Tuple(_)
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::Unknown => {
                return gap(format!("a placeholder type in a declaration: {t:?}"), span)
            }
        })
    }

    /// A value of type `actual` where `expected` is declared: unification plus the widenings the
    /// language allows at such a position (`check.rs`'s `coerce` is the specification).
    fn coerce(&mut self, actual: &It, expected: &It, span: &Span, what: &str) -> Res<()> {
        let (a, e) = (self.table.shallow(actual), self.table.shallow(expected));
        // A scalar integer widens to a wider declared one; inside a container it must agree.
        if let (It::Prim(x), It::Prim(y)) = (&a, &e) {
            if x.widens_to(*y) {
                return Ok(());
            }
        }
        self.agree(&a, &e).or_else(|message| {
            reject(
                format!(
                    "{what} is {}, but {} is expected ({message})",
                    self.table.show(&a),
                    self.table.show(&e)
                ),
                span,
            )
        })
    }

    /// Whether two types are interchangeable here, binding variables as it goes.
    fn agree(&mut self, a: &It, e: &It) -> Result<(), String> {
        let (a, e) = (self.table.shallow(a), self.table.shallow(e));
        let char_ty = It::Prim(Primitive::Char);
        let error = It::Named(aipl_syntax::ERROR.to_string());
        let str_ty = It::Prim(Primitive::Str);
        match (&a, &e) {
            (It::Var(_), _) | (_, It::Var(_)) => self.table.unify(&a, &e),
            (It::Named(inst), It::Gen(base, targs)) | (It::Gen(base, targs), It::Named(inst)) => {
                self.link_instance(inst, base, targs)
            }
            // `Error` is a `str` underneath.
            (x, y) if (*x == str_ty && *y == error) || (*x == error && *y == str_ty) => Ok(()),
            // A `str` is the `char[]` (and pins an open element to `char`).
            (It::Prim(Primitive::Str), It::Arr(t)) | (It::Arr(t), It::Prim(Primitive::Str)) => {
                self.table.unify(t, &char_ty)
            }
            // A constructor named as a value stands for its case: a function building `V`, or (for a
            // case without a payload) a `V`.
            (It::Fn(_, ret), It::Case(v)) | (It::Case(v), It::Fn(_, ret)) => self.agree(ret, v),
            (It::Named(_), It::Case(v)) | (It::Case(v), It::Named(_))
                if {
                    let inner = self.table.shallow(v);
                    matches!(inner, It::Var(_) | It::Named(_))
                } =>
            {
                let other = if matches!(a, It::Case(_)) { &e } else { &a };
                self.agree(v, other)
            }
            // A `Case<V>` is its tag, an integer.
            (It::Case(_), It::Prim(Primitive::I64 | Primitive::U64))
            | (It::Prim(Primitive::I64 | Primitive::U64), It::Case(_)) => Ok(()),
            (It::Opt(x), It::Opt(y)) | (It::Arr(x), It::Arr(y)) | (It::Case(x), It::Case(y)) => {
                self.agree(x, y)
            }
            (It::Set(x, ox), It::Set(y, oy)) => {
                // A parameter that promises no order takes a set that promises one.
                let given = self.table.order(*ox);
                let wanted = self.table.order(*oy);
                let widening = matches!(given, Ord::Known(PinnedOrder::Asc | PinnedOrder::Desc))
                    && wanted == Ord::Known(PinnedOrder::Unordered);
                if !widening {
                    self.table.unify_order(*ox, *oy)?;
                }
                self.agree(x, y)
            }
            (It::Dict(k1, v1), It::Dict(k2, v2)) | (It::Res(k1, v1), It::Res(k2, v2)) => {
                self.agree(k1, k2)?;
                self.agree(v1, v2)
            }
            (It::Fn(p1, r1), It::Fn(p2, r2)) if p1.len() == p2.len() => {
                for (x, y) in p1.iter().zip(p2) {
                    self.agree(y, x)?;
                }
                self.agree(r1, r2)
            }
            _ => self.table.unify(&a, &e),
        }
    }

    /// An argument to a variadic parameter of sequence type `seq`: the sequence itself, one
    /// element, or an optional element.
    fn variadic_arg(&mut self, seq: &It, arg: &It) -> Result<(), String> {
        let char_ty = It::Prim(Primitive::Char);
        let elem = match self.table.shallow(seq) {
            It::Prim(Primitive::Str) => char_ty,
            It::Arr(e) => *e,
            other => return Err(format!("{} is not a sequence", self.table.show(&other))),
        };
        match self.table.shallow(arg) {
            It::Opt(x) => self.agree(&x, &elem),
            a @ (It::Arr(_) | It::Set(..) | It::Prim(Primitive::Str)) => {
                // A sequence for a sequence; a `str` where the element is a `str`.
                match self.agree(&a, seq) {
                    Ok(()) => Ok(()),
                    Err(_) => self.agree(&a, &elem),
                }
            }
            a => self.agree(&a, &elem),
        }
    }

    /// The argument types a call's parameters see: what a parameter that reads a sequence takes
    /// from a `str`, a set or (for the size queries) a dict or span, without the call changing it.
    fn adapt_receiver(&mut self, callee: &Callee, param: &It, arg: &It) -> Result<bool, String> {
        let (p, a) = (self.table.shallow(param), self.table.shallow(arg));
        let It::Arr(elem) = &p else { return Ok(false) };
        match &a {
            It::Prim(Primitive::Str) => self.table.unify(elem, &It::Prim(Primitive::Char))?,
            It::Set(e, _) => self.table.unify(elem, e)?,
            It::Dict(..) | It::Named(_)
                if matches!(callee, Callee::Len | Callee::IsEmpty | Callee::IsNonempty) => {}
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn node(&self, kind: TExprKind<It>, ty: It, e: &Expr) -> TExpr<It> {
        TExpr {
            kind,
            ty,
            span: e.span.clone(),
            value_span: e.value_span.clone(),
            name_span: e.name_span.clone(),
        }
    }

    fn expr(&mut self, e: &Expr) -> Res<TExpr<It>> {
        use TExprKind as K;
        let bool_ty = It::Prim(Primitive::Bool);
        Ok(match &e.kind {
            ExprKind::Num(n) => {
                let ty = self.table.fresh(Kind::Int);
                self.node(K::Int(*n), ty, e)
            }
            ExprKind::Float(text) => {
                let ty = self.table.fresh(Kind::Float);
                self.node(K::Float(text.clone()), ty, e)
            }
            ExprKind::Bool(b) => self.node(K::Bool(*b), bool_ty, e),
            ExprKind::Str(s) => self.node(K::Str(s.clone()), It::Prim(Primitive::Str), e),
            ExprKind::Char(c) => self.node(K::Char(*c), It::Prim(Primitive::Char), e),
            ExprKind::Unit => self.node(K::Unit, It::Unit, e),
            ExprKind::Ident(name) => match self.env.get(name) {
                Some(b) => {
                    let ty = b.ty.clone();
                    self.node(K::Ident(name.clone()), ty, e)
                }
                None => return self.reference(e, name),
            },
            ExprKind::Neg(x) => {
                let x = self.expr(x)?;
                let ty = x.ty.clone();
                self.node(K::Neg(Box::new(x)), ty, e)
            }
            ExprKind::None => {
                let inner = self.fresh();
                self.node(K::None, It::Opt(Box::new(inner)), e)
            }
            ExprKind::ArrayLit(items) => {
                let elem = self.fresh();
                let mut out = Vec::new();
                for item in items {
                    let item = self.expr(item)?;
                    self.agree(&item.ty, &elem).or_else(|m| {
                        reject(format!("array elements disagree ({m})"), &item.span)
                    })?;
                    out.push(item);
                }
                self.node(K::ArrayLit(out), It::Arr(Box::new(elem)), e)
            }
            ExprKind::If(c, ta, tb) => {
                let c = self.expr(c)?;
                self.coerce(&c.ty, &bool_ty, &c.span, "an `if` condition")?;
                let a = self.expr(ta)?;
                let b = self.expr(tb)?;
                let (da, db) = (aipl_syntax::diverges(ta), aipl_syntax::diverges(tb));
                // A branch that leaves the function has no value to agree with.
                if !da && !db {
                    self.agree(&a.ty, &b.ty).or_else(|m| {
                        reject(format!("the branches of an `if` disagree ({m})"), &e.span)
                    })?;
                }
                let ty = if da { b.ty.clone() } else { a.ty.clone() };
                self.node(K::If(Box::new(c), Box::new(a), Box::new(b)), ty, e)
            }
            ExprKind::While(c, body) => {
                let c = self.expr(c)?;
                self.coerce(&c.ty, &bool_ty, &c.span, "a `while` condition")?;
                let body = self.expr(body)?;
                self.node(
                    K::While(Box::new(c), Box::new(body)),
                    It::Prim(Primitive::I64),
                    e,
                )
            }
            ExprKind::Seq(a, b) => {
                let a = self.expr(a)?;
                let b = self.expr(b)?;
                let ty = b.ty.clone();
                self.node(K::Seq(Box::new(a), Box::new(b)), ty, e)
            }
            ExprKind::Return(x) => {
                let x = self.expr(x)?;
                let ret = self.ret.clone();
                self.coerce(&x.ty, &ret, &x.span, "the returned value")?;
                let ty = self.fresh();
                self.node(K::Return(Box::new(x)), ty, e)
            }
            ExprKind::Let(name, ann, v, body) | ExprKind::LetMut(name, ann, v, body) => {
                let mutable = matches!(e.kind, ExprKind::LetMut(..));
                let (v, ty) = match ann {
                    Some(t) => {
                        let t = self.lower(t, &HashMap::new(), &e.span)?;
                        let v = self.arg(v, &t)?;
                        self.coerce(&v.ty, &t, &v.span, "the bound value")?;
                        (v, t)
                    }
                    None => {
                        let v = self.expr(v)?;
                        let ty = v.ty.clone();
                        (v, ty)
                    }
                };
                let old = self.env.insert(name.clone(), Binding { ty, mutable });
                let body = self.expr(body)?;
                match old {
                    Some(b) => self.env.insert(name.clone(), b),
                    None => self.env.remove(name),
                };
                let ty = body.ty.clone();
                let kind = if mutable {
                    K::LetMut(name.clone(), Box::new(v), Box::new(body))
                } else {
                    K::Let(name.clone(), Box::new(v), Box::new(body))
                };
                self.node(kind, ty, e)
            }
            ExprKind::Assign(lhs, v, rest) => {
                let Some((root, _)) = aipl_syntax::ast::assign_target(lhs) else {
                    return reject(
                        "an assignment target must be a variable or a field of one",
                        &lhs.span,
                    );
                };
                let Some(b) = self.env.get(root).cloned() else {
                    return reject(format!("{root:?} is not defined"), &lhs.span);
                };
                if !b.mutable {
                    return reject(format!("{root:?} is not `mut`"), &lhs.span);
                }
                // The place is typed as the expression it is; a field path reads through structs.
                let lhs = self.expr(lhs)?;
                let v = self.arg(v, &lhs.ty)?;
                self.coerce(&v.ty, &lhs.ty, &v.span, "the assigned value")?;
                let rest = self.expr(rest)?;
                let ty = rest.ty.clone();
                self.node(K::Assign(Box::new(lhs), Box::new(v), Box::new(rest)), ty, e)
            }
            ExprKind::Call(callee, args, method_style) => {
                return self.call(e, callee, args, *method_style)
            }
            ExprKind::For(var, index, iter, body) => {
                let iter = self.expr(iter)?;
                let it = self.known(&iter.ty, &iter.span)?;
                let (elem, key) = match &it {
                    It::Arr(t) | It::Set(t, _) => ((**t).clone(), It::Prim(Primitive::U64)),
                    It::Prim(Primitive::Str) => {
                        (It::Prim(Primitive::Char), It::Prim(Primitive::U64))
                    }
                    It::Named(n) if n == aipl_syntax::SPAN_TYPE => {
                        (It::Prim(Primitive::U64), It::Prim(Primitive::U64))
                    }
                    It::Dict(k, v) => ((**v).clone(), (**k).clone()),
                    other => {
                        return reject(
                            format!(
                            "for-loop iterable must be a str, array, set, dict, or range, got {}",
                            self.table.show(other)
                        ),
                            &iter.span,
                        )
                    }
                };
                let mut scope = vec![(var.clone(), elem)];
                if let Some(i) = index {
                    scope.push((i.clone(), key));
                }
                let (saved, body) = self.scoped(scope, |p| p.expr(body))?;
                self.restore(saved);
                self.node(
                    K::For(var.clone(), index.clone(), Box::new(iter), Box::new(body)),
                    It::Prim(Primitive::I64),
                    e,
                )
            }
            ExprKind::Construct(name, inits) => {
                let (fields, targs, result) = if let Some(f) = self.decls.structs.get(name) {
                    (f.clone(), HashMap::new(), It::Named(name.clone()))
                } else if let Some(t) = self.decls.generic_structs.get(name).cloned() {
                    let (vars, map) = self.fresh_args(&t.type_vars);
                    let fields = t
                        .fields
                        .iter()
                        .map(|f| (f.name.text.clone(), f.ty.clone(), f.default.is_some()))
                        .collect();
                    (fields, map, It::Gen(name.clone(), vars))
                } else {
                    return gap(format!("construction of {name:?}"), &e.span);
                };
                let mut out = Vec::new();
                for init in inits {
                    let Some((_, fty, _)) = fields.iter().find(|(n, _, _)| *n == init.name.text)
                    else {
                        return reject(
                            format!("struct {name:?} has no field {:?}", init.name.text),
                            &e.span,
                        );
                    };
                    let fty = self.lower(fty, &targs, &init.value.span)?;
                    let value = self.arg(&init.value, &fty)?;
                    self.coerce(&value.ty, &fty, &value.span, "a field value")?;
                    out.push(aipl_syntax::typed::TFieldInit {
                        name: init.name.clone(),
                        value,
                    });
                }
                for (fname, _, has_default) in &fields {
                    if !has_default && !inits.iter().any(|i| i.name.text == *fname) {
                        return reject(format!("missing field {fname:?} of {name:?}"), &e.span);
                    }
                }
                self.node(K::Construct(name.clone(), out), result, e)
            }
            ExprKind::Field(x, field) => {
                let x = self.expr(x)?;
                let owner = self.known(&x.ty, &x.span)?;
                let ty = self.field_type(&owner, field, &e.span)?;
                self.node(K::Field(Box::new(x), field.clone()), ty, e)
            }
            ExprKind::Index(a, i) => {
                let a = self.expr(a)?;
                let i = self.expr(i)?;
                let container = self.known(&a.ty, &a.span)?;
                // A span picks a sub-range, which is the same kind of container.
                if matches!(self.table.shallow(&i.ty), It::Named(ref n) if n == aipl_syntax::SPAN_TYPE)
                {
                    return Ok(self.node(K::Index(Box::new(a), Box::new(i)), container, e));
                }
                let elem = match container {
                    It::Arr(t) => *t,
                    It::Prim(Primitive::Str) => It::Prim(Primitive::Char),
                    It::Dict(k, v) => {
                        self.agree(&i.ty, &k).or_else(|m| {
                            reject(format!("the key does not fit the dict ({m})"), &i.span)
                        })?;
                        *v
                    }
                    _ => return gap("indexing this type", &e.span),
                };
                if !matches!(self.table.shallow(&a.ty), It::Dict(..)) {
                    self.index_operand(&i)?;
                }
                self.node(
                    K::Index(Box::new(a), Box::new(i)),
                    It::Opt(Box::new(elem)),
                    e,
                )
            }
            ExprKind::Slice(a, start, end) => {
                let a = self.expr(a)?;
                let container = self.known(&a.ty, &a.span)?;
                if !matches!(container, It::Arr(_) | It::Prim(Primitive::Str)) {
                    return reject("only an array or str can be sliced", &a.span);
                }
                let start = self.expr(start)?;
                self.index_operand(&start)?;
                let end = match end {
                    Some(end) => {
                        let end = self.expr(end)?;
                        self.index_operand(&end)?;
                        Some(Box::new(end))
                    }
                    None => None,
                };
                self.node(K::Slice(Box::new(a), Box::new(start), end), container, e)
            }
            ExprKind::SetLit(items, order) => {
                let elem = self.fresh();
                let mut out = Vec::new();
                for item in items {
                    let item = self.expr(item)?;
                    self.agree(&item.ty, &elem)
                        .or_else(|m| reject(format!("set elements disagree ({m})"), &item.span))?;
                    out.push(item);
                }
                if items.is_empty() {
                    // `#{}` is a set or a dict: its position says which.
                    let ty = self.table.fresh(Kind::Brace);
                    return Ok(self.node(K::SetLit(out, PinnedOrder::Unordered), ty, e));
                }
                let ord = match PinnedOrder::of(*order) {
                    Some(k) => Ord::Known(k),
                    None => self.table.fresh_order(),
                };
                let pinned = PinnedOrder::of(*order).unwrap_or(PinnedOrder::Unordered);
                self.node(K::SetLit(out, pinned), It::Set(Box::new(elem), ord), e)
            }
            ExprKind::TupleLit(items) => {
                let mut out = Vec::new();
                for item in items {
                    out.push(self.expr(item)?);
                }
                let tys: Vec<It> = out.iter().map(|i| i.ty.clone()).collect();
                let template = format!("{}{}", crate::check::TUPLE_TEMPLATE, tys.len());
                self.node(K::TupleLit(out), It::Gen(template, tys), e)
            }
            ExprKind::Shim(effect, bindings, body) => {
                let body = self.expr(body)?;
                let ty = body.ty.clone();
                self.node(
                    K::Shim(effect.clone(), bindings.clone(), Box::new(body)),
                    ty,
                    e,
                )
            }
            ExprKind::DictLit(pairs) => {
                let (k, v) = (self.fresh(), self.fresh());
                let mut out = Vec::new();
                for (pk, pv) in pairs {
                    let (pk, pv) = (self.expr(pk)?, self.expr(pv)?);
                    self.agree(&pk.ty, &k)
                        .or_else(|m| reject(format!("dict keys disagree ({m})"), &pk.span))?;
                    self.agree(&pv.ty, &v)
                        .or_else(|m| reject(format!("dict values disagree ({m})"), &pv.span))?;
                    out.push((pk, pv));
                }
                self.node(K::DictLit(out), It::Dict(Box::new(k), Box::new(v)), e)
            }
            ExprKind::IfLet(arm, scrut, else_src) => {
                let scrut = self.expr(scrut)?;
                let st = self.known(&scrut.ty, &scrut.span)?;
                let (pattern, binds) = self.pattern(&arm.pattern, &st, &arm.span)?;
                let (saved, body) = self.scoped(binds, |p| p.expr(&arm.body))?;
                self.restore(saved);
                let otherwise = self.expr(else_src)?;
                // An `if let` with no `else` has a unit one, and is run for effect.
                let ty = if aipl_syntax::diverges(&arm.body) {
                    otherwise.ty.clone()
                } else if aipl_syntax::diverges(else_src) {
                    body.ty.clone()
                } else if matches!(self.table.shallow(&otherwise.ty), It::Unit) {
                    It::Unit
                } else {
                    self.agree(&body.ty, &otherwise.ty).or_else(|m| {
                        reject(
                            format!("the branches of an `if let` disagree ({m})"),
                            &e.span,
                        )
                    })?;
                    body.ty.clone()
                };
                let arm = TMatchArm {
                    pattern,
                    body,
                    span: arm.span.clone(),
                };
                self.node(
                    K::IfLet(Box::new(arm), Box::new(scrut), Box::new(otherwise)),
                    ty,
                    e,
                )
            }
            ExprKind::Try(x) => {
                let x = self.expr(x)?;
                let ret = self.known(&self.ret.clone(), &e.span)?;
                let ty = match (self.known(&x.ty, &x.span)?, ret) {
                    (It::Opt(t), It::Opt(_)) => *t,
                    (It::Res(t, err), It::Res(_, want)) => {
                        self.table.unify(&err, &want).or_else(|m| {
                            reject(format!("`?` propagates a different error ({m})"), &e.span)
                        })?;
                        *t
                    }
                    _ => {
                        return reject(
                            "`?` needs an optional or result in a function returning one",
                            &e.span,
                        )
                    }
                };
                self.node(K::Try(Box::new(x)), ty, e)
            }
            ExprKind::Match(scrut, arms) => {
                let scrut = self.expr(scrut)?;
                let st = self.known(&scrut.ty, &scrut.span)?;
                let result = self.fresh();
                let mut out = Vec::new();
                for arm in arms {
                    let (pattern, binds) = self.pattern(&arm.pattern, &st, &arm.span)?;
                    let (saved, body) = self.scoped(binds, |p| p.expr(&arm.body))?;
                    self.restore(saved);
                    if !aipl_syntax::diverges(&arm.body) {
                        self.agree(&body.ty, &result).or_else(|m| {
                            reject(format!("the arms of a `match` disagree ({m})"), &arm.span)
                        })?;
                    }
                    out.push(TMatchArm {
                        pattern,
                        body,
                        span: arm.span.clone(),
                    });
                }
                self.node(K::Match(Box::new(scrut), out), result, e)
            }
            other => return gap(format!("expression form {}", form_name(other)), &e.span),
        })
    }

    /// Fresh variables for a template's type parameters, in order and by name.
    fn fresh_args(
        &mut self,
        tvs: &[aipl_syntax::ast::TypeParam],
    ) -> (Vec<It>, HashMap<String, It>) {
        let vars: Vec<It> = tvs.iter().map(|_| self.fresh()).collect();
        let map = tvs
            .iter()
            .map(|t| t.name.clone())
            .zip(vars.iter().cloned())
            .collect();
        (vars, map)
    }

    /// The element types of a tuple type, if `t` is one: a tuple template applied to its elements,
    /// or the struct a tuple of known elements lowers to.
    fn tuple_elems(&mut self, t: &It, span: &Span) -> Res<Option<Vec<It>>> {
        match t {
            It::Gen(base, args) if base.starts_with(crate::check::TUPLE_TEMPLATE) => {
                Ok(Some(args.clone()))
            }
            It::Named(n) if n.starts_with("__tuple$") => {
                let Some(fields) = self.decls.structs.get(n).cloned() else {
                    return Ok(None);
                };
                let mut out = Vec::new();
                for (_, ty, _) in &fields {
                    out.push(self.lower(ty, &HashMap::new(), span)?);
                }
                Ok(Some(out))
            }
            _ => Ok(None),
        }
    }

    /// The type of `owner.field`, for a struct or an application of a generic one.
    fn field_type(&mut self, owner: &It, field: &str, span: &Span) -> Res<It> {
        if let Some(elems) = self.tuple_elems(owner, span)? {
            let slot = field
                .strip_prefix('_')
                .and_then(|n| n.parse::<usize>().ok());
            return match slot.and_then(|i| elems.get(i)) {
                Some(t) => Ok(t.clone()),
                None => reject(format!("a tuple has no field {field:?}"), span),
            };
        }
        let (fields, map): (Vec<(String, Type, bool)>, HashMap<String, It>) = match owner {
            It::Named(n) => match self.decls.structs.get(n) {
                Some(f) => (f.clone(), HashMap::new()),
                None => return reject(format!("{n:?} is not a struct"), span),
            },
            It::Gen(n, args) => match self.decls.generic_structs.get(n) {
                Some(t) => (
                    t.fields
                        .iter()
                        .map(|f| (f.name.text.clone(), f.ty.clone(), f.default.is_some()))
                        .collect(),
                    t.type_vars
                        .iter()
                        .map(|v| v.name.clone())
                        .zip(args.iter().cloned())
                        .collect(),
                ),
                None => return reject(format!("{n:?} is not a struct"), span),
            },
            other => {
                return reject(
                    format!("no field {field:?} on {}", self.table.show(other)),
                    span,
                )
            }
        };
        let Some((_, fty, _)) = fields.iter().find(|(n, _, _)| n == field) else {
            return reject(format!("no field {field:?}"), span);
        };
        self.lower(fty, &map, span)
    }

    /// A name that is not a local: a function used as a value, or a constructor without arguments.
    fn reference(&mut self, e: &Expr, name: &str) -> Res<TExpr<It>> {
        if let Some(variant) = self.decls.ctors.get(name).cloned() {
            let bare = name.split('@').next().unwrap_or(name);
            let payload = self.decls.variants[&variant]
                .iter()
                .find(|(c, _)| c == bare)
                .map(|(_, p)| p.len())
                .unwrap_or(0);
            // A case with a payload, named without arguments, is its tag.
            let ty = if payload == 0 {
                It::Named(variant)
            } else {
                It::Case(Box::new(It::Named(variant)))
            };
            return Ok(self.node(TExprKind::Ident(name.to_string()), ty, e));
        }
        if let Some(base) = self.decls.generic_ctors.get(name).cloned() {
            let tmpl = self.decls.generic_variants[&base].clone();
            let (vars, _) = self.fresh_args(&tmpl.type_vars);
            let bare = name.split('@').next().unwrap_or(name);
            let payload = tmpl
                .cases
                .iter()
                .find(|c| c.name.text == bare)
                .map(|c| c.payload.len())
                .unwrap_or(0);
            let v = It::Gen(base, vars);
            let ty = if payload == 0 {
                v
            } else {
                It::Case(Box::new(v))
            };
            return Ok(self.node(TExprKind::Ident(name.to_string()), ty, e));
        }
        if let Some(sig) = self.decls.sigs.get(name).cloned() {
            let sig = sig.sig;
            let (ptys, ret, _) = self.instantiate(&sig, &e.span)?;
            return Ok(self.node(
                TExprKind::Ident(name.to_string()),
                It::Fn(ptys, Box::new(ret)),
                e,
            ));
        }
        reject(format!("{name:?} is not defined"), &e.span)
    }

    /// A callee's signature with its type variables (and anonymous `any`s) made fresh: its
    /// parameter types, its result, and the type arguments in order.
    fn instantiate(&mut self, sig: &Signature, span: &Span) -> Res<(Vec<It>, It, Vec<It>)> {
        let mut instance = HashMap::new();
        let mut type_args = Vec::new();
        for tv in &sig.type_vars {
            let v = self.fresh();
            instance.insert(tv.name.clone(), v.clone());
            type_args.push(v);
        }
        let before = std::mem::replace(&mut self.instantiating, true);
        self.anonymous_vars.clear();
        let lowered = (|| {
            let mut ptys = Vec::new();
            for param in &sig.params {
                ptys.push(self.lower(&param.ty, &instance, span)?);
            }
            let ret = if sig.is_mutating() {
                ptys[0].clone()
            } else {
                match &sig.return_ty {
                    Some(t) => self.lower(t, &instance, span)?,
                    None => It::Unit,
                }
            };
            Ok((ptys, ret))
        })();
        self.instantiating = before;
        let (ptys, ret) = lowered?;
        type_args.extend(std::mem::take(&mut self.anonymous_vars));
        Ok((ptys, ret, type_args))
    }

    /// The type two operands of one operator share: the wider of two integers, else what they
    /// agree on.
    fn join_operands(&mut self, l: &It, r: &It, span: &Span) -> Res<It> {
        let (a, b) = (self.table.shallow(l), self.table.shallow(r));
        if let (It::Prim(x), It::Prim(y)) = (&a, &b) {
            if x.widens_to(*y) {
                return Ok(b);
            }
            if y.widens_to(*x) {
                return Ok(a);
            }
        }
        self.agree(&a, &b).or_else(|m| {
            reject(
                format!(
                    "the operands are {} and {} ({m})",
                    self.table.show(&a),
                    self.table.show(&b)
                ),
                span,
            )
        })?;
        Ok(a)
    }

    /// `t`, with its variables followed, if its outermost shape is known. A rule that has to look
    /// at a type's shape before it can say anything cannot run on a type nothing has constrained
    /// yet; that is a gap in the pinner, not an error in the program.
    fn known(&self, t: &It, span: &Span) -> Res<It> {
        match self.table.shallow(t) {
            It::Var(_) => gap("a rule needs a type that is not known yet", span),
            t => Ok(t),
        }
    }

    /// Bring `bindings` into scope for `f`, handing back what they shadowed.
    fn scoped<R>(
        &mut self,
        bindings: Vec<(String, It)>,
        f: impl FnOnce(&mut Self) -> Res<R>,
    ) -> Res<(Vec<(String, Option<Binding>)>, R)> {
        let saved = bindings
            .into_iter()
            .map(|(name, ty)| {
                let old = self
                    .env
                    .insert(name.clone(), Binding { ty, mutable: false });
                (name, old)
            })
            .collect::<Vec<_>>();
        match f(self) {
            Ok(r) => Ok((saved, r)),
            Err(e) => {
                self.restore(saved);
                Err(e)
            }
        }
    }

    fn restore(&mut self, saved: Vec<(String, Option<Binding>)>) {
        for (name, old) in saved.into_iter().rev() {
            match old {
                Some(b) => self.env.insert(name, b),
                None => self.env.remove(&name),
            };
        }
    }

    /// An index or length operand: some integer.
    fn index_operand(&mut self, i: &TExpr<It>) -> Res<()> {
        match self.table.shallow(&i.ty) {
            // A bare literal takes the position type.
            It::Var(_) => self
                .table
                .unify(&i.ty, &It::Prim(Primitive::U64))
                .or_else(|m| reject(format!("an index must be an integer ({m})"), &i.span)),
            It::Prim(p) if p.is_int() => Ok(()),
            other => reject(
                format!(
                    "an index must be an integer, got {}",
                    self.table.show(&other)
                ),
                &i.span,
            ),
        }
    }

    /// The payload types of the case `bare` of the scrutinee type `st`.
    fn ctor_payload(&mut self, st: &It, bare: &str, span: &Span) -> Res<Vec<It>> {
        Ok(match st {
            It::Opt(t) if bare == "some" => vec![(**t).clone()],
            It::Opt(_) if bare == "none" => Vec::new(),
            It::Res(t, _) if bare == "ok" => vec![(**t).clone()],
            It::Res(_, e) if bare == "err" => vec![(**e).clone()],
            It::Gen(v, targs) if self.decls.generic_variants.contains_key(v) => {
                let tmpl = self.decls.generic_variants[v].clone();
                let map: HashMap<String, It> = tmpl
                    .type_vars
                    .iter()
                    .map(|t| t.name.clone())
                    .zip(targs.iter().cloned())
                    .collect();
                let Some(case) = tmpl.cases.iter().find(|c| c.name.text == bare) else {
                    return reject(format!("{v:?} has no case {bare:?}"), span);
                };
                let mut out = Vec::new();
                for slot in &case.payload {
                    out.push(self.lower(&slot.ty, &map, span)?);
                }
                out
            }
            It::Named(v) if self.decls.variants.contains_key(v) => {
                let cases = self.decls.variants[v].clone();
                let Some((_, tys)) = cases.iter().find(|(c, _)| c == bare) else {
                    return reject(format!("{v:?} has no case {bare:?}"), span);
                };
                let mut out = Vec::new();
                for t in tys {
                    out.push(self.lower(t, &HashMap::new(), span)?);
                }
                out
            }
            _ => return gap("a pattern on this scrutinee", span),
        })
    }

    /// Whether `name` spells a case of the scrutinee type `st`.
    fn is_case_of(&self, st: &It, name: &str) -> bool {
        let bare = name.split('@').next().unwrap_or(name);
        match st {
            It::Opt(_) => matches!(bare, "some" | "none"),
            It::Res(..) => matches!(bare, "ok" | "err"),
            It::Named(v) => self
                .decls
                .variants
                .get(v)
                .is_some_and(|cs| cs.iter().any(|(c, _)| c == bare)),
            It::Gen(v, _) => self
                .decls
                .generic_variants
                .get(v)
                .is_some_and(|t| t.cases.iter().any(|c| c.name.text == bare)),
            _ => false,
        }
    }

    /// The pattern as a typed one and the names it binds, against a scrutinee of type `st`.
    fn pattern(
        &mut self,
        pattern: &Pattern,
        st: &It,
        span: &Span,
    ) -> Res<(TPattern<It>, Vec<(String, It)>)> {
        let st = &self.table.shallow(st);
        Ok(match pattern {
            Pattern::Wildcard => (TPattern::Wildcard, Vec::new()),
            Pattern::Str(s) => (TPattern::Str(s.clone()), Vec::new()),
            Pattern::Char(c) => (TPattern::Char(*c), Vec::new()),
            Pattern::Int(n) => {
                if !matches!(st, It::Var(_) | It::Prim(_)) {
                    return reject("an integer pattern needs an integer scrutinee", span);
                }
                (TPattern::Int(*n), Vec::new())
            }
            Pattern::Bind(name) => {
                if self.is_case_of(st, name) {
                    (TPattern::Bind(name.clone()), Vec::new())
                } else {
                    (
                        TPattern::Bind(name.clone()),
                        vec![(name.clone(), st.clone())],
                    )
                }
            }
            Pattern::Ctor {
                name,
                bindings,
                ignore_payload,
            } => {
                let bare = name.split('@').next().unwrap_or(name);
                let payload = self.ctor_payload(st, bare, span)?;
                // The success of a `!E` result carries nothing to bind.
                let void_ok = bare == "ok"
                    && bindings.is_empty()
                    && matches!(payload.as_slice(), [t] if self.table.shallow(t) == It::Unit);
                if !*ignore_payload && !void_ok && bindings.len() != payload.len() {
                    return reject(format!("{bare:?} binds the wrong number of values"), span);
                }
                let binds = if *ignore_payload || void_ok {
                    Vec::new()
                } else {
                    bindings.iter().cloned().zip(payload).collect()
                };
                (
                    TPattern::Ctor {
                        name: name.clone(),
                        bindings: bindings.clone(),
                        ignore_payload: *ignore_payload,
                    },
                    binds,
                )
            }
            Pattern::Nested { name, args } => {
                let bare = name.split('@').next().unwrap_or(name);
                let payload = self.ctor_payload(st, bare, span)?;
                if args.len() != payload.len() {
                    return reject(format!("{bare:?} has {} slot(s)", payload.len()), span);
                }
                let (mut subs, mut binds) = (Vec::new(), Vec::new());
                for (sub, ty) in args.iter().zip(&payload) {
                    let (p, b) = self.pattern(sub, ty, span)?;
                    subs.push(p);
                    binds.extend(b);
                }
                (
                    TPattern::Nested {
                        name: name.clone(),
                        args: subs,
                    },
                    binds,
                )
            }
            Pattern::Tuple(ps) => {
                let mut subs = Vec::new();
                let mut binds = Vec::new();
                for (i, sub) in ps.iter().enumerate() {
                    let ty = self.field_type(st, &format!("_{i}"), span)?;
                    let (p, b) = self.pattern(sub, &ty, span)?;
                    subs.push(p);
                    binds.extend(b);
                }
                (TPattern::Tuple(subs), binds)
            }
            Pattern::ArrayNested(ps) => {
                let elem = self.sequence_elem(st, span)?;
                let (mut subs, mut binds) = (Vec::new(), Vec::new());
                for sub in ps {
                    let (p, b) = self.pattern(sub, &elem, span)?;
                    subs.push(p);
                    binds.extend(b);
                }
                (TPattern::ArrayNested(subs), binds)
            }
            Pattern::Array(items) | Pattern::ArrayRest(items) => {
                let elem = self.sequence_elem(st, span)?;
                let (mut typed, mut binds) = (Vec::new(), Vec::new());
                for item in items {
                    match &item.kind {
                        // A name binds the element at its position; `_` ignores it.
                        ExprKind::Ident(name) => {
                            if name != "_" {
                                binds.push((name.clone(), elem.clone()));
                            }
                            typed.push(self.node(
                                TExprKind::Ident(name.clone()),
                                elem.clone(),
                                item,
                            ));
                        }
                        _ => {
                            let lit = self.expr(item)?;
                            self.agree(&lit.ty, &elem).or_else(|m| {
                                reject(format!("a pattern literal does not fit ({m})"), &item.span)
                            })?;
                            typed.push(lit);
                        }
                    }
                }
                let rest = matches!(pattern, Pattern::ArrayRest(_));
                (
                    if rest {
                        TPattern::ArrayRest(typed)
                    } else {
                        TPattern::Array(typed)
                    },
                    binds,
                )
            }
        })
    }

    /// The element an array or `str` pattern matches at each position.
    fn sequence_elem(&self, st: &It, span: &Span) -> Res<It> {
        match st {
            It::Arr(t) => Ok((**t).clone()),
            It::Prim(Primitive::Str) => Ok(It::Prim(Primitive::Char)),
            _ => reject("an array pattern needs an array or str scrutinee", span),
        }
    }

    /// `inst` is a synthesized instance of the generic `base`: its arguments are whatever make the
    /// template's fields (or payloads) the instance's.
    fn link_instance(&mut self, inst: &str, base: &str, targs: &[It]) -> Result<(), String> {
        let is_instance = inst.split('$').next() == Some(base)
            || (base.starts_with(crate::check::TUPLE_TEMPLATE) && inst.starts_with("__tuple$"));
        if !is_instance {
            return Err(format!("{inst} is not an instance of {base}"));
        }
        // A recursive type's fields name the application being linked; once it is under way the
        // question is answered.
        let key = format!(
            "{inst}|{base}|{:?}",
            targs
                .iter()
                .map(|a| self.table.shallow(a))
                .collect::<Vec<_>>()
        );
        if !self.linking.insert(key.clone()) {
            return Ok(());
        }
        let result = self.link_fields(inst, base, targs);
        self.linking.remove(&key);
        result
    }

    fn link_fields(&mut self, inst: &str, base: &str, targs: &[It]) -> Result<(), String> {
        if base.starts_with(crate::check::TUPLE_TEMPLATE) {
            let elems = self
                .tuple_elems(&It::Named(inst.to_string()), &Span::default())
                .map_err(|_| "a tuple element".to_string())?
                .ok_or_else(|| format!("{inst} is not a known tuple"))?;
            if elems.len() != targs.len() {
                return Err("tuples of different sizes".into());
            }
            for (got, want) in elems.iter().zip(targs) {
                self.agree(got, want)?;
            }
            return Ok(());
        }
        let span = Span::default();
        let pairs: Vec<(Type, Type)> = if let (Some(t), Some(fields)) = (
            self.decls.generic_structs.get(base),
            self.decls.structs.get(inst),
        ) {
            t.fields
                .iter()
                .zip(fields)
                .map(|(tf, (_, ity, _))| (tf.ty.clone(), ity.clone()))
                .collect()
        } else if let (Some(t), Some(cases)) = (
            self.decls.generic_variants.get(base),
            self.decls.variants.get(inst),
        ) {
            t.cases
                .iter()
                .zip(cases)
                .flat_map(|(tc, (_, ipay))| {
                    tc.payload.iter().map(|p| p.ty.clone()).zip(ipay.clone())
                })
                .collect()
        } else {
            return Err(format!("{inst} is not a known instance of {base}"));
        };
        let tvs = self
            .decls
            .generic_structs
            .get(base)
            .map(|t| t.type_vars.clone())
            .or_else(|| {
                self.decls
                    .generic_variants
                    .get(base)
                    .map(|t| t.type_vars.clone())
            })
            .unwrap_or_default();
        let map: HashMap<String, It> = tvs
            .iter()
            .map(|t| t.name.clone())
            .zip(targs.iter().cloned())
            .collect();
        for (template_ty, instance_ty) in pairs {
            let want = self
                .lower(&template_ty, &map, &span)
                .map_err(|_| "a field type".to_string())?;
            let got = self
                .lower(&instance_ty, &HashMap::new(), &span)
                .map_err(|_| "a field type".to_string())?;
            self.agree(&got, &want)?;
        }
        Ok(())
    }

    fn call(
        &mut self,
        e: &Expr,
        callee: &Callee,
        args: &[Expr],
        method_style: bool,
    ) -> Res<TExpr<It>> {
        use TExprKind as K;
        let name = callee.name();
        let bool_ty = It::Prim(Primitive::Bool);
        let call = |args: Vec<TExpr<It>>, ret: It, p: &Self| {
            p.node(
                K::Call {
                    callee: callee.clone(),
                    type_args: Vec::new(),
                    args,
                    method_style,
                },
                ret,
                e,
            )
        };
        // A local that holds a function is called by its name.
        if let Some(Binding { ty, .. }) = self.env.get(name).cloned() {
            let It::Fn(ptys, ret) = self.table.shallow(&ty) else {
                return reject(format!("{name:?} is not a function"), &e.span);
            };
            if ptys.len() != args.len() {
                return reject(
                    format!("{name:?} takes {} argument(s)", ptys.len()),
                    &e.span,
                );
            }
            let mut typed = Vec::new();
            for (arg, pty) in args.iter().zip(&ptys) {
                let t = self.arg(arg, pty)?;
                self.coerce(&t.ty, pty, &t.span, &format!("an argument to {name:?}"))?;
                typed.push(t);
            }
            return Ok(call(typed, *ret, self));
        }
        match callee {
            Callee::Some if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                let ty = It::Opt(Box::new(x.ty.clone()));
                return Ok(call(vec![x], ty, self));
            }
            Callee::Ok | Callee::Err if args.len() <= 1 => {
                let x = args.first().map(|a| self.expr(a)).transpose()?;
                let other = self.fresh();
                let (ok, err) = match (callee, &x) {
                    (Callee::Ok, Some(x)) => (x.ty.clone(), other),
                    (Callee::Ok, None) => (It::Unit, other),
                    (_, Some(x)) => (other, x.ty.clone()),
                    (_, None) => return reject("\"err\" expects 1 argument", &e.span),
                };
                let ty = It::Res(Box::new(ok), Box::new(err));
                return Ok(call(x.into_iter().collect(), ty, self));
            }
            Callee::Narrow if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                if !matches!(
                    self.table.shallow(&x.ty),
                    It::Var(_)
                        | It::Prim(
                            Primitive::I8
                                | Primitive::I16
                                | Primitive::I32
                                | Primitive::I64
                                | Primitive::U8
                                | Primitive::U16
                                | Primitive::U32
                                | Primitive::U64
                        )
                ) {
                    return reject("\"narrow\" converts an integer", &args[0].span);
                }
                // The integer it converts to is whichever its position asks for.
                let target = self.table.fresh(Kind::Int);
                return Ok(call(vec![x], It::Opt(Box::new(target)), self));
            }
            Callee::ToFloat if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                let target = self.table.fresh(Kind::Float);
                return Ok(call(vec![x], target, self));
            }
            Callee::ToFixed if args.len() == 2 => {
                let x = self.expr(&args[0])?;
                let digits = self.expr(&args[1])?;
                self.index_operand(&digits)?;
                return Ok(call(vec![x, digits], It::Prim(Primitive::Str), self));
            }
            Callee::ToSet if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                let xt = self.known(&x.ty, &x.span)?;
                let elem = match (&xt, self.tuple_elems(&xt, &x.span)?) {
                    (_, Some(elems)) => {
                        let first = elems[0].clone();
                        for other in &elems[1..] {
                            self.agree(other, &first).or_else(|m| {
                                reject(
                                    format!("a tuple's elements must share a type ({m})"),
                                    &x.span,
                                )
                            })?;
                        }
                        first
                    }
                    (It::Prim(Primitive::Str), _) => It::Prim(Primitive::Char),
                    (It::Arr(t), _) => (**t).clone(),
                    _ => return gap("to_set on this receiver", &e.span),
                };
                let order = self.table.fresh_order();
                return Ok(call(vec![x], It::Set(Box::new(elem), order), self));
            }
            Callee::ToArray if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                let xt = self.known(&x.ty, &x.span)?;
                if let Some(elems) = self.tuple_elems(&xt, &x.span)? {
                    let first = elems[0].clone();
                    for other in &elems[1..] {
                        self.agree(other, &first).or_else(|m| {
                            reject(
                                format!("a tuple's elements must share a type ({m})"),
                                &x.span,
                            )
                        })?;
                    }
                    return Ok(call(vec![x], It::Arr(Box::new(first)), self));
                }
                let It::Set(elem, _) = xt else {
                    return gap("to_array on this receiver", &e.span);
                };
                return Ok(call(vec![x], It::Arr(elem), self));
            }
            Callee::LogicalNot if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                self.coerce(&x.ty, &bool_ty, &x.span, "the operand of `!`")?;
                return Ok(call(vec![x], bool_ty, self));
            }
            _ => {}
        }
        // Operators: after import resolution an operator is a call to its builtin.
        let op = aipl_syntax::binop_for_builtin(callee);
        let arith = matches!(
            callee,
            Callee::WrappingAdd
                | Callee::SaturatingAdd
                | Callee::WrappingSub
                | Callee::SaturatingSub
                | Callee::WrappingMul
        ) || matches!(
            op,
            Some(aipl_syntax::ast::BinOp::Div | aipl_syntax::ast::BinOp::Rem)
        );
        let float = aipl_syntax::float_operation_yields_bool(callee);
        if args.len() == 2 && (op.is_some() || arith || float.is_some()) {
            let l = self.expr(&args[0])?;
            let r = self.expr(&args[1])?;
            let operand = self.join_operands(&l.ty, &r.ty, &e.span)?;
            let logical = matches!(
                op,
                Some(aipl_syntax::ast::BinOp::And | aipl_syntax::ast::BinOp::Or)
            );
            if logical {
                self.agree(&operand, &bool_ty).or_else(|m| {
                    reject(format!("a logical operand must be bool ({m})"), &e.span)
                })?;
            }
            let ret = if arith {
                match self.table.shallow(&operand) {
                    It::Prim(p) if p.is_int() => operand,
                    It::Var(_) => operand,
                    other => {
                        return reject(
                            format!("arithmetic needs integers, got {}", self.table.show(&other)),
                            &e.span,
                        )
                    }
                }
            } else if float == Some(false) {
                operand
            } else {
                bool_ty
            };
            return Ok(call(vec![l, r], ret, self));
        }
        // `starts_with`/`ends_with`/`contains` and `extend`: the second argument is variadic over
        // the receiver's sequence (the sequence, one element, or an optional element).
        let at_arity = usize::from(matches!(callee, Callee::StartsWithAt(_)));
        if (callee.seq_shape().is_some() && args.len() == 2 + at_arity)
            || (matches!(callee, Callee::Extend) && args.len() == 2)
        {
            let recv = self.expr(&args[0])?;
            let seq = match self.table.shallow(&recv.ty) {
                t @ (It::Prim(Primitive::Str) | It::Arr(_)) => Some(t),
                _ => None,
            };
            if let Some(seq) = seq {
                let pat = self.expr(&args[1])?;
                self.variadic_arg(&seq, &pat.ty)
                    .or_else(|m| reject(format!("the argument of {name:?}: {m}"), &args[1].span))?;
                let mut typed = vec![recv, pat];
                if at_arity == 1 {
                    let at = self.expr(&args[2])?;
                    self.index_operand(&at)?;
                    typed.push(at);
                }
                let ret = if matches!(callee, Callee::Extend) {
                    typed[0].ty.clone()
                } else {
                    bool_ty
                };
                return Ok(call(typed, ret, self));
            }
        }
        // A constructor of a generic variant: its arguments pin the type arguments.
        if let Some(base) = self.decls.generic_ctors.get(name).cloned() {
            let tmpl = self.decls.generic_variants[&base].clone();
            let bare = name.split('@').next().unwrap_or(name);
            let Some(case) = tmpl.cases.iter().find(|c| c.name.text == bare) else {
                return reject(format!("{base:?} has no case {bare:?}"), &e.span);
            };
            if case.payload.len() != args.len() {
                return reject(
                    format!(
                        "constructor {bare:?} expects {} argument(s)",
                        case.payload.len()
                    ),
                    &e.span,
                );
            }
            let (vars, map) = self.fresh_args(&tmpl.type_vars);
            let mut typed = Vec::new();
            for (arg, slot) in args.iter().zip(&case.payload) {
                let pty = self.lower(&slot.ty, &map, &arg.span)?;
                let t = self.arg(arg, &pty)?;
                self.coerce(&t.ty, &pty, &t.span, "a constructor argument")?;
                typed.push(t);
            }
            return Ok(call(typed, It::Gen(base, vars), self));
        }
        // A variant constructor `Case@Variant(..)`.
        if let Some(variant) = self.decls.ctors.get(name).cloned() {
            let bare = name.split('@').next().unwrap_or(name);
            let payload = self.decls.variants[&variant]
                .iter()
                .find(|(c, _)| c == bare)
                .map(|(_, p)| p.clone())
                .unwrap_or_default();
            if payload.len() != args.len() {
                return reject(
                    format!("constructor {bare:?} expects {} argument(s)", payload.len()),
                    &e.span,
                );
            }
            let mut typed = Vec::new();
            for (arg, pty) in args.iter().zip(&payload) {
                let pty = self.lower(pty, &HashMap::new(), &arg.span)?;
                let t = self.arg(arg, &pty)?;
                self.coerce(&t.ty, &pty, &t.span, "a constructor argument")?;
                typed.push(t);
            }
            return Ok(call(typed, It::Named(variant), self));
        }
        let Some(sig) = self.decls.sigs.get(name).cloned() else {
            return gap(format!("call to {name:?}"), &e.span);
        };
        let mut sig = sig.sig;
        if *callee == Callee::Map && !args.is_empty() {
            let first = self.expr(&args[0])?;
            match self.table.shallow(&first.ty) {
                It::Opt(_) => sig = optional_map_signature(&sig),
                It::Named(n) if n == aipl_syntax::SPAN_TYPE => sig = span_map_signature(&sig),
                _ => {}
            }
        }
        if sig.params.len() != args.len() {
            return reject(
                format!(
                    "{name:?} expects {} argument(s), got {}",
                    sig.params.len(),
                    args.len()
                ),
                &e.span,
            );
        }
        let (ptys, ret, type_args) = self.instantiate(&sig, &e.span)?;
        // Plain arguments first, so a closure is typed knowing what the others pinned.
        let mut slots: Vec<Option<TExpr<It>>> = vec![None; args.len()];
        for pass in 0..2 {
            for (i, arg) in args.iter().enumerate() {
                let is_closure = matches!(arg.kind, ExprKind::Lambda(..));
                if slots[i].is_some() || is_closure != (pass == 1) {
                    continue;
                }
                let typed = self.arg(arg, &ptys[i])?;
                let adapted = if sig.params[i].arity.is_variadic() {
                    self.variadic_arg(&ptys[i], &typed.ty)
                        .or_else(|m| reject(format!("an argument to {name:?}: {m}"), &arg.span))?;
                    true
                } else {
                    self.adapt_receiver(callee, &ptys[i], &typed.ty)
                        .or_else(|m| reject(format!("an argument to {name:?}: {m}"), &arg.span))?
                };
                if !adapted {
                    self.coerce(
                        &typed.ty,
                        &ptys[i],
                        &arg.span,
                        &format!("an argument to {name:?}"),
                    )?;
                }
                slots[i] = Some(typed);
            }
        }
        let typed_args: Vec<TExpr<It>> = slots.into_iter().map(|s| s.unwrap()).collect();
        Ok(self.node(
            K::Call {
                callee: callee.clone(),
                type_args,
                args: typed_args,
                method_style,
            },
            ret,
            e,
        ))
    }

    /// An argument, typed with the parameter type it lands in as its context. A lambda needs the
    /// context to type its own parameters.
    fn arg(&mut self, arg: &Expr, expected: &It) -> Res<TExpr<It>> {
        if let ExprKind::Lambda(params, body) = &arg.kind {
            let It::Fn(ptys, ret) = self.table.shallow(expected) else {
                return reject("a closure where no function is expected", &arg.span);
            };
            if ptys.len() != params.len() {
                return reject(
                    "the closure takes the wrong number of parameters",
                    &arg.span,
                );
            }
            let mut typed = Vec::new();
            let mut saved = Vec::new();
            for (param, pty) in params.iter().zip(&ptys) {
                // A written type is the parameter's; the position's must accept it.
                let pty = match &param.ty {
                    Some(t) => {
                        let written = self.lower(t, &HashMap::new(), &param.span)?;
                        self.agree(pty, &written).or_else(|m| {
                            reject(
                                format!("the closure parameter does not fit ({m})"),
                                &param.span,
                            )
                        })?;
                        written
                    }
                    None => pty.clone(),
                };
                let pty = &pty;
                saved.push((
                    param.name.clone(),
                    self.env.insert(
                        param.name.clone(),
                        Binding {
                            ty: pty.clone(),
                            mutable: false,
                        },
                    ),
                ));
                typed.push(TLambdaParam {
                    name: param.name.clone(),
                    ty: pty.clone(),
                    span: param.span.clone(),
                });
            }
            let saved_ret = std::mem::replace(&mut self.ret, (*ret).clone());
            let body = self.expr(body);
            self.ret = saved_ret;
            for (name, old) in saved.into_iter().rev() {
                match old {
                    Some(b) => self.env.insert(name, b),
                    None => self.env.remove(&name),
                };
            }
            let body = body?;
            self.coerce(&body.ty, &ret, &body.span, "the closure's result")?;
            let ty = It::Fn(ptys, ret);
            return Ok(self.node(TExprKind::Lambda(typed, Box::new(body)), ty, arg));
        }
        self.expr(arg)
    }
}

fn form_name(k: &ExprKind) -> String {
    let text = format!("{k:?}");
    text.split(['(', ' ', '{']).next().unwrap_or("").to_string()
}
