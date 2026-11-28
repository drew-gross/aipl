//! The type pinner: types a lowered program by unification and hands back a [`TProgram`], a tree
//! in which every node carries a final type and no placeholder can be written down.
//!
//! This is the type checker: what it rejects is what does
//! not compile, and what it pins is what every later pass reads. It also records what each written
//! name refers to ([`Pinned::resolutions`]), which the language server and the index read.
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
//! **Gaps.** A form the pinner does not type is a [`Failure::Gap`] naming it, never a guess.

use std::collections::{HashMap, HashSet};

use crate::typing::{
    check_result_inspected, display, no_outer_assign, optional_map_signature, span_map_signature,
    undeclared_effect_message, Pos, ValueForm,
};
use aipl_syntax::ast::{
    Callee, Capture, Expr, ExprKind, Function, Item, Pattern, Primitive, Program, Signature, Type,
};
use aipl_syntax::resolution::{Resolution, Target};
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
    Fn(Vec<It>, Box<It>, Vec<String>, bool),
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
    fn unit() -> Self {
        It::Unit
    }
    fn is_unit(&self) -> bool {
        *self == It::Unit
    }
    fn is_str(&self) -> bool {
        *self == It::Prim(Primitive::Str)
    }
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
    /// Variables that became `char` because a `str` was taken for a `char[]`. An array of one is
    /// spelled `str` once resolved: a sequence that came from text is text.
    from_text: HashSet<usize>,
    /// The instance names [`Table::resolve`] has spelled for a generic application, with the
    /// application each one is.
    instances: std::cell::RefCell<Instances>,
    /// The refinement parameters of the calls under way, by the `$X#n` name each goes by: the
    /// case names it stands for, once a call has said.
    case_slots: HashMap<String, Option<Vec<String>>>,
}

/// What the pinner calls the type variable of the n-th anonymous `any` of a function: this followed by n.
/// Monomorphization names those variables its own way, so a type mentioning one cannot be substituted there.
pub const ANONYMOUS_ANY: &str = "any$";

/// A generic instance's name (`Box$i64`) -> the template and arguments it is the application of.
pub type Instances = HashMap<String, (String, Vec<Ty>)>;

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

    /// Whether a variable on the way from `t` to what it is was made `char` by taking a `str` for
    /// a `char[]`.
    fn chain_from_text(&self, t: &It) -> bool {
        let mut cur = t.clone();
        while let It::Var(v) = cur {
            if self.from_text.contains(&v) {
                return true;
            }
            match &self.slots[v].bound {
                Some(next) => cur = next.clone(),
                None => return false,
            }
        }
        false
    }

    /// Whether `t` is a variable that became `char` because a `str` was taken for a `char[]`.
    fn element_from_text(&self, t: &It) -> bool {
        let mut cur = t.clone();
        let mut text = false;
        while let It::Var(v) = cur {
            text |= self.from_text.contains(&v);
            match &self.slots[v].bound {
                Some(next) => cur = next.clone(),
                None => return false,
            }
        }
        text && cur == It::Prim(Primitive::Char)
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
            out.extend(self.expand(&x));
            cur = self.chase(&inner);
        }
        out
    }

    /// `x`, or the exclusions a call's refinement parameter stands for once it is settled.
    fn expand(&self, x: &aipl_syntax::ast::Exclusion) -> Vec<aipl_syntax::ast::Exclusion> {
        use aipl_syntax::ast::Exclusion;
        let (Exclusion::NamedCase(n) | Exclusion::Throughout(n)) = x else {
            return vec![x.clone()];
        };
        match self.case_slots.get(n) {
            Some(Some(members)) => members
                .iter()
                .map(|m| match x {
                    Exclusion::Throughout(_) => Exclusion::Throughout(m.clone()),
                    _ => Exclusion::NamedCase(m.clone()),
                })
                .collect(),
            _ => vec![x.clone()],
        }
    }

    /// Settle the refinement parameters of a call that `wanted` mentions and nothing has settled
    /// yet, to the cases `given` excludes.
    fn bind_open_slots(&mut self, given: &It, wanted: &It) {
        use aipl_syntax::ast::Exclusion;
        let have = self.exclusions(given);
        let mut cur = self.chase(wanted);
        while let It::Without(inner, x) = cur {
            if let Exclusion::NamedCase(n) | Exclusion::Throughout(n) = &x {
                if matches!(self.case_slots.get(n), Some(None)) {
                    let deep = matches!(x, Exclusion::Throughout(_));
                    let mut members: Vec<String> = Vec::new();
                    for h in &have {
                        let name = match (h, deep) {
                            (Exclusion::Throughout(c), _) | (Exclusion::NamedCase(c), false) => c,
                            _ => continue,
                        };
                        if !members.contains(name) {
                            members.push(name.clone());
                        }
                    }
                    self.case_slots.insert(n.clone(), Some(members));
                }
            }
            cur = self.chase(&inner);
        }
    }

    fn occurs(&self, v: usize, t: &It) -> bool {
        match self.shallow(t) {
            It::Var(w) => v == w,
            It::Unit | It::Prim(_) | It::Named(_) | It::Rigid(_) => false,
            It::Case(t) | It::Opt(t) | It::Arr(t) | It::Set(t, _) => self.occurs(v, &t),
            It::Dict(a, b) | It::Res(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            It::Gen(_, args) => args.iter().any(|a| self.occurs(v, a)),
            It::Without(t, _) => self.occurs(v, &t),
            It::Fn(ps, r, _, _) => ps.iter().any(|p| self.occurs(v, p)) || self.occurs(v, &r),
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
                if self.from_text.contains(&v) || self.from_text.contains(w) {
                    self.from_text.insert(v);
                    self.from_text.insert(*w);
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
        // A variable equated with one that is `char` for being text is text too.
        if let It::Var(x) = &ca {
            if self.chain_from_text(b) {
                self.from_text.insert(*x);
            }
        }
        if let It::Var(y) = &cb {
            if self.chain_from_text(a) {
                self.from_text.insert(*y);
            }
        }
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
            (It::Fn(p1, r1, e1, m1), It::Fn(p2, r2, e2, m2))
                if p1.len() == p2.len() && e1 == e2 && m1 == m2 =>
            {
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
            It::Arr(t) if self.element_from_text(&t) => Ty::Primitive(Primitive::Str),
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
                    let name = crate::typing::generic_instance_name(&base, &written);
                    self.instances
                        .borrow_mut()
                        .insert(name.clone(), (base, args));
                    Ty::Named(name)
                }
            }
            It::Dict(k, v) => Ty::Dict(boxed(&k), boxed(&v)),
            It::Res(a, b) => Ty::Result(boxed(&a), boxed(&b)),
            It::Fn(ps, r, effects, mutating) => Ty::Fn(
                ps.iter().map(|p| self.resolve(p)).collect(),
                boxed(&r),
                effects,
                mutating,
            ),
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

/// Where a value is handed to a type, for the message when it does not fit.
#[derive(Clone, Copy)]
enum Site<'a> {
    /// A function's result: its name.
    Result(&'a str),
    /// A `let` binding's initializer: the binding's name.
    Binding(&'a str),
    /// A call's argument: the function and the argument's position.
    Argument(&'a str, usize),
    /// A struct's field: the struct and the field.
    Field(&'a str, &'a str),
    /// A closure's body against what its position returns.
    Lambda,
    /// A `return`'s value against the function's declared result.
    Return,
    /// Anything else, in a few words.
    Other(&'a str),
}

impl<'a> From<&'a str> for Site<'a> {
    fn from(what: &'a str) -> Self {
        Site::Other(what)
    }
}

impl<'a> From<&'a String> for Site<'a> {
    fn from(what: &'a String) -> Self {
        Site::Other(what)
    }
}

impl Site<'_> {
    /// What is wrong when `given` meets `wanted` here; `hint` is appended.
    fn mismatch(&self, given: &str, wanted: &str, hint: &str) -> String {
        spelled_as_written(&self.mismatch_marked(given, wanted, hint))
    }

    fn mismatch_marked(&self, given: &str, wanted: &str, hint: &str) -> String {
        match self {
            Site::Result(f) => format!(
                "fn {f:?}: body returns {given}, but the declared return type is {wanted}{hint}"
            ),
            Site::Binding(n) => {
                format!("binding {n:?} is declared {wanted}, but its value is {given}{hint}")
            }
            Site::Argument(f, i) => {
                format!("fn {f:?} arg {i}: expected {wanted}, got {given}{hint}")
            }
            Site::Field(s, f) => {
                format!("struct {s:?} field {f:?}: expected {wanted}, got {given}{hint}")
            }
            Site::Lambda => format!("lambda body returns {given}, but {wanted} was expected{hint}"),
            Site::Return => {
                format!("return value is {given}, but the function returns {wanted}{hint}")
            }
            Site::Other(what) => format!("{what}: expected {wanted}, got {given}{hint}"),
        }
    }
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
    /// Filled by every function pinned: see [`Table::instances`].
    instances: std::cell::RefCell<Instances>,
    /// What every written name in the functions pinned so far was taken to refer to.
    resolutions: std::cell::RefCell<Vec<Resolution>>,
    sigs: HashMap<String, Sig>,
    /// Field name, type and whether it has a default, in declaration order.
    structs: HashMap<String, Vec<(String, Type, bool)>>,
    /// Variant name -> its cases and their payload types.
    variants: HashMap<String, Vec<(String, Vec<Type>)>>,
    /// The declaration of each plain variant, for what the types leave out: the names of a
    /// case's payload slots.
    variant_decls: HashMap<String, aipl_syntax::ast::VariantDecl>,
    /// A constructor's loader-qualified name (`Case@Variant`) -> its variant.
    ctors: HashMap<String, String>,
    generic_templates: HashSet<String>,
    generic_structs: HashMap<String, aipl_syntax::ast::StructDecl>,
    generic_variants: HashMap<String, aipl_syntax::ast::VariantDecl>,
    /// A generic variant's constructor (`Case@Variant`) -> the template it belongs to.
    generic_ctors: HashMap<String, String>,
}

impl Decls {
    fn tables(&self) -> crate::typing::DeclTables<'_> {
        crate::typing::DeclTables {
            structs: &self.structs,
            variants: &self.variants,
            generic_structs: &self.generic_structs,
            generic_variants: &self.generic_variants,
        }
    }

    /// Whether `t`, as written, may appear in a declaration or annotation.
    fn validate(&self, t: &Type, type_params: &[String], ctx: &str, at: &Span) -> Res<()> {
        crate::typing::validate_type(&self.tables(), t, type_params, ctx)
            .map_err(|e| Failure::Rejected(e.or_at(at.clone())))
    }
}

/// Whether the exclusions `given` include `x`. A value that lacks a case throughout lacks it.
fn excluded_by(given: &[aipl_syntax::ast::Exclusion], x: &aipl_syntax::ast::Exclusion) -> bool {
    use aipl_syntax::ast::Exclusion;
    given.contains(x)
        || matches!(x, Exclusion::NamedCase(c) if given.contains(&Exclusion::Throughout(c.clone())))
}

/// `text` with the names the checker gave refinement parameters (`$X`, `$X#3`) as they are
/// written (`X`).
fn spelled_as_written(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // A `$` inside a name (`any$1`) is another mangling and stays.
        let inside = out
            .chars()
            .last()
            .is_some_and(|p| p.is_alphanumeric() || p == '_');
        if c != '$' || inside {
            out.push(c);
            continue;
        }
        while let Some(&n) = chars.peek() {
            if n.is_alphanumeric() || n == '_' {
                out.push(n);
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek() == Some(&'#') {
            chars.next();
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
            }
        }
    }
    out
}

/// `span`, unless it is the placeholder for a name written nowhere.
fn written(span: &Span) -> Option<Span> {
    (span.start < span.end).then(|| span.clone())
}

#[derive(Clone)]
struct Binding {
    ty: It,
    mutable: bool,
    /// Where the name was introduced, if it is written anywhere.
    binder: Option<Span>,
    /// Whether the variable lives in a cell, because a closure's capture list names it: the local
    /// of the typed tree is then `<name>$cell`, the handle on the cell, and a read of the variable
    /// reads the cell.
    cell: bool,
}

/// Every name a closure's capture list in `e` takes or starts, `[mut x]` and `[mut x = ..]`.
fn captured_names(e: &Expr, out: &mut HashSet<String>) {
    if let ExprKind::Lambda(_, _, captures) = &e.kind {
        out.extend(captures.iter().map(|c| c.name.clone()));
    }
    for child in e.children() {
        captured_names(child, out);
    }
}

/// What a lambda's capture list changed in the environment while its body is typed.
#[derive(Default)]
struct CaptureScope {
    /// The variables of the enclosing function it takes.
    taken: Vec<String>,
    /// The bindings to put back.
    saved: Vec<(String, Option<Binding>)>,
    /// The state the closure starts with, `[mut n = 0]`: each is a `mut` variable around the
    /// closure that the closure then takes, so nothing after the pinner sees a second kind of
    /// capture.
    owned: Vec<(String, TExpr<It>)>,
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
    /// The arm parameters of the calls made so far, which must have been settled by the end:
    /// the function called, the parameter, its slot and the call.
    arm_slots: Vec<(String, String, String, Span)>,
    /// Generic variant constructions, whose type arguments must come from their payload, the
    /// expected type or an annotation.
    variant_constructions: Vec<(String, Vec<It>, Span)>,
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
    /// While typing a lambda that has no function type to take its effects from, the effects its
    /// body has performed so far: they are what the lambda's type says instead of an error.
    inferring: Option<Vec<String>>,
    /// While typing the body of a closure against the function type it was given, where what it
    /// may perform is that type's effects: the effects of the function it is written in.
    closure_outer: Option<Vec<String>>,
    /// How many loop bodies enclose what is being typed.
    loops: u32,
    /// The names some closure's capture list in the function takes: a `mut` local of one of them
    /// is a cell.
    cells: HashSet<String>,
    /// Typing a `.test` or `.benchmark` block, where a failed `?` fails the test: the block
    /// returns nothing, so the operand's shape is the only constraint.
    in_test: bool,
    /// Each `narrow` call's target integer type and span: where its value goes has to say which.
    narrows: Vec<(It, Span)>,
    /// What each written name was taken to refer to, for the language server.
    resolutions: Vec<Resolution>,
    /// The names the pattern being typed writes, and how many of each case have been met.
    pattern_names: (aipl_syntax::ast::WrittenNames, HashMap<String, usize>),
    /// Errors in statements the walk went on past: see [`Pinner::recover`].
    errors: Vec<Error>,
    /// The name a refinement parameter of the signature being lowered goes by in an exclusion:
    /// `$X` for this function's own (it equals only itself), a fresh `$X#n` for a callee's, which
    /// the call settles.
    case_names: HashMap<String, String>,
    /// The variant whose cases each such name stands for some of.
    domains: HashMap<String, String>,
    /// `(case, parameter)` pairs known inside the code being typed: a value that matched `case`
    /// and is a `T without parameter` shows that `parameter` does not hold `case`.
    assumed: std::cell::RefCell<Vec<(String, String)>>,
    /// The field paths a `match` arm has narrowed (`self.kind` inside the arm for `Call`), each
    /// with where its root local was introduced, so a shadowing local of the same name is not
    /// taken for it, and the type the path has inside the arm.
    narrowed_paths: Vec<(String, Span, It)>,
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
    /// The generic applications behind the instance names in [`Pinned::program`]'s types.
    pub instances: Instances,
    /// What each name written in a pinned function's body was taken to refer to.
    pub resolutions: Vec<Resolution>,
    /// How many of [`Pinned::program`]'s leading items stand for the declarations the pinned
    /// program had ahead of it ([`pin_behind`]).
    pub declared: usize,
}

/// Pin every function of `program`. Functions are independent: one that fails is reported and
/// left out of [`Pinned::program`], and the rest are still pinned.
pub fn pin(program: &Program) -> Pinned {
    pin_behind(program, 0)
}

/// `pinned` without the items that stand for the declarations it was pinned behind.
pub fn drop_declarations(pinned: &mut Pinned) {
    pinned.program.items.drain(..pinned.declared);
    pinned.declared = 0;
}

/// [`pin`], the first `declarations` items of `program` being declarations the rest is pinned
/// against: [`Pinned::declared`] says how many of them are in the result.
pub fn pin_behind(program: &Program, declarations: usize) -> Pinned {
    let mut decls = Decls {
        instances: Default::default(),
        resolutions: Default::default(),
        sigs: HashMap::new(),
        structs: HashMap::new(),
        variants: HashMap::new(),
        variant_decls: HashMap::new(),
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
                decls.variant_decls.insert(v.name.text.clone(), v.clone());
            }
            _ => {}
        }
    }
    let mut items = Vec::new();
    let mut failures = Vec::new();
    let mut declared = 0;
    for (position, item) in program.items.iter().enumerate() {
        if position == declarations {
            declared = items.len();
        }
        match item {
            Item::Fn(f) => match pin_function(&decls, f) {
                Ok(t) => items.push(TItem::Fn(t)),
                Err(es) => failures.extend(es.into_iter().map(|e| (f.name.text.clone(), e))),
            },
            Item::Struct(s) if !crate::typing::defaults_trail(&s.type_vars) => {
                failures.push((
                    s.name.text.clone(),
                    Failure::Rejected(crate::typing::defaults_must_trail("struct", &s.name)),
                ));
            }
            Item::Variant(v) if !crate::typing::defaults_trail(&v.type_vars) => {
                failures.push((
                    v.name.text.clone(),
                    Failure::Rejected(crate::typing::defaults_must_trail("variant", &v.name)),
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
    if declarations >= program.items.len() {
        declared = items.len();
    }
    Pinned {
        program: TProgram {
            items,
            sources: program.sources.clone(),
            doc: program.doc.clone(),
        },
        declared,
        failures,
        instances: decls.instances.take(),
        resolutions: aipl_syntax::resolution::settled(decls.resolutions.take()),
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
            Some(d) => {
                let what = format!(
                    "default for struct {:?} field {:?}",
                    s.name.text, f.name.text
                );
                Some(pin_slot_default(
                    decls,
                    &s.name.text,
                    &f.ty,
                    d,
                    &[],
                    &s.type_vars,
                    &what,
                )?)
            }
            None => None,
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
                Some(d) => {
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
                        &v.type_vars,
                        &what,
                    )?)
                }
                None => None,
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
        Type::Fn(ps, r, effects, mutating) => Type::Fn(
            ps.iter().map(peel).collect(),
            b(r),
            effects.clone(),
            *mutating,
        ),
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
        arm_slots: Vec::new(),
        variant_constructions: Vec::new(),
        own_bounds: type_vars
            .iter()
            .map(|t| (t.name.clone(), t.bound))
            .collect(),
        explicit_args: None,
        validations: Vec::new(),
        fn_name: fn_name.to_string(),
        effects: effects.to_vec(),
        inferring: None,
        closure_outer: None,
        loops: 0,
        cells: HashSet::new(),
        in_test: false,
        errors: Vec::new(),
        narrows: Vec::new(),
        resolutions: Vec::new(),
        pattern_names: Default::default(),
        case_names: HashMap::new(),
        domains: HashMap::new(),
        assumed: Default::default(),
        narrowed_paths: Vec::new(),
    }
}

/// A struct field's or variant payload slot's default, typed against the slot. `earlier` holds the
/// named slots declared before it, the only ones it may read.
fn pin_slot_default(
    decls: &Decls,
    owner: &str,
    ty: &Type,
    default: &Expr,
    earlier: &[(String, Type)],
    type_vars: &[aipl_syntax::ast::TypeParam],
    what: &str,
) -> Res<TExpr<Ty>> {
    let mut p = pinner_for(decls, owner, type_vars, &[]);
    let none = HashMap::new();
    for (name, t) in earlier {
        let t = p.lower(t, &none, &default.span)?;
        p.env.insert(
            name.clone(),
            Binding {
                ty: t,
                mutable: false,
                binder: None,
                cell: false,
            },
        );
    }
    let expected = p.lower(ty, &none, &default.span)?;
    let e = p.expr(default)?;
    p.coerce_expr(&e, &expected, &default.span, what)?;
    p.settle()?;
    p.finish(e, &default.span)
}

fn pin_function(decls: &Decls, f: &Function) -> Result<TFunction, Vec<Failure>> {
    let mut p = pinner_for(decls, &f.name.text, &f.sig.type_vars, &f.sig.effects);
    let result = pin_function_in(&mut p, decls, f);
    with_recovered(p.errors, result)
}

/// What typing one body came to, given the errors recovered from on the way: all of them when
/// there were any (a failure that stopped the walk too is one more, unless it is only the pinner
/// not knowing a form, which a recovered error explains away), else the walk's own outcome.
fn with_recovered<T>(recovered: Vec<Error>, result: Res<T>) -> Result<T, Vec<Failure>> {
    // One construct can be reached twice (a desugaring that names the same value twice).
    let mut seen = HashSet::new();
    let mut failures: Vec<Failure> = recovered
        .into_iter()
        .filter(|e| seen.insert((e.message.clone(), e.span.as_ref().map(|s| s.start))))
        .map(Failure::Rejected)
        .collect();
    match result {
        Ok(t) if failures.is_empty() => Ok(t),
        Ok(_) => Err(failures),
        Err(last) => {
            if failures.is_empty() || matches!(last, Failure::Rejected(_)) {
                failures.push(last);
            }
            Err(failures)
        }
    }
}

fn pin_function_in(p: &mut Pinner<'_>, decls: &Decls, f: &Function) -> Res<TFunction> {
    // The function a test driver makes of a `.test` block is a test body wherever it is found.
    p.in_test = aipl_syntax::is_test_body(&f.name.text)
        || aipl_syntax::benchmark_source_name(&f.name.text).is_some();
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
    let domains = match case_domains(&f.sig) {
        Ok(d) => d,
        Err(m) => return reject(m, &f.name.span),
    };
    for (name, variant) in domains {
        p.case_names.insert(name.clone(), format!("${name}"));
        p.domains.insert(format!("${name}"), variant);
    }
    let mut param_tys = Vec::new();
    for param in &f.sig.params {
        let ty = p.lower(&param.ty, &none, &f.name.span)?;
        param_tys.push(ty.clone());
        p.env.insert(
            param.name.clone(),
            Binding {
                ty,
                mutable: param.mutable,
                binder: written(&param.name_span.0),
                cell: false,
            },
        );
    }
    p.ret = match &f.sig.return_ty {
        Some(t) => p.lower(t, &none, &f.name.span)?,
        None => It::Unit,
    };
    check_type_param_defaults(&p, f)?;
    check_method_declaration(f)?;
    let type_var_names: Vec<String> = f
        .sig
        .type_vars
        .iter()
        .map(|t| t.name.clone())
        .chain(f.sig.case_params.iter().map(|c| format!("${c}")))
        .collect();
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
                            binder: written(&q.name_span.0),
                            cell: false,
                        },
                    )
                })
                .collect();
            let e = p.expr(default)?;
            if !crate::typing::mentions_typevar(&param.ty) {
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
    let mut written = f.body.clone();
    if let Some(returned @ Type::Fn(..)) = &f.sig.return_ty {
        typed_returned_lambdas(&mut written, returned);
    }
    captured_names(&written, &mut p.cells);
    let body = p.expr(&written)?;
    let ret = p.ret.clone();
    p.coerce_expr(
        &body,
        &ret,
        f.body.value_span(),
        Site::Result(display(&f.name.text)),
    )?;
    p.settle()?;
    // Recovery left placeholders in the tree, which is not worth finishing.
    p.stop_if_recovered(&f.body.span)?;
    let body = p.finish(body, &f.body.span)?;
    decls.resolutions.borrow_mut().append(&mut p.resolutions);
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
            name: format!("{ANONYMOUS_ANY}{n}"),
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
        doc: f.doc.clone(),
    })
}

