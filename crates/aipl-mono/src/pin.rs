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

use crate::check::{
    check_result_inspected, display, no_outer_assign, optional_map_signature, span_map_signature,
    undeclared_effect_message, Pos, ValueForm,
};
use aipl_syntax::ast::{
    Callee, Expr, ExprKind, Function, Item, Pattern, Primitive, Program, Signature, Type,
};
use aipl_syntax::typed::{
    PinnedOrder, TCaseParam, TExpr, TExprKind, TFieldDecl, TFunction, TItem, TLambdaParam,
    TMatchArm, TParam, TPattern, TProgram, TSignature, TStructDecl, TTypeParam, TVariantCase,
    TVariantDecl, Ty, TyRepr,
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
    /// A refinement (`T[] without []`). Invisible to every rule but the ones about refinements:
    /// [`Table::shallow`] looks through it.
    Without(Box<It>, aipl_syntax::ast::Exclusion),
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
    /// The type of an expression that never produces a value (`return`): if nothing asks it to be
    /// a type, it is [`Ty::Never`] rather than the default number.
    diverges: bool,
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
        self.slots.push(Slot {
            bound: None,
            kind,
            diverges: false,
        });
        It::Var(self.slots.len() - 1)
    }

    /// `t` with its outermost variable chain followed.
    fn shallow(&self, t: &It) -> It {
        let mut cur = t.clone();
        loop {
            match cur {
                It::Var(v) => match &self.slots[v].bound {
                    Some(next) => cur = next.clone(),
                    None => return It::Var(v),
                },
                It::Without(inner, _) => cur = *inner,
                other => return other,
            }
        }
    }

    /// `t` with its variables followed but its refinements kept.
    fn chase(&self, t: &It) -> It {
        let mut cur = t.clone();
        while let It::Var(v) = cur {
            match &self.slots[v].bound {
                Some(next) => cur = next.clone(),
                None => return It::Var(v),
            }
        }
        cur
    }

    /// What `t` excludes, through every refinement stacked on it.
    fn exclusions(&self, t: &It) -> Vec<aipl_syntax::ast::Exclusion> {
        let mut out = Vec::new();
        let mut cur = self.chase(t);
        while let It::Without(inner, x) = cur {
            out.push(x);
            cur = self.chase(&inner);
        }
        out
    }

    fn occurs(&self, v: usize, t: &It) -> bool {
        match self.shallow(t) {
            It::Var(w) => v == w,
            It::Unit | It::Prim(_) | It::Named(_) | It::Rigid(_) => false,
            It::Case(t) | It::Opt(t) | It::Arr(t) | It::Set(t, _) => self.occurs(v, &t),
            It::Dict(a, b) | It::Res(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            It::Gen(_, args) => args.iter().any(|a| self.occurs(v, a)),
            It::Without(t, _) => self.occurs(v, &t),
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
                if self.slots[v].diverges {
                    self.slots[*w].diverges = true;
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
        // A variable takes the type as written, refinements and all; structure is compared
        // through them.
        let (ca, cb) = (self.chase(a), self.chase(b));
        match (&ca, &cb) {
            (It::Var(x), It::Var(y)) if x == y => return Ok(()),
            (It::Var(x), _) => return self.bind(*x, cb),
            (_, It::Var(y)) => return self.bind(*y, ca),
            _ => {}
        }
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
            It::Without(t, _) => self.resolve(&t),
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
                Kind::Any if self.slots[v].diverges => Ty::Never,
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
    /// Every signature as declared, for validating written types (`check::validate_type`).
    plain_sigs: HashMap<String, Signature>,
}

impl Decls {
    /// Whether `t`, as written, may appear in a declaration or annotation.
    fn validate(&self, t: &Type, type_params: &[String], ctx: &str, at: &Span) -> Res<()> {
        crate::check::validate_type(
            &crate::check::DeclTables {
                structs: &self.structs,
                variants: &self.variants,
                ctors: &self.ctors,
                generic_structs: &self.generic_structs,
                generic_variants: &self.generic_variants,
                generic_ctors: &self.generic_ctors,
                sigs: &self.plain_sigs,
            },
            t,
            type_params,
            ctx,
        )
        .map_err(|e| Failure::Rejected(e.or_at(at.clone())))
    }
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
    /// Each generic call's type arguments, checked once the body is done: see [`CallRecord`].
    calls: Vec<CallRecord>,
    /// Generic struct constructions, whose type arguments must be pinned by their fields.
    constructions: Vec<(String, Vec<It>, Span)>,
    /// The bound of each of this function's type variables.
    own_bounds: HashMap<String, aipl_syntax::ast::Bound>,
    /// Type arguments written at the call about to be typed (`f::<u8>(x)`).
    explicit_args: Option<Vec<Type>>,
    /// The name of the function being typed, for messages.
    fn_name: String,
    /// Collection types to be validated once their elements are known (`validate_type`).
    validations: Vec<(It, Span)>,
    /// The effects the code being typed may perform: the function's own, plus those of the
    /// `shim`s around it.
    effects: Vec<String>,
    /// Typing a `.test` or `.benchmark` block, where a failed `?` fails the test: the block
    /// returns nothing, so the operand's shape is the only constraint.
    in_test: bool,
}

/// A generic call's type arguments, held until the whole body has been typed.
struct CallRecord {
    callee: String,
    names: Vec<String>,
    bounds: Vec<aipl_syntax::ast::Bound>,
    vars: Vec<It>,
    /// Whether the variable appears in no parameter, so only the result's position can say what it
    /// is.
    result_only: Vec<bool>,
    defaults: Vec<Option<Type>>,
    span: Span,
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
        plain_sigs: HashMap::new(),
    };
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                decls
                    .sigs
                    .insert(f.name.text.clone(), Sig { sig: f.sig.clone() });
                decls.plain_sigs.insert(f.name.text.clone(), f.sig.clone());
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
                Ok(mut t) => {
                    if let Some(block) = &f.test_body {
                        match pin_test_body(&decls, f, block) {
                            Ok(body) => t.test_body = Some(body),
                            Err(e) => failures.push((format!("{}.test", f.name.text), e)),
                        }
                    }
                    items.push(TItem::Fn(t));
                }
                Err(e) => failures.push((f.name.text.clone(), e)),
            },
            Item::Struct(s) if s.type_vars.iter().any(|tp| tp.default.is_some()) => {
                failures.push((
                    s.name.text.clone(),
                    Failure::Rejected(crate::check::defaults_only_on_functions("struct", &s.name)),
                ));
            }
            Item::Variant(v) if v.type_vars.iter().any(|tp| tp.default.is_some()) => {
                failures.push((
                    v.name.text.clone(),
                    Failure::Rejected(crate::check::defaults_only_on_functions("variant", &v.name)),
                ));
            }
            Item::Struct(s) if !s.is_generic() && declaration_error(&decls, item).is_some() => {
                failures.push((
                    s.name.text.clone(),
                    declaration_error(&decls, item).unwrap(),
                ));
            }
            Item::Variant(v) if !v.is_generic() && declaration_error(&decls, item).is_some() => {
                failures.push((
                    v.name.text.clone(),
                    declaration_error(&decls, item).unwrap(),
                ));
            }
            Item::Struct(s) => match pin_struct(&decls, s) {
                Ok(Some(t)) => items.push(TItem::Struct(t)),
                Ok(None) => {}
                Err(e) => failures.push((s.name.text.clone(), e)),
            },
            Item::Variant(v) => match pin_variant(&decls, v) {
                Ok(Some(t)) => items.push(TItem::Variant(t)),
                Ok(None) => {}
                Err(e) => failures.push((v.name.text.clone(), e)),
            },
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

/// The first written type in a concrete struct or variant declaration that may not appear there.
fn declaration_error(decls: &Decls, item: &Item) -> Option<Failure> {
    match item {
        Item::Struct(s) => s.fields.iter().find_map(|f| {
            decls
                .validate(
                    &f.ty,
                    &[],
                    &format!("struct {:?} field {:?}", s.name.text, f.name.text),
                    &s.name.span,
                )
                .err()
        }),
        Item::Variant(v) => v.cases.iter().find_map(|c| {
            c.payload.iter().find_map(|slot| {
                decls
                    .validate(
                        &slot.ty,
                        &[],
                        &format!("variant {:?} case {:?} payload", v.name.text, c.name.text),
                        &c.name.span,
                    )
                    .err()
            })
        }),
        _ => None,
    }
}

/// A struct's declaration with its field defaults typed. A generic struct's defaults wait for an
/// instance to meet their field's type, so they are left out.
fn pin_struct(decls: &Decls, s: &aipl_syntax::ast::StructDecl) -> Res<Option<TStructDecl>> {
    let Some(type_vars) = s
        .type_vars
        .iter()
        .map(pin_type_param)
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let mut fields = Vec::new();
    for f in &s.fields {
        let Some(ty) = declared(&f.ty) else {
            return Ok(None);
        };
        let default = match &f.default {
            Some(d) if !s.is_generic() => {
                let what = format!(
                    "default for struct {:?} field {:?}",
                    s.name.text, f.name.text
                );
                Some(pin_slot_default(decls, &s.name.text, &f.ty, d, &[], &what)?)
            }
            _ => None,
        };
        fields.push(TFieldDecl {
            name: f.name.clone(),
            ty,
            default,
        });
    }
    Ok(Some(TStructDecl {
        name: s.name.clone(),
        doc: s.doc.clone(),
        type_vars,
        fields,
    }))
}

/// A variant's declaration with its payload defaults typed; each sees the named slots before it.
fn pin_variant(decls: &Decls, v: &aipl_syntax::ast::VariantDecl) -> Res<Option<TVariantDecl>> {
    let Some(type_vars) = v
        .type_vars
        .iter()
        .map(pin_type_param)
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let mut cases = Vec::new();
    for c in &v.cases {
        let mut earlier: Vec<(String, Type)> = Vec::new();
        let mut payload = Vec::new();
        for slot in &c.payload {
            let Some(ty) = declared(&slot.ty) else {
                return Ok(None);
            };
            let default = match &slot.default {
                Some(d) if !v.is_generic() => {
                    let what = format!(
                        "default for variant {:?} case {:?} payload {:?}",
                        v.name.text,
                        c.name.text,
                        slot.name.as_deref().unwrap_or_default()
                    );
                    Some(pin_slot_default(
                        decls,
                        &v.name.text,
                        &slot.ty,
                        d,
                        &earlier,
                        &what,
                    )?)
                }
                _ => None,
            };
            if let Some(n) = &slot.name {
                earlier.push((n.clone(), slot.ty.clone()));
            }
            payload.push(TCaseParam {
                name: slot.name.clone(),
                ty,
                default,
                implicit_some: slot.implicit_some,
                doc: slot.doc.clone(),
            });
        }
        cases.push(TVariantCase {
            name: c.name.clone(),
            doc: c.doc.clone(),
            payload,
        });
    }
    Ok(Some(TVariantDecl {
        name: v.name.clone(),
        doc: v.doc.clone(),
        type_vars,
        cases,
    }))
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

/// A pinner for one body: the type variables it may mention, and the effects it may use.
fn pinner_for<'a>(
    decls: &'a Decls,
    fn_name: &str,
    type_vars: &[aipl_syntax::ast::TypeParam],
    effects: &[String],
) -> Pinner<'a> {
    Pinner {
        decls,
        table: Table::default(),
        ret: It::Unit,
        rigid: type_vars.iter().map(|t| t.name.clone()).collect(),
        env: HashMap::new(),
        instantiating: false,
        anonymous: 0,
        anonymous_vars: Vec::new(),
        linking: HashSet::new(),
        calls: Vec::new(),
        constructions: Vec::new(),
        own_bounds: type_vars
            .iter()
            .map(|t| (t.name.clone(), t.bound))
            .collect(),
        explicit_args: None,
        validations: Vec::new(),
        fn_name: fn_name.to_string(),
        effects: effects.to_vec(),
        in_test: false,
    }
}

/// An attached `.test` or `.benchmark` block, typed as the function the test driver makes of it:
/// no parameters, no type variables, no effects of its own (a benchmark has `measure`), and a
/// unit result.
fn pin_test_body(decls: &Decls, f: &Function, block: &Expr) -> Res<TExpr<Ty>> {
    let effects = if aipl_syntax::benchmark_source_name(&f.name.text).is_some() {
        vec!["measure".to_string()]
    } else {
        Vec::new()
    };
    let mut p = pinner_for(decls, &f.name.text, &[], &effects);
    p.in_test = true;
    let body = p.expr(block)?;
    p.coerce_expr(&body, &It::Unit, &block.span, "the function's result")?;
    p.settle()?;
    p.finish(body, &block.span)
}

/// A struct field's or variant payload slot's default, typed against the slot. `earlier` holds the
/// named slots declared before it, the only ones it may read.
fn pin_slot_default(
    decls: &Decls,
    owner: &str,
    ty: &Type,
    default: &Expr,
    earlier: &[(String, Type)],
    what: &str,
) -> Res<TExpr<Ty>> {
    let mut p = pinner_for(decls, owner, &[], &[]);
    let none = HashMap::new();
    for (name, t) in earlier {
        let t = p.lower(t, &none, &default.span)?;
        p.env.insert(
            name.clone(),
            Binding {
                ty: t,
                mutable: false,
            },
        );
    }
    let expected = p.lower(ty, &none, &default.span)?;
    let e = p.expr(default)?;
    p.coerce_expr(&e, &expected, &default.span, what)?;
    p.settle()?;
    p.finish(e, &default.span)
}

fn pin_function(decls: &Decls, f: &Function) -> Res<TFunction> {
    let mut p = pinner_for(decls, &f.name.text, &f.sig.type_vars, &f.sig.effects);
    for e in &f.sig.effects {
        if !aipl_syntax::KNOWN_EFFECTS.contains(&e.as_str()) {
            return reject(
                format!(
                    "fn {:?} declares unknown effect \"!{e}\"",
                    display(&f.name.text)
                ),
                &f.name.span,
            );
        }
    }
    // An effect with no default implementation can only be discharged by a shim, and `main` is
    // where nothing is left to discharge it.
    if f.name.text == "main" {
        if let Some(e) = f
            .sig
            .effects
            .iter()
            .find(|e| aipl_syntax::SHIM_ONLY_EFFECTS.contains(&e.as_str()))
        {
            return reject(
                format!(
                    "fn \"main\" cannot declare effect \"!{e}\": it has no default \
                     implementation, so install one with `shim {e} <provider> {{ .. }}` and \
                     declare only what the provider needs"
                ),
                &f.name.span,
            );
        }
    }
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
    check_type_param_defaults(&p, f)?;
    let type_var_names: Vec<String> = f.sig.type_vars.iter().map(|t| t.name.clone()).collect();
    let fn_ctx = format!("fn {:?}", display(&f.name.text));
    for param in &f.sig.params {
        decls.validate(&param.ty, &type_var_names, &fn_ctx, &f.name.span)?;
    }
    if let Some(rt) = &f.sig.return_ty {
        decls.validate(rt, &type_var_names, &fn_ctx, &f.name.span)?;
    }
    // A default reads only the parameters before it. One whose parameter's type mentions a type
    // variable (`pad: T = 0`) has no type to meet until a call fixes `T`; it is checked on its
    // own and left out of the pinned signature.
    let mut defaults: Vec<Option<TExpr<It>>> = Vec::new();
    let everything = std::mem::take(&mut p.env);
    for (i, param) in f.sig.params.iter().enumerate() {
        let mut pinned = None;
        if let Some(default) = &param.default {
            p.env = f.sig.params[..i]
                .iter()
                .zip(&param_tys)
                .map(|(q, t)| {
                    (
                        q.name.clone(),
                        Binding {
                            ty: t.clone(),
                            mutable: false,
                        },
                    )
                })
                .collect();
            let e = p.expr(default)?;
            if !crate::check::mentions_typevar(&param.ty) {
                let what = format!(
                    "default for fn {:?} parameter {:?}",
                    display(&f.name.text),
                    param.name
                );
                p.coerce_expr(&e, &param_tys[i], &default.span, &what)?;
                pinned = Some(e);
            }
        }
        defaults.push(pinned);
    }
    p.env = everything;
    let body = p.expr(&f.body)?;
    let ret = p.ret.clone();
    p.coerce_expr(&body, &ret, &f.body.span, "the function's result")?;
    p.settle()?;
    let body = p.finish(body, &f.body.span)?;
    let defaults = defaults
        .into_iter()
        .map(|d| d.map(|d| p.finish(d, &f.name.span)).transpose())
        .collect::<Res<Vec<_>>>()?;
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
            .zip(defaults)
            .map(|((param, ty), default)| TParam {
                name: param.name.clone(),
                name_span: param.name_span.clone(),
                ty: p.table.resolve(ty),
                mutable: param.mutable,
                arity: param.arity,
                default,
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

/// A type parameter's default is what it takes when nothing pins it, so it must be a type that
/// parameter could have been pinned to, and a parameter after a defaulted one needs a default too.
fn check_type_param_defaults(p: &Pinner, f: &Function) -> Res<()> {
    let mut defaulted = false;
    let is_variant =
        |n: &str| p.decls.variants.contains_key(n) || p.decls.generic_variants.contains_key(n);
    for tp in &f.sig.type_vars {
        match &tp.default {
            Some(default) => {
                defaulted = true;
                let own: Vec<&str> = f.sig.type_vars.iter().map(|t| t.name.as_str()).collect();
                if crate::check::mentions_typevar(default)
                    || crate::check::names_a_type_named(default, &own)
                {
                    return reject(
                        format!(
                            "fn {:?}: the default of type parameter {:?} must be a type that \
                             names no type parameter",
                            display(&f.name.text),
                            tp.name
                        ),
                        &f.name.span,
                    );
                }
                if !tp.bound.accepts(default, &is_variant) {
                    return reject(
                        format!(
                            "fn {:?}: type parameter {:?} requires \"{}\", but its default {} \
                             is not one",
                            display(&f.name.text),
                            tp.name,
                            tp.bound.name(),
                            aipl_syntax::type_name(default)
                        ),
                        &f.name.span,
                    );
                }
            }
            None if defaulted => {
                return reject(
                    format!(
                        "fn {:?}: type parameter {:?} has no default but follows one that does; \
                         give it one or move it before the defaulted ones",
                        display(&f.name.text),
                        tp.name
                    ),
                    &f.name.span,
                );
            }
            None => {}
        }
    }
    Ok(())
}

impl Pinner<'_> {
    /// What can only be judged once the whole body is typed: a variable nothing pinned takes its
    /// declared default or, if only a result position could have named it, is ambiguous; every
    /// type argument meets its bound; a generic struct's arguments are all determined.
    fn settle(&mut self) -> Res<()> {
        let names: Vec<String> = self.rigid.iter().cloned().collect();
        let ctx = format!("fn {:?}", display(&self.fn_name));
        for (ty, span) in std::mem::take(&mut self.validations) {
            let written = self.refined_type(&ty);
            self.decls.validate(&written, &names, &ctx, &span)?;
        }
        let calls = std::mem::take(&mut self.calls);
        for rec in &calls {
            for (i, var) in rec.vars.iter().enumerate() {
                if matches!(self.table.shallow(var), It::Var(_)) {
                    if let Some(default) = &rec.defaults[i] {
                        let ty = self.lower(default, &HashMap::new(), &rec.span)?;
                        self.agree(var, &ty).ok();
                    }
                }
            }
        }
        let is_variant = |n: &str| {
            self.decls.variants.contains_key(n) || self.decls.generic_variants.contains_key(n)
        };
        for rec in &calls {
            for (i, var) in rec.vars.iter().enumerate() {
                // A number nothing pins is the default number, and meets its bound as that.
                let free = match self.table.shallow(var) {
                    It::Var(v) => self.table.slots[v].kind == Kind::Any,
                    _ => false,
                };
                if free {
                    if rec.result_only[i] && rec.defaults[i].is_none() {
                        return reject(
                            format!(
                                "fn {:?} takes a type from where its result goes, and nothing \
                                 here says which — bind the result to a declared type, as in \
                                 `let n: u8? = ..;`",
                                rec.callee
                            ),
                            &rec.span,
                        );
                    }
                    continue;
                }
                let ty = self.refined_type(var);
                let need = rec.bounds[i];
                let ok = match &ty {
                    Type::TypeVar(v) => {
                        let have = self
                            .own_bounds
                            .get(v)
                            .copied()
                            .unwrap_or(aipl_syntax::ast::Bound::Any);
                        need == aipl_syntax::ast::Bound::Any
                            || have == need
                            || (need == aipl_syntax::ast::Bound::Ord
                                && have == aipl_syntax::ast::Bound::Int)
                    }
                    other => need.accepts(other, &is_variant),
                };
                if !ok {
                    let why = match &ty {
                        Type::TypeVar(v) => format!(
                            "but the enclosing {v:?} is declared \"{}\"",
                            self.own_bounds
                                .get(v)
                                .copied()
                                .unwrap_or(aipl_syntax::ast::Bound::Any)
                                .name()
                        ),
                        other => format!("but was inferred as {}", aipl_syntax::type_name(other)),
                    };
                    return reject(
                        format!(
                            "fn {:?}: type parameter {:?} requires \"{}\", {why}",
                            rec.callee,
                            rec.names[i],
                            need.name()
                        ),
                        &rec.span,
                    );
                }
            }
        }
        for (name, vars, span) in std::mem::take(&mut self.constructions) {
            let tvs = &self.decls.generic_structs[&name].type_vars;
            if let Some(i) = vars.iter().position(|v| match self.table.shallow(v) {
                // A number nothing else pins is the default number.
                It::Var(x) => self.table.slots[x].kind == Kind::Any,
                _ => false,
            }) {
                return reject(
                    format!(
                        "cannot infer type parameter {:?} of generic struct {:?} — provide a \
                         field whose value determines it",
                        tvs[i].name,
                        display(&name)
                    ),
                    &span,
                );
            }
        }
        Ok(())
    }

    /// Resolve every inference type in `body`.
    fn finish(&self, body: TExpr<It>, span: &Span) -> Res<TExpr<Ty>> {
        let typed = body
            .map_types(&mut |t| Some(self.table.resolve(t)))
            .ok_or_else(|| Failure::Gap("unresolved type".into(), span.clone()))?;
        check_literals(&typed)?;
        Ok(typed)
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
                if !self.decls.structs.contains_key(n)
                    && !self.decls.variants.contains_key(n)
                    && n != aipl_syntax::ERROR
                {
                    let mut msg = format!("unknown type {:?}", display(n));
                    if n == aipl_syntax::SPAN_TYPE || n.starts_with("__builtin_") {
                        msg.push_str(" — a builtin type; import it from builtins");
                    }
                    return reject(msg, span);
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
            Type::Without(t, x) => It::Without(Box::new(go(self, t)?), x.clone()),
            Type::Tuple(_)
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::Unknown => {
                return gap(format!("a placeholder type in a declaration: {t:?}"), span)
            }
        })
    }

    /// [`Pinner::coerce`] for the value `e`, which can also be shown not to be an excluded value by
    /// what it is: a non-empty array literal is not `[]`, a constructor of case `C` is not case `D`.
    fn coerce_expr(&mut self, e: &TExpr<It>, expected: &It, span: &Span, what: &str) -> Res<()> {
        let given = self.table.exclusions(&e.ty);
        let shown = |x: &aipl_syntax::ast::Exclusion, p: &Self| p.shows(e, x);
        let unshown: Vec<_> = self
            .table
            .exclusions(expected)
            .into_iter()
            .filter(|x| !given.contains(x) && !shown(x, self))
            .collect();
        if let Some(x) = unshown.first() {
            return reject(
                format!(
                    "{what} is {}, but {} is expected: nothing shows it is not {}",
                    self.table.show(&e.ty),
                    self.table.show(expected),
                    x.spelling()
                ),
                span,
            );
        }
        // What was shown is not part of the type the value has; compare the bases.
        let base = self.table.shallow(expected);
        // Each element of a literal is shown to meet the refinement its slot asks for.
        if let (TExprKind::ArrayLit(items), It::Arr(want)) = (&e.kind, &base) {
            if !items.is_empty() && !self.table.exclusions(want).is_empty() {
                for item in items {
                    self.coerce_expr(item, want, &item.span, what)?;
                }
                return Ok(());
            }
        }
        self.coerce(&e.ty, &base, span, what)
    }

    /// Whether what `e` is shows it is not the excluded value `x`: a non-empty array literal is not
    /// `[]`, and a constructor of case `C` is not case `D`. Reaches through the forms that pass a
    /// value on (a block's tail, both arms of an `if`, every arm of a `match`).
    fn shows(&self, e: &TExpr<It>, x: &aipl_syntax::ast::Exclusion) -> bool {
        use aipl_syntax::ast::Exclusion;
        match &e.kind {
            TExprKind::Seq(_, tail)
            | TExprKind::Let(_, _, tail)
            | TExprKind::LetMut(_, _, tail)
            | TExprKind::Assign(_, _, tail) => self.shows(tail, x),
            TExprKind::If(_, a, b) => self.shows(a, x) && self.shows(b, x),
            TExprKind::IfLet(arm, _, other) => self.shows(&arm.body, x) && self.shows(other, x),
            TExprKind::Match(_, arms) => arms.iter().all(|a| self.shows(&a.body, x)),
            TExprKind::Return(_) => true,
            TExprKind::ArrayLit(items) => matches!(x, Exclusion::EmptyArray) && !items.is_empty(),
            _ => match x {
                Exclusion::NamedCase(d) => self.constructed_case(e).is_some_and(|c| c != *d),
                Exclusion::EmptyArray => false,
            },
        }
    }

    /// The bare name of the case a constructor expression builds.
    fn constructed_case(&self, e: &TExpr<It>) -> Option<String> {
        let name = match &e.kind {
            TExprKind::Call { callee, .. } => callee.name(),
            TExprKind::Ident(name) => name.as_str(),
            _ => return None,
        };
        (self.decls.ctors.contains_key(name) || self.decls.generic_ctors.contains_key(name))
            .then(|| name.split('@').next().unwrap_or(name).to_string())
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
        // A refined value goes where its base does; the reverse needs every exclusion shown.
        let (given, wanted) = (self.table.exclusions(a), self.table.exclusions(e));
        if let Some(missing) = wanted.iter().find(|x| !given.contains(x)) {
            return Err(format!("it is not shown to exclude {}", missing.spelling()));
        }
        let (raw_a, raw_e) = (a.clone(), e.clone());
        let (a, e) = (self.table.shallow(a), self.table.shallow(e));
        let char_ty = It::Prim(Primitive::Char);
        let error = It::Named(aipl_syntax::ERROR.to_string());
        let str_ty = It::Prim(Primitive::Str);
        match (&a, &e) {
            (It::Var(_), _) | (_, It::Var(_)) => self.table.unify(&raw_a, &raw_e),
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
            It::Set(e, _) => self.agree(e, elem)?,
            // An empty `#{}` nothing else has decided is a set when a sequence is wanted.
            It::Var(v) if self.table.slots[*v].kind == Kind::Brace => {
                let order = self.table.fresh_order();
                self.table.unify(&a, &It::Set(elem.clone(), order))?;
            }
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
        self.expr_at(e, Pos::Value)
    }

    /// `e`, told whether its value is used (see [`Pos`]).
    fn expr_at(&mut self, e: &Expr, pos: Pos) -> Res<TExpr<It>> {
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
                self.coerce_expr(&c, &bool_ty, &c.span, "an `if` condition")?;
                let a = self.expr_at(ta, pos)?;
                let b = self.expr_at(tb, pos)?;
                // An expression `if` produces a value, so neither branch may mutate outside it.
                if pos == Pos::Value {
                    let declared = HashSet::new();
                    no_outer_assign(ta, &declared, ValueForm::If).map_err(Failure::Rejected)?;
                    no_outer_assign(tb, &declared, ValueForm::If).map_err(Failure::Rejected)?;
                }
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
                self.coerce_expr(&c, &bool_ty, &c.span, "a `while` condition")?;
                let body = self.expr_at(body, Pos::Discard)?;
                self.node(
                    K::While(Box::new(c), Box::new(body)),
                    It::Prim(Primitive::I64),
                    e,
                )
            }
            ExprKind::Seq(first, rest) => {
                // A mutating method called for effect would lose its mutation.
                if let ExprKind::Call(name, cargs, true) = &first.kind {
                    if self
                        .decls
                        .sigs
                        .get(name.name())
                        .is_some_and(|s| s.sig.is_mutating())
                    {
                        let method = name.display_name();
                        let recv = match cargs.first().map(|a| &a.kind) {
                            Some(ExprKind::Ident(v)) => v.clone(),
                            _ => "recv".to_string(),
                        };
                        return reject(
                            format!(
                                "the result of mutating method \"{method}\" is discarded, \
                                 losing the mutation; write \"set {recv}.{method}(..)\" to \
                                 mutate {recv} in place, or use the returned value"
                            ),
                            &first.span,
                        );
                    }
                }
                let a = self.expr_at(first, Pos::Discard)?;
                if matches!(self.table.shallow(&a.ty), It::Res(..)) {
                    return reject(
                        "this result is discarded, ignoring its possible error; handle it \
                         with `match` or propagate it with `?`",
                        &first.span,
                    );
                }
                let b = self.expr_at(rest, pos)?;
                let ty = b.ty.clone();
                self.node(K::Seq(Box::new(a), Box::new(b)), ty, e)
            }
            ExprKind::Return(x) => {
                let x = self.expr(x)?;
                let ret = self.ret.clone();
                self.coerce_expr(&x, &ret, &x.span, "the returned value")?;
                let ty = self.fresh();
                if let It::Var(v) = ty {
                    self.table.slots[v].diverges = true;
                }
                self.node(K::Return(Box::new(x)), ty, e)
            }
            ExprKind::Let(name, ann, v, body_src) | ExprKind::LetMut(name, ann, v, body_src) => {
                let mutable = matches!(e.kind, ExprKind::LetMut(..));
                let (v, ty) = match ann {
                    Some(t) => {
                        let names: Vec<String> = self.rigid.iter().cloned().collect();
                        self.decls.validate(
                            t,
                            &names,
                            &format!("fn {:?}", display(&self.fn_name)),
                            &v.span,
                        )?;
                        let t = self.lower(t, &HashMap::new(), &e.span)?;
                        let v = self.arg(v, &t)?;
                        self.coerce_expr(&v, &t, &v.span, "the bound value")?;
                        (v, t)
                    }
                    None => {
                        let v = match self.shim_closure_type(e, name, v)? {
                            Some(expected) => self.arg(v, &expected)?,
                            None => self.expr(v)?,
                        };
                        let ty = v.ty.clone();
                        (v, ty)
                    }
                };
                let old = self.env.insert(name.clone(), Binding { ty, mutable });
                let body = self.expr_at(body_src, pos)?;
                if let It::Res(..) = self.table.shallow(&v.ty) {
                    let shown = Type::Result(Box::new(Type::Unit), Box::new(Type::Unit));
                    check_result_inspected(name, &shown, body_src, v.span.clone())
                        .map_err(Failure::Rejected)?;
                }
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
                self.coerce_expr(&v, &lhs.ty, &v.span, "the assigned value")?;
                let rest = self.expr_at(rest, pos)?;
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
                let (saved, body) = self.scoped(scope, |p| p.expr_at(body, Pos::Discard))?;
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
                    self.constructions
                        .push((name.clone(), vars.clone(), e.span.clone()));
                    (fields, map, It::Gen(name.clone(), vars))
                } else {
                    let mut msg = format!("unknown struct {name:?}");
                    if aipl_syntax::IMPORTABLE_BUILTIN_TYPES.contains(&name.as_str()) {
                        msg.push_str(&format!(
                            " — {name:?} is a builtin type; import it with `import {{ {name} }} from builtins;`"
                        ));
                    }
                    return reject(msg, &e.span);
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
                    if name == aipl_syntax::SPAN_TYPE {
                        // `start..end` builds a span, whose bounds are slice bounds.
                        self.index_operand(&value)?;
                    } else {
                        self.coerce_expr(&value, &fty, &value.span, "a field value")?;
                    }
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
                let obj = x;
                let x = self.expr(obj)?;
                // The loader reads a tuple pattern's slots off a temporary it names, so a value that
                // is not a tuple fails on a field access the user never wrote.
                if let ExprKind::Ident(n) = &obj.kind {
                    let pattern = if n.starts_with(aipl_syntax::LET_TUPLE_BASE_PREFIX) {
                        Some(crate::check::TuplePattern::Let)
                    } else if n.starts_with(aipl_syntax::FOR_TUPLE_BASE_PREFIX) {
                        Some(crate::check::TuplePattern::For)
                    } else {
                        None
                    };
                    if let Some(pattern) = pattern {
                        let t = self.table.shallow(&x.ty);
                        let tuple = match &t {
                            It::Gen(base, _) => base.starts_with(crate::check::TUPLE_TEMPLATE),
                            It::Named(n) => n.starts_with("__tuple$"),
                            _ => false,
                        };
                        if !tuple {
                            return reject(pattern.not_a_tuple(&self.table.show(&t)), &e.span);
                        }
                    }
                }
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
                if let Some(cell) = self.grid_cell(&container, &i.ty, &e.span)? {
                    return Ok(self.node(
                        K::Index(Box::new(a), Box::new(i)),
                        It::Opt(Box::new(cell)),
                        e,
                    ));
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
                let ordered = matches!(
                    PinnedOrder::of(*order),
                    Some(PinnedOrder::Asc | PinnedOrder::Desc)
                );
                if items.is_empty() && !ordered {
                    // `#{}` is a set or a dict: its position says which.
                    let ty = self.table.fresh(Kind::Brace);
                    return Ok(self.node(K::SetLit(out, PinnedOrder::Unordered), ty, e));
                }
                let ord = match PinnedOrder::of(*order) {
                    Some(k) => Ord::Known(k),
                    None => self.table.fresh_order(),
                };
                let pinned = PinnedOrder::of(*order).unwrap_or(PinnedOrder::Unordered);
                let ty = It::Set(Box::new(elem), ord);
                self.validations.push((ty.clone(), e.span.clone()));
                self.node(K::SetLit(out, pinned), ty, e)
            }
            ExprKind::TypeArgs(call, tys) => {
                let ExprKind::Call(callee, ..) = &call.kind else {
                    return reject("type arguments belong on a call", &e.span);
                };
                let Some(sig) = self.decls.sigs.get(callee.name()).map(|s| s.sig.clone()) else {
                    return reject(format!("call to undefined fn {:?}", callee.name()), &e.span);
                };
                let required = sig
                    .type_vars
                    .iter()
                    .take_while(|tp| tp.default.is_none())
                    .count();
                if tys.len() < required || tys.len() > sig.type_vars.len() {
                    let declared: Vec<String> =
                        sig.type_vars.iter().map(|tp| tp.name.clone()).collect();
                    let which = if required == sig.type_vars.len() {
                        "all of them or none".to_string()
                    } else {
                        "all of them, all but the defaulted ones, or none".to_string()
                    };
                    return reject(
                        format!(
                            "fn {:?} takes {} type argument(s) ({}), got {}; name {which}",
                            display(callee.name()),
                            declared.len(),
                            declared.join(", "),
                            tys.len()
                        ),
                        &call.span,
                    );
                }
                self.explicit_args = Some(tys.clone());
                let typed = self.expr_at(call, pos);
                self.explicit_args = None;
                return typed;
            }
            ExprKind::TupleLit(items) => {
                let mut out = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    if matches!(&item.kind, ExprKind::None)
                        || matches!(&item.kind, ExprKind::ArrayLit(v) | ExprKind::SetLit(v, _) if v.is_empty())
                    {
                        return reject(
                            format!(
                                "tuple element {i} has no type of its own here — a bare `none` or `[]` takes its type from context, and a tuple slot gives it none; annotate the binding it comes from (e.g. `mut p: i64? = none;`)"
                            ),
                            &item.span,
                        );
                    }
                    out.push(self.expr(item)?);
                }
                let tys: Vec<It> = out.iter().map(|i| i.ty.clone()).collect();
                let template = format!("{}{}", crate::check::TUPLE_TEMPLATE, tys.len());
                self.node(K::TupleLit(out), It::Gen(template, tys), e)
            }
            ExprKind::Shim(effect, bindings, body) => {
                self.check_shim(effect, bindings, &e.span)?;
                // The body may use the effect; the enclosing function need not declare it.
                let added = !self.effects.iter().any(|x| x == effect);
                if added {
                    self.effects.push(effect.clone());
                }
                let body_typed = self.expr_at(body, pos);
                if added {
                    self.effects.pop();
                }
                let body_typed = body_typed?;
                if pos == Pos::Value {
                    no_outer_assign(body, &HashSet::new(), ValueForm::Shim)
                        .map_err(Failure::Rejected)?;
                }
                let ty = body_typed.ty.clone();
                self.node(
                    K::Shim(effect.clone(), bindings.clone(), Box::new(body_typed)),
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
                let ty = It::Dict(Box::new(k), Box::new(v));
                self.validations.push((ty.clone(), e.span.clone()));
                self.node(K::DictLit(out), ty, e)
            }
            ExprKind::IfLet(arm, scrut, else_src) => {
                let scrut = self.expr(scrut)?;
                let st = self.known(&scrut.ty, &scrut.span)?;
                let (pattern, binds) = self.pattern(&arm.pattern, &st, &arm.span)?;
                if arm.pattern.is_nested() {
                    let view = TypeView { decls: self.decls };
                    let ty = self.refined_type(&st);
                    crate::match_check::bind_nested(&view, &arm.pattern, &ty, arm.span.clone())
                        .map_err(Failure::Rejected)?;
                }
                let declared: HashSet<String> = arm.pattern.bindings().into_iter().collect();
                let (saved, body) = self.scoped(binds, |p| p.expr_at(&arm.body, pos))?;
                self.restore(saved);
                let otherwise = self.expr_at(else_src, pos)?;
                if pos == Pos::Value {
                    no_outer_assign(&arm.body, &declared, ValueForm::If)
                        .map_err(Failure::Rejected)?;
                    no_outer_assign(else_src, &HashSet::new(), ValueForm::If)
                        .map_err(Failure::Rejected)?;
                }
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
                    (It::Opt(t) | It::Res(t, _), _) if self.in_test => *t,
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
                let excluded = self.table.exclusions(&scrut.ty);
                let result = self.fresh();
                let mut out = Vec::new();
                for arm in arms {
                    self.excluded_case(&st, &excluded, &arm.pattern, &arm.span)?;
                    // A `_` stands for the unnamed cases of a variant; an optional's and a
                    // result's two cases are each named. (A nested match matches on structure.)
                    if matches!(arm.pattern, Pattern::Wildcard)
                        && matches!(st, It::Opt(_) | It::Res(..))
                        && !arms.iter().any(|a| a.pattern.is_nested())
                    {
                        return reject(
                            format!(
                                "a `_` arm stands for the cases of a variant not named above it, and {} \
                                 is not a variant — an optional's \"some\"/\"none\" and a result's \
                                 \"ok\"/\"err\" are two cases with a name each, so name them",
                                self.table.show(&st)
                            ),
                            &arm.span,
                        );
                    }
                    let (pattern, binds) = self.pattern(&arm.pattern, &st, &arm.span)?;
                    let (saved, body) = self.scoped(binds, |p| p.expr_at(&arm.body, pos))?;
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
                self.match_rules(&scrut.ty, arms, &e.span)?;
                self.match_kind(&result, arms, pos, &e.span)?;
                self.node(K::Match(Box::new(scrut), out), result, e)
            }
            // A closure with nothing to take its parameter types from has to write them.
            ExprKind::Lambda(params, body) if params.iter().all(|p| p.ty.is_some()) => {
                let mut ptys = Vec::new();
                let mut scope = Vec::new();
                let mut typed = Vec::new();
                for param in params {
                    let written = param.ty.as_ref().expect("checked above");
                    let t = self.lower(written, &HashMap::new(), &param.span)?;
                    scope.push((param.name.clone(), t.clone()));
                    ptys.push(t.clone());
                    typed.push(TLambdaParam {
                        name: param.name.clone(),
                        ty: t,
                        span: param.span.clone(),
                    });
                }
                let (saved, body) = self.scoped(scope, |p| p.expr(body))?;
                self.restore(saved);
                let ty = It::Fn(ptys, Box::new(body.ty.clone()));
                self.node(K::Lambda(typed, Box::new(body)), ty, e)
            }
            ExprKind::Lambda(..) => {
                return reject(
                    "a closure used as a value needs the type of every parameter written \
                     (`|x: i64| ..`), or a position that declares the function type",
                    &e.span,
                )
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
            if sig.is_generic() {
                return reject(
                    format!(
                        "generic function {:?} cannot be used as a value \
                         (a function value is a single concrete address)",
                        display(name)
                    ),
                    &e.span,
                );
            }
            if !sig.effects.is_empty() {
                return reject(
                    format!(
                        "function {:?} has effects ({}), so it cannot be used as a value \
                         (its effects couldn't be accounted for at an indirect call)",
                        display(name),
                        sig.effects
                            .iter()
                            .map(|x| format!("!{x}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    &e.span,
                );
            }
            let (ptys, ret, _) = self.instantiate(name, &sig, &e.span)?;
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
    fn instantiate(
        &mut self,
        name: &str,
        sig: &Signature,
        span: &Span,
    ) -> Res<(Vec<It>, It, Vec<It>)> {
        let mut instance = HashMap::new();
        let mut type_args = Vec::new();
        for tv in &sig.type_vars {
            let v = self.fresh();
            instance.insert(tv.name.clone(), v.clone());
            type_args.push(v);
        }
        if !sig.type_vars.is_empty() {
            self.calls.push(CallRecord {
                callee: display(name).to_string(),
                names: sig.type_vars.iter().map(|t| t.name.clone()).collect(),
                bounds: sig.type_vars.iter().map(|t| t.bound).collect(),
                vars: type_args.clone(),
                result_only: sig
                    .type_vars
                    .iter()
                    .map(|t| {
                        !sig.params
                            .iter()
                            .any(|p| crate::check::mentions_type_named(&p.ty, &t.name))
                            && sig
                                .return_ty
                                .as_ref()
                                .is_some_and(|r| crate::check::mentions_type_named(r, &t.name))
                    })
                    .collect(),
                defaults: sig.type_vars.iter().map(|t| t.default.clone()).collect(),
                span: span.clone(),
            });
        }
        // `f::<u8>(x)`: the written type arguments are the variables' types.
        if let Some(written) = self.explicit_args.take() {
            let mut given = written;
            for tp in &sig.type_vars[given.len()..] {
                given.push(tp.default.clone().expect("the rest have defaults"));
            }
            for (var, ty) in type_args.iter().zip(&given) {
                let ty = self.lower(ty, &HashMap::new(), span)?;
                self.agree(var, &ty)
                    .or_else(|m| reject(format!("the type argument does not fit ({m})"), span))?;
            }
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
        // Nothing widens at an operator: both operands are one type.
        let (a, b) = (self.table.shallow(l), self.table.shallow(r));
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

    /// Whether a float is anywhere inside a value of type `t`.
    fn contains_float(&mut self, t: &It, seen: &mut HashSet<String>) -> bool {
        match self.table.shallow(t) {
            It::Prim(p) => p.is_float(),
            It::Var(v) => self.table.slots[v].kind == Kind::Float,
            It::Arr(i) | It::Opt(i) | It::Set(i, _) => self.contains_float(&i, seen),
            It::Dict(k, v) | It::Res(k, v) => {
                self.contains_float(&k, seen) || self.contains_float(&v, seen)
            }
            owner @ (It::Named(_) | It::Gen(..)) => {
                let key = format!("{owner:?}");
                if !seen.insert(key) {
                    return false;
                }
                let names: Vec<String> = match &owner {
                    It::Named(n) => self
                        .decls
                        .structs
                        .get(n)
                        .map(|f| f.iter().map(|(n, _, _)| n.clone()).collect())
                        .unwrap_or_default(),
                    It::Gen(n, _) if n.starts_with(crate::check::TUPLE_TEMPLATE) => {
                        let count = match &owner {
                            It::Gen(_, a) => a.len(),
                            _ => 0,
                        };
                        (0..count).map(|i| format!("_{i}")).collect()
                    }
                    It::Gen(n, _) => self
                        .decls
                        .generic_structs
                        .get(n)
                        .map(|t| t.fields.iter().map(|f| f.name.text.clone()).collect())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                names
                    .iter()
                    .any(|f| match self.field_type(&owner, f, &Span::default()) {
                        Ok(ft) => self.contains_float(&ft, seen),
                        Err(_) => false,
                    })
            }
            _ => false,
        }
    }

    /// `t` as a written type, keeping every refinement in it and a tuple as its template applied
    /// to its elements (the instance of an unseen tuple is not a declaration anyone holds).
    fn refined_type(&self, t: &It) -> Type {
        let boxed = |t: &It| Box::new(self.refined_type(t));
        let raw = self.table.chase(t);
        match raw {
            It::Without(inner, x) => Type::Without(boxed(&inner), x),
            It::Opt(t) => Type::Optional(boxed(&t)),
            It::Arr(t) => Type::Array(boxed(&t)),
            It::Case(t) => Type::Case(boxed(&t)),
            It::Res(a, b) => Type::Result(boxed(&a), boxed(&b)),
            It::Dict(k, v) => Type::Dict(boxed(&k), boxed(&v)),
            It::Gen(base, args) if base.starts_with(crate::check::TUPLE_TEMPLATE) => {
                Type::Generic(base, args.iter().map(|a| self.refined_type(a)).collect())
            }
            other => self.table.resolve(&other).to_type(),
        }
    }

    /// The checks on a `match` that depend on its patterns alone: each arm's patterns fit the
    /// scrutinee, every case is covered once, nothing is unreachable.
    fn match_rules(&self, st: &It, arms: &[aipl_syntax::ast::MatchArm], span: &Span) -> Res<()> {
        let view = TypeView { decls: self.decls };
        let ty = self.refined_type(st);
        let rejected = |e| Failure::Rejected(e);
        if arms.iter().any(|a| a.pattern.is_nested()) {
            let mut rows = Vec::new();
            for arm in arms {
                let (pat, _) =
                    crate::match_check::bind_nested(&view, &arm.pattern, &ty, arm.span.clone())
                        .map_err(rejected)?;
                rows.push(vec![pat]);
            }
            crate::match_check::check_nested_exhaustive(&view, &rows, &ty, arms, span.clone())
                .map_err(rejected)
        } else {
            crate::match_check::check_match_exhaustive(&view, &ty, arms, span.clone())
                .map_err(rejected)
        }
    }

    /// A pattern may not name a case the scrutinee has been refined to lack.
    fn excluded_case(
        &self,
        st: &It,
        excluded: &[aipl_syntax::ast::Exclusion],
        pattern: &Pattern,
        span: &Span,
    ) -> Res<()> {
        let (Pattern::Ctor { name, .. } | Pattern::Nested { name, .. }) = pattern else {
            return Ok(());
        };
        let bare = name.split('@').next().unwrap_or(name);
        if excluded
            .iter()
            .any(|x| matches!(x, aipl_syntax::ast::Exclusion::NamedCase(c) if c == bare))
        {
            return reject(
                format!(
                    "{} has no constructor {bare:?}",
                    aipl_syntax::type_name(&self.refined_type(st))
                ),
                span,
            );
        }
        Ok(())
    }

    /// The type of `xs.collect_arms(Circle)` or `x.as_case(Circle)`: the array (or optional) of
    /// the variant less every case but the one named, so a `match` on it needs that arm alone.
    fn refine_collected_arms(&mut self, ret: It, arm: &Expr, builtin: &str) -> Res<It> {
        let shallow = self.table.shallow(&ret);
        let (elem, wrap): (It, fn(It) -> It) = match &shallow {
            It::Arr(elem) => ((**elem).clone(), |t| It::Arr(Box::new(t))),
            It::Opt(elem) => ((**elem).clone(), |t| It::Opt(Box::new(t))),
            _ => return Ok(ret),
        };
        let elem_ty = self.table.resolve(&elem).to_type();
        let view = TypeView { decls: self.decls };
        // Inside a generic body the variant is a type variable and has no cases to name.
        let Some(cases) = crate::match_check::MatchInfo::cases_of(&view, &elem_ty) else {
            return Ok(ret);
        };
        let Some((case, _)) = aipl_syntax::ctor_ref_case(arm) else {
            return reject(
                format!(
                    "{short:?} needs the case written as a constructor name, as in \
                     `x.{short}(Circle)` — a value of type `Case<..>` does not say \
                     which case it holds until run time, so the result could not be typed by it",
                    short = builtin.trim_start_matches("__builtin_"),
                ),
                &arm.span,
            );
        };
        let case = case.split('@').next().unwrap_or(case);
        if !cases.iter().any(|(c, _)| c == case) {
            return reject(
                format!(
                    "{case:?} is not a case of {}",
                    aipl_syntax::type_name(&elem_ty)
                ),
                &arm.span,
            );
        }
        let refined = cases
            .iter()
            .filter(|(c, _)| c != case)
            .fold(elem, |acc, (c, _)| {
                It::Without(
                    Box::new(acc),
                    aipl_syntax::ast::Exclusion::NamedCase(c.clone()),
                )
            });
        Ok(wrap(refined))
    }

    /// A `match` is a statement or an expression by what its arms produce, and must sit where its
    /// value is discarded or used accordingly.
    fn match_kind(
        &self,
        result: &It,
        arms: &[aipl_syntax::ast::MatchArm],
        pos: Pos,
        span: &Span,
    ) -> Res<()> {
        let ty = self.table.shallow(result);
        if let It::Var(v) = ty {
            // A number is a value; anything else unresolved says nothing yet.
            if self.table.slots[v].kind == Kind::Any {
                return Ok(());
            }
        }
        match (ty == It::Unit, pos) {
            (true, Pos::Discard) => Ok(()),
            (true, Pos::Value) => reject(
                "this `match` produces no value, so it is a statement — end it with \";\" \
                 (to use it as an expression, every arm must produce a value)",
                span,
            ),
            (false, Pos::Discard) => reject(
                format!(
                    "this `match` produces {}, but its value is unused — bind it with \
                     \"let\", return it, or make every arm a statement so the `match` is \
                     a statement too",
                    self.table.show(&ty)
                ),
                span,
            ),
            (false, Pos::Value) => {
                for arm in arms {
                    let declared: HashSet<String> = arm.pattern.bindings().into_iter().collect();
                    no_outer_assign(&arm.body, &declared, ValueForm::Match)
                        .map_err(Failure::Rejected)?;
                }
                Ok(())
            }
        }
    }

    /// The function type a shim's closure must have: the operation's own. `None` unless `name` is
    /// the parser's name for a closure bound in a shim (`e` is the `let` that binds it).
    fn shim_closure_type(&mut self, e: &Expr, name: &str, value: &Expr) -> Res<Option<It>> {
        if !aipl_syntax::is_shim_closure_name(name) {
            return Ok(None);
        }
        let ExprKind::Lambda(params, body) = &value.kind else {
            return Ok(None);
        };
        let Some((_, shim)) = aipl_syntax::shim_closure_chain(e) else {
            return reject(
                "a shim closure's name is reserved for the parser's own lowering",
                &e.span,
            );
        };
        let ExprKind::Shim(_, bindings, _) = &shim.kind else {
            return Ok(None);
        };
        let Some(op) = bindings
            .iter()
            .find(|(_, f)| f == name)
            .map(|(op, _)| op.clone())
        else {
            return Ok(None);
        };
        // An unknown operation is reported by `check_shim`, which names the effect's operations.
        let Some(op_sig) = self
            .decls
            .sigs
            .get(&format!("__builtin_{op}"))
            .map(|s| s.sig.clone())
        else {
            return Ok(None);
        };
        if params.len() != op_sig.params.len() {
            return reject(
                format!(
                    "the closure bound to \"{op}\" takes {} parameter(s), but \"{op}\" takes {}",
                    params.len(),
                    op_sig.params.len()
                ),
                &value.span,
            );
        }
        let mut leaves = None;
        aipl_syntax::each_subexpr(body, &mut |x| {
            if leaves.is_none() && matches!(x.kind, ExprKind::Return(_) | ExprKind::Try(_)) {
                leaves = Some(x.span.clone());
            }
        });
        if let Some(at) = leaves {
            return reject(
                "a closure bound to a shim cannot `return` or use `?`; its value is its last expression",
                &at,
            );
        }
        let (ptys, ret, _) = self.instantiate(&format!("__builtin_{op}"), &op_sig, &value.span)?;
        Ok(Some(It::Fn(ptys, Box::new(ret))))
    }

    /// The rules for a `shim`: a shimmable effect, every operation bound once, each bound function
    /// matching its operation, and the functions' own effects declared by the installer.
    fn check_shim(&self, effect: &str, bindings: &[(String, String)], span: &Span) -> Res<()> {
        let Some(ops) = aipl_syntax::effect_operations(effect) else {
            let known = aipl_syntax::SHIMMABLE_EFFECTS
                .iter()
                .map(|(n, _)| format!("\"!{n}\""))
                .collect::<Vec<_>>()
                .join(", ");
            return reject(
                format!("effect \"!{effect}\" cannot be shimmed; shimmable effects are: {known}"),
                span,
            );
        };
        let mut seen: Vec<&str> = Vec::new();
        for (op, _) in bindings {
            if !ops.contains(&op.as_str()) {
                return reject(
                    format!(
                        "\"{op}\" is not an operation of effect \"!{effect}\"; its operations are: {}",
                        ops.join(", ")
                    ),
                    span,
                );
            }
            if seen.contains(&op.as_str()) {
                return reject(
                    format!("operation \"{op}\" is shimmed more than once"),
                    span,
                );
            }
            seen.push(op);
        }
        for op in ops {
            if !seen.contains(op) {
                return reject(
                    format!(
                        "shim of \"!{effect}\" does not cover operation \"{op}\"; a partial shim \
                         cannot discharge the effect"
                    ),
                    span,
                );
            }
        }
        for (op, f) in bindings {
            if aipl_syntax::is_shim_closure_name(f) {
                continue;
            }
            let Some(shim_sig) = self.decls.sigs.get(f).map(|s| &s.sig) else {
                return reject(
                    format!("unknown function {:?} bound to \"{op}\"", display(f)),
                    span,
                );
            };
            let Some(op_sig) = self
                .decls
                .sigs
                .get(&format!("__builtin_{op}"))
                .map(|s| &s.sig)
            else {
                continue;
            };
            let show = |s: &Signature| {
                let ps = s
                    .param_types()
                    .iter()
                    .map(aipl_syntax::type_name)
                    .collect::<Vec<_>>()
                    .join(", ");
                let r = s
                    .return_ty
                    .as_ref()
                    .map_or("()".to_string(), aipl_syntax::type_name);
                format!("({ps}) -> {r}")
            };
            if shim_sig.param_types() != op_sig.param_types()
                || shim_sig.return_ty != op_sig.return_ty
            {
                return reject(
                    format!(
                        "fn {:?} cannot shim \"{op}\": it is {}, but \"{op}\" is {}",
                        display(f),
                        show(shim_sig),
                        show(op_sig)
                    ),
                    span,
                );
            }
            for e in &shim_sig.effects {
                if !self.effects.contains(e) {
                    return reject(
                        format!(
                            "shim fn {:?} has effect \"!{e}\" but the function installing the \
                             shim does not declare it",
                            display(f)
                        ),
                        span,
                    );
                }
            }
        }
        Ok(())
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
    /// The cells of a grid (`T[,]`, an instance of the builtin grid struct) as the array they are
    /// stored in, or `None` for any other type.
    fn grid_cells(&mut self, grid: &It, span: &Span) -> Res<Option<It>> {
        let n = match self.table.shallow(grid) {
            // Inside a generic function the grid may still be an application (`T[,]`).
            It::Gen(base, args) if base == aipl_syntax::GRID_TYPE && args.len() == 1 => {
                return Ok(Some(It::Arr(Box::new(args[0].clone()))));
            }
            It::Named(n) => n,
            _ => return Ok(None),
        };
        if !aipl_syntax::is_grid_instance(&n) {
            return Ok(None);
        }
        let Some(cells) = self
            .decls
            .structs
            .get(&n)
            .and_then(|fields| fields.iter().find(|(f, _, _)| f == "cells"))
            .map(|(_, ty, _)| ty.clone())
        else {
            return Ok(None);
        };
        Ok(Some(self.lower(&cells, &HashMap::new(), span)?))
    }

    /// The element type of `grid` when `index` is a `(row, column)` pair that addresses one of its
    /// cells; the pair's numbers are positions, `u64` unless they are already something else.
    fn grid_cell(&mut self, grid: &It, index: &It, span: &Span) -> Res<Option<It>> {
        let Some(It::Arr(elem)) = self.grid_cells(grid, span)? else {
            return Ok(None);
        };
        let shallow = self.table.shallow(index);
        let Some(slots) = self.tuple_elems(&shallow, span)? else {
            return Ok(None);
        };
        if slots.len() != 2 {
            return Ok(None);
        }
        for slot in &slots {
            match self.table.shallow(slot) {
                It::Var(v) if self.table.slots[v].kind != Kind::Float => {
                    self.table
                        .unify(slot, &It::Prim(Primitive::U64))
                        .or_else(|m| reject(m, span))?;
                }
                It::Prim(p) if p.is_int() => {}
                _ => return Ok(None),
            }
        }
        Ok(Some(*elem))
    }

    fn index_operand(&mut self, i: &TExpr<It>) -> Res<()> {
        match self.table.shallow(&i.ty) {
            // An integer variable stays what its other uses make it (`i64` when nothing does):
            // an index accepts any integer type.
            It::Var(v) if self.table.slots[v].kind != Kind::Float => {
                if self.table.slots[v].kind == Kind::Any {
                    self.table.slots[v].kind = Kind::Int;
                }
                Ok(())
            }
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
            Pattern::Str(s) => {
                if !matches!(st, It::Var(_) | It::Prim(Primitive::Str)) {
                    return reject(
                        format!(
                            "string-literal pattern matches a str, not {}",
                            self.table.show(st)
                        ),
                        span,
                    );
                }
                (TPattern::Str(s.clone()), Vec::new())
            }
            Pattern::Char(c) => {
                if !matches!(st, It::Var(_) | It::Prim(Primitive::Char)) {
                    return reject(
                        format!(
                            "char-literal pattern matches a char, not {}",
                            self.table.show(st)
                        ),
                        span,
                    );
                }
                (TPattern::Char(*c), Vec::new())
            }
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
                if *ignore_payload && payload.is_empty() {
                    return reject(
                        format!(
                            "constructor {name:?} carries no payload, so \"..\" ignores nothing — write {bare:?} instead"
                        ),
                        span,
                    );
                }
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
                self.coerce_expr(&t, pty, &t.span, &format!("an argument to {name:?}"))?;
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
            Callee::IntParse if args.len() == 1 => {
                // The integer parsed is whichever type its position asks for.
                let x = self.expr(&args[0])?;
                self.coerce_expr(&x, &It::Prim(Primitive::Str), &x.span, "the text to parse")?;
                let target = self.table.fresh(Kind::Int);
                return Ok(call(vec![x], It::Opt(Box::new(target)), self));
            }
            Callee::ToFloat if args.len() == 1 => {
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
                    return reject(
                        format!(
                            "\"to_float\" converts an integer, but its receiver is {}",
                            self.table.show(&x.ty)
                        ),
                        &args[0].span,
                    );
                }
                let target = self.table.fresh(Kind::Float);
                return Ok(call(vec![x], target, self));
            }
            Callee::ToFixed if args.len() == 2 => {
                let x = self.expr(&args[0])?;
                match self.table.shallow(&x.ty) {
                    It::Prim(p) if p.is_float() => {}
                    It::Var(v) if self.table.slots[v].kind == Kind::Float => {}
                    It::Rigid(_) => {}
                    other => {
                        return reject(
                            format!(
                                "\"render\" is for floats, but this is {}",
                                self.table.show(&other)
                            ),
                            &args[0].span,
                        )
                    }
                }
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
                let ty = It::Set(Box::new(elem), order);
                self.validations.push((ty.clone(), e.span.clone()));
                return Ok(call(vec![x], ty, self));
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
                let It::Set(elem, order) = xt else {
                    return gap("to_array on this receiver", &e.span);
                };
                if self.table.order(order) == Ord::Known(PinnedOrder::Unordered) {
                    return reject(
                        format!(
                            "\"to_array\" needs an ordered set (`#>{{..}}` or `#<{{..}}`) — a {} promises \
                             no order for its elements to come out in",
                            self.table.show(&x.ty)
                        ),
                        &args[0].span,
                    );
                }
                return Ok(call(vec![x], It::Arr(elem), self));
            }
            Callee::LogicalNot if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                self.coerce_expr(&x, &bool_ty, &x.span, "the operand of `!`")?;
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
            let (lt, rt) = (self.table.shallow(&l.ty), self.table.shallow(&r.ty));
            let is_float = |p: &Self, t: &It| match t {
                It::Prim(prim) => prim.is_float(),
                It::Var(v) => p.table.slots[*v].kind == Kind::Float,
                _ => false,
            };
            let as_type = |p: &Self, t: &It| p.table.resolve(t).to_type();
            use aipl_syntax::ast::BinOp;
            let spelling = match (callee, op) {
                (Callee::WrappingAdd | Callee::SaturatingAdd, _) => Some("+"),
                (Callee::WrappingSub | Callee::SaturatingSub, _) => Some("-"),
                (Callee::WrappingMul, _) => Some("*"),
                (_, Some(o)) => Some(aipl_syntax::binop_spelling(o)),
                _ => None,
            };
            if let Some(name) = float.map(|_| callee.name()) {
                // The float operations take two floats of one type.
                for (t, a) in [(&lt, &args[0]), (&rt, &args[1])] {
                    if !is_float(self, t) {
                        return reject(
                            format!(
                                "\"{name}\" takes IEEE 754 floats, but this operand is {}: floats \
                                 and integers never mix, and nothing converts between them",
                                self.table.show(t)
                            ),
                            &a.span,
                        );
                    }
                }
            } else if !matches!(op, Some(BinOp::And | BinOp::Or)) {
                // The integer and `equal`/`less_than` operators do not take floats.
                for (t, a) in [(&lt, &args[0]), (&rt, &args[1])] {
                    if is_float(self, t) {
                        if let Some(bo) = op {
                            crate::check::reject_float_operand(bo, &as_type(self, t), &a.span)
                                .map_err(Failure::Rejected)?;
                        }
                        let word = spelling.unwrap_or("+");
                        let hint = aipl_syntax::operator_named_forms(word)
                            .into_iter()
                            .find(|n| n.starts_with("float_"))
                            .map(|n| format!("; import `{n} as {word}` to {word} floats"))
                            .unwrap_or_default();
                        return reject(
                            format!(
                                "\"{word}\" here is integer arithmetic and does not take {}: floats \
                                 never mix with the integer operations{hint}",
                                self.table.show(t)
                            ),
                            &a.span,
                        );
                    }
                }
            }
            let logical = matches!(op, Some(BinOp::And | BinOp::Or));
            if logical {
                self.agree(&operand, &bool_ty).or_else(|m| {
                    reject(format!("a logical operand must be bool ({m})"), &e.span)
                })?;
            }
            if matches!(op, Some(BinOp::Eq | BinOp::Ne)) {
                for (t, a) in [(&l.ty, &args[0]), (&r.ty, &args[1])] {
                    if self.contains_float(t, &mut HashSet::new()) {
                        return reject(
                            format!(
                                "{:?} does not compare {}: it holds floats, whose equality is \
                                 IEEE 754's (NaN is not equal to itself) and not a structural \
                                 one; compare the float parts with `float_equal`",
                                spelling.unwrap_or("=="),
                                self.table.show(t)
                            ),
                            &a.span,
                        );
                    }
                }
                // A constructor's name beside a `Case<V>` is that case, which compares by its tag.
                let beside_case = matches!(
                    (&lt, &rt),
                    (It::Case(_), It::Fn(..)) | (It::Fn(..), It::Case(_))
                );
                if (matches!(lt, It::Fn(..)) || matches!(rt, It::Fn(..))) && !beside_case {
                    return reject(
                        format!(
                            "{:?} is not supported for function values",
                            spelling.unwrap_or("==")
                        ),
                        &e.span,
                    );
                }
            }
            if matches!(op, Some(BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge)) {
                // Integers of one type, or two strs, order; nothing else does.
                let orders = match self.table.shallow(&operand) {
                    It::Prim(p) => p.is_int() || p == Primitive::Str,
                    It::Var(v) => self.table.slots[v].kind != Kind::Float,
                    It::Rigid(_) => true,
                    _ => false,
                };
                if !orders {
                    return reject(
                        format!(
                            "comparison operand: expected i64, got {}",
                            self.table.show(&operand)
                        ),
                        &e.span,
                    );
                }
            }
            let ret = if arith {
                match self.table.shallow(&operand) {
                    It::Prim(p) if p.is_int() => operand,
                    It::Var(_) => operand,
                    other => {
                        return reject(
                            format!(
                                "arithmetic operand: expected i64, got {}",
                                self.table.show(&other)
                            ),
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
                self.coerce_expr(&t, &pty, &t.span, "a constructor argument")?;
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
                self.coerce_expr(&t, &pty, &t.span, "a constructor argument")?;
                typed.push(t);
            }
            return Ok(call(typed, It::Named(variant), self));
        }
        let Some(sig) = self.decls.sigs.get(name).cloned() else {
            // A call through a function-valued struct field: `recv.f(rest)` is stored as
            // `f(recv, rest)`, and consulted only once no function of that name exists.
            if !args.is_empty() {
                let recv = self.expr(&args[0])?;
                if let It::Named(_) | It::Gen(..) = self.table.shallow(&recv.ty) {
                    let owner = self.table.shallow(&recv.ty);
                    if let Ok(field) = self.field_type(&owner, name, &e.span) {
                        if let It::Fn(ptys, ret) = self.table.shallow(&field) {
                            if ptys.len() != args.len() - 1 {
                                return reject(
                                    format!(
                                        "field {name:?} is a function value expecting {} arg(s), got {}",
                                        ptys.len(),
                                        args.len() - 1
                                    ),
                                    &e.span,
                                );
                            }
                            let mut typed = vec![recv];
                            for (arg, pty) in args[1..].iter().zip(&ptys) {
                                let t = self.arg(arg, pty)?;
                                self.coerce_expr(
                                    &t,
                                    pty,
                                    &t.span,
                                    &format!("an argument to {name:?}"),
                                )?;
                                typed.push(t);
                            }
                            return Ok(call(typed, *ret, self));
                        }
                    }
                }
            }
            let mut msg = format!("call to undefined fn {:?}", display(name));
            if let Some(import) = aipl_syntax::builtin::Builtin::import_statement(name) {
                msg.push_str(&format!(
                    " — \"{name}\" is a builtin; import it with `{import}`"
                ));
            }
            return reject(msg, &e.span);
        };
        let mut sig = sig.sig;
        // `(row, column).is_in_bounds_of(grid)`, and `grid.try_set((row, column), value)`.
        if *callee == Callee::IsInBoundsOf && args.len() == 2 {
            let index = self.expr(&args[0])?;
            let grid = self.expr(&args[1])?;
            if self.grid_cell(&grid.ty, &index.ty, &e.span)?.is_some() {
                return Ok(call(vec![index, grid], bool_ty, self));
            }
        }
        if *callee == Callee::WithOrSelf && args.len() == 3 {
            let grid = self.expr(&args[0])?;
            let index = self.expr(&args[1])?;
            if let Some(cell) = self.grid_cell(&grid.ty, &index.ty, &e.span)? {
                let value = self.arg(&args[2], &cell)?;
                self.coerce_expr(&value, &cell, &args[2].span, "grid cell")?;
                let ty = grid.ty.clone();
                return Ok(call(vec![grid, index, value], ty, self));
            }
        }
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
        for effect in &sig.effects {
            if !self.effects.contains(effect) {
                return reject(
                    undeclared_effect_message(display(name), effect, &self.effects),
                    &e.span,
                );
            }
        }
        let (ptys, ret, type_args) = self.instantiate(name, &sig, &e.span)?;
        // Plain arguments first, so a closure is typed knowing what the others pinned.
        let mut slots: Vec<Option<TExpr<It>>> = vec![None; args.len()];
        for pass in 0..2 {
            for (i, arg) in args.iter().enumerate() {
                let is_closure = matches!(arg.kind, ExprKind::Lambda(..));
                if slots[i].is_some() || is_closure != (pass == 1) {
                    continue;
                }
                let mut typed = self.arg(arg, &ptys[i])?;
                // A span given to `cartesian_product` is the array of its positions.
                let as_positions = *callee == Callee::CartesianProduct
                    && matches!(self.table.shallow(&typed.ty), It::Named(ref n) if n == aipl_syntax::SPAN_TYPE);
                // `any` takes a grid as the array of its cells.
                let cells = if *callee == Callee::Any && i == 0 {
                    self.grid_cells(&typed.ty, &arg.span)?
                } else {
                    None
                };
                let as_positions = as_positions || cells.is_some();
                let seen_ty = if let Some(cells) = cells {
                    cells
                } else if as_positions {
                    It::Arr(Box::new(It::Prim(Primitive::U64)))
                } else {
                    typed.ty.clone()
                };
                let adapted = if sig.params[i].arity.is_variadic() {
                    // A sequence that is the parameter's own type, refinement included (a
                    // literal with an element in it for a `T+`), is taken as it stands.
                    let whole = matches!(
                        self.table.shallow(&seen_ty),
                        It::Arr(_) | It::Set(..) | It::Prim(Primitive::Str)
                    ) && self
                        .coerce_expr(&typed, &ptys[i], &arg.span, "an argument")
                        .is_ok();
                    if !whole {
                        self.variadic_arg(&ptys[i], &seen_ty).or_else(|m| {
                            reject(format!("an argument to {name:?}: {m}"), &arg.span)
                        })?;
                    }
                    true
                } else {
                    self.adapt_receiver(callee, &ptys[i], &seen_ty)
                        .or_else(|m| reject(format!("an argument to {name:?}: {m}"), &arg.span))?
                };
                if !adapted {
                    if as_positions {
                        self.coerce(
                            &seen_ty,
                            &ptys[i],
                            &arg.span,
                            &format!("an argument to {name:?}"),
                        )?;
                    } else {
                        self.coerce_expr(
                            &typed,
                            &ptys[i],
                            &arg.span,
                            &format!("an argument to {name:?}"),
                        )?;
                    }
                }
                let _ = &mut typed;
                slots[i] = Some(typed);
            }
        }
        let typed_args: Vec<TExpr<It>> = slots.into_iter().map(|s| s.unwrap()).collect();
        // `render` is for floats; another type would fail inside the builtin's own source.
        if *callee == Callee::Render && !typed_args.is_empty() {
            let receiver = self.table.shallow(&typed_args[0].ty);
            let float = match &receiver {
                It::Prim(p) => p.is_float(),
                It::Var(v) => self.table.slots[*v].kind != Kind::Int,
                It::Rigid(_) => true,
                _ => false,
            };
            if !float {
                return reject(
                    format!(
                        "\"render\" is for floats, but this is {}",
                        self.table.show(&receiver)
                    ),
                    &args[0].span,
                );
            }
        }
        // `xs.collect_arms(Circle)` / `x.as_case(Circle)`: the elements are all that one case, so
        // the result is typed as the variant less every other case.
        // A set in the result takes the order of the set the call was made on.
        let ret = match self.table.shallow(&ret) {
            It::Set(inner, o) if self.table.order(o) == Ord::Known(PinnedOrder::Unordered) => {
                let from = ptys.iter().zip(&typed_args).find_map(|(p, a)| {
                    match (self.table.shallow(p), self.table.shallow(&a.ty)) {
                        (It::Set(..), It::Set(_, order)) => Some(order),
                        _ => None,
                    }
                });
                match from {
                    Some(order) => It::Set(inner, order),
                    None => ret,
                }
            }
            _ => ret,
        };
        let case_param = ptys.iter().position(|p| {
            let (It::Case(v), It::Arr(e) | It::Opt(e)) =
                (self.table.shallow(p), self.table.shallow(&ret))
            else {
                return false;
            };
            self.table.resolve(&v) == self.table.resolve(&e)
        });
        let ret = match case_param {
            Some(i) => self.refine_collected_arms(ret, &args[i], name)?,
            None => ret,
        };
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
        // A named function given where a function is wanted, generic or not.
        if let ExprKind::Ident(name) = &arg.kind {
            if !self.env.contains_key(name)
                && !self.decls.ctors.contains_key(name)
                && !self.decls.generic_ctors.contains_key(name)
            {
                if let Some(sig) = self.decls.sigs.get(name).map(|s| s.sig.clone()) {
                    let (ptys, ret, _) = self.instantiate(name, &sig, &arg.span)?;
                    let ty = It::Fn(ptys, Box::new(ret));
                    return Ok(self.node(TExprKind::Ident(name.clone()), ty, arg));
                }
            }
        }
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
            self.coerce_expr(&body, &ret, &body.span, "the closure's result")?;
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

/// Every integer literal must fit the type it was pinned to.
fn check_literals(e: &TExpr<Ty>) -> Res<()> {
    let name = |t: &Ty| match t {
        Ty::Primitive(p) => Some(p.name().to_string()),
        _ => None,
    };
    match &e.kind {
        TExprKind::Neg(inner) => {
            if let TExprKind::Int(n) = inner.kind {
                if let Some(name) = name(&e.ty) {
                    if !aipl_syntax::int_fits(n.wrapping_neg(), &name) || n == i64::MIN {
                        return reject(
                            format!("integer literal -{n} does not fit in {name}"),
                            &e.span,
                        );
                    }
                }
                return Ok(());
            }
        }
        TExprKind::Int(n) => {
            if let Some(name) = name(&e.ty) {
                if !aipl_syntax::int_fits(*n, &name) {
                    return reject(
                        format!("integer literal {n} does not fit in {name}"),
                        &e.span,
                    );
                }
            }
        }
        _ => {}
    }
    let mut result = Ok(());
    e.for_each_child(&mut |c| {
        if result.is_ok() {
            result = check_literals(c);
        }
    });
    result
}

/// The declarations seen as the match rules need them (`match_check`).
struct TypeView<'a> {
    decls: &'a Decls,
}

impl crate::patterns::TypeInfo for TypeView<'_> {
    fn signature(&self, ty: &Type) -> crate::patterns::Signature {
        use crate::patterns::Signature;
        match ty {
            Type::Optional(inner) => Signature::Complete(vec![
                ("some".to_string(), vec![(**inner).clone()]),
                ("none".to_string(), vec![]),
            ]),
            Type::Result(ok, err) => Signature::Complete(vec![
                (
                    "ok".to_string(),
                    if matches!(**ok, Type::Unit) {
                        vec![]
                    } else {
                        vec![(**ok).clone()]
                    },
                ),
                ("err".to_string(), vec![(**err).clone()]),
            ]),
            _ if crate::match_check::MatchInfo::cases_of(self, ty).is_some() => {
                Signature::Complete(
                    crate::match_check::MatchInfo::cases_of(self, ty).expect("just checked"),
                )
            }
            _ => match crate::match_check::MatchInfo::tuple_elems(self, ty) {
                Some(elems) => {
                    Signature::Complete(vec![(crate::patterns::TUPLE.to_string(), elems)])
                }
                None => Signature::Open,
            },
        }
    }
}

impl crate::match_check::MatchInfo for TypeView<'_> {
    fn cases_of(&self, ty: &Type) -> Option<Vec<(String, Vec<Type>)>> {
        match ty {
            // `V without C` has the cases of `V` less `C`.
            Type::Without(base, aipl_syntax::ast::Exclusion::NamedCase(excluded)) => {
                let mut cases = self.cases_of(base)?;
                cases.retain(|(c, _)| c != excluded);
                Some(cases)
            }
            Type::Without(base, aipl_syntax::ast::Exclusion::EmptyArray) => self.cases_of(base),
            Type::Named(n) => self.decls.variants.get(n).cloned(),
            Type::Generic(base, args) => {
                let tmpl = self.decls.generic_variants.get(base)?;
                let map = crate::check::zip_type_args(&tmpl.type_vars, args);
                Some(
                    tmpl.cases
                        .iter()
                        .map(|c| {
                            (
                                c.name.text.clone(),
                                c.payload
                                    .iter()
                                    .map(|p| crate::subst_type_params(&p.ty, &map))
                                    .collect(),
                            )
                        })
                        .collect(),
                )
            }
            _ => None,
        }
    }

    fn tuple_elems(&self, ty: &Type) -> Option<Vec<Type>> {
        match ty {
            Type::Named(n) if n.starts_with("__tuple$") => Some(
                self.decls
                    .structs
                    .get(n)?
                    .iter()
                    .map(|(_, t, _)| t.clone())
                    .collect(),
            ),
            Type::Generic(base, args) if base.starts_with(crate::check::TUPLE_TEMPLATE) => {
                Some(args.clone())
            }
            _ => None,
        }
    }
}