/// `e`, the body of a function that returns a function of type `returned`, with the untyped
/// parameters of every lambda it can end in, or `return`, given that type's parameter types: where
/// the result is, the type is known, as it is for a lambda in a field. A lambda that itself returns
/// a function is typed the same way, one level in.
fn typed_returned_lambdas(e: &mut Expr, returned: &Type) {
    // A `return` belongs to the function it is written in, so a lambda's body is not searched.
    if !matches!(e.kind, ExprKind::Lambda(..)) {
        for child in e.children_mut() {
            typed_returned_lambdas_in_returns(child, returned);
        }
    }
    typed_result_lambdas(e, returned);
}

fn typed_returned_lambdas_in_returns(e: &mut Expr, returned: &Type) {
    if let ExprKind::Return(value) = &mut e.kind {
        typed_result_lambdas(value, returned);
    }
    if !matches!(e.kind, ExprKind::Lambda(..)) {
        for child in e.children_mut() {
            typed_returned_lambdas_in_returns(child, returned);
        }
    }
}

fn typed_result_lambdas(e: &mut Expr, returned: &Type) {
    let Type::Fn(params, ret, _, _) = returned else {
        return;
    };
    match &mut e.kind {
        ExprKind::Lambda(lambda_params, body, _) if lambda_params.len() == params.len() => {
            for (p, ty) in lambda_params.iter_mut().zip(params) {
                p.ty.get_or_insert_with(|| ty.clone());
            }
            typed_returned_lambdas(body, ret);
        }
        ExprKind::Let(_, _, _, rest) | ExprKind::LetMut(_, _, _, rest) => {
            typed_result_lambdas(rest, returned)
        }
        ExprKind::Seq(_, rest) => typed_result_lambdas(rest, returned),
        ExprKind::If(_, then, otherwise) => {
            typed_result_lambdas(then, returned);
            typed_result_lambdas(otherwise, returned);
        }
        ExprKind::IfLet(arm, _, otherwise) => {
            typed_result_lambdas(&mut arm.body, returned);
            typed_result_lambdas(otherwise, returned);
        }
        ExprKind::Match(_, arms) => {
            for arm in arms {
                typed_result_lambdas(&mut arm.body, returned);
            }
        }
        _ => {}
    }
}

/// The rules on how a function may be declared as a method.
fn check_method_declaration(f: &Function) -> Res<()> {
    let name = display(&f.name.text);
    let fail = |what: &str| reject(format!("fn {name:?}: {what}"), &f.name.span);
    if f.sig.params.iter().skip(1).any(|p| p.mutable) {
        return fail("only the first parameter may be \"mut\"");
    }
    if f.sig.is_mutating() {
        if !f.sig.is_method() {
            return fail("a \"mut\" receiver must be named \"self\"");
        }
        if f.sig.return_ty.is_some() {
            return fail("a mutating method cannot return a value");
        }
    }
    Ok(())
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
                if crate::typing::mentions_typevar(default)
                    || crate::typing::names_a_type_named(default, &own)
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
                p.decls.validate(
                    default,
                    &[],
                    &format!(
                        "fn {:?} type parameter {:?} default",
                        display(&f.name.text),
                        tp.name
                    ),
                    &f.name.span,
                )?;
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
        let names: Vec<String> = self
            .rigid
            .iter()
            .cloned()
            .chain(self.case_names.keys().map(|c| format!("${c}")))
            .collect();
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
                    // After an error a variable may be free only because what pinned it was
                    // the statement that failed.
                    if rec.result_only[i] && rec.defaults[i].is_none() && self.errors.is_empty() {
                        self.errors.push(Error::at(
                            format!(
                                "fn {:?} takes a type from where its result goes, and nothing \
                                 here says which — bind the result to a declared type, as in \
                                 `let n: u8? = ..;`",
                                rec.callee
                            ),
                            rec.span.clone(),
                        ));
                    }
                    continue;
                }
                // A bound is about the type, not what it is refined to.
                let ty = aipl_syntax::unrefined(&self.refined_type(var)).clone();
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
                    self.errors.push(Error::at(
                        format!(
                            "fn {:?}: type parameter {:?} requires \"{}\", {why}",
                            rec.callee,
                            rec.names[i],
                            need.name()
                        ),
                        rec.span.clone(),
                    ));
                }
            }
        }
        for (target, span) in std::mem::take(&mut self.narrows) {
            if matches!(self.table.shallow(&target), It::Var(_)) && self.errors.is_empty() {
                self.errors.push(Error::at(
                    "\"narrow\" takes the integer type it converts to from where its value \
                     goes, and nothing here says which — bind it with one, as in \
                     `let b: u8? = n.narrow();`",
                    span,
                ));
            }
        }
        for (callee, arm, slot, span) in std::mem::take(&mut self.arm_slots) {
            if matches!(self.table.case_slots.get(&slot), Some(None)) && self.errors.is_empty() {
                self.errors.push(Error::at(
                    format!(
                        "cannot infer which case {arm:?} stands for in a call of {callee:?} — an \
                         argument of that case would say, or write it as a type argument, as in \
                         `{callee}::<Circle>(..)`"
                    ),
                    span,
                ));
            }
        }
        for (name, vars, span) in std::mem::take(&mut self.constructions) {
            let tvs = &self.decls.generic_structs[&name].type_vars;
            if let Some(i) = vars.iter().position(|v| match self.table.shallow(v) {
                // A number nothing else pins is the default number.
                It::Var(x) => self.table.slots[x].kind == Kind::Any,
                _ => false,
            }) {
                if self.errors.is_empty() {
                    self.errors.push(Error::at(
                        format!(
                            "cannot infer type parameter {:?} of generic struct {:?} — provide \
                             a field whose value determines it",
                            tvs[i].name,
                            display(&name)
                        ),
                        span,
                    ));
                }
            }
        }
        for (name, vars, span) in std::mem::take(&mut self.variant_constructions) {
            let tvs = &self.decls.generic_variants[&name].type_vars;
            if let Some(i) = vars.iter().position(|v| match self.table.shallow(v) {
                // A number nothing else pins is the default number.
                It::Var(x) => self.table.slots[x].kind == Kind::Any,
                _ => false,
            }) {
                if self.errors.is_empty() {
                    self.errors.push(Error::at(
                        format!(
                            "cannot infer type parameter {:?} of generic variant {:?} — \
                             annotate the binding or give the construction a type to match, as \
                             in `let r: {}<..> = ..;`",
                            tvs[i].name,
                            display(&name),
                            display(&name)
                        ),
                        span,
                    ));
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, body: TExpr<It>, span: &Span) -> Res<TExpr<Ty>> {
        let typed = body
            .map_types(&mut |t| Some(self.table.resolve(t)))
            .ok_or_else(|| Failure::Gap("unresolved type".into(), span.clone()))?;
        check_literals(&typed, &typed.span.clone())?;
        self.decls
            .instances
            .borrow_mut()
            .extend(self.table.instances.take());
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
            // A case of a generic variant written without arguments: the variant applied to
            // variables the value settles, less every other case.
            Type::Named(n) if self.bare_generic_case(n).is_some() => {
                let (variant, others) = self.bare_generic_case(n).expect("checked above");
                let arity = self.decls.generic_variants[&variant].type_vars.len();
                let vars = (0..arity).map(|_| self.fresh()).collect();
                others
                    .into_iter()
                    .fold(It::Gen(variant, vars), |acc, case| {
                        It::Without(Box::new(acc), aipl_syntax::ast::Exclusion::NamedCase(case))
                    })
            }
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
                    It::Rigid(format!("{ANONYMOUS_ANY}{}", self.anonymous))
                }
            }
            Type::Dict(k, v) => It::Dict(Box::new(go(self, k)?), Box::new(go(self, v)?)),
            Type::Result(a, b) => It::Res(Box::new(go(self, a)?), Box::new(go(self, b)?)),
            Type::Fn(ps, r, effects, mutating) => It::Fn(
                ps.iter().map(|p| go(self, p)).collect::<Res<Vec<_>>>()?,
                Box::new(go(self, r)?),
                effects.clone(),
                *mutating,
            ),
            Type::Without(t, x) => {
                use aipl_syntax::ast::Exclusion;
                let x = match x {
                    Exclusion::NamedCase(n) if self.case_names.contains_key(n) => {
                        Exclusion::NamedCase(self.case_names[n].clone())
                    }
                    Exclusion::Throughout(n) if self.case_names.contains_key(n) => {
                        Exclusion::Throughout(self.case_names[n].clone())
                    }
                    other => other.clone(),
                };
                It::Without(Box::new(go(self, t)?), x)
            }
            Type::Tuple(_) | Type::Unknown => {
                return gap(format!("a placeholder type in a declaration: {t:?}"), span)
            }
        })
    }

    /// [`Pinner::coerce`] for the value `e`, which can also be shown not to be an excluded value by
    /// what it is: a non-empty array literal is not `[]`, a constructor of case `C` is not case `D`.
    fn coerce_expr<'s>(
        &mut self,
        e: &TExpr<It>,
        expected: &It,
        span: &Span,
        site: impl Into<Site<'s>>,
    ) -> Res<()> {
        let site = site.into();
        self.table.bind_open_slots(&e.ty, expected);
        let given = self.table.exclusions(&e.ty);
        let shown = |x: &aipl_syntax::ast::Exclusion, p: &Self| p.shows(e, x);
        let unshown: Vec<_> = self
            .table
            .exclusions(expected)
            .into_iter()
            .filter(|x| !excluded_by(&given, x) && !shown(x, self))
            .collect();
        if let Some(x) = unshown.first() {
            let wanted = aipl_syntax::type_name(&self.refined_type(expected));
            let spelled: Vec<_> = unshown.iter().map(|x| x.spelling()).collect();
            let hint = format!(": nothing shows it is not {}", spelled.join(", "));
            // A value passed on by a block, a branch or an arm is blamed where it is produced.
            let culprit = self.unshown_at(e, x);
            let at = if std::ptr::eq(culprit, e) {
                span
            } else {
                &culprit.span
            };
            return reject(
                site.mismatch(&self.table.show(&culprit.ty), &wanted, &hint),
                at,
            );
        }
        // A value that is one of several (a block's tail, the branches of an `if`, the arms of a
        // `match`) meets a refinement inside its type through each of them.
        if !matches!(self.table.chase(expected), It::Without(..))
            && self.mentions_refinement(expected)
        {
            if let Some(parts) = Self::result_positions(e) {
                for part in parts {
                    self.coerce_expr(part, expected, &part.span, site)?;
                }
                let plain = self.unrefined(expected);
                return self.coerce(&e.ty, &plain, span, site);
            }
        }
        // A call whose result is made of its type parameters (`some(x)` is a `T?`) is held to
        // the refinement each parameter's slot asks for, by what its argument is; the call itself
        // is compared without the refinements inside it.
        if let TExprKind::Call { callee, args, .. } = &e.kind {
            if self.mentions_refinement(expected) {
                if let Some(slots) = self.argument_slots(callee, args.len(), expected) {
                    for (arg, slot) in args.iter().zip(&slots) {
                        if let Some(slot) = slot {
                            self.coerce_expr(arg, slot, &arg.span, site)?;
                        }
                    }
                    let plain = self.unrefined(expected);
                    return self.coerce(&e.ty, &plain, span, site);
                }
            }
        }
        // What was shown is not part of the type the value has; compare the bases.
        let base = self.table.shallow(expected);
        // Each element of a literal is shown to meet the refinement its slot asks for.
        if let (TExprKind::ArrayLit(items), It::Arr(want)) = (&e.kind, &base) {
            if !items.is_empty() && !self.table.exclusions(want).is_empty() {
                for item in items {
                    self.coerce_expr(item, want, &item.span, site)?;
                }
                return Ok(());
            }
        }
        self.coerce(&e.ty, &base, span, site)
    }

    /// The expressions a value of `e` comes from, when it is a block, an `if` or a `match`: the
    /// forms `shows` reaches through.
    fn result_positions(e: &TExpr<It>) -> Option<Vec<&TExpr<It>>> {
        Some(match &e.kind {
            TExprKind::Seq(_, tail)
            | TExprKind::Let(_, _, tail)
            | TExprKind::LetMut(_, _, tail)
            | TExprKind::Assign(_, _, tail) => vec![tail],
            TExprKind::If(_, a, b) => vec![a, b],
            TExprKind::IfLet(arm, _, other) => vec![&arm.body, other],
            TExprKind::Match(_, arms) => arms.iter().map(|a| &a.body).collect(),
            _ => return None,
        })
    }

    /// Whether `t` is refined anywhere inside it.
    fn mentions_refinement(&self, t: &It) -> bool {
        match self.table.chase(t) {
            It::Without(..) => true,
            It::Opt(i) | It::Arr(i) | It::Case(i) | It::Set(i, _) => self.mentions_refinement(&i),
            It::Dict(a, b) | It::Res(a, b) => {
                self.mentions_refinement(&a) || self.mentions_refinement(&b)
            }
            It::Gen(_, args) => args.iter().any(|a| self.mentions_refinement(a)),
            It::Fn(ps, r, _, _) => {
                ps.iter().any(|p| self.mentions_refinement(p)) || self.mentions_refinement(&r)
            }
            _ => false,
        }
    }

    /// `t` without any of the refinements in it.
    fn unrefined(&self, t: &It) -> It {
        let go = |i: &It| Box::new(self.unrefined(i));
        match self.table.chase(t) {
            It::Without(inner, _) => self.unrefined(&inner),
            It::Opt(i) => It::Opt(go(&i)),
            It::Arr(i) => It::Arr(go(&i)),
            It::Case(i) => It::Case(go(&i)),
            It::Set(i, o) => It::Set(go(&i), o),
            It::Dict(a, b) => It::Dict(go(&a), go(&b)),
            It::Res(a, b) => It::Res(go(&a), go(&b)),
            It::Gen(n, args) => It::Gen(n, args.iter().map(|a| self.unrefined(a)).collect()),
            other => other,
        }
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
            TExprKind::IfLet(arm, scrutinee, other) => {
                let notes = self.note_matched(&scrutinee.ty, typed_pattern_case(&arm.pattern));
                let arm_shows = self.shows(&arm.body, x);
                self.forget(notes);
                arm_shows && self.shows(other, x)
            }
            TExprKind::Match(scrutinee, arms) => arms.iter().all(|a| {
                let notes = self.note_matched(&scrutinee.ty, typed_pattern_case(&a.pattern));
                let arm_shows = self.shows(&a.body, x);
                self.forget(notes);
                arm_shows
            }),
            TExprKind::Return(_) => true,
            TExprKind::ArrayLit(items) => match x {
                Exclusion::EmptyArray => !items.is_empty(),
                Exclusion::Throughout(c) => items.iter().all(|item| self.clean_of(item, c)),
                Exclusion::NamedCase(_) => false,
            },
            _ => match x {
                Exclusion::NamedCase(d) => {
                    excluded_by(&self.table.exclusions(&e.ty), x)
                        || self
                            .constructed_case(e)
                            .is_some_and(|c| self.case_differs(&c, d))
                }
                Exclusion::Throughout(c) => {
                    self.type_is_clean_of(&e.ty, c) || self.shows_throughout(e, c)
                }
                Exclusion::EmptyArray => excluded_by(&self.table.exclusions(&e.ty), x),
            },
        }
    }

    /// The part of `e` that fails to show it is not `x`, found through the same forms `shows`
    /// reaches: the tail of a block, the branch or arm that is not shown, or `e` itself.
    fn unshown_at<'e>(&self, e: &'e TExpr<It>, x: &aipl_syntax::ast::Exclusion) -> &'e TExpr<It> {
        let first = |parts: &[&'e TExpr<It>]| {
            parts
                .iter()
                .copied()
                .find(|p| !self.shows(p, x))
                .map(|p| self.unshown_at(p, x))
        };
        let found = match &e.kind {
            TExprKind::Seq(_, tail)
            | TExprKind::Let(_, _, tail)
            | TExprKind::LetMut(_, _, tail)
            | TExprKind::Assign(_, _, tail) => first(&[tail]),
            TExprKind::If(_, a, b) => first(&[a, b]),
            TExprKind::IfLet(arm, _, other) => first(&[&arm.body, other]),
            TExprKind::Match(scrutinee, arms) => arms.iter().find_map(|a| {
                let notes = self.note_matched(&scrutinee.ty, typed_pattern_case(&a.pattern));
                let found = (!self.shows(&a.body, x)).then(|| self.unshown_at(&a.body, x));
                self.forget(notes);
                found
            }),
            _ => None,
        };
        found.unwrap_or(e)
    }

    /// Whether a value built as case `built` is not a case `c`: a different name, or, where `c`
    /// is a refinement parameter, a case the code being typed has matched a `T without c` as.
    fn case_differs(&self, built: &str, c: &str) -> bool {
        if self.domains.contains_key(c) {
            self.assumed
                .borrow()
                .iter()
                .any(|(k, p)| k == built && p == c)
        } else {
            built != c
        }
    }

    /// Whether what `e` is shows it holds no case `c` anywhere: a constructor of another case,
    /// or a struct literal, whose every part is clean in turn.
    fn shows_throughout(&self, e: &TExpr<It>, c: &str) -> bool {
        match &e.kind {
            TExprKind::Call { callee, args, .. } => match self.constructed_case(e) {
                Some(case) => {
                    self.case_differs(&case, c) && args.iter().all(|a| self.clean_of(a, c))
                }
                None => self.call_keeps_clean(callee, args, c),
            },
            TExprKind::Construct(_, inits) => inits.iter().all(|i| self.clean_of(&i.value, c)),
            // A case with no payload.
            TExprKind::Ident(_) => self
                .constructed_case(e)
                .is_some_and(|case| self.case_differs(&case, c)),
            TExprKind::None | TExprKind::Unit => true,
            _ => false,
        }
    }

    /// The signature `callee` is declared with: a function's own, or the pseudo signature of
    /// `some`, `ok` and `err`.
    fn declared_signature(&self, callee: &Callee) -> Option<&Signature> {
        match self.decls.sigs.get(callee.name()) {
            Some(declared) => Some(&declared.sig),
            None => crate::pseudo_sigs().get(callee.name()),
        }
    }

    /// For a call to `callee` with `arguments` arguments whose result is wanted as `expected`:
    /// the type each argument is wanted as, when the declared result is made of the type
    /// parameters and nothing in `expected` asks for a refinement of anything else. An argument
    /// not simply a type parameter's value has no slot (`None`), and a signature that does not
    /// fit that shape gives nothing.
    fn argument_slots(
        &self,
        callee: &Callee,
        arguments: usize,
        expected: &It,
    ) -> Option<Vec<Option<It>>> {
        let sig = self.declared_signature(callee)?;
        let ret = sig.return_ty.as_ref()?;
        if sig.params.len() != arguments || sig.is_mutating() {
            return None;
        }
        let mut found = Vec::new();
        if !self.type_parameter_slots(ret, expected, &mut found) {
            return None;
        }
        sig.params
            .iter()
            .map(|param| match &param.ty {
                Type::TypeVar(n) if !param.arity.is_variadic() => {
                    Some(found.iter().find(|(v, _)| v == n).map(|(_, t)| t.clone()))
                }
                other => (!crate::typing::mentions_typevar(other)).then_some(None),
            })
            .collect()
    }

    /// Match the declared result `declared` against the wanted type, recording what is wanted
    /// where a type parameter stands. False when `expected` asks for a refinement of a part that
    /// is not a type parameter, or the shapes differ.
    fn type_parameter_slots(
        &self,
        declared: &Type,
        expected: &It,
        found: &mut Vec<(String, It)>,
    ) -> bool {
        if let Type::TypeVar(n) = declared {
            // As written, refinements included: `shallow` would strip them.
            found.push((n.clone(), expected.clone()));
            return true;
        }
        match (declared, self.table.shallow(expected)) {
            (Type::Optional(d), It::Opt(i)) | (Type::Array(d), It::Arr(i)) => {
                self.type_parameter_slots(d, &i, found)
            }
            (Type::Result(da, db), It::Res(a, b)) => {
                self.type_parameter_slots(da, &a, found) && self.type_parameter_slots(db, &b, found)
            }
            (other, wanted) => {
                !crate::typing::mentions_typevar(other) && !self.mentions_refinement(&wanted)
            }
        }
    }

    /// Whether a call to `callee` yields a value holding no case `c`, judged from its declared
    /// signature alone: the result's type holds a `c` only through a type parameter, and every
    /// argument that supplies a type parameter holds none. A function cannot make a value of a
    /// type parameter, only pass on the ones it was given, so it adds no `c` of its own. A
    /// function-typed parameter is the exception, since what it returns is not an argument.
    fn call_keeps_clean(&self, callee: &Callee, args: &[TExpr<It>], c: &str) -> bool {
        let Some(sig) = self.declared_signature(callee) else {
            return false;
        };
        let Some(ret) = &sig.return_ty else {
            return false;
        };
        sig.params.len() == args.len()
            && !sig.is_mutating()
            && !self.declared_holds_case(ret, c)
            && sig.params.iter().zip(args).all(|(param, arg)| {
                if !crate::typing::mentions_typevar(&param.ty) {
                    return true;
                }
                !param.arity.is_variadic()
                    && !declared_has_function(&param.ty)
                    && self.clean_of(arg, c)
            })
    }

    /// Whether the declared type `t` can hold a case `c` other than through a type parameter
    /// (which the caller's arguments decide).
    fn declared_holds_case(&self, t: &Type, c: &str) -> bool {
        match t {
            Type::TypeVar(_) | Type::Any | Type::Unit | Type::Primitive(_) => false,
            Type::Named(n) => self.name_reaches(n, c),
            Type::Generic(n, args) => {
                self.name_reaches(n, c) || args.iter().any(|a| self.declared_holds_case(a, c))
            }
            Type::Case(i)
            | Type::Optional(i)
            | Type::Array(i)
            | Type::Set(i, _)
            | Type::Without(i, _) => self.declared_holds_case(i, c),
            Type::Dict(a, b) | Type::Result(a, b) => {
                self.declared_holds_case(a, c) || self.declared_holds_case(b, c)
            }
            Type::Tuple(es) => es.iter().any(|e| self.declared_holds_case(e, c)),
            // Anything not understood is taken to hold one.
            Type::Fn(..) | Type::Unknown | Type::ConcatStr => true,
        }
    }

    /// Whether the part `e` of a value holds no case `c`: its type cannot hold one, or is
    /// refined to hold none, or what `e` is shows it.
    fn clean_of(&self, e: &TExpr<It>, c: &str) -> bool {
        self.type_is_clean_of(&e.ty, c)
            || self.shows(e, &aipl_syntax::ast::Exclusion::Throughout(c.to_string()))
    }

    /// Whether every value of type `t` holds no case `c`: nothing in `t` can hold one, or the
    /// parts that can are refined to hold none (the elements of an array of refined values).
    fn type_is_clean_of(&self, t: &It, c: &str) -> bool {
        if !self.holds_case(t, c) {
            return true;
        }
        let wanted = aipl_syntax::ast::Exclusion::Throughout(c.to_string());
        let given = self.table.exclusions(t);
        if given.contains(&wanted) {
            return true;
        }
        // A variant that is not the case, and whose payloads cannot hold it, holds none.
        if !self.domains.contains_key(c)
            && given.contains(&aipl_syntax::ast::Exclusion::NamedCase(c.to_string()))
        {
            if let It::Named(n) | It::Gen(n, _) = self.table.shallow(t) {
                if !self.decls.tables().payloads_reach_case(&n, c) {
                    return true;
                }
            }
        }
        match self.table.shallow(t) {
            It::Opt(i) | It::Arr(i) | It::Case(i) | It::Set(i, _) => self.type_is_clean_of(&i, c),
            It::Dict(a, b) | It::Res(a, b) => {
                self.type_is_clean_of(&a, c) && self.type_is_clean_of(&b, c)
            }
            // An application (a tuple of refined parts) is clean when its own declaration holds
            // none and every argument is.
            It::Gen(n, args) => {
                !self.name_reaches(&n, c) && args.iter().all(|a| self.type_is_clean_of(a, c))
            }
            _ => false,
        }
    }

    /// Whether a value of type `t` can hold a case `c`. A type nothing has fixed holds none.
    fn holds_case(&self, t: &It, c: &str) -> bool {
        match self.table.shallow(t) {
            It::Named(n) => self.name_reaches(&n, c),
            It::Gen(n, args) => {
                self.name_reaches(&n, c) || args.iter().any(|a| self.holds_case(a, c))
            }
            It::Case(i) | It::Opt(i) | It::Arr(i) | It::Set(i, _) => self.holds_case(&i, c),
            It::Dict(a, b) | It::Res(a, b) => self.holds_case(&a, c) || self.holds_case(&b, c),
            It::Unit | It::Prim(_) | It::Rigid(_) | It::Fn(..) | It::Var(_) | It::Without(..) => {
                false
            }
        }
    }

    /// Whether the declared type `n` can hold a case `c`, by its own fields or payloads.
    fn name_reaches(&self, n: &str, c: &str) -> bool {
        let tables = self.decls.tables();
        match self.domains.get(c) {
            Some(variant) => tables.named_reaches_variant(n, variant),
            None => tables.named_reaches_case(n, c),
        }
    }

    /// The cases `t` is refined to lack throughout (`V without C throughout`).
    fn throughout_cases(&self, t: &It) -> Vec<String> {
        self.table
            .exclusions(t)
            .into_iter()
            .filter_map(|x| match x {
                aipl_syntax::ast::Exclusion::Throughout(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// `t`, a type found inside a value refined to lack the cases `deep` throughout, carrying
    /// that refinement for every part of it that can hold one.
    fn deepen(&self, t: &It, deep: &[String]) -> It {
        if deep.is_empty() {
            return t.clone();
        }
        let go = |i: &It| Box::new(self.deepen(i, deep));
        match self.table.chase(t) {
            It::Without(inner, x) => It::Without(go(&inner), x),
            named @ (It::Named(_) | It::Gen(..)) => {
                if let It::Gen(base, args) = &named {
                    if base.starts_with(crate::typing::TUPLE_TEMPLATE) {
                        return It::Gen(
                            base.clone(),
                            args.iter().map(|a| self.deepen(a, deep)).collect(),
                        );
                    }
                }
                let mut out = named.clone();
                for c in deep {
                    let have = self.table.exclusions(&out);
                    let wanted = aipl_syntax::ast::Exclusion::Throughout(c.clone());
                    if self.holds_case(&named, c) && !have.contains(&wanted) {
                        out = It::Without(Box::new(out), wanted);
                    }
                }
                out
            }
            It::Opt(i) => It::Opt(go(&i)),
            It::Arr(i) => It::Arr(go(&i)),
            It::Case(i) => It::Case(go(&i)),
            It::Set(i, o) => It::Set(go(&i), o),
            It::Dict(k, v) => It::Dict(go(&k), go(&v)),
            It::Res(a, b) => It::Res(go(&a), go(&b)),
            other => other,
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
    /// language allows at such a position (`typing.rs`'s `coerce` is the specification).
    fn coerce<'s>(
        &mut self,
        actual: &It,
        expected: &It,
        span: &Span,
        site: impl Into<Site<'s>>,
    ) -> Res<()> {
        let site = site.into();
        let (a, e) = (self.table.shallow(actual), self.table.shallow(expected));
        // A scalar integer widens to a wider declared one; inside a container it must agree.
        if let (It::Prim(x), It::Prim(y)) = (&a, &e) {
            if x.widens_to(*y) {
                return Ok(());
            }
        }
        // The value keeps its refinements, so a variable it meets is fixed to the refined type.
        self.agree(actual, &e).or_else(|_| {
            let (given, wanted) = (
                aipl_syntax::type_name(&self.spelled_type(actual)),
                aipl_syntax::type_name(&self.spelled_type(expected)),
            );
            let hint = match (&a, &e) {
                (It::Prim(x), It::Prim(y)) if x.is_int() && y.is_int() => format!(
                    " — {wanted} cannot hold every {given}, so convert with `.narrow()`, which \
                     gives `none` when the value does not fit"
                ),
                _ => String::new(),
            };
            reject(site.mismatch(&given, &wanted, &hint), span)
        })
    }

    /// The type of a value that is `a` or `b`: their bases agree, and it keeps each exclusion
    /// both are known to meet, by their types or by what they are.
    fn joined(&mut self, a: &TExpr<It>, b: &TExpr<It>) -> Result<It, String> {
        let (base_a, base_b) = (self.table.shallow(&a.ty), self.table.shallow(&b.ty));
        self.agree(&base_a, &base_b)?;
        let (given_a, given_b) = (self.table.exclusions(&a.ty), self.table.exclusions(&b.ty));
        let mut out = base_a;
        for x in given_a.iter().chain(&given_b) {
            let met = |given: &[aipl_syntax::ast::Exclusion], e: &TExpr<It>| {
                excluded_by(given, x) || self.shows(e, x)
            };
            let wrapped = self.table.exclusions(&out).contains(x);
            if !wrapped && met(&given_a, a) && met(&given_b, b) {
                out = It::Without(Box::new(out), x.clone());
            }
        }
        Ok(out)
    }

    /// [`Pinner::joined`] for any number of values that already agree on what they are: the type
    /// they share, which keeps each exclusion every one of them is known to meet.
    fn joined_all(&mut self, values: &[&TExpr<It>]) -> Result<It, String> {
        let first = values.first().expect("at least one value");
        let base = self.table.shallow(&first.ty);
        let mut candidates: Vec<aipl_syntax::ast::Exclusion> = Vec::new();
        for v in values {
            for x in self.table.exclusions(&v.ty) {
                if !candidates.contains(&x) {
                    candidates.push(x);
                }
            }
        }
        let mut out = base;
        for x in candidates {
            let met = values
                .iter()
                .all(|v| excluded_by(&self.table.exclusions(&v.ty), &x) || self.shows(v, &x));
            if met && !self.table.exclusions(&out).contains(&x) {
                out = It::Without(Box::new(out), x);
            }
        }
        Ok(out)
    }

    /// Whether two types are interchangeable here, binding variables as it goes.
    fn agree(&mut self, a: &It, e: &It) -> Result<(), String> {
        // A refined value goes where its base does; the reverse needs every exclusion shown.
        self.table.bind_open_slots(a, e);
        // A type nothing has fixed yet (the element of `[]`) takes the refinement it meets.
        let open = matches!(self.table.chase(a), It::Var(_));
        let (given, wanted) = (self.table.exclusions(a), self.table.exclusions(e));
        if let Some(missing) = wanted.iter().find(|x| !open && !excluded_by(&given, x)) {
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
                if let It::Var(v) = self.table.shallow(t) {
                    self.table.from_text.insert(v);
                }
                self.table.unify(t, &char_ty)
            }
            // A constructor named as a value stands for its case: a function building `V`, or (for a
            // case without a payload) a `V`.
            (It::Fn(_, ret, _, _), It::Case(v)) | (It::Case(v), It::Fn(_, ret, _, _)) => {
                let (ret, v) = (self.table.shallow(ret), self.table.shallow(v));
                self.agree(&ret, &v)
            }
            (It::Named(_), It::Case(v)) | (It::Case(v), It::Named(_))
                if {
                    let inner = self.table.shallow(v);
                    matches!(inner, It::Var(_) | It::Named(_))
                } =>
            {
                let other = if matches!(a, It::Case(_)) { &e } else { &a };
                let v = self.table.shallow(v);
                self.agree(&v, other)
            }
            // A `Case<V>` is its tag, an integer.
            (It::Case(_), It::Prim(Primitive::I64 | Primitive::U64))
            | (It::Prim(Primitive::I64 | Primitive::U64), It::Case(_)) => Ok(()),
            // A case is a tag, so what its variant is refined to is no part of it.
            (It::Case(x), It::Case(y)) => {
                let (x, y) = (self.table.shallow(x), self.table.shallow(y));
                self.agree(&x, &y)
            }
            (It::Opt(x), It::Opt(y)) | (It::Arr(x), It::Arr(y)) => self.agree(x, y),
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
            (It::Fn(p1, r1, given, gm), It::Fn(p2, r2, wanted, wm)) if p1.len() == p2.len() => {
                if *gm && !*wm {
                    return Err(format!(
                        "it mutates what it captured, which {} does not allow",
                        self.table.show(&e)
                    ));
                }
                // A function performing fewer effects is accepted where one performing more is
                // expected, never the other way.
                if let Some(extra) = given.iter().find(|e| !wanted.contains(e)) {
                    return Err(format!(
                        "it performs \"!{extra}\", which {} does not allow",
                        self.table.show(&e)
                    ));
                }
                for (x, y) in p1.iter().zip(p2) {
                    self.agree(y, x)?;
                }
                self.agree(r1, r2)
            }
            // The same generic applied to arguments: each argument agrees, so one that is
            // written as an application and one that is the instance it names meet.
            (It::Gen(b1, x), It::Gen(b2, y)) if b1 == b2 && x.len() == y.len() => {
                for (p, q) in x.iter().zip(y) {
                    self.agree(p, q)?;
                }
                Ok(())
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
            derived: false,
            annotation: None,
        }
    }

    fn expr(&mut self, e: &Expr) -> Res<TExpr<It>> {
        self.expr_at(e, Pos::Value)
    }

    /// The local that holds the handle on the cell of the variable `name`.
    fn cell_handle(name: &str) -> String {
        format!("{name}$cell")
    }

    /// A call of one of the cell intrinsics, which carry the type of what the cell holds as their
    /// one type argument.
    fn cell_call(
        &self,
        callee: Callee,
        held: &It,
        args: Vec<TExpr<It>>,
        ret: It,
        at: &Expr,
    ) -> TExpr<It> {
        self.node(
            TExprKind::Call {
                callee,
                type_args: vec![held.clone()],
                args,
                method_style: false,
            },
            ret,
            at,
        )
    }

    /// The handle on the cell of the variable `name`, which holds a `held`.
    fn cell_local(&self, name: &str, held: &It, at: &Expr) -> TExpr<It> {
        self.node(
            TExprKind::Ident(Self::cell_handle(name)),
            It::Arr(Box::new(held.clone())),
            at,
        )
    }

    /// What the cell of the variable `name` holds now.
    fn cell_read(&self, name: &str, held: It, at: &Expr) -> TExpr<It> {
        let handle = self.cell_local(name, &held, at);
        self.cell_call(Callee::CellGet, &held, vec![handle], held.clone(), at)
    }

    /// `e`, told whether its value is used (see [`Pos`]).
    ///
    /// A stamp already on `e` (`Expr::ty`) is the context its type was decided in, recorded where
    /// that context was in view. A pass may since have moved `e` out from under it, so a stamp is
    /// met with the type found here: it can decide what nothing around `e` now does (the order of a
    /// set an inlined call was asked to return).
    fn expr_at(&mut self, e: &Expr, pos: Pos) -> Res<TExpr<It>> {
        let typed = self.expr_unstamped(e, pos)?;
        if let Some(recorded) = &e.ty {
            if let Ok(want) = self.lower(recorded, &HashMap::new(), &e.span) {
                // A stamp that cannot be met is the stamp's mistake, not the program's.
                let _ = self.agree(&typed.ty, &want);
            }
        }
        Ok(typed)
    }

    fn expr_unstamped(&mut self, e: &Expr, pos: Pos) -> Res<TExpr<It>> {
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
            ExprKind::Ident(name) => match self.env.get(name).cloned() {
                Some(b) => {
                    if let Some(binder) = b.binder {
                        self.resolve(&e.name_span, Target::Local(binder));
                    }
                    if b.cell {
                        self.cell_read(name, b.ty, e)
                    } else {
                        self.node(K::Ident(name.clone()), b.ty, e)
                    }
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
                    let typed = (|| {
                        let typed = self.expr(item)?;
                        self.coerce(&typed.ty, &elem, &typed.span, "array element")?;
                        Ok(typed)
                    })();
                    out.push(self.recover(typed, item)?);
                }
                self.node(K::ArrayLit(out), It::Arr(Box::new(elem)), e)
            }
            ExprKind::If(c, ta, tb) => {
                let c = self.expr(c)?;
                self.coerce_expr(&c, &bool_ty, &c.span, "if condition")?;
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
                let mut ty = if da { b.ty.clone() } else { a.ty.clone() };
                if !da && !db {
                    ty = self.joined(&a, &b).or_else(|_| {
                        reject(
                            format!(
                                "if branches have mismatched types: {} vs {}",
                                self.table.show(&a.ty),
                                self.table.show(&b.ty)
                            ),
                            &e.span,
                        )
                    })?;
                }
                self.node(K::If(Box::new(c), Box::new(a), Box::new(b)), ty, e)
            }
            ExprKind::While(c, body) => {
                let c = self.expr(c)?;
                self.coerce_expr(&c, &bool_ty, &c.span, "while condition")?;
                self.loops += 1;
                let body = self.expr_at(body, Pos::Discard);
                self.loops -= 1;
                let body = body?;
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
                        self.errors.push(Error::at(
                            format!(
                                "the result of mutating method \"{method}\" is discarded, \
                                 losing the mutation; write \"set {recv}.{method}(..)\" to \
                                 mutate {recv} in place, or use the returned value"
                            ),
                            first.span.clone(),
                        ));
                    }
                }
                let typed = self.expr_at(first, Pos::Discard);
                let a = self.recover(typed, first)?;
                if matches!(self.table.shallow(&a.ty), It::Res(..)) {
                    self.errors.push(Error::at(
                        "this result is discarded, ignoring its possible error; handle it \
                         with `match` or propagate it with `?`",
                        first.span.clone(),
                    ));
                }
                let b = self.expr_at(rest, pos)?;
                let ty = b.ty.clone();
                self.node(K::Seq(Box::new(a), Box::new(b)), ty, e)
            }
            ExprKind::Return(x) => {
                let x = self.expr(x)?;
                let ret = self.ret.clone();
                self.coerce_expr(&x, &ret, &x.span, Site::Return)?;
                let ty = self.fresh();
                if let It::Var(v) = ty {
                    self.table.slots[v].diverges = true;
                }
                self.node(K::Return(Box::new(x)), ty, e)
            }
            ExprKind::Let(name, ann, v, body_src) | ExprKind::LetMut(name, ann, v, body_src) => {
                let mutable = matches!(e.kind, ExprKind::LetMut(..));
                let bound = (|| match ann {
                    Some(t) => {
                        let names: Vec<String> = self
                            .rigid
                            .iter()
                            .cloned()
                            .chain(self.case_names.keys().map(|c| format!("${c}")))
                            .collect();
                        self.decls.validate(
                            t,
                            &names,
                            &format!("fn {:?}", display(&self.fn_name)),
                            &v.span,
                        )?;
                        let t = self.lower(t, &HashMap::new(), &e.span)?;
                        let v = self.value_arg(v, &t)?;
                        self.coerce_expr(&v, &t, &v.span, Site::Binding(name))?;
                        // The temporary a destructuring reads its parts off is the value
                        // itself, so it keeps the refinement the value has.
                        let t = if name.starts_with(aipl_syntax::DESTRUCTURE_BASE_PREFIX)
                            && !self.table.exclusions(&v.ty).is_empty()
                        {
                            v.ty.clone()
                        } else {
                            t
                        };
                        Ok((v, t))
                    }
                    None => {
                        let v = match self.shim_closure_type(e, name, v, pos)? {
                            Some(expected) => self.arg(v, &expected)?,
                            None => self.expr(v)?,
                        };
                        let ty = v.ty.clone();
                        Ok((v, ty))
                    }
                })();
                // A binding's initializer is not a place to recover: the body is typed against
                // what it produced, so going on would report the echoes of its failure.
                let (v, ty) = bound?;
                let held = ty.clone();
                if self.table.shallow(&ty) == It::Unit {
                    return reject(
                        format!("cannot bind {name:?} to a value of type ()"),
                        &v.span,
                    );
                }
                let binder = written(&e.name_span.as_deref().cloned().unwrap_or_default());
                let declared = ann.is_some().then(|| ty.clone());
                // A variable a closure's capture list names is a cell: the closure and the code
                // around it share it.
                let cell = mutable && self.cells.contains(name);
                let old = self.env.insert(
                    name.clone(),
                    Binding {
                        ty: ty.clone(),
                        mutable,
                        binder,
                        cell,
                    },
                );
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
                if cell {
                    // A declared type reaches a literal through an annotated binding of it.
                    let (initial, temporary) = match declared {
                        Some(_) => {
                            let temp = format!("{name}$initial");
                            let read = self.node(K::Ident(temp.clone()), held.clone(), e);
                            (read, Some((temp, v)))
                        }
                        None => (v, None),
                    };
                    let made = self.cell_call(
                        Callee::CellNew,
                        &held,
                        vec![initial],
                        It::Arr(Box::new(held.clone())),
                        e,
                    );
                    let node = self.node(
                        K::Let(Self::cell_handle(name), Box::new(made), Box::new(body)),
                        ty.clone(),
                        e,
                    );
                    return Ok(match temporary {
                        Some((temp, v)) => {
                            let mut outer =
                                self.node(K::Let(temp, Box::new(v), Box::new(node)), ty, e);
                            outer.annotation = declared;
                            outer
                        }
                        None => node,
                    });
                }
                let kind = if mutable {
                    K::LetMut(name.clone(), Box::new(v), Box::new(body))
                } else {
                    K::Let(name.clone(), Box::new(v), Box::new(body))
                };
                let mut node = self.node(kind, ty, e);
                node.annotation = declared;
                node
            }
            ExprKind::Assign(lhs, v, rest) => {
                // A variable in a cell is read, changed and put back around the assignment: the
                // place and the value see an ordinary local of its name.
                let cell_root = aipl_syntax::ast::assign_target(lhs).and_then(|(root, _)| {
                    let b = self.env.get(root).filter(|b| b.cell)?;
                    Some((root.to_string(), b.ty.clone()))
                });
                if let Some((root, _)) = &cell_root {
                    if let Some(b) = self.env.get_mut(root) {
                        b.cell = false;
                    }
                }
                let assigned = (|| {
                    let Some((root, _)) = aipl_syntax::ast::assign_target(lhs) else {
                        return reject(
                            "an assignment target must be a variable or a field of one",
                            &lhs.span,
                        );
                    };
                    let Some(b) = self.env.get(root).cloned() else {
                        return reject(format!("set: undeclared variable {root:?}"), &lhs.span);
                    };
                    if let Some(binder) = &b.binder {
                        let mut base = &**lhs;
                        while let ExprKind::Field(obj, _) = &base.kind {
                            base = obj;
                        }
                        self.resolve(&base.name_span, Target::Local(binder.clone()));
                    }
                    if !b.mutable {
                        return reject(
                            format!(
                                "set: cannot assign to immutable binding {root:?} (use \"let mut\")"
                            ),
                            &lhs.span,
                        );
                    }
                    // The place is typed as the expression it is; a field path reads through
                    // structs.
                    let lhs = self.expr(lhs)?;
                    let v = self.arg(v, &lhs.ty)?;
                    self.coerce_expr(&v, &lhs.ty, &v.span, "set")?;
                    Ok((lhs, v))
                })();
                if let Some((root, _)) = &cell_root {
                    if let Some(b) = self.env.get_mut(root) {
                        b.cell = true;
                    }
                }
                let (lhs, v) = match assigned {
                    Ok(done) => done,
                    Err(failure) => {
                        let place = self.recover(Err(failure), e)?;
                        (place.clone(), place)
                    }
                };
                let rest = self.expr_at(rest, pos)?;
                let ty = rest.ty.clone();
                match cell_root {
                    Some((root, held)) => {
                        let handle = self.cell_local(&root, &held, e);
                        let value = self.node(K::Ident(root.clone()), held.clone(), e);
                        let store = self.cell_call(
                            Callee::CellSet,
                            &held,
                            vec![handle, value],
                            It::Unit,
                            e,
                        );
                        let rest =
                            self.node(K::Seq(Box::new(store), Box::new(rest)), ty.clone(), e);
                        let assign = self.node(
                            K::Assign(Box::new(lhs), Box::new(v), Box::new(rest)),
                            ty.clone(),
                            e,
                        );
                        let read = self.cell_read(&root, held, e);
                        self.node(K::LetMut(root, Box::new(read), Box::new(assign)), ty, e)
                    }
                    None => self.node(K::Assign(Box::new(lhs), Box::new(v), Box::new(rest)), ty, e),
                }
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
                let mut scope = vec![(
                    var.clone(),
                    elem,
                    written(&e.name_span.as_deref().cloned().unwrap_or_default()),
                )];
                if let Some(i) = index {
                    scope.push((i.clone(), key, None));
                }
                self.loops += 1;
                let scoped = self.scoped(scope, |p| p.expr_at(body, Pos::Discard));
                self.loops -= 1;
                let (saved, body) = scoped?;
                self.restore(saved);
                self.node(
                    K::For(var.clone(), index.clone(), Box::new(iter), Box::new(body)),
                    It::Prim(Primitive::I64),
                    e,
                )
            }
            ExprKind::Construct(name, inits) => {
                self.resolve(&e.name_span, Target::Struct(name.clone()));
                for init in inits.iter().filter(|i| !i.name.is_empty()) {
                    self.resolve_at(
                        &init.name.span,
                        Target::Field {
                            owner: name.clone(),
                            field: init.name.text.clone(),
                        },
                    );
                }
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
                            " — {name:?} is a builtin type; import it with `import {{ {name} }} from {};`",
                            aipl_syntax::builtin_type_library(name)
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
                            &init.value.span,
                        );
                    };
                    let fty = self.lower(fty, &targs, &init.value.span)?;
                    let value = self.value_arg(&init.value, &fty)?;
                    if name == aipl_syntax::SPAN_TYPE {
                        // `start..end` builds a span, whose bounds are slice bounds.
                        self.index_operand(&value)?;
                    } else {
                        self.coerce_expr(
                            &value,
                            &fty,
                            &value.span,
                            Site::Field(display(name), &init.name.text),
                        )?;
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
                        Some(crate::typing::TuplePattern::Let)
                    } else if n.starts_with(aipl_syntax::FOR_TUPLE_BASE_PREFIX) {
                        Some(crate::typing::TuplePattern::For)
                    } else {
                        None
                    };
                    if let Some(pattern) = pattern {
                        let t = self.table.shallow(&x.ty);
                        let tuple = match &t {
                            It::Gen(base, _) => base.starts_with(crate::typing::TUPLE_TEMPLATE),
                            It::Named(n) => n.starts_with("__tuple$"),
                            _ => false,
                        };
                        if !tuple {
                            return reject(pattern.not_a_tuple(&self.table.show(&t)), &e.span);
                        }
                    }
                }
                let owner = self.known(&x.ty, &x.span)?;
                // A value known to be one case of a variant is read by that case's payload
                // names, as a struct is by its fields.
                if let Some((case, slots, index)) = self.case_field(&x.ty, &owner, field) {
                    // `Case@Variant`: the case's constructor, which names both.
                    if let Some((bare, variant)) = case.split_once('@') {
                        self.resolve(
                            &e.name_span,
                            Target::CaseField {
                                variant: variant.to_string(),
                                case: bare.to_string(),
                                field: field.clone(),
                            },
                        );
                    }
                    return self.read_case_field(x, case, slots, index, e);
                }
                if !self.is_struct_like(&owner, &e.span)? {
                    let named_by = self.cases_naming(&owner, field);
                    let hint = match named_by.as_slice() {
                        [] => String::new(),
                        cases => format!(
                            " — {field:?} is a field of {}, which the value is not known to be",
                            cases.join(" and ")
                        ),
                    };
                    return reject(
                        format!(
                            "field access on non-struct value of type {}{hint}",
                            self.table.show(&owner)
                        ),
                        &x.span,
                    );
                }
                if let Some(declaring) = self.declaring_struct(&owner) {
                    self.resolve(
                        &e.name_span,
                        Target::Field {
                            owner: declaring,
                            field: field.clone(),
                        },
                    );
                }
                let ty = self.field_type(&owner, field, &e.span)?;
                // A value refined to lack a case throughout holds only parts that lack it too.
                let ty = self.deepen(&ty, &self.throughout_cases(&x.ty));
                // A path a `match` arm has narrowed is what the arm made of it.
                let ty = self.narrowed_path(e).unwrap_or(ty);
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
                    other => {
                        return reject(
                            format!("cannot index a value of type {}", self.table.show(&other)),
                            &a.span,
                        )
                    }
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
                    return reject(
                        format!(
                            "cannot slice a value of type {}",
                            self.table.show(&container)
                        ),
                        &a.span,
                    );
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
                    let typed = (|| {
                        let typed = self.expr(item)?;
                        self.coerce(&typed.ty, &elem, &typed.span, "set element")?;
                        Ok(typed)
                    })();
                    out.push(self.recover(typed, item)?);
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
                if !sig.arm_params.is_empty() {
                    // The list is every type parameter and then every arm parameter, or the
                    // same without the type parameters the arm parameters determine.
                    let lengths = explicit_lengths(&sig);
                    if !lengths.contains(&tys.len()) {
                        let listed: Vec<String> = sig
                            .type_vars
                            .iter()
                            .map(|tp| tp.name.clone())
                            .chain(sig.arm_params.iter().map(|(arm, _)| arm.clone()))
                            .collect();
                        return reject(
                            format!(
                                "fn {:?} takes type arguments ({}), got {}; name all of them, \
                                 or all but the ones a case determines ({})",
                                display(callee.name()),
                                listed.join(", "),
                                tys.len(),
                                sig.arm_params
                                    .iter()
                                    .map(|(_, of)| of.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            &call.span,
                        );
                    }
                    self.explicit_args = Some(tys.clone());
                    let typed = self.expr_at(call, pos);
                    self.explicit_args = None;
                    return typed;
                }
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
                let template = format!("{}{}", crate::typing::TUPLE_TEMPLATE, tys.len());
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
                self.begin_pattern(arm);
                let (pattern, binds) = self.pattern(&arm.pattern, &scrut.ty, &arm.span)?;
                if arm.pattern.is_nested() {
                    let view = TypeView::new(self.decls);
                    let ty = self.refined_type(&st);
                    let bound =
                        crate::match_check::bind_nested(&view, &arm.pattern, &ty, arm.span.clone());
                    self.take_notes(&view);
                    bound.map_err(Failure::Rejected)?;
                }
                let declared: HashSet<String> = arm.pattern.bindings().into_iter().collect();
                let binds = binds
                    .into_iter()
                    .map(|(n, t)| {
                        let binder = arm.written.binder(&n);
                        (n, t, binder)
                    })
                    .collect();
                let assumed = self.note_matched(&scrut.ty, pattern_case(&arm.pattern));
                let scoped = self.scoped(binds, |p| p.expr_at(&arm.body, pos));
                self.forget(assumed);
                let (saved, body) = scoped?;
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
                        self.agree(&err, &want).or_else(|m| {
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
            ExprKind::Match(scrut_src, arms) => {
                let scrut = self.expr(scrut_src)?;
                let st = self.known(&scrut.ty, &scrut.span)?;
                let excluded = self.table.exclusions(&scrut.ty);
                let result = self.fresh();
                let mut out = Vec::new();
                let mut live: Vec<usize> = Vec::new();
                let mut covered: Vec<String> = Vec::new();
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
                    self.begin_pattern(arm);
                    // What a `_` or a binder stands for is the scrutinee less the cases above it.
                    let remaining = if matches!(arm.pattern, Pattern::Wildcard | Pattern::Bind(_)) {
                        self.narrowed(&scrut.ty, &covered)
                    } else {
                        scrut.ty.clone()
                    };
                    let (pattern, binds) = self.pattern(&arm.pattern, &remaining, &arm.span)?;
                    let mut binds: Vec<(String, It, Option<Span>)> = binds
                        .into_iter()
                        .map(|(n, t)| {
                            let binder = arm.written.binder(&n);
                            (n, t, binder)
                        })
                        .collect();
                    // Inside the arm the scrutinee is what the arm's pattern leaves it: a `_` or a
                    // binder the cases above it have not matched, and a case's own arm that case
                    // alone.
                    let inside = match pattern_case(&arm.pattern) {
                        Some(case) => {
                            let case = case.split('@').next().unwrap_or(case);
                            self.only_case(&scrut.ty, case)
                        }
                        None => remaining.clone(),
                    };
                    let narrows = self.table.exclusions(&inside).len() > excluded.len();
                    // A scrutinee that is a plain local is the narrowed value inside the arm.
                    if let (ExprKind::Ident(name), true) = (&scrut_src.kind, narrows) {
                        if let Some(b) = self.env.get(name).filter(|b| !b.mutable) {
                            let binder = b.binder.clone();
                            if !binds.iter().any(|(n, ..)| n == name) {
                                binds.push((name.clone(), inside.clone(), binder));
                            }
                        }
                    }
                    // So is a field path off a local that never changes (`self.kind`), wherever the
                    // body reads it.
                    let paths = self.narrowed_paths.len();
                    if let (Some((root, path)), true) = (place_path(scrut_src), narrows) {
                        if !matches!(scrut_src.kind, ExprKind::Ident(_)) {
                            if let Some(binder) = self
                                .env
                                .get(&root)
                                .filter(|b| !b.mutable)
                                .and_then(|b| b.binder.clone())
                            {
                                self.narrowed_paths.push((path, binder, inside.clone()));
                            }
                        }
                    }
                    let assumed = self.note_matched(&scrut.ty, pattern_case(&arm.pattern));
                    let scoped = self.scoped(binds, |p| p.expr_at(&arm.body, pos));
                    self.narrowed_paths.truncate(paths);
                    self.forget(assumed);
                    let (saved, body) = scoped?;
                    self.restore(saved);
                    if !aipl_syntax::diverges(&arm.body) {
                        // The arms agree on what they are; what each is known not to be is joined
                        // once they are all typed.
                        let base = self.table.shallow(&body.ty);
                        self.agree(&base, &result).or_else(|m| {
                            reject(format!("the arms of a `match` disagree ({m})"), &arm.span)
                        })?;
                        live.push(out.len());
                    }
                    if let Pattern::Ctor { name, .. } = &arm.pattern {
                        covered.push(name.split('@').next().unwrap_or(name).to_string());
                    }
                    out.push(TMatchArm {
                        pattern,
                        body,
                        span: arm.span.clone(),
                    });
                }
                self.match_rules(&scrut.ty, arms, &e.span)?;
                // What the value is known not to be is what every arm that has one is known not to be.
                let ty = if live.is_empty() {
                    result
                } else {
                    let bodies: Vec<&TExpr<It>> = live.iter().map(|&i| &out[i].body).collect();
                    self.joined_all(&bodies).or_else(|m| {
                        reject(format!("the arms of a `match` disagree ({m})"), &e.span)
                    })?
                };
                self.match_kind(&ty, arms, pos, &e.span)?;
                self.node(K::Match(Box::new(scrut), out), ty, e)
            }
            // A closure with nothing to take its parameter types from has to write them.
            ExprKind::Lambda(params, body, captures) if params.iter().all(|p| p.ty.is_some()) => {
                let mut ptys = Vec::new();
                let mut scope = Vec::new();
                let mut typed = Vec::new();
                for param in params {
                    let annotation = param.ty.as_ref().expect("checked above");
                    let t = self.lower(annotation, &HashMap::new(), &param.span)?;
                    scope.push((param.name.clone(), t.clone(), written(&param.span)));
                    ptys.push(t.clone());
                    typed.push(TLambdaParam {
                        name: param.name.clone(),
                        ty: t,
                        span: param.span.clone(),
                    });
                }
                self.reject_mutable_captures(params, body, captures, &e.span)?;
                let taken = self.enter_captures(captures, &e.span)?;
                let saved_test = std::mem::replace(&mut self.in_test, false);
                // With no function type to take them from, the effects the body performs are
                // the lambda's.
                let saved_effects = std::mem::take(&mut self.effects);
                let saved_inferring = self.inferring.replace(Vec::new());
                let scoped = self.scoped(scope, |p| p.expr(body));
                let performed = std::mem::replace(&mut self.inferring, saved_inferring);
                self.effects = saved_effects;
                self.in_test = saved_test;
                self.restore_captures(taken.saved);
                let (saved, body) = scoped?;
                self.restore(saved);
                // A closure that mutates what it captured returns nothing.
                let returns = if captures.is_empty() {
                    body.ty.clone()
                } else {
                    self.unit_body(&body)?;
                    self.finish_captures(captures, &body)?;
                    It::Unit
                };
                let ty = It::Fn(
                    ptys,
                    Box::new(returns),
                    sorted_effects(&performed.unwrap_or_default()),
                    !captures.is_empty(),
                );
                let lambda = self.node(K::Lambda(typed, Box::new(body)), ty, e);
                self.with_owned_state(taken.owned, lambda, e)
            }
            ExprKind::Lambda(params, _, _) => {
                let untyped = params.iter().find(|p| p.ty.is_none());
                let (name, span) = untyped.map_or(("x", &e.span), |p| (p.name.as_str(), &p.span));
                return reject(
                    format!(
                        "lambda parameter {name:?} used as a value must be typed, \
                         e.g. `|{name}: i64| ...`"
                    ),
                    span,
                );
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
            It::Gen(base, args) if base.starts_with(crate::typing::TUPLE_TEMPLATE) => {
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

    /// The body of a closure that mutates what it captured: it returns nothing, as a mutating
    /// method does.
    fn unit_body(&mut self, body: &TExpr<It>) -> Res<()> {
        self.agree(&body.ty, &It::Unit).or_else(|_| {
            reject(
                format!(
                    "a closure that mutates what it captures returns nothing, but its body is {}",
                    self.table.show(&body.ty)
                ),
                &body.span,
            )
        })
    }

    /// What a lambda's capture list does to the code around it while its body is typed: the
    /// variables it takes are the ones the body may `set`, and no other `mut` variable of the
    /// enclosing function may be set there. State a capture starts with is a binding of the body.
    /// Returns what to put back afterwards.
    fn enter_captures(&mut self, captures: &[Capture], _span: &Span) -> Res<CaptureScope> {
        let mut scope = CaptureScope::default();
        let mut owned: Vec<(String, It, Option<Span>)> = Vec::new();
        for (i, c) in captures.iter().enumerate() {
            if captures[..i].iter().any(|earlier| earlier.name == c.name) {
                return reject(format!("{:?} is captured twice", c.name), &c.span);
            }
            match &c.init {
                Some(init) => {
                    let value = self.expr(init)?;
                    owned.push((c.name.clone(), value.ty.clone(), written(&c.span)));
                    scope.owned.push((c.name.clone(), value));
                }
                None => {
                    let Some(b) = self.env.get(&c.name).cloned() else {
                        return reject(
                            format!("`[mut {}]` names no variable here", c.name),
                            &c.span,
                        );
                    };
                    if !b.mutable {
                        return reject(
                            format!(
                                "`[mut {0}]` takes a variable the closure mutates, and {0:?} is \
                                 not a `mut` binding",
                                c.name
                            ),
                            &c.span,
                        );
                    }
                    if !b.cell {
                        return reject(
                            format!(
                                "`[mut {0}]` takes a local variable, and {0:?} is a parameter",
                                c.name
                            ),
                            &c.span,
                        );
                    }
                    scope.taken.push(c.name.clone());
                }
            }
        }
        if captures.is_empty() {
            return Ok(scope);
        }
        let others: Vec<String> = self
            .env
            .iter()
            .filter(|(name, b)| b.mutable && !scope.taken.contains(name))
            .map(|(name, _)| name.clone())
            .collect();
        for name in others {
            let old = self.env[&name].clone();
            scope.saved.push((name.clone(), Some(old.clone())));
            self.env.insert(
                name,
                Binding {
                    mutable: false,
                    ..old
                },
            );
        }
        for (name, ty, binder) in owned {
            let old = self.env.insert(
                name.clone(),
                Binding {
                    ty,
                    mutable: true,
                    binder,
                    cell: true,
                },
            );
            scope.saved.push((name, old));
        }
        Ok(scope)
    }

    /// `lambda` with the state its capture list starts it with bound around it.
    fn with_owned_state(
        &mut self,
        owned: Vec<(String, TExpr<It>)>,
        lambda: TExpr<It>,
        at: &Expr,
    ) -> TExpr<It> {
        let ty = lambda.ty.clone();
        owned.into_iter().rev().fold(lambda, |inner, (name, init)| {
            let held = init.ty.clone();
            let made = self.cell_call(
                Callee::CellNew,
                &held,
                vec![init],
                It::Arr(Box::new(held.clone())),
                at,
            );
            self.node(
                TExprKind::Let(Self::cell_handle(&name), Box::new(made), Box::new(inner)),
                ty.clone(),
                at,
            )
        })
    }

    /// Puts back what [`Pinner::enter_captures`] changed.
    fn restore_captures(&mut self, saved: Vec<(String, Option<Binding>)>) {
        for (name, old) in saved.into_iter().rev() {
            match old {
                Some(b) => self.env.insert(name, b),
                None => self.env.remove(&name),
            };
        }
    }

    /// Once the body is typed: it has to set everything it takes.
    fn finish_captures(&mut self, captures: &[Capture], body: &TExpr<It>) -> Res<()> {
        let mut assigned = HashSet::new();
        crate::typed_util::assigned_names(body, &mut assigned);
        for c in captures {
            if !assigned.contains(&c.name) {
                return reject(
                    format!("`[mut {}]` is never set in the closure's body", c.name),
                    &c.span,
                );
            }
        }
        Ok(())
    }

    /// A function value keeps a copy of what its body reads of the function it was written in, taken
    /// when it is made, so a `mut` binding it names would be read as it was then and not as it is
    /// later: refused until a mutable capture has a meaning.
    fn reject_mutable_captures(
        &self,
        params: &[aipl_syntax::ast::LambdaParam],
        body: &Expr,
        captures: &[Capture],
        span: &Span,
    ) -> Res<()> {
        let mutable: HashMap<String, Type> = self
            .env
            .iter()
            .filter(|(name, binding)| binding.mutable && !captures.iter().any(|c| &c.name == *name))
            .map(|(name, _)| (name.clone(), Type::Unit))
            .collect();
        match crate::free_vars(body, params, &mutable).into_iter().next() {
            Some((captured, _)) => reject(
                format!(
                    "a lambda used as a value cannot capture the `mut` binding {captured:?}: it \
                     would keep the value it has now, not the later ones (write `[mut {captured}]` \
                     in front of it to let the closure take {captured:?} and change it)"
                ),
                span,
            ),
            None => Ok(()),
        }
    }

    /// An argument of a constructor, stored in the value it makes: a lambda there is a function
    /// value and keeps what it captures.
    fn constructor_arg(&mut self, arg: &Expr, expected: &It) -> Res<TExpr<It>> {
        if let ExprKind::Lambda(..) = &arg.kind {
            return self.value_arg(arg, expected);
        }
        self.arg(arg, expected)
    }

    /// An argument that is stored as a value (a field, a binding, an assigned place), where a
    /// closure is a function value: it performs no effect.
    fn value_arg(&mut self, arg: &Expr, expected: &It) -> Res<TExpr<It>> {
        let ExprKind::Lambda(params, body, captures) = &arg.kind else {
            return self.arg(arg, expected);
        };
        self.reject_mutable_captures(params, body, captures, &arg.span)?;
        self.arg(arg, expected)
    }

    /// Whether a value of this type has fields: a struct, an application of a generic one, or a
    /// tuple.
    /// The payload slot `field` names, when `value` (of the plain variant `owner`) has been refined
    /// to exactly one case: that case's loader-qualified name, the names of its payload slots and
    /// the position of `field` among them.
    fn case_field(
        &self,
        value: &It,
        owner: &It,
        field: &str,
    ) -> Option<(String, Vec<Option<String>>, usize)> {
        use aipl_syntax::ast::Exclusion;
        let (It::Named(variant) | It::Gen(variant, _)) = owner else {
            return None;
        };
        let decl = self
            .decls
            .variant_decls
            .get(variant)
            .or_else(|| self.decls.generic_variants.get(variant))?;
        let lacking = self.table.exclusions(value);
        let mut left = decl
            .cases
            .iter()
            .filter(|c| !excluded_by(&lacking, &Exclusion::NamedCase(c.name.text.clone())));
        let case = left.next()?;
        if left.next().is_some() {
            return None;
        }
        let slots: Vec<Option<String>> = case.payload.iter().map(|p| p.name.clone()).collect();
        let index = slots.iter().position(|n| n.as_deref() == Some(field))?;
        Some((format!("{}@{variant}", case.name.text), slots, index))
    }

    /// `value.field` for a value known to be one case: a `match` with that case's one arm, which
    /// binds the slot and returns it. Nothing after typing sees a field read of a variant.
    fn read_case_field(
        &mut self,
        value: TExpr<It>,
        case: String,
        slots: Vec<Option<String>>,
        index: usize,
        e: &Expr,
    ) -> Res<TExpr<It>> {
        let binder = "__case_field".to_string();
        let bindings = (0..slots.len())
            .map(|i| {
                if i == index {
                    binder.clone()
                } else {
                    "_".to_string()
                }
            })
            .collect();
        let arm = aipl_syntax::ast::MatchArm {
            pattern: Pattern::Ctor {
                name: case,
                bindings,
                ignore_payload: false,
            },
            body: Expr::new(ExprKind::Ident(binder), e.span.clone()),
            span: e.span.clone(),
            written: Default::default(),
        };
        self.begin_pattern(&arm);
        let (pattern, binds) = self.pattern(&arm.pattern, &value.ty, &arm.span)?;
        let binds = binds.into_iter().map(|(n, t)| (n, t, None)).collect();
        let scoped = self.scoped(binds, |p| p.expr(&arm.body));
        let (saved, body) = scoped?;
        self.restore(saved);
        let ty = body.ty.clone();
        let arms = vec![TMatchArm {
            pattern,
            body,
            span: e.span.clone(),
        }];
        Ok(self.node(TExprKind::Match(Box::new(value), arms), ty, e))
    }

    /// The cases of the variant `owner` whose payload has a slot called `field`.
    fn cases_naming(&self, owner: &It, field: &str) -> Vec<String> {
        let (It::Named(variant) | It::Gen(variant, _)) = owner else {
            return Vec::new();
        };
        let Some(decl) = self
            .decls
            .variant_decls
            .get(variant)
            .or_else(|| self.decls.generic_variants.get(variant))
        else {
            return Vec::new();
        };
        decl.cases
            .iter()
            .filter(|c| c.payload.iter().any(|p| p.name.as_deref() == Some(field)))
            .map(|c| c.name.text.clone())
            .collect()
    }

    /// The variant and the other cases of `Case@Variant` when it is a case of a generic variant.
    fn bare_generic_case(&self, name: &str) -> Option<(String, Vec<String>)> {
        let (case, variant) = name.split_once('@')?;
        let decl = self.decls.generic_variants.get(variant)?;
        let others = decl
            .cases
            .iter()
            .map(|c| c.name.text.clone())
            .filter(|c| c != case)
            .collect();
        Some((variant.to_string(), others))
    }

    fn is_struct_like(&mut self, owner: &It, span: &Span) -> Res<bool> {
        let declared = match owner {
            It::Named(n) => self.decls.structs.contains_key(n),
            It::Gen(n, _) => self.decls.generic_structs.contains_key(n),
            _ => false,
        };
        Ok(declared || self.tuple_elems(owner, span)?.is_some())
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
            return reject(
                format!("struct {} has no field {field:?}", self.table.show(owner)),
                span,
            );
        };
        self.lower(fty, &map, span)
    }

    /// A name that is not a local: a function used as a value, or a constructor without arguments.
    fn reference(&mut self, e: &Expr, name: &str) -> Res<TExpr<It>> {
        if let Some(variant) = self.decls.ctors.get(name).cloned() {
            let bare = name.split('@').next().unwrap_or(name);
            self.resolve(
                &e.name_span,
                Target::Case {
                    variant: variant.clone(),
                    case: bare.to_string(),
                },
            );
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
            self.resolve(
                &e.name_span,
                Target::Case {
                    variant: base.clone(),
                    case: bare.to_string(),
                },
            );
            let payload = tmpl
                .cases
                .iter()
                .find(|c| c.name.text == bare)
                .map(|c| c.payload.len())
                .unwrap_or(0);
            if payload == 0 {
                self.variant_constructions
                    .push((base.clone(), vars.clone(), e.span.clone()));
            }
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
            self.resolve(&e.name_span, function_target(name));
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
            let (ptys, ret, _) = self.instantiate(name, &sig, &e.span)?;
            return Ok(self.node(
                TExprKind::Ident(name.to_string()),
                It::Fn(ptys, Box::new(ret), sorted_effects(&sig.effects), false),
                e,
            ));
        }
        reject(format!("unknown identifier {name:?}"), &e.span)
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
                            .any(|p| crate::typing::mentions_type_named(&p.ty, &t.name))
                            && sig
                                .return_ty
                                .as_ref()
                                .is_some_and(|r| crate::typing::mentions_type_named(r, &t.name))
                    })
                    .collect(),
                defaults: sig.type_vars.iter().map(|t| t.default.clone()).collect(),
                span: span.clone(),
            });
        }
        // `f::<u8>(x)`: the written type arguments are the variables' types.
        let mut written_arms: Vec<(String, It)> = Vec::new();
        if let Some(written) = self.explicit_args.take() {
            let mut given = written;
            if sig.arm_params.is_empty() {
                for tp in &sig.type_vars[given.len()..] {
                    given.push(tp.default.clone().expect("the rest have defaults"));
                }
            }
            // With arm parameters the list is the type parameters (less the ones a case
            // determines, when the list is the short one), then the arm parameters.
            let determined: Vec<&String> = sig.arm_params.iter().map(|(_, of)| of).collect();
            let all = given.len() == sig.type_vars.len() + sig.arm_params.len();
            let listed: Vec<usize> = (0..sig.type_vars.len())
                .filter(|i| {
                    sig.arm_params.is_empty()
                        || all
                        || !determined.contains(&&sig.type_vars[*i].name)
                })
                .collect();
            for (slot, ty) in given.iter().enumerate() {
                let ty = self.lower(ty, &HashMap::new(), span)?;
                match listed.get(slot) {
                    Some(i) if sig.arm_params.is_empty() || slot < listed.len() => {
                        self.agree(&type_args[*i], &ty).or_else(|m| {
                            reject(format!("the type argument does not fit ({m})"), span)
                        })?;
                    }
                    _ => {
                        let arm = &sig.arm_params[slot - listed.len()];
                        written_arms.push((arm.0.clone(), ty));
                    }
                }
            }
        }
        // Each refinement parameter is a name of its own at this call, settled by the arguments.
        let saved_names = if sig.case_params.is_empty() {
            None
        } else {
            let domains = match case_domains(sig) {
                Ok(d) => d,
                Err(m) => return reject(m, span),
            };
            let mut names = HashMap::new();
            for (param, variant) in domains {
                let slot = format!("${param}#{}", self.table.case_slots.len());
                self.table.case_slots.insert(slot.clone(), None);
                self.domains.insert(slot.clone(), variant);
                if sig.arm_params.iter().any(|(arm, _)| *arm == param) {
                    self.arm_slots.push((
                        display(name).to_string(),
                        param.clone(),
                        slot.clone(),
                        span.clone(),
                    ));
                }
                names.insert(param, slot);
            }
            // `f::<Circle>`: a case written for an arm parameter is its arm type. Its base is
            // the variant the parameter's type parameter stands for, and the cases it excludes
            // are what the parameter stands for.
            for (arm, ty) in std::mem::take(&mut written_arms) {
                let of = &sig
                    .arm_params
                    .iter()
                    .find(|(a, _)| *a == arm)
                    .expect("an arm parameter")
                    .1;
                let members: Vec<String> = self
                    .table
                    .exclusions(&ty)
                    .into_iter()
                    .filter_map(|x| match x {
                        aipl_syntax::ast::Exclusion::NamedCase(c) => Some(c),
                        _ => None,
                    })
                    .collect();
                let base = self.unrefined(&ty);
                let one_case = match &base {
                    It::Named(v) => self.decls.variants.get(v.as_str()).is_some_and(|cases| {
                        cases.iter().filter(|(c, _)| !members.contains(c)).count() == 1
                    }),
                    _ => false,
                };
                if !one_case {
                    return reject(
                        format!(
                            "{arm:?} stands for one case of {}, so the type argument has to be a single \
                             case, not {}",
                            self.table.show(&base),
                            self.table.show(&ty)
                        ),
                        span,
                    );
                }
                if let Some(i) = sig.type_vars.iter().position(|tp| tp.name == *of) {
                    self.agree(&type_args[i], &base).or_else(|m| {
                        reject(format!("the type argument does not fit ({m})"), span)
                    })?;
                }
                self.table
                    .case_slots
                    .insert(names[&arm].clone(), Some(members));
            }
            Some(std::mem::replace(&mut self.case_names, names))
        };
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
        if let Some(names) = saved_names {
            self.case_names = names;
        }
        let (ptys, ret) = lowered?;
        type_args.extend(std::mem::take(&mut self.anonymous_vars));
        Ok((ptys, ret, type_args))
    }

    /// The type two operands of one operator share: the wider of two integers, else what they
    /// agree on.
    fn join_operands(
        &mut self,
        l: &It,
        r: &It,
        span: &Span,
        callee: &Callee,
        op: Option<aipl_syntax::ast::BinOp>,
    ) -> Res<It> {
        // Nothing widens at an operator: both operands are one type.
        let (a, b) = (self.table.shallow(l), self.table.shallow(r));
        self.agree(&a, &b).or_else(|_| {
            let (left, right) = (self.table.show(&a), self.table.show(&b));
            let arithmetic = matches!(
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
            let message = match op {
                _ if arithmetic => format!("arithmetic operand: expected {left}, got {right}"),
                Some(o) => format!(
                    "\"{}\" between {left} and {right}: both sides must be the same type",
                    aipl_syntax::binop_spelling(o)
                ),
                None => format!(
                    "\"{}\" between {left} and {right}: both sides must be the same type",
                    display(callee.name())
                ),
            };
            reject(message, span)
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
                    It::Gen(n, _) if n.starts_with(crate::typing::TUPLE_TEMPLATE) => {
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
        self.refined(t, false)
    }

    /// [`Pinner::refined_type`] for a message: a variable nothing has constrained is `_`, not the
    /// `i64` it would be pinned to, which would read as a requirement.
    fn spelled_type(&self, t: &It) -> Type {
        self.refined(t, true)
    }

    fn refined(&self, t: &It, unconstrained_as_unknown: bool) -> Type {
        let boxed = |t: &It| Box::new(self.refined(t, unconstrained_as_unknown));
        let raw = self.table.chase(t);
        match raw {
            It::Without(inner, x) => self
                .table
                .expand(&x)
                .into_iter()
                .fold(*boxed(&inner), |t, x| Type::Without(Box::new(t), x)),
            It::Opt(t) => Type::Optional(boxed(&t)),
            It::Arr(t) => Type::Array(boxed(&t)),
            It::Case(t) => Type::Case(boxed(&t)),
            It::Fn(ps, r, effects, mutating) => Type::Fn(
                ps.iter()
                    .map(|p| self.refined(p, unconstrained_as_unknown))
                    .collect(),
                boxed(&r),
                effects,
                mutating,
            ),
            It::Res(a, b) => Type::Result(boxed(&a), boxed(&b)),
            It::Dict(k, v) => Type::Dict(boxed(&k), boxed(&v)),
            It::Gen(base, args) if base.starts_with(crate::typing::TUPLE_TEMPLATE) => {
                Type::Generic(
                    base,
                    args.iter()
                        .map(|a| self.refined(a, unconstrained_as_unknown))
                        .collect(),
                )
            }
            It::Var(v)
                if unconstrained_as_unknown
                    && matches!(self.table.slots[v].kind, Kind::Any)
                    && !self.table.slots[v].diverges =>
            {
                Type::Unknown
            }
            other => self.table.resolve(&other).to_type(),
        }
    }

    /// The checks on a `match` that depend on its patterns alone: each arm's patterns fit the
    /// scrutinee, every case is covered once, nothing is unreachable.
    fn match_rules(
        &mut self,
        st: &It,
        arms: &[aipl_syntax::ast::MatchArm],
        span: &Span,
    ) -> Res<()> {
        let view = TypeView::new(self.decls);
        let ty = self.refined_type(st);
        let rejected = |e| Failure::Rejected(e);
        if arms.iter().any(|a| a.pattern.is_nested()) {
            let mut rows = Vec::new();
            for arm in arms {
                self.begin_pattern(arm);
                let bound =
                    crate::match_check::bind_nested(&view, &arm.pattern, &ty, arm.span.clone());
                self.take_notes(&view);
                let (pat, _) = bound.map_err(rejected)?;
                rows.push(vec![pat]);
            }
            crate::match_check::check_nested_exhaustive(&view, &rows, &ty, arms, span.clone())
                .map_err(rejected)
        } else {
            crate::match_check::check_match_exhaustive(&view, &ty, arms, span.clone())
                .map_err(rejected)
        }
    }

    /// `st`, a scrutinee's type, as the value is once the cases in `covered` are matched: a
    /// variant less those cases. Anything that is not a variant is unchanged.
    fn narrowed(&self, st: &It, covered: &[String]) -> It {
        let mut out = self.table.chase(st);
        if covered.is_empty() || !matches!(self.table.shallow(&out), It::Named(_) | It::Gen(..)) {
            return out;
        }
        let view = TypeView::new(self.decls);
        let Some(cases) = crate::match_check::MatchInfo::cases_of(&view, &self.refined_type(st))
        else {
            return out;
        };
        for case in covered {
            let wanted = aipl_syntax::ast::Exclusion::NamedCase(case.clone());
            if cases.iter().any(|(c, _)| c == case)
                && !self.table.exclusions(&out).contains(&wanted)
            {
                out = It::Without(Box::new(out), wanted);
            }
        }
        out
    }

    /// The type a `match` arm has narrowed the field path `e` to, when it is in one of them.
    fn narrowed_path(&self, e: &Expr) -> Option<It> {
        let (root, path) = place_path(e)?;
        let binder = self.env.get(&root)?.binder.clone()?;
        self.narrowed_paths
            .iter()
            .rev()
            .find(|(p, b, _)| *p == path && *b == binder)
            .map(|(_, _, t)| t.clone())
    }

    /// `st`, a scrutinee's type, as the value is inside the arm that matched `case`: the variant
    /// with every other case excluded. Anything that is not a variant is unchanged.
    fn only_case(&self, st: &It, case: &str) -> It {
        let view = TypeView::new(self.decls);
        let Some(cases) = crate::match_check::MatchInfo::cases_of(&view, &self.refined_type(st))
        else {
            return self.table.chase(st);
        };
        if !cases.iter().any(|(c, _)| c == case) {
            return self.table.chase(st);
        }
        let others: Vec<String> = cases
            .iter()
            .map(|(c, _)| c.clone())
            .filter(|c| c != case)
            .collect();
        self.narrowed(st, &others)
    }

    /// Note, for an arm of a match on `scrutinee`, that the scrutinee matched the case `case`
    /// names: a refinement parameter the scrutinee lacks is therefore not that case, since a
    /// value that was could not have got here. Returns how many notes it made, for
    /// [`Pinner::forget`].
    fn note_matched(&self, scrutinee: &It, case: Option<&str>) -> usize {
        let Some(case) = case else { return 0 };
        let bare = case.split('@').next().unwrap_or(case).to_string();
        let params: Vec<String> = self
            .table
            .exclusions(scrutinee)
            .into_iter()
            .filter_map(|x| match x {
                aipl_syntax::ast::Exclusion::NamedCase(p)
                | aipl_syntax::ast::Exclusion::Throughout(p) => Some(p),
                aipl_syntax::ast::Exclusion::EmptyArray => None,
            })
            .filter(|p| self.domains.contains_key(p))
            .collect();
        let mut assumed = self.assumed.borrow_mut();
        for p in &params {
            assumed.push((bare.clone(), p.clone()));
        }
        params.len()
    }

    fn forget(&self, notes: usize) {
        let mut assumed = self.assumed.borrow_mut();
        let keep = assumed.len() - notes;
        assumed.truncate(keep);
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
        if excluded.iter().any(|x| {
            matches!(x, aipl_syntax::ast::Exclusion::NamedCase(c)
                    | aipl_syntax::ast::Exclusion::Throughout(c) if c == bare)
        }) {
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

    /// The type of `xs.collect_arms::<Circle>()` or `x.as_case::<Circle>()`: the array (or optional) of
    /// the variant less every case but the one named, so a `match` on it needs that arm alone.
    /// When `element` is a generic struct holding `variant` as a type argument: the struct's
    /// template, its arguments and which of them is the variant. A concrete application is an
    /// instance named for the template and its arguments (`Tagged$Shape`), which is read back
    /// here when every argument is the variant, the one case an instance name says in full.
    fn holds_variant(&self, element: &It, variant: &It) -> Option<(String, Vec<It>, usize)> {
        let wanted = self.table.resolve(variant);
        match self.table.shallow(element) {
            It::Gen(name, args) => {
                let i = args.iter().position(|a| self.table.resolve(a) == wanted)?;
                Some((name, args, i))
            }
            It::Named(inst) => {
                let (template, parts) = inst.split_once('$')?;
                let declared = self.decls.generic_structs.get(template)?;
                let It::Named(plain) = self.table.shallow(variant) else {
                    return None;
                };
                let parts: Vec<&str> = parts.split('$').collect();
                if parts.len() != declared.type_vars.len() || parts.iter().any(|p| *p != plain) {
                    return None;
                }
                let args = vec![It::Named(plain); parts.len()];
                Some((template.to_string(), args, 0))
            }
            _ => None,
        }
    }

    fn refine_collected_arms(
        &mut self,
        ret: It,
        arm: &Expr,
        builtin: &str,
        variant: &It,
    ) -> Res<It> {
        let shallow = self.table.shallow(&ret);
        let (element, wrap): (It, fn(It) -> It) = match &shallow {
            It::Arr(elem) => ((**elem).clone(), |t| It::Arr(Box::new(t))),
            It::Opt(elem) => ((**elem).clone(), |t| It::Opt(Box::new(t))),
            _ => return Ok(ret),
        };
        // The element is the variant, or a generic struct holding it as a type argument
        // (`Expr<ExprKind>`), whose argument is what is refined.
        let wanted = self.table.resolve(variant);
        let (elem, rebuild): (It, Box<dyn Fn(It) -> It>) = if self.table.resolve(&element) == wanted
        {
            (element, Box::new(|t| t))
        } else if let Some((name, args, i)) = self.holds_variant(&element, variant) {
            let slot = args[i].clone();
            (
                slot,
                Box::new(move |t| {
                    let mut args = args.clone();
                    args[i] = t;
                    It::Gen(name.clone(), args)
                }),
            )
        } else {
            return Ok(ret);
        };
        let elem_ty = self.table.resolve(&elem).to_type();
        let view = TypeView::new(self.decls);
        // Inside a generic body the variant is a type variable and has no cases to name.
        let Some(cases) = crate::match_check::MatchInfo::cases_of(&view, &elem_ty) else {
            return Ok(ret);
        };
        let Some((case, _)) = aipl_syntax::ctor_ref_case(arm) else {
            return reject(
                format!(
                    "{short:?} needs the case written as a type argument, as in \
                     `x.{short}::<Circle>()` — a value of type `Case<..>` does not say \
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
        Ok(wrap(rebuild(refined)))
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
    fn shim_closure_type(
        &mut self,
        e: &Expr,
        name: &str,
        value: &Expr,
        pos: Pos,
    ) -> Res<Option<It>> {
        if !aipl_syntax::is_shim_closure_name(name) {
            return Ok(None);
        }
        let ExprKind::Lambda(params, body, _) = &value.kind else {
            return Ok(None);
        };
        let Some((_, shim)) = aipl_syntax::shim_closure_chain(e) else {
            return reject(
                "a shim closure's name is reserved for the parser's own lowering",
                &e.span,
            );
        };
        let ExprKind::Shim(_, bindings, shim_body) = &shim.kind else {
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
        for (param, pty) in params.iter().zip(&ptys) {
            if let Some(written) = &param.ty {
                let written = self.lower(written, &HashMap::new(), &value.span)?;
                if self.agree(pty, &written).is_err() {
                    return reject(
                        format!(
                            "closure parameter {:?} is {}, but \"{op}\" passes a {}",
                            param.name,
                            self.table.show(&written),
                            self.table.show(pty)
                        ),
                        &value.span,
                    );
                }
            }
        }
        // What the closure sets of its captures, which the shim's body must leave alone and which
        // the shim writes back when it ends.
        for name in aipl_syntax::assigned_names(body) {
            if params.iter().any(|p| p.name == name) || !self.env.contains_key(&name) {
                continue;
            }
            if pos == Pos::Value {
                return reject(
                    format!(
                        "the closure bound to \"{op}\" sets \"{name}\", so this shim changes \
                         the scope around it: end it with `;`"
                    ),
                    &value.span,
                );
            }
            let mut touched: Option<Span> = None;
            aipl_syntax::each_subexpr(shim_body, &mut |x| {
                if touched.is_none() && matches!(&x.kind, ExprKind::Ident(n) if *n == name) {
                    touched = Some(x.span.clone());
                }
            });
            if let Some(at) = touched {
                return reject(
                    format!(
                        "\"{name}\" is set by the closure bound to \"{op}\", which holds it \
                         until the shim ends; the shim's body cannot use it"
                    ),
                    &at,
                );
            }
        }
        Ok(Some(It::Fn(ptys, Box::new(ret), Vec::new(), false)))
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

    /// Start typing the pattern of `arm`: its names are the ones to resolve.
    fn begin_pattern(&mut self, arm: &aipl_syntax::ast::MatchArm) {
        self.pattern_names = (arm.written.clone(), HashMap::new());
    }

    /// Note that the pattern being typed names case `bare` of the variant `owner`, at the next
    /// place the pattern writes that case.
    fn note_case(&mut self, owner: String, bare: &str) {
        let (written, seen) = &mut self.pattern_names;
        let nth = seen.entry(bare.to_string()).or_insert(0);
        let at = written
            .cases
            .iter()
            .filter(|c| c.text == bare)
            .nth(*nth)
            .map(|c| c.span.clone());
        *nth += 1;
        if let Some(at) = at {
            self.resolve_at(
                &at,
                Target::Case {
                    variant: owner,
                    case: bare.to_string(),
                },
            );
        }
    }

    /// Take the cases a [`TypeView`] was told about.
    fn take_notes(&mut self, view: &TypeView<'_>) {
        for (ty, bare) in view.noted.take() {
            let owner = match &ty {
                Type::Named(n) => Some(self.template_of(n)),
                Type::Generic(base, _) => Some(base.clone()),
                _ => None,
            };
            if let Some(owner) = owner {
                self.note_case(owner, &bare);
            }
        }
    }

    /// Note that the name written at `named` refers to `target`.
    fn resolve(&mut self, named: &Option<Box<Span>>, target: Target) {
        if let Some(at) = named {
            self.resolve_at(at, target);
        }
    }

    /// [`Pinner::resolve`] for a name whose span is held bare; an empty one is the "written
    /// nowhere" placeholder and records nothing.
    fn resolve_at(&mut self, at: &Span, target: Target) {
        if at.start < at.end {
            self.resolutions.push(Resolution {
                in_fn: self.fn_name.clone(),
                at: at.clone(),
                target,
            });
        }
    }

    /// The template a generic instance (`Opt$i64`) was made from; any other name is its own.
    fn template_of(&self, name: &str) -> String {
        if let Some((base, _)) = self.table.instances.borrow().get(name).or(self
            .decls
            .instances
            .borrow()
            .get(name))
        {
            return base.clone();
        }
        match name.split_once('$') {
            Some((base, _)) if self.decls.generic_templates.contains(base) => base.to_string(),
            _ => name.to_string(),
        }
    }

    /// The item that declares the struct `ty` names: the struct itself, or the template an
    /// instance was made from. `None` for a type that is no struct.
    fn declaring_struct(&self, ty: &It) -> Option<String> {
        match self.table.shallow(ty) {
            It::Named(n) => Some(self.template_of(&n)),
            It::Gen(base, _) => Some(base),
            _ => None,
        }
    }

    /// Go on past a statement the program got wrong. The error is kept, and a node of a type
    /// nothing is known about stands where the statement was, so the statements after it are
    /// still checked. A form the pinner cannot type once an error is held is taken as a
    /// consequence of it: the function is rejected either way.
    fn recover(&mut self, typed: Res<TExpr<It>>, e: &Expr) -> Res<TExpr<It>> {
        match typed {
            Err(Failure::Rejected(err)) => self.errors.push(err),
            Err(Failure::Gap(..)) if !self.errors.is_empty() => {}
            other => return other,
        }
        let ty = self.fresh();
        Ok(self.node(TExprKind::Unit, ty, e))
    }

    /// Stop walking a body that has had errors, where checking more would only echo them.
    fn stop_if_recovered(&self, span: &Span) -> Res<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            gap("a body with errors", span)
        }
    }

    /// `t`, with its variables followed, if its outermost shape is known. A rule that has to look
    /// at a type's shape before it can say anything cannot run on a type nothing has constrained
    /// yet; that is a gap in the pinner, not an error in the program.
    fn known(&self, t: &It, span: &Span) -> Res<It> {
        match self.table.shallow(t) {
            // A number nothing has fixed the width of is an `i64` as far as a rule about what a
            // value can be used for goes; the variable itself is left for the context to decide.
            It::Var(v) if self.table.slots[v].kind == Kind::Int => Ok(It::Prim(Primitive::I64)),
            It::Var(_) => gap("a rule needs a type that is not known yet", span),
            t => Ok(t),
        }
    }

    /// Bring `bindings` into scope for `f`, handing back what they shadowed.
    fn scoped<R>(
        &mut self,
        bindings: Vec<(String, It, Option<Span>)>,
        f: impl FnOnce(&mut Self) -> Res<R>,
    ) -> Res<(Vec<(String, Option<Binding>)>, R)> {
        let saved = bindings
            .into_iter()
            .map(|(name, ty, binder)| {
                let old = self.env.insert(
                    name.clone(),
                    Binding {
                        ty,
                        mutable: false,
                        binder,
                        cell: false,
                    },
                );
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
        // What the scrutinee is refined to lack: the shape below sees only its base, so the
        // refinement is carried to the payloads and the binders from here.
        let raw = self.table.chase(st);
        let deep = self.throughout_cases(&raw);
        if matches!(pattern, Pattern::Ctor { .. } | Pattern::Nested { .. }) {
            self.excluded_case(&raw, &self.table.exclusions(&raw), pattern, span)?;
        }
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
                        vec![(name.clone(), self.deepen(&raw, &deep))],
                    )
                }
            }
            Pattern::Ctor {
                name,
                bindings,
                ignore_payload,
            } => {
                if matches!(st, It::Prim(Primitive::Str)) {
                    return reject(
                        "\"match\" on a str expects string literals, `[..]` patterns, or `_`",
                        span,
                    );
                }
                let bare = name.split('@').next().unwrap_or(name);
                if matches!(st, It::Named(_) | It::Gen(..)) {
                    if let Some(owner) = self.declaring_struct(st) {
                        self.note_case(owner, bare);
                    }
                }
                let payload: Vec<It> = self
                    .ctor_payload(st, bare, span)?
                    .iter()
                    .map(|t| self.deepen(t, &deep))
                    .collect();
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
                    return reject(
                        format!(
                            "constructor {name:?} binds {} value(s), but {} given",
                            payload.len(),
                            bindings.len()
                        ),
                        span,
                    );
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
                let payload: Vec<It> = self
                    .ctor_payload(st, bare, span)?
                    .iter()
                    .map(|t| self.deepen(t, &deep))
                    .collect();
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
            || (base.starts_with(crate::typing::TUPLE_TEMPLATE) && inst.starts_with("__tuple$"));
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
        if base.starts_with(crate::typing::TUPLE_TEMPLATE) {
            let elems = self
                .tuple_elems(&It::Named(inst.to_string()), &Span::default())
                .map_err(|_| "a tuple element".to_string())?
                .ok_or_else(|| format!("{inst} is not a known tuple"))?;
            if elems.len() != targs.len() {
                return Err("tuples of different sizes".into());
            }
            // The instance is the tuple of the plain types; what its elements are refined to
            // is the application's own.
            for (got, want) in elems.iter().zip(targs) {
                let (got, want) = (self.table.shallow(got), self.table.shallow(want));
                self.agree(&got, &want)?;
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
            // An instance is laid out for the plain types; what an argument is refined to is the
            // application's own, so it is not held against the instance's fields.
            let want = self.unrefined(&want);
            self.agree(&got, &want)?;
        }
        Ok(())
    }

    /// The call that `e` (a use of a spread operand: its length, the seed of the literal, or a
    /// piece appended to it) stands for once the operand's type is known, or `None` when the call
    /// is not one of those or the operand is an array and needs no change. An optional operand
    /// reads as the array of what it holds: `[v]` for `some(v)`, `[]` for `none`, and a length of
    /// one or zero.
    fn spread_operand(&mut self, e: &Expr, callee: &Callee, args: &[Expr]) -> Res<Option<Expr>> {
        use aipl_syntax::ast::{MatchArm, WrittenNames};
        // The in-place form appends to the literal's own accumulator, a name no program can write.
        let appends = matches!(callee, Callee::Extend | Callee::ExtendMap)
            && matches!(args.first().map(|a| &a.kind), Some(ExprKind::Ident(n)) if n.starts_with("__spread$"));
        let slot = match callee {
            Callee::SpreadLen | Callee::ArrReserve => 0,
            Callee::ArrConcat => 1,
            Callee::Extend | Callee::ExtendMap if appends => 1,
            _ => return Ok(None),
        };
        let Some(operand) = args.get(slot) else {
            return Ok(None);
        };
        let typed = self.expr(operand)?;
        let optional = matches!(self.table.shallow(&typed.ty), It::Opt(_));
        let span = operand.span.clone();
        let node = |kind| Expr::new(kind, span.clone());
        if *callee == Callee::SpreadLen {
            if !optional {
                return Ok(Some(Expr::new(
                    ExprKind::Call(Callee::Len, args.to_vec(), true),
                    e.span.clone(),
                )));
            }
            // What an optional holds counts as one, and nothing as zero: `u64`s, as every other
            // length summed beside it is.
            let count = |n: i64| {
                let name = format!("__spread_count${}_{n}", span.start);
                node(ExprKind::Let(
                    name.clone(),
                    Some(Type::Primitive(Primitive::U64)),
                    Box::new(node(ExprKind::Num(n))),
                    Box::new(node(ExprKind::Ident(name))),
                ))
            };
            let present = MatchArm {
                pattern: Pattern::Ctor {
                    name: "some".to_string(),
                    bindings: Vec::new(),
                    ignore_payload: true,
                },
                body: count(1),
                span: span.clone(),
                written: WrittenNames::default(),
            };
            return Ok(Some(node(ExprKind::IfLet(
                Box::new(present),
                Box::new(operand.clone()),
                Box::new(count(0)),
            ))));
        }
        // What an optional holds, as an array: `[v]` for `some(v)`, `[]` for `none`.
        let held = format!("__spread_held${}", span.start);
        let as_array = |value: Expr| {
            let arm = MatchArm {
                pattern: Pattern::Ctor {
                    name: "some".to_string(),
                    bindings: vec![held.clone()],
                    ignore_payload: false,
                },
                body: node(ExprKind::ArrayLit(vec![node(ExprKind::Ident(
                    held.clone(),
                ))])),
                span: span.clone(),
                written: WrittenNames::default(),
            };
            node(ExprKind::IfLet(
                Box::new(arm),
                Box::new(value),
                Box::new(node(ExprKind::ArrayLit(Vec::new()))),
            ))
        };
        if !optional {
            return Ok(None);
        }
        // `extend_map(acc, optional, f)` appends what `optional.map(f)` holds.
        if *callee == Callee::ExtendMap {
            let [accumulator, receiver, function] = args else {
                return Ok(None);
            };
            let mapped = Expr::new(
                ExprKind::Call(Callee::Map, vec![receiver.clone(), function.clone()], true),
                e.span.clone(),
            );
            return Ok(Some(Expr::new(
                ExprKind::Call(
                    Callee::Extend,
                    vec![accumulator.clone(), as_array(mapped)],
                    true,
                ),
                e.span.clone(),
            )));
        }
        let mut rewritten = args.to_vec();
        rewritten[slot] = as_array(operand.clone());
        let method_style = matches!(callee, Callee::Extend);
        Ok(Some(Expr::new(
            ExprKind::Call(callee.clone(), rewritten, method_style),
            e.span.clone(),
        )))
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
        let call_with = |type_args: Vec<It>, args: Vec<TExpr<It>>, ret: It, p: &Self| {
            p.node(
                K::Call {
                    callee: callee.clone(),
                    type_args,
                    args,
                    method_style,
                },
                ret,
                e,
            )
        };
        let call = |args: Vec<TExpr<It>>, ret: It, p: &Self| call_with(Vec::new(), args, ret, p);
        // The loader's array-literal spread names its operand wherever it uses it, before any type
        // is known. An operand that is an optional is read as the zero or one elements it holds.
        if let Some(rewritten) = self.spread_operand(e, callee, args)? {
            return self.expr(&rewritten);
        }
        // A local that holds a function is called by its name.
        if let Some(Binding { ty, binder, .. }) = self.env.get(name).cloned() {
            if let Some(binder) = binder {
                self.resolve(&e.name_span, Target::Local(binder));
            }
            let It::Fn(ptys, ret, effects, mutating) = self.table.shallow(&ty) else {
                return reject(format!("{name:?} is not a function"), &e.span);
            };
            for effect in &effects {
                self.require_effect(display(name), effect, &e.span, true)?;
            }
            if ptys.len() != args.len() {
                return reject(
                    format!("{name:?} takes {} argument(s)", ptys.len()),
                    &e.span,
                );
            }
            let mut typed = Vec::new();
            for (i, (arg, pty)) in args.iter().zip(&ptys).enumerate() {
                let t = self.arg(arg, pty)?;
                self.coerce_expr(&t, pty, &t.span, Site::Argument(display(name), i))?;
                typed.push(t);
            }
            // The type of the function called is the call's one type argument: it says whether the
            // call is of a closure that mutates, and what effects it performs, which what reorders
            // calls has to know.
            let _ = mutating;
            return Ok(call_with(vec![ty.clone()], typed, *ret, self));
        }
        if method_style && !args.is_empty() {
            if let Some(s) = self.decls.sigs.get(name).filter(|s| !s.sig.is_method()) {
                let _ = s;
                return reject(
                    format!(
                        "fn {:?} cannot be called as a method (its first parameter must be \
                         named \"self\")",
                        callee.display_name()
                    ),
                    &args[0].span,
                );
            }
        }
        // A builtin the compiler types itself is still the builtin the author named, whichever
        // shape it was decided to have.
        if matches!(callee, Callee::Some | Callee::IntParse)
            || aipl_syntax::builtin::Builtin::of(&callee).is_some()
        {
            self.resolve(&e.name_span, function_target(name));
        }
        match callee {
            Callee::Some if args.len() == 1 => {
                let x = self.expr(&args[0])?;
                let ty = It::Opt(Box::new(x.ty.clone()));
                return Ok(call_with(vec![x.ty.clone()], vec![x], ty, self));
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
                self.narrows.push((target.clone(), e.span.clone()));
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
            let is_float = |p: &Self, t: &It| match t {
                It::Prim(prim) => prim.is_float(),
                It::Var(v) => p.table.slots[*v].kind == Kind::Float,
                _ => false,
            };
            // The float operations name the operand that is not a float, or that is the other
            // float type.
            if float.is_some() {
                let (lt, rt) = (self.table.shallow(&l.ty), self.table.shallow(&r.ty));
                for (t, arg) in [(&lt, &args[0]), (&rt, &args[1])] {
                    if !is_float(self, t) {
                        return reject(
                            format!(
                                "\"{}\" takes IEEE 754 floats, but this operand is {}: floats \
                                 and integers never mix, and nothing converts between them",
                                display(callee.name()),
                                self.table.show(t)
                            ),
                            &arg.span,
                        );
                    }
                }
                if self.agree(&lt, &rt).is_err() {
                    return reject(
                        format!(
                            "\"{}\" takes two floats of one type, got {} and {}",
                            display(callee.name()),
                            self.table.show(&lt),
                            self.table.show(&rt)
                        ),
                        &args[1].span,
                    );
                }
            }
            let operand = self.join_operands(&l.ty, &r.ty, &e.span, callee, op)?;
            let (lt, rt) = (self.table.shallow(&l.ty), self.table.shallow(&r.ty));
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
                            crate::typing::reject_float_operand(bo, &as_type(self, t), &a.span)
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
                self.coerce(&operand, &bool_ty, &e.span, "logical operand")?;
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
            // A constructor's name compared with a `Case<V>` is that case, as it is where a
            // `Case<V>` is a parameter's type.
            let (mut l, mut r) = (l, r);
            if matches!(op, Some(BinOp::Eq | BinOp::Ne)) {
                match (self.table.shallow(&l.ty), self.table.shallow(&r.ty)) {
                    (It::Fn(..), It::Case(_)) => l.ty = r.ty.clone(),
                    (It::Case(_), It::Fn(..)) => r.ty = l.ty.clone(),
                    _ => {}
                }
            }
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
                self.variadic_arg(&seq, &pat.ty).or_else(|m| {
                    reject(
                        format!("the argument of {:?}: {m}", display(name)),
                        &args[1].span,
                    )
                })?;
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
                let element = match &seq {
                    It::Arr(t) => (**t).clone(),
                    _ => It::Prim(Primitive::Char),
                };
                return Ok(call_with(vec![element], typed, ret, self));
            }
        }
        // A constructor of a generic variant: its arguments pin the type arguments.
        if let Some(base) = self.decls.generic_ctors.get(name).cloned() {
            let tmpl = self.decls.generic_variants[&base].clone();
            let bare = name.split('@').next().unwrap_or(name);
            self.resolve(
                &e.name_span,
                Target::Case {
                    variant: base.clone(),
                    case: bare.to_string(),
                },
            );
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
                let t = self.constructor_arg(arg, &pty)?;
                self.coerce_expr(&t, &pty, &t.span, "a constructor argument")?;
                typed.push(t);
            }
            self.variant_constructions
                .push((base.clone(), vars.clone(), e.span.clone()));
            return Ok(call(typed, It::Gen(base, vars), self));
        }
        // A variant constructor `Case@Variant(..)`.
        if let Some(variant) = self.decls.ctors.get(name).cloned() {
            let bare = name.split('@').next().unwrap_or(name);
            self.resolve(
                &e.name_span,
                Target::Case {
                    variant: variant.clone(),
                    case: bare.to_string(),
                },
            );
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
                let t = self.constructor_arg(arg, &pty)?;
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
                        if let It::Fn(ptys, ret, effects, _) = self.table.shallow(&field) {
                            for effect in &effects {
                                self.require_effect(display(name), effect, &e.span, true)?;
                            }
                            if let Some(declaring) = self.declaring_struct(&owner) {
                                self.resolve(
                                    &e.name_span,
                                    Target::Field {
                                        owner: declaring,
                                        field: name.to_string(),
                                    },
                                );
                            }
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
                                    &format!("an argument to {:?}", display(name)),
                                )?;
                                typed.push(t);
                            }
                            // The field's function type, as for any call of a function value.
                            return Ok(call_with(vec![field.clone()], typed, *ret, self));
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
        self.resolve(&e.name_span, function_target(name));
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
                // A set reads as a sequence everywhere else, so `map` is the obvious reach, and
                // `set_map` the answer.
                It::Set(..) => {
                    return reject(
                        format!(
                            "map has no set form, so it does not take {} — use \"set_map\", \
                             which maps a set to a set. It is a separate name because the \
                             result can be *smaller* than the receiver, when two elements \
                             map to the same answer; `map` promises one element out per \
                             element in, and over a set it could not keep that promise",
                            aipl_syntax::type_name(&self.refined_type(&first.ty))
                        ),
                        &args[0].span,
                    );
                }
                _ => {}
            }
        }
        if sig.params.len() != args.len() {
            return reject(
                format!(
                    "fn {:?} expects {} arg(s), got {}",
                    display(name),
                    sig.params.len(),
                    args.len()
                ),
                &e.span,
            );
        }
        for effect in &sig.effects {
            self.require_effect(display(name), effect, &e.span, false)?;
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
                let mut typed = if sig.params[i].arity == aipl_syntax::ast::Arity::ReturnsMany {
                    self.returns_many_arg(arg, &ptys[i])?
                } else {
                    self.arg(arg, &ptys[i])?
                };
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
                if i == 0
                    && matches!(callee, Callee::Contains(_))
                    && matches!(self.table.shallow(&seen_ty), It::Set(..))
                {
                    return reject(
                        "\"contains\" takes an array or str receiver; for set membership use \
                         \"has\"",
                        &arg.span,
                    );
                }
                let adapted = if sig.params[i].arity.is_variadic() {
                    // A sequence that is the parameter's own type, refinement included (a
                    // literal with an element in it for a `T+`), is taken as it stands.
                    let whole = matches!(
                        self.table.shallow(&seen_ty),
                        It::Arr(_) | It::Set(..) | It::Prim(Primitive::Str)
                    ) && self
                        .coerce_expr(&typed, &ptys[i], &arg.span, "an argument")
                        .is_ok();
                    if !whole && sig.params[i].arity == aipl_syntax::ast::Arity::OneOrMore {
                        // At least one element: an optional, or an array that may be empty and
                        // is not itself the element, is neither.
                        let elem = match self.table.shallow(&ptys[i]) {
                            It::Arr(t) => self.table.shallow(&t),
                            other => other,
                        };
                        let given = self.table.shallow(&seen_ty);
                        if matches!(given, It::Opt(_))
                            || (matches!(given, It::Arr(_)) && !matches!(elem, It::Arr(_)))
                        {
                            return reject(
                                format!(
                                    "fn {:?} arg {i}: variadic parameter expects {} or {} — at \
                                     least one element, so no optional and no array that may be \
                                     empty, got {}",
                                    display(name),
                                    aipl_syntax::type_name(&self.refined_type(&ptys[i])),
                                    self.table.show(&elem),
                                    self.table.show(&seen_ty),
                                ),
                                &arg.span,
                            );
                        }
                    }
                    if !whole {
                        self.variadic_arg(&ptys[i], &seen_ty).or_else(|_| {
                            let elem = match self.table.shallow(&ptys[i]) {
                                It::Arr(t) => self.table.show(&t),
                                other => self.table.show(&other),
                            };
                            reject(
                                format!(
                                    "fn {:?} arg {i}: variadic parameter expects {}, {elem}, or \
                                     {elem}?, got {}",
                                    display(name),
                                    self.table.show(&ptys[i]),
                                    self.table.show(&seen_ty)
                                ),
                                &arg.span,
                            )
                        })?;
                    }
                    true
                } else {
                    self.adapt_receiver(callee, &ptys[i], &seen_ty)
                        .or_else(|m| {
                            reject(
                                format!("an argument to {:?}: {m}", display(name)),
                                &arg.span,
                            )
                        })?
                };
                if !adapted {
                    if as_positions {
                        self.coerce(
                            &seen_ty,
                            &ptys[i],
                            &arg.span,
                            Site::Argument(display(name), i),
                        )?;
                    } else {
                        self.coerce_expr(
                            &typed,
                            &ptys[i],
                            &arg.span,
                            Site::Argument(display(name), i),
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
        // `xs.collect_arms::<Circle>()` / `x.as_case::<Circle>()`: the elements are all that one case, so
        // the result is typed as the variant less every other case.
        // A function that hands back the sequence it was given hands back a `str` as a `str`: it
        // is an array of chars to the signature, and only to it.
        let ret = match (ptys.first(), typed_args.first()) {
            (Some(p0), Some(a0))
                if matches!(self.table.shallow(&a0.ty), It::Prim(Primitive::Str))
                    && matches!(self.table.shallow(p0), It::Arr(_))
                    && self.table.shallow(p0) == self.table.shallow(&ret) =>
            {
                It::Prim(Primitive::Str)
            }
            _ => ret,
        };
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
            let wanted = self.table.resolve(&v);
            self.table.resolve(&e) == wanted || self.holds_variant(&e, &v).is_some()
        });
        let ret = match case_param {
            Some(i) => {
                let It::Case(variant) = self.table.shallow(&ptys[i]) else {
                    unreachable!("found as a `Case` above")
                };
                self.refine_collected_arms(ret, &args[i], name, &variant)?
            }
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

    /// An argument to a `(A) -> T*` parameter, whose type `expected` is the function as the body
    /// sees it, `(A) -> T[]`. A function that returns `T[]` is taken as it stands; one that
    /// returns `T?` or `T` is wrapped in a function that returns the array of what it returned
    /// (`[]` for `none`), so the body only ever calls one that returns the sequence.
    fn returns_many_arg(&mut self, arg: &Expr, expected: &It) -> Res<TExpr<It>> {
        use aipl_syntax::ast::{LambdaParam, MatchArm, Pattern};
        let (It::Fn(params, sequence, _, _), span) =
            (self.table.shallow(expected), arg.span.clone())
        else {
            return self.arg(arg, expected);
        };
        let It::Arr(elem) = self.table.shallow(&sequence) else {
            return self.arg(arg, expected);
        };
        // The function as written, its result left for it to say.
        let result = self.fresh();
        let loose = It::Fn(params.clone(), Box::new(result.clone()), Vec::new(), false);
        let probe = self.arg(arg, &loose)?;
        self.coerce_expr(&probe, &loose, &span, "an argument")?;
        let wrong = |p: &mut Self, given: &It| -> Res<TExpr<It>> {
            reject(
                format!(
                    "an argument to a parameter that takes a function returning {} (or one \
                     element, or an optional one), got one returning {}",
                    p.table.show(&sequence),
                    p.table.show(given),
                ),
                &span,
            )
        };
        let written = self.table.shallow(&result);
        let optional = match &written {
            It::Opt(x) => {
                if self.agree(x, &elem).is_err() {
                    return wrong(self, &written);
                }
                true
            }
            It::Arr(_) => {
                if self.agree(&written, &sequence).is_err() {
                    return wrong(self, &written);
                }
                return Ok(probe);
            }
            _ => {
                if self.agree(&written, &elem).is_err() {
                    return wrong(self, &written);
                }
                false
            }
        };
        let at = |kind| Expr::new(kind, span.clone());
        // What an element or an optional element is as the sequence.
        let wrap = |value: Expr| {
            if optional {
                let arm = |pattern, body| MatchArm {
                    pattern,
                    body,
                    span: span.clone(),
                    written: Default::default(),
                };
                at(ExprKind::Match(
                    Box::new(value),
                    vec![
                        arm(
                            Pattern::Ctor {
                                name: "some".to_string(),
                                bindings: vec!["$many_item".to_string()],
                                ignore_payload: false,
                            },
                            at(ExprKind::ArrayLit(vec![at(ExprKind::Ident(
                                "$many_item".to_string(),
                            ))])),
                        ),
                        arm(
                            Pattern::Ctor {
                                name: "none".to_string(),
                                bindings: Vec::new(),
                                ignore_payload: false,
                            },
                            at(ExprKind::ArrayLit(Vec::new())),
                        ),
                    ],
                ))
            } else {
                at(ExprKind::ArrayLit(vec![value]))
            }
        };
        let wrapper = match &arg.kind {
            // The lambda, its result and each `return` of its own wrapped.
            ExprKind::Lambda(lambda_params, body, captures) => {
                fn wrap_returns(e: &mut Expr, wrap: &dyn Fn(Expr) -> Expr) {
                    if matches!(e.kind, ExprKind::Lambda(..)) {
                        return;
                    }
                    for child in e.children_mut() {
                        wrap_returns(child, wrap);
                    }
                    if let ExprKind::Return(value) = &mut e.kind {
                        let inner = std::mem::replace(
                            &mut **value,
                            Expr::new(ExprKind::Unit, e.span.clone()),
                        );
                        **value = wrap(inner);
                    }
                }
                let mut body = (**body).clone();
                wrap_returns(&mut body, &wrap);
                at(ExprKind::Lambda(
                    lambda_params.clone(),
                    Box::new(wrap(body)),
                    captures.clone(),
                ))
            }
            // `|p0, p1| wrap(f(p0, p1))`.
            ExprKind::Ident(function) if !self.env.contains_key(function) => {
                let names: Vec<String> = (0..params.len()).map(|k| format!("$many{k}")).collect();
                let call = at(ExprKind::Call(
                    Callee::User(function.clone()),
                    names
                        .iter()
                        .map(|n| at(ExprKind::Ident(n.clone())))
                        .collect(),
                    false,
                ));
                at(ExprKind::Lambda(
                    names
                        .into_iter()
                        .map(|name| LambdaParam {
                            name,
                            ty: None,
                            span: span.clone(),
                        })
                        .collect(),
                    Box::new(wrap(call)),
                    Vec::new(),
                ))
            }
            _ => {
                return reject(
                    "a function that returns one element or an optional one, where a function \
                     returning a sequence is also taken, has to be a lambda or the name of a \
                     function",
                    &span,
                )
            }
        };
        self.arg(&wrapper, expected)
    }

    /// An argument, typed with the parameter type it lands in as its context. A lambda needs the
    /// context to type its own parameters.
    /// What performing `effect` here needs: the function being typed declares it, or it is
    /// recorded as performed by the lambda whose effects are being inferred. `callee` names what
    /// performs it, for the message.
    fn require_effect(
        &mut self,
        callee: &str,
        effect: &str,
        span: &Span,
        is_value: bool,
    ) -> Res<()> {
        if self.effects.iter().any(|e| e == effect) {
            return Ok(());
        }
        match &mut self.inferring {
            Some(performed) => {
                if !performed.iter().any(|e| e == effect) {
                    performed.push(effect.to_string());
                }
                Ok(())
            }
            None if self.closure_outer.is_some() => {
                let outer = self.closure_outer.clone().unwrap_or_default();
                reject(slot_effect_message(callee, effect, &outer), span)
            }
            // A function value is called through a name that is not a function's, which the
            // message says.
            None if is_value
                && effect != "measure"
                && !self.effects.iter().any(|e| e == "measure") =>
            {
                reject(
                    format!(
                        "{callee:?} is a function value that performs \"!{effect}\", but the \
                         calling function does not declare it"
                    ),
                    span,
                )
            }
            None => reject(
                undeclared_effect_message(callee, effect, &self.effects),
                span,
            ),
        }
    }

    fn arg(&mut self, arg: &Expr, expected: &It) -> Res<TExpr<It>> {
        let mut typed = self.arg_inner(arg, expected)?;
        // A constructor's name where a `Case<V>` is wanted is that case, not the constructor function.
        if matches!(self.table.shallow(expected), It::Case(_))
            && matches!(
                self.table.shallow(&typed.ty),
                It::Fn(..) | It::Named(_) | It::Gen(..)
            )
            && aipl_syntax::ctor_ref_case(arg).is_some()
        {
            // The variant the constructor builds is the variant the `Case` is a case of.
            let built = match self.table.shallow(&typed.ty) {
                It::Fn(_, ret, _, _) => *ret,
                other => other,
            };
            if let It::Case(variant) = self.table.shallow(expected) {
                // The case is a tag: what its variant is refined to is no part of it.
                let (built, variant) = (self.table.shallow(&built), self.table.shallow(&variant));
                self.agree(&built, &variant)
                    .map_err(|m| Failure::Rejected(Error::at(m, arg.span.clone())))?;
            }
            typed.ty = expected.clone();
        }
        Ok(typed)
    }

    fn arg_inner(&mut self, arg: &Expr, expected: &It) -> Res<TExpr<It>> {
        // A named function given where a function is wanted, generic or not.
        if let ExprKind::Ident(name) = &arg.kind {
            if !self.env.contains_key(name)
                && !self.decls.ctors.contains_key(name)
                && !self.decls.generic_ctors.contains_key(name)
            {
                if let Some(sig) = self.decls.sigs.get(name).map(|s| s.sig.clone()) {
                    self.resolve(&arg.name_span, function_target(name));
                    // The function keeps its effects in its type, and the type it is handed
                    // for has to accept them.
                    let allowed = match self.table.shallow(expected) {
                        It::Fn(_, _, allowed, _) => allowed,
                        _ => sig.effects.clone(),
                    };
                    if let Some(effect) = sig.effects.iter().find(|e| !allowed.contains(e)) {
                        return reject(
                            slot_effect_message(display(name), effect, &self.effects),
                            &arg.span,
                        );
                    }
                    let (ptys, ret, _) = self.instantiate(name, &sig, &arg.span)?;
                    let ty = It::Fn(ptys, Box::new(ret), sorted_effects(&sig.effects), false);
                    return Ok(self.node(TExprKind::Ident(name.clone()), ty, arg));
                }
            }
        }
        if let ExprKind::Lambda(params, body, captures) = &arg.kind {
            let It::Fn(ptys, ret, effects, mutating) = self.table.shallow(expected) else {
                return reject("a closure where no function is expected", &arg.span);
            };
            if ptys.len() != params.len() {
                return reject(
                    format!(
                        "lambda has {} parameter(s), but {} was expected",
                        params.len(),
                        self.table.show(expected)
                    ),
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
                            binder: written(&param.span),
                            cell: false,
                        },
                    ),
                ));
                typed.push(TLambdaParam {
                    name: param.name.clone(),
                    ty: pty.clone(),
                    span: param.span.clone(),
                });
            }
            // What the capture list takes is settled before anything else is swapped in, so a
            // refusal leaves the function's own state as it was (the parameters go back first).
            let taken = match self.enter_captures(captures, &arg.span) {
                Ok(taken) => taken,
                Err(failure) => {
                    for (name, old) in saved.into_iter().rev() {
                        match old {
                            Some(b) => self.env.insert(name, b),
                            None => self.env.remove(&name),
                        };
                    }
                    return Err(failure);
                }
            };
            // A `?` in a closure leaves the closure, so a test's `?` rule is not in force there.
            let saved_test = std::mem::replace(&mut self.in_test, false);
            let saved_ret = std::mem::replace(&mut self.ret, (*ret).clone());
            // The closure may perform what the type it is given says, and nothing else.
            let saved_effects = std::mem::replace(&mut self.effects, effects.clone());
            let saved_inferring = self.inferring.take();
            let saved_closure =
                std::mem::replace(&mut self.closure_outer, Some(saved_effects.clone()));
            let body = self.expr(body);
            self.restore_captures(taken.saved);
            self.closure_outer = saved_closure;
            self.inferring = saved_inferring;
            self.effects = saved_effects;
            self.ret = saved_ret;
            self.in_test = saved_test;
            for (name, old) in saved.into_iter().rev() {
                match old {
                    Some(b) => self.env.insert(name, b),
                    None => self.env.remove(&name),
                };
            }
            let body = body?;
            if captures.is_empty() {
                self.coerce_expr(&body, &ret, &body.span, Site::Lambda)?;
            } else {
                // A closure that mutates what it captured returns nothing.
                self.unit_body(&body)?;
                self.agree(&It::Unit, &ret).or_else(|m| {
                    reject(
                        format!(
                            "a closure that mutates what it captures returns nothing, but {} \
                             is expected back ({m})",
                            self.table.show(&ret)
                        ),
                        &arg.span,
                    )
                })?;
                self.finish_captures(captures, &body)?;
            }
            let ty = It::Fn(ptys, ret, effects, mutating || !captures.is_empty());
            let lambda = self.node(TExprKind::Lambda(typed, Box::new(body)), ty, arg);
            return Ok(self.with_owned_state(taken.owned, lambda, arg));
        }
        self.expr(arg)
    }
}

/// Why a function that performs `effect` is refused by the function type it is given, which allows
/// no such effect. `outer` is what the function the code is written in declares: a benchmark
/// block has its own reason to refuse every effect.
fn slot_effect_message(callee: &str, effect: &str, outer: &[String]) -> String {
    if effect == "measure" || outer.iter().any(|e| e == "measure") {
        undeclared_effect_message(callee, effect, outer)
    } else {
        format!(
            "fn {callee:?} has effect \"!{effect}\" but the function type it is given does not \
             allow it"
        )
    }
}

/// The effects of a function type: sorted, each once.
fn sorted_effects(effects: &[String]) -> Vec<String> {
    let mut sorted = effects.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
}

fn form_name(k: &ExprKind) -> String {
    let text = format!("{k:?}");
    text.split(['(', ' ', '{']).next().unwrap_or("").to_string()
}

/// Every integer literal must fit the type it was pinned to. A literal that does not is reported
/// at `site`: the expression it was fitted as part of, which is the one a value is handed to a
/// type as (an argument, a bound value, a field, a returned value, the body), so
/// `if (b) { 1 } else { 300 }` is wrong as a whole.
fn check_literals(e: &TExpr<Ty>, site: &Span) -> Res<()> {
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
                            site,
                        );
                    }
                }
                return Ok(());
            }
        }
        TExprKind::Int(n) => {
            if let Some(name) = name(&e.ty) {
                if !aipl_syntax::int_fits(*n, &name) {
                    return reject(format!("integer literal {n} does not fit in {name}"), site);
                }
            }
        }
        _ => {}
    }
    let mut result = Ok(());
    // The children a value is handed to a type as.
    let (value, every): (Option<&TExpr<Ty>>, bool) = match &e.kind {
        TExprKind::Let(_, v, _) | TExprKind::LetMut(_, v, _) | TExprKind::Assign(_, v, _) => {
            (Some(&**v), false)
        }
        TExprKind::Call { .. } | TExprKind::Construct(..) | TExprKind::Return(_) => (None, true),
        _ => (None, false),
    };
    e.for_each_child(&mut |c| {
        if result.is_ok() {
            let is_site = every || value.is_some_and(|v| std::ptr::eq(v, c));
            result = check_literals(c, if is_site { &c.span } else { site });
        }
    });
    result
}

/// The case a source pattern names, if it names one.
/// The local a place is rooted in, and the whole path written: `self.kind` is `("self",
/// "self.kind")`. `None` for anything that is not a name or a field path off one.
fn place_path(e: &Expr) -> Option<(String, String)> {
    match &e.kind {
        ExprKind::Ident(name) => Some((name.clone(), name.clone())),
        ExprKind::Field(obj, field) => {
            let (root, path) = place_path(obj)?;
            Some((root, format!("{path}.{field}")))
        }
        _ => None,
    }
}

fn pattern_case(p: &Pattern) -> Option<&str> {
    match p {
        Pattern::Ctor { name, .. } | Pattern::Nested { name, .. } => Some(name),
        _ => None,
    }
}

/// The case a typed pattern names, if it names one.
fn typed_pattern_case(p: &TPattern<It>) -> Option<&str> {
    match p {
        TPattern::Ctor { name, .. } | TPattern::Nested { name, .. } => Some(name),
        _ => None,
    }
}

/// The variant each refinement parameter of `sig` takes its cases from: the base of a
/// How many type arguments a call of a function with arm parameters may write: all of its type
/// parameters and arm parameters, or all but the type parameters its arm parameters determine.
fn explicit_lengths(sig: &aipl_syntax::ast::Signature) -> Vec<usize> {
    let all = sig.type_vars.len() + sig.arm_params.len();
    let mut determined: Vec<&String> = sig.arm_params.iter().map(|(_, of)| of).collect();
    determined.sort();
    determined.dedup();
    vec![all, all - determined.len()]
}

/// `T without X` written in the signature, which has to be a named type.
fn case_domains(sig: &aipl_syntax::ast::Signature) -> Result<HashMap<String, String>, String> {
    fn walk(
        t: &Type,
        params: &[String],
        arms: &[(String, String)],
        out: &mut HashMap<String, String>,
    ) {
        match t {
            Type::Without(base, aipl_syntax::ast::Exclusion::NamedCase(n))
                if params.contains(n) =>
            {
                match aipl_syntax::unrefined(base) {
                    Type::Named(v) => {
                        out.entry(n.clone()).or_insert_with(|| v.clone());
                    }
                    // An arm parameter's variant is a type parameter, so the domain is not a
                    // declared variant: it is named for the parameter, which no declaration
                    // reaches.
                    Type::TypeVar(v) if arms.iter().any(|(arm, of)| arm == n && of == v) => {
                        out.entry(n.clone()).or_insert_with(|| format!("<{v}>"));
                    }
                    _ => {}
                }
                walk(base, params, arms, out);
            }
            Type::Without(i, _)
            | Type::Case(i)
            | Type::Optional(i)
            | Type::Array(i)
            | Type::Set(i, _) => walk(i, params, arms, out),
            Type::Dict(a, b) | Type::Result(a, b) => {
                walk(a, params, arms, out);
                walk(b, params, arms, out);
            }
            Type::Generic(_, args) | Type::Tuple(args) => {
                args.iter().for_each(|a| walk(a, params, arms, out))
            }
            Type::Fn(ps, r, _, _) => {
                ps.iter().for_each(|p| walk(p, params, arms, out));
                walk(r, params, arms, out);
            }
            _ => {}
        }
    }
    let mut out = HashMap::new();
    for p in &sig.params {
        walk(&p.ty, &sig.case_params, &sig.arm_params, &mut out);
    }
    if let Some(r) = &sig.return_ty {
        walk(r, &sig.case_params, &sig.arm_params, &mut out);
    }
    for name in &sig.case_params {
        if !out.contains_key(name) {
            return Err(format!(
                "refinement parameter {name:?} has to be written as `T without {name}` somewhere in the signature, so the cases it stands for belong to a named type"
            ));
        }
    }
    Ok(out)
}

/// `t` as found inside a value that lacks the case `case` throughout: every part of it that can
/// hold one is refined to lack it too.
fn refine_throughout(tables: &crate::typing::DeclTables, t: &Type, case: &str) -> Type {
    let go = |i: &Type| Box::new(refine_throughout(tables, i, case));
    let wanted = aipl_syntax::ast::Exclusion::Throughout(case.to_string());
    match t {
        Type::Named(_) | Type::Generic(..) => {
            if tables.reaches_case(t, case) {
                Type::Without(Box::new(t.clone()), wanted)
            } else {
                t.clone()
            }
        }
        Type::Without(inner, x) if *x == wanted => Type::Without(inner.clone(), x.clone()),
        Type::Without(inner, x) => Type::Without(go(inner), x.clone()),
        Type::Optional(i) => Type::Optional(go(i)),
        Type::Array(i) => Type::Array(go(i)),
        Type::Case(i) => Type::Case(go(i)),
        Type::Set(i, o) => Type::Set(go(i), *o),
        Type::Dict(k, v) => Type::Dict(go(k), go(v)),
        Type::Result(a, b) => Type::Result(go(a), go(b)),
        other => other.clone(),
    }
}

/// The declarations seen as the match rules need them (`match_check`).
struct TypeView<'a> {
    decls: &'a Decls,
    /// The cases the patterns checked against it named, in the order they did.
    noted: std::cell::RefCell<Vec<(Type, String)>>,
}

impl<'a> TypeView<'a> {
    fn new(decls: &'a Decls) -> Self {
        TypeView {
            decls,
            noted: Default::default(),
        }
    }
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
    fn note_case(&self, ty: &Type, bare: &str) {
        self.noted.borrow_mut().push((ty.clone(), bare.to_string()));
    }

    fn cases_of(&self, ty: &Type) -> Option<Vec<(String, Vec<Type>)>> {
        match ty {
            // `V without C` has the cases of `V` less `C`.
            Type::Without(base, aipl_syntax::ast::Exclusion::NamedCase(excluded)) => {
                let mut cases = self.cases_of(base)?;
                cases.retain(|(c, _)| c != excluded);
                Some(cases)
            }
            Type::Without(base, aipl_syntax::ast::Exclusion::EmptyArray) => self.cases_of(base),
            // `V without C throughout` has the cases of `V` less `C`, and each payload is
            // refined the same way.
            Type::Without(base, aipl_syntax::ast::Exclusion::Throughout(excluded)) => {
                let tables = self.decls.tables();
                let mut cases = self.cases_of(base)?;
                cases.retain(|(c, _)| c != excluded);
                let refine = |t: &Type| refine_throughout(&tables, t, excluded);
                Some(
                    cases
                        .into_iter()
                        .map(|(c, payload)| (c, payload.iter().map(&refine).collect()))
                        .collect(),
                )
            }
            Type::Named(n) => self.decls.variants.get(n).cloned(),
            Type::Generic(base, args) => {
                let tmpl = self.decls.generic_variants.get(base)?;
                let map = crate::typing::zip_type_args(&tmpl.type_vars, args);
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
            Type::Generic(base, args) if base.starts_with(crate::typing::TUPLE_TEMPLATE) => {
                Some(args.clone())
            }
            _ => None,
        }
    }
}

/// What a function name written in the program refers to. The shared wrapper
/// the loader makes of a constructor used as a value (`xs.map(Circle)`) is the
/// case the author wrote, not a function they declared.
fn function_target(name: &str) -> Target {
    match name
        .strip_prefix(aipl_syntax::CTOR_WRAPPER_PREFIX)
        .and_then(|rest| rest.split_once('$'))
    {
        Some((variant, case)) => Target::Case {
            variant: variant.to_string(),
            case: case.to_string(),
        },
        None => Target::Function(name.to_string()),
    }
}

/// Whether the declared type `t` has a function type anywhere in it.
fn declared_has_function(t: &Type) -> bool {
    match t {
        Type::Fn(..) => true,
        Type::Case(i)
        | Type::Optional(i)
        | Type::Array(i)
        | Type::Set(i, _)
        | Type::Without(i, _) => declared_has_function(i),
        Type::Dict(a, b) | Type::Result(a, b) => {
            declared_has_function(a) || declared_has_function(b)
        }
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(declared_has_function),
        _ => false,
    }
}
