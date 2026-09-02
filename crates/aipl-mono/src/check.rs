//! Standalone type-checking pass over the *non-monomorphized* source.
//!
//! This validates every function in isolation — every signature and every body,
//! concrete or generic — so that a program's validity can be determined from the
//! source alone, independent of which instances monomorphization later chooses
//! to emit. Running it before codegen means errors in functions that are never
//! reached (and would otherwise be dropped by lazy instantiation) are reported.
//!
//! Concrete bodies are checked directly. A *generic* body is checked abstractly:
//! each type variable becomes `__typevar__`, which coerces only with itself —
//! so structural rules still bite (you can't return a `T[]` where `i64` is
//! declared, or `+` two `T`s, since `T: any` promises neither) while `==`,
//! container operations, binding, and `return T` are allowed.
//!
//! It uses the same coercions codegen does, so it never rejects a valid program.
//! Codegen still re-checks bodies as a backstop — the abstract pass is permissive
//! about operations whose validity depends on the concrete instantiation, and
//! some of those (e.g. `==` over every element type) aren't fully implemented in
//! codegen yet, so we don't trust this pass enough to drop those checks.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast;
use aipl_syntax::ast::Bound;
use aipl_syntax::ast::{
    BinOp, Expr, ExprKind, FieldInit, Function, Item, LambdaParam, MatchArm, Pattern, Primitive,
    Program, Signature, StructDecl, Type, VariantDecl,
};
use aipl_syntax::{
    binop_spelling, is_array_elem, is_dict_key, is_error, is_none_inner, is_set_elem, is_str_repr,
    type_name, Error, Span,
};

/// A lambda body's type with its error side filled in from a `?` the body
/// propagated.
///
/// `ok(v)` names no error, so it types as `v!__none__` — which is right for a
/// value but wrong for a *lambda*, whose failures leave through the `?` in its
/// body rather than through an `err(..)` the literal mentions. Without this,
/// `xs.try_map(|p| ok(T { .. f(p)? .. }))` leaves `try_map`'s `E` unpinned and
/// unpinnable: `E` appears nowhere but the callback's own type, and the only
/// thing naming it is the `?`.
///
/// `tries` is what the `?`s in the body propagated, in order; the first one that
/// names a real error type wins. A body whose error side is already concrete
/// keeps it, so an explicit `err(..)` always outranks this inference — and a
/// body with conflicting `?` error types still fails where it did before, in the
/// enclosing-return check.
pub(crate) fn err_side_from_tries(body_ty: Type, tries: &[Type]) -> Type {
    let Type::Result(ok, e) = &body_ty else {
        return body_ty;
    };
    if !is_none_inner(e) {
        return body_ty;
    }
    match tries.iter().find(|t| !is_none_inner(t)) {
        Some(named) => Type::Result(ok.clone(), Box::new(named.clone())),
        None => body_ty,
    }
}

/// Mangle a type into a fragment usable inside a synthetic struct/variant name
/// (all `$`/`!`/nesting flattened to `_`). Shared by [`tuple_struct_name`] and
/// [`generic_instance_name`] so the two naming schemes agree on how a type is
/// spelled.
pub(crate) fn mangle_type(ty: &Type) -> String {
    match ty {
        Type::Unit => panic!("Synthetic-type members cannot be unit"),
        // Tuple/generic members are parsed straight from source syntax; these
        // are compiler-internal pseudo-types that never appear there.
        Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => {
            panic!("Synthetic-type members cannot be a compiler pseudo-type")
        }
        Type::Primitive(p) => p.name().into(),
        Type::Named(n) => n.replace(['$', '!'], "_"),
        // Spelled exactly as the old `Named("__typevar__$T")` sentinel mangled,
        // so instance names generated before and after this split agree.
        Type::TypeVar(v) => format!("{}{v}", TYPEVAR.replace('$', "_")),
        Type::Array(e) => format!("arr_{}", mangle_type(e)),
        Type::Optional(e) => format!("opt_{}", mangle_type(e)),
        Type::Set(e) => format!("set_{}", mangle_type(e)),
        Type::Dict(k, v) => format!("dict_{}_{}", mangle_type(k), mangle_type(v)),
        Type::Result(ok, err) => format!("res_{}_{}", mangle_type(ok), mangle_type(err)),
        Type::Fn(ps, ret) => {
            let args = ps.iter().map(mangle_type).collect::<Vec<_>>().join("_");
            format!("fn_{}_{}", args, mangle_type(ret))
        }
        Type::Tuple(es) => {
            format!(
                "tuple_{}",
                es.iter().map(mangle_type).collect::<Vec<_>>().join("_")
            )
        }
        Type::Generic(name, args) => {
            format!(
                "{name}_{}",
                args.iter().map(mangle_type).collect::<Vec<_>>().join("_")
            )
        }
    }
}

/// Generate the canonical synthetic-struct name for a tuple with the given
/// element types. Matches the name produced by mono's `lower_tuples`.
pub(crate) fn tuple_struct_name(elems: &[Type]) -> String {
    format!(
        "__tuple${}",
        elems.iter().map(mangle_type).collect::<Vec<_>>().join("$")
    )
}

/// Canonical synthetic name for a monomorphic instance of the generic
/// struct/variant `base` applied to concrete `args` — e.g. `Box<i64>` →
/// `Box$i64`, `Pair<i64, str>` → `Pair$i64$str`. `$` can't appear in a
/// user-written identifier, so these never collide with source names. Shared
/// by `lower_generics` (annotation lowering) and the checker/mono construction
/// inference so both agree on the instance name.
pub(crate) fn generic_instance_name(base: &str, args: &[Type]) -> String {
    format!(
        "{base}${}",
        args.iter().map(mangle_type).collect::<Vec<_>>().join("$")
    )
}

/// Effects the language recognizes. `prints` = writes to stdout; `read_files` =
/// reads from the filesystem; `write_files` = writes to the filesystem;
/// `list_files` = enumerates the filesystem (directory listing);
/// `execute_program` = spawns a child process; `clock` = reads the wall clock
/// (so a function's result depends on when it ran).
const KNOWN_EFFECTS: &[&str] = &[
    "prints",
    "read_files",
    "write_files",
    "list_files",
    "execute_program",
    "clock",
];

/// Whether an expression's value is used by whatever encloses it.
///
/// AIPL requires each `match` to be *either* a statement or an expression, and
/// this is half of how that is decided (the other half is whether its arms
/// produce a value):
///
/// - arms produce a value → an **expression** `match`: its value must be used
///   ([`Pos::Value`]), and its arms may not assign to anything declared outside
///   it, so that reading it is enough to know what it does;
/// - arms produce nothing → a **statement** `match`: it may assign freely, and
///   must sit where its (absent) value is discarded ([`Pos::Discard`]).
///
/// The practical effect is that a `match` run purely for effect is written
/// `match (x) { .. };` — with the semicolon that makes it a statement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    /// The value is consumed: a binding's initializer, a call argument, a
    /// block's trailing expression, an operand.
    Value,
    /// The value is thrown away: a `;`-terminated statement, or a loop body.
    Discard,
}

/// The first assignment in `body` to a name `body` did not itself declare —
/// i.e. a mutation reaching outside the expression. Returns the target's
/// spelling and the `set`'s span.
///
/// `declared` starts as the names bound *inside* the arm (its pattern bindings)
/// and grows as the walk enters `let`/`mut`/`for` scopes, so shadowing is
/// handled: `mut n = 0; set n = 1;` inside an arm is local and fine, while a
/// `set` of an enclosing `mut` is not. Only [`ExprKind::Assign`] mutates — plain
/// `set`, a field store, and the `set recv.m(..)` writeback all parse to it — so
/// this one walk covers every form.
fn outer_assign(body: &Expr, declared: &HashSet<String>) -> Option<(String, Span)> {
    match &body.kind {
        ExprKind::Assign(lhs, val, rest) => {
            if let Some((name, _)) = ast::assign_target(lhs) {
                if !declared.contains(name) {
                    return Some((name.to_string(), body.span.clone()));
                }
            }
            outer_assign(val, declared).or_else(|| outer_assign(rest, declared))
        }
        // A binding is in scope for its body only, never its own initializer.
        ExprKind::Let(name, _, val, rest) | ExprKind::LetMut(name, _, val, rest) => {
            outer_assign(val, declared).or_else(|| {
                let mut inner = declared.clone();
                inner.insert(name.clone());
                outer_assign(rest, &inner)
            })
        }
        ExprKind::For(var, iter, rest) => outer_assign(iter, declared).or_else(|| {
            let mut inner = declared.clone();
            inner.insert(var.clone());
            outer_assign(rest, &inner)
        }),
        ExprKind::Match(scrut, arms) => outer_assign(scrut, declared).or_else(|| {
            arms.iter().find_map(|a| {
                let mut inner = declared.clone();
                inner.extend(a.pattern.bindings());
                outer_assign(&a.body, &inner)
            })
        }),
        // A lambda body is a separate function; its captures are checked there.
        ExprKind::Lambda(_, _) => None,
        _ => crate::children(body)
            .into_iter()
            .find_map(|c| outer_assign(c, declared)),
    }
}

/// A bound name's type and whether it's reassignable (`let mut` / `mut self`).
#[derive(Clone)]
struct Binding {
    ty: Type,
    mutable: bool,
}

type Env = HashMap<String, Binding>;

struct Cx<'a> {
    structs: &'a HashMap<String, Vec<(String, Type, bool)>>,
    /// Synthetic struct layouts created on-the-fly when a `TupleLit` or a
    /// generic-struct construction (`Box { value: 5 }`) is seen during checking.
    /// Looked up alongside `structs` by `struct_fields`.
    syn_structs: std::cell::RefCell<HashMap<String, Vec<(String, Type, bool)>>>,
    /// Variant (sum) types: name → ordered cases `(ctor, payload types)`.
    variants: &'a HashMap<String, Vec<(String, Vec<Type>)>>,
    /// Synthetic variant layouts created on-the-fly for a generic-variant
    /// instance (`Opt$i64`). Looked up alongside `variants`.
    syn_variants: std::cell::RefCell<HashMap<String, Vec<(String, Vec<Type>)>>>,
    /// Constructor name → the variant it belongs to (for typing `Ctor(..)`).
    ctors: &'a HashMap<String, String>,
    /// Generic struct templates by name (`Box` → its `StructDecl`), used to
    /// infer and instantiate a construction `Box { value: 5 }`.
    generic_structs: &'a HashMap<String, StructDecl>,
    /// Generic variant templates by name (`Opt` → its `VariantDecl`).
    generic_variants: &'a HashMap<String, VariantDecl>,
    /// Constructor name → the *generic* variant template it belongs to. A
    /// construction of one resolves to a specific instance by inferring the
    /// template's type arguments from the constructor's payload (or the expected
    /// type). Its constructors are shared by name across every instance, so they
    /// aren't in the unique `ctors` map.
    generic_ctors: &'a HashMap<String, String>,
    sigs: &'a HashMap<String, Signature>,
    /// The declared return type of the function currently being checked (with
    /// type-vars substituted), so a `return value;` can be checked against it.
    /// Functions are top-level (never nested), so a single slot suffices.
    current_ret: std::cell::RefCell<Type>,
    /// The name and type parameters of the function currently being checked, so
    /// a `let x: T = ..` annotation deep in its body can be validated with
    /// [`Cx::check_ty`] (which needs both) and blamed on the right function.
    /// Single slots for the same reason as `current_ret`: functions never nest.
    current_fn: std::cell::RefCell<String>,
    current_type_params: std::cell::RefCell<Vec<String>>,
    /// The type an expression is being checked *against*, when something more
    /// local than the function's return type says so — currently an annotated
    /// binding. Consulted before `current_ret` when a generic construction's
    /// own arguments don't pin every type variable, so
    /// `let r: Rule<Tok> = Spelling("(")` resolves from the annotation.
    /// Separate from `current_ret` because `return` inside the value still
    /// means the *function's* return type.
    current_expected: std::cell::RefCell<Option<Type>>,
    /// Types resolved for *context-dependent* expressions, keyed by source span
    /// and stamped onto the program `check` returns (see `Expr::ty`).
    ///
    /// A bare `none` or empty `[]` types as a placeholder here and takes its
    /// real type from wherever it sits; a generic constructor its arguments
    /// don't pin takes its instance from the expected type. Both stop being
    /// derivable the moment a later pass *moves* them, so the answer is recorded
    /// while the context is still in view.
    ///
    /// Keyed by the *address* of the expression node in the program being
    /// checked, which `stamp_locks` then walks in lockstep with its clone.
    ///
    /// Not by span: `check` runs over the synthesized builtin declarations
    /// followed by the real program, and a span is a byte offset **within its
    /// own file**. Offset 432 exists in both, so a span key silently applied one
    /// file's answer to the other's expression — an empty `[]` in a builtin decl
    /// stamping its `str[]` onto a user's `#{}` at the same offset. Node
    /// identity has no such overlap.
    ///
    /// `None` as a value means "seen with conflicting types", so nothing can be
    /// locked for it — a wrong lock is far worse than a missing one.
    locks: std::cell::RefCell<HashMap<usize, Option<Type>>>,
    /// The bound declared for each of `current_type_params`, so a call inside a
    /// generic body can check a callee's bound against the *enclosing*
    /// variable's (see `bound_satisfied`).
    current_type_bounds: std::cell::RefCell<HashMap<String, Bound>>,
    /// Instance names [`Cx::instance_args`] is part-way through recovering the
    /// type arguments of. A *recursive* generic (`Rule<K> = Term(K) |
    /// Then(Rule<K>, Rule<K>)`) has a payload naming its own instance, so
    /// unifying that payload asks for the very arguments being recovered —
    /// without this the two would call each other until the stack ran out.
    resolving: std::cell::RefCell<HashSet<String>>,
    /// Error types propagated by a `?` while checking the expression currently
    /// in progress — see [`err_side_from_tries`], which reads the entries a
    /// lambda body pushed.
    try_errs: std::cell::RefCell<Vec<Type>>,
}

impl<'a> Cx<'a> {
    fn struct_fields(&self, name: &str) -> Option<Vec<(String, Type, bool)>> {
        self.structs
            .get(name)
            .cloned()
            .or_else(|| self.syn_structs.borrow().get(name).cloned())
    }
    /// If struct `sn` has a field named `field` holding a function value,
    /// return its `(param types, return type)`. Used to call through a
    /// function-valued field with method syntax (`recv.f(args)`).
    fn struct_fn_field(&self, sn: &str, field: &str) -> Option<(Vec<Type>, Type)> {
        self.struct_fields(sn)?
            .into_iter()
            .find_map(|(n, t, _)| match t {
                Type::Fn(ptys, ret) if n == field => Some((ptys, *ret)),
                _ => None,
            })
    }
    fn has_struct(&self, name: &str) -> bool {
        self.structs.contains_key(name) || self.syn_structs.borrow().contains_key(name)
    }
    fn add_syn_struct(&self, name: String, fields: Vec<(String, Type, bool)>) {
        self.syn_structs.borrow_mut().insert(name, fields);
    }
    /// Does `ty` satisfy `need`?
    ///
    /// Concretely that is [`Bound::accepts`]. But inside a generic body every
    /// type variable is an abstract sentinel, not a concrete type, so `accepts`
    /// can never say yes — which used to make *any* bounded builtin uncallable
    /// from generic code (`xs.minimum()` inside `fn f<T: ord>`). When the
    /// inferred type is one of the enclosing function's own variables, the
    /// question is instead whether that variable's declared bound implies the
    /// one required here.
    ///
    /// Implication is deliberately just equality, plus `any` (which promises
    /// nothing, so everything satisfies it): `ord` and `variant` are unrelated,
    /// and inventing a lattice before something needs one would be guesswork.
    fn bound_satisfied(&self, need: Bound, ty: &Type) -> bool {
        if let Some(var) = typevar_name(ty) {
            let have = self.current_type_bounds.borrow().get(var).copied();
            return need == Bound::Any || have == Some(need);
        }
        need.accepts(ty, &|n: &str| self.has_variant(n))
    }

    /// Record `ty` as the resolved type of `e`, if `e` is one of the
    /// context-dependent shapes worth locking and `ty` is concrete enough to be
    /// worth recording. Idempotent; a conflicting second answer poisons the
    /// entry rather than picking one.
    fn lock(&self, e: &Expr, ty: &Type) {
        if needs_lock(&e.kind) {
            self.lock_node(node_id(e), ty);
        }
    }

    /// [`Cx::lock`] by node id, for a resolution reached without the `Expr` in
    /// hand — a generic constructor, whose whole call is what gets an instance.
    fn lock_node(&self, key: usize, ty: &Type) {
        if mentions_placeholder(ty) || mentions_typevar(ty) {
            return;
        }
        let mut locks = self.locks.borrow_mut();
        match locks.get(&key) {
            None => {
                locks.insert(key, Some(ty.clone()));
            }
            Some(Some(prev)) if prev != ty => {
                locks.insert(key, None);
            }
            _ => {}
        }
    }

    fn has_variant(&self, name: &str) -> bool {
        self.variants.contains_key(name) || self.syn_variants.borrow().contains_key(name)
    }

    /// The fields of a struct-typed value: a concrete named struct, or an
    /// (abstract) generic-struct application `Box<T>` — for the latter the
    /// template's fields are returned with its type variables substituted by the
    /// application's arguments (which may still be abstract inside a generic
    /// function). This is what lets `b.value` type-check where `b: Box<T>`.
    fn fields_of(&self, ty: &Type) -> Option<Vec<(String, Type, bool)>> {
        match ty {
            Type::Named(sn) => self.struct_fields(sn),
            Type::Generic(base, args) => {
                let tmpl = self.generic_structs.get(base)?;
                let map = zip_type_args(&tmpl.type_vars, args);
                Some(
                    tmpl.fields
                        .iter()
                        .map(|fd| {
                            (
                                fd.name.clone(),
                                crate::subst_type_params(&fd.ty, &map),
                                fd.default.is_some(),
                            )
                        })
                        .collect(),
                )
            }
            _ => None,
        }
    }

    /// The cases of a variant-typed value: a concrete named variant, or an
    /// (abstract) generic-variant application `Emit<K>` (template cases with its
    /// type variables substituted). Lets a `match` on such a value resolve.
    fn cases_of(&self, ty: &Type) -> Option<Vec<(String, Vec<Type>)>> {
        match ty {
            Type::Named(n) => self.variant_cases(n),
            Type::Generic(base, args) => {
                let tmpl = self.generic_variants.get(base)?;
                let map = zip_type_args(&tmpl.type_vars, args);
                Some(
                    tmpl.cases
                        .iter()
                        .map(|c| {
                            (
                                c.name.clone(),
                                c.payload
                                    .iter()
                                    .map(|p| crate::subst_type_params(p, &map))
                                    .collect(),
                            )
                        })
                        .collect(),
                )
            }
            _ => None,
        }
    }
    /// Cases of variant `name` (a source variant or a synthesized generic
    /// instance).
    fn variant_cases(&self, name: &str) -> Option<Vec<(String, Vec<Type>)>> {
        self.variants
            .get(name)
            .cloned()
            .or_else(|| self.syn_variants.borrow().get(name).cloned())
    }

    /// Resolve every `Type::Generic` in `t` to a synthetic monomorphic `Named`
    /// type, registering the instance layout (recursively) into the syn maps.
    /// Concrete annotations are already resolved by `lower_generics`; this is the
    /// on-demand path for instances that only arise from construction inference.
    fn resolve_generic_ty(&self, t: &Type) -> Result<Type, Error> {
        Ok(match t {
            Type::Generic(base, args) => {
                let args: Vec<Type> = args
                    .iter()
                    .map(|a| self.resolve_generic_ty(a))
                    .collect::<Result<_, _>>()?;
                // An application with an abstract argument (`Rule<K>` in a
                // generic body) names no instance yet — leave it applied.
                if args.iter().any(mentions_typevar) {
                    Type::Generic(base.clone(), args)
                } else {
                    Type::Named(self.instantiate_generic(base, &args)?)
                }
            }
            Type::Optional(i) => Type::Optional(Box::new(self.resolve_generic_ty(i)?)),
            Type::Array(i) => Type::Array(Box::new(self.resolve_generic_ty(i)?)),
            Type::Set(i) => Type::Set(Box::new(self.resolve_generic_ty(i)?)),
            Type::Dict(k, v) => Type::Dict(
                Box::new(self.resolve_generic_ty(k)?),
                Box::new(self.resolve_generic_ty(v)?),
            ),
            Type::Result(ok, err) => Type::Result(
                Box::new(self.resolve_generic_ty(ok)?),
                Box::new(self.resolve_generic_ty(err)?),
            ),
            _ => t.clone(),
        })
    }

    /// Bind the type variables in `vars` by unifying a construction's declared
    /// field type against the provided value's type — like
    /// [`collect_var_bindings`], but also matching a generic-application field
    /// type (`Emit<K>`) against an already-synthesized instance value
    /// (`Emit$Tok`) by recovering the instance's type arguments. This is what
    /// lets `TokenRule { emit: OfInt(..) }` infer `K` from a nested generic field.
    fn bind_field(
        &self,
        field_ty: &Type,
        value_ty: &Type,
        vars: &HashSet<&str>,
        map: &mut HashMap<String, Type>,
    ) {
        match (field_ty, value_ty) {
            (Type::Generic(base, params), Type::Named(inst)) => {
                if let Some((b, args)) = self.instance_args(inst) {
                    if b == *base && args.len() == params.len() {
                        for (p, a) in params.iter().zip(&args) {
                            self.bind_field(p, a, vars, map);
                        }
                    }
                }
            }
            (Type::Generic(b1, ps), Type::Generic(b2, as_))
                if b1 == b2 && ps.len() == as_.len() =>
            {
                for (p, a) in ps.iter().zip(as_) {
                    self.bind_field(p, a, vars, map);
                }
            }
            (Type::Array(p), Type::Array(a)) if !is_none_inner(a) => {
                self.bind_field(p, a, vars, map)
            }
            (Type::Optional(p), Type::Optional(a)) if !is_none_inner(a) => {
                self.bind_field(p, a, vars, map)
            }
            (Type::Set(p), Type::Set(a)) if !is_none_inner(a) => self.bind_field(p, a, vars, map),
            (Type::Dict(pk, pv), Type::Dict(ak, av)) => {
                self.bind_field(pk, ak, vars, map);
                self.bind_field(pv, av, vars, map);
            }
            (Type::Result(po, pe), Type::Result(ao, ae)) => {
                self.bind_field(po, ao, vars, map);
                self.bind_field(pe, ae, vars, map);
            }
            (Type::Fn(ps, pr), Type::Fn(as_, ar)) => {
                for (p, a) in ps.iter().zip(as_) {
                    self.bind_field(p, a, vars, map);
                }
                self.bind_field(pr, ar, vars, map);
            }
            // Leaf (a bare type variable, `char[]`↔`str`, etc.).
            _ => collect_var_bindings(field_ty, value_ty, vars, map),
        }
    }

    /// Recover the concrete type arguments of a synthesized generic instance
    /// (`Emit$Tok` → `("Emit", [Tok])`) by unifying its generic template's
    /// structure against the instance's concrete decl. `None` if `inst` isn't a
    /// generic instance or some type variable can't be pinned.
    fn instance_args(&self, inst: &str) -> Option<(String, Vec<Type>)> {
        // Re-entry for the same instance means a recursive generic reached its
        // own payload; that occurrence pins nothing this call doesn't already
        // know, so bow out and let the type's other cases do the pinning.
        if !self.resolving.borrow_mut().insert(inst.to_string()) {
            return None;
        }
        let args = self.instance_args_inner(inst);
        self.resolving.borrow_mut().remove(inst);
        args
    }

    /// [`Cx::instance_args`] proper. Call that, never this: the guard it holds
    /// is what keeps the mutual recursion below finite.
    fn instance_args_inner(&self, inst: &str) -> Option<(String, Vec<Type>)> {
        for (base, tmpl) in self.generic_structs {
            if inst.starts_with(&format!("{base}$")) {
                if let Some(inst_fields) = self.struct_fields(inst) {
                    let vars: HashSet<&str> =
                        tmpl.type_vars.iter().map(|t| t.name.as_str()).collect();
                    let mut map = HashMap::new();
                    for fd in &tmpl.fields {
                        if let Some((_, ity, _)) =
                            inst_fields.iter().find(|(n, _, _)| *n == fd.name)
                        {
                            self.bind_field(&fd.ty, ity, &vars, &mut map);
                        }
                    }
                    if let Some(args) = collect_args(&tmpl.type_vars, &map) {
                        return Some((base.clone(), args));
                    }
                }
            }
        }
        for (base, tmpl) in self.generic_variants {
            if inst.starts_with(&format!("{base}$")) {
                if let Some(inst_cases) = self.variant_cases(inst) {
                    let vars: HashSet<&str> =
                        tmpl.type_vars.iter().map(|t| t.name.as_str()).collect();
                    let mut map = HashMap::new();
                    for c in &tmpl.cases {
                        if let Some((_, ipayload)) = inst_cases.iter().find(|(n, _)| *n == c.name) {
                            for (pt, it) in c.payload.iter().zip(ipayload) {
                                self.bind_field(pt, it, &vars, &mut map);
                            }
                        }
                    }
                    if let Some(args) = collect_args(&tmpl.type_vars, &map) {
                        return Some((base.clone(), args));
                    }
                }
            }
        }
        None
    }

    /// The single existing instance of generic-variant template `base`, if
    /// exactly one has been synthesized (`Opt` → `Opt$i64` when only `Opt<i64>`
    /// is used). `None` if there are zero or several — the latter being an
    /// ambiguity a nullary construction can't resolve.
    fn sole_instance(&self, base: &str) -> Option<String> {
        let prefix = format!("{base}$");
        let mut names: Vec<String> = self
            .variants
            .keys()
            .filter(|n| n.starts_with(&prefix))
            .cloned()
            .collect();
        names.extend(
            self.syn_variants
                .borrow()
                .keys()
                .filter(|n| n.starts_with(&prefix))
                .cloned(),
        );
        names.sort();
        names.dedup();
        if names.len() == 1 {
            names.pop()
        } else {
            None
        }
    }

    /// The type arguments a construction of generic `base` should take from the
    /// *expected* (enclosing function's return) type, when the provided fields
    /// don't pin every variable — e.g. `StepResult { tokens: [], .. }` in a
    /// `-> StepResult<K>!LexError` function. Searches the return type for an
    /// application of `base` (a `Generic` in a generic function, or a synthesized
    /// `Named` instance once concrete).
    fn ret_generic_args(&self, base: &str) -> Option<Vec<Type>> {
        if let Some(exp) = self.current_expected.borrow().clone() {
            if let Some(args) = self.find_generic_args(&exp, base) {
                return Some(args);
            }
        }
        let ret = self.current_ret.borrow().clone();
        self.find_generic_args(&ret, base)
    }

    fn find_generic_args(&self, ty: &Type, base: &str) -> Option<Vec<Type>> {
        match ty {
            Type::Generic(b, args) if b == base => Some(args.clone()),
            Type::Named(n) => self
                .instance_args(n)
                .filter(|(b, _)| b == base)
                .map(|(_, a)| a),
            Type::Optional(i) | Type::Array(i) | Type::Set(i) => self.find_generic_args(i, base),
            Type::Dict(k, v) => self
                .find_generic_args(k, base)
                .or_else(|| self.find_generic_args(v, base)),
            Type::Result(a, b) => self
                .find_generic_args(a, base)
                .or_else(|| self.find_generic_args(b, base)),
            _ => None,
        }
    }

    /// Register (if new) the monomorphic instance of generic `base` applied to
    /// concrete `args`, returning its synthetic name.
    fn instantiate_generic(&self, base: &str, args: &[Type]) -> Result<String, Error> {
        let name = generic_instance_name(base, args);
        if self.has_struct(&name) || self.has_variant(&name) {
            return Ok(name);
        }
        if let Some(tmpl) = self.generic_structs.get(base) {
            let map = crate::bind_type_args(base, &tmpl.type_vars, args)?;
            // Register a placeholder before recursing so a (mutually) recursive
            // generic refers to the in-progress name rather than looping.
            self.add_syn_struct(name.clone(), Vec::new());
            let mut fields = Vec::with_capacity(tmpl.fields.len());
            for fd in &tmpl.fields {
                let ty = self.resolve_generic_ty(&crate::subst_type_params(&fd.ty, &map))?;
                fields.push((fd.name.clone(), ty, fd.default.is_some()));
            }
            self.add_syn_struct(name.clone(), fields);
        } else if let Some(tmpl) = self.generic_variants.get(base) {
            let map = crate::bind_type_args(base, &tmpl.type_vars, args)?;
            self.syn_variants
                .borrow_mut()
                .insert(name.clone(), Vec::new());
            let mut cases = Vec::with_capacity(tmpl.cases.len());
            for c in &tmpl.cases {
                let payload = c
                    .payload
                    .iter()
                    .map(|p| self.resolve_generic_ty(&crate::subst_type_params(p, &map)))
                    .collect::<Result<_, _>>()?;
                cases.push((c.name.clone(), payload));
            }
            self.syn_variants.borrow_mut().insert(name.clone(), cases);
        } else {
            return Err(Error::msg(format!(
                "unknown generic type {base:?} (no such generic struct or variant)"
            )));
        }
        Ok(name)
    }

    /// Infer and check a generic-struct construction `Box { value: 5 }`: bind the
    /// template's type variables from the provided field values, then treat it as
    /// an ordinary construction of the synthesized instance. Returns the instance
    /// type.
    fn infer_generic_construct(
        &self,
        name: &str,
        inits: &[FieldInit],
        env: &Env,
        effects: &[String],
        span: Span,
    ) -> Result<Type, Error> {
        let tmpl = self.generic_structs.get(name).expect("caller checked");
        let vars: HashSet<&str> = tmpl.type_vars.iter().map(|t| t.name.as_str()).collect();
        let mut map: HashMap<String, Type> = HashMap::new();
        // Type each provided init (against a real field), collecting type-var
        // bindings from the value types.
        let mut provided: HashMap<String, (Type, Span)> = HashMap::new();
        for fi in inits {
            let Some(fd) = tmpl.fields.iter().find(|f| f.name == fi.name) else {
                return Err(Error::at(
                    format!("struct {name:?} has no field {:?}", fi.name),
                    fi.value.span.clone(),
                ));
            };
            let vt = self.check_expr(&fi.value, env, effects)?;
            self.bind_field(&fd.ty, &vt, &vars, &mut map);
            provided.insert(fi.name.clone(), (vt, fi.value.span.clone()));
        }
        // Every field without a default must be provided.
        for fd in &tmpl.fields {
            if fd.default.is_none() && !provided.contains_key(&fd.name) {
                return Err(Error::at(
                    format!(
                        "struct {name:?} field {:?} has no default and was not provided",
                        fd.name
                    ),
                    span.clone(),
                ));
            }
        }
        // Every type variable must be pinned by a provided field, or — when the
        // fields don't determine it (an empty `StepResult { tokens: [], .. }`) —
        // by the enclosing function's expected return type.
        let expected = self.ret_generic_args(name);
        let args: Vec<Type> = tmpl
            .type_vars
            .iter()
            .enumerate()
            .map(|(i, tv)| {
                map.get(&tv.name)
                    .cloned()
                    .or_else(|| expected.as_ref().and_then(|a| a.get(i).cloned()))
                    .ok_or_else(|| {
                        Error::at(
                            format!(
                                "cannot infer type parameter {:?} of generic struct {name:?} \
                                 — provide a field whose value determines it",
                                tv.name
                            ),
                            span.clone(),
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        // Inside a generic function the arguments may be abstract (`Box { value:
        // x }` where `x: T`); keep the construction generic — monomorphization
        // pins it once `T` is concrete. `bind_field` already checked consistency.
        if args.iter().any(mentions_typevar) {
            return Ok(Type::Generic(name.to_string(), args));
        }
        let inst = self.instantiate_generic(name, &args)?;
        // Check each provided value against its concrete (substituted) field type.
        let concrete = self.struct_fields(&inst).expect("just instantiated");
        for fi in inits {
            let (_, expected, _) = concrete
                .iter()
                .find(|(n, _, _)| *n == fi.name)
                .expect("field validated above");
            let (vt, vspan) = &provided[&fi.name];
            expect(
                vt,
                expected,
                &format!("struct {name:?} field {:?}", fi.name),
                vspan.clone(),
            )?;
        }
        Ok(Type::Named(inst))
    }

    /// Infer and check a generic-variant construction `Some(5)` / `Nothing`:
    /// bind the template's type variables from the constructor's argument types
    /// (or, when they don't pin every variable, from the expected return type),
    /// instantiate, and check the arguments against the concrete payload. Returns
    /// the instance type.
    fn infer_generic_variant_ctor(
        &self,
        base: &str,
        ctor: &str,
        args: &[Expr],
        env: &Env,
        effects: &[String],
        span: Span,
        node: usize,
    ) -> Result<Type, Error> {
        // `ctor` is the variant-qualified `Case@Template`; cases are named bare.
        let bare = ctor.split('@').next().unwrap_or(ctor);
        let tmpl = self.generic_variants.get(base).expect("caller checked");
        let case = tmpl
            .cases
            .iter()
            .find(|c| c.name == bare)
            .expect("ctor belongs to this template");
        if args.len() != case.payload.len() {
            return Err(Error::at(
                format!(
                    "constructor {bare:?} expects {} argument(s), got {}",
                    case.payload.len(),
                    args.len()
                ),
                span.clone(),
            ));
        }
        let vars: HashSet<&str> = tmpl.type_vars.iter().map(|t| t.name.as_str()).collect();
        let mut map: HashMap<String, Type> = HashMap::new();
        // The argument expression is kept alongside its type: the payload checks
        // below flex a bare literal to the *substituted* payload type, and
        // `flex_int` needs the expression, not just what it inferred to.
        let mut arg_tys: Vec<(&Expr, Type)> = Vec::with_capacity(args.len());
        // A bare integer literal payload is deferred to the fallback chain below
        // rather than pinning here — see [`literal_pins_nothing`].
        let mut deferred: Vec<(&Type, Type)> = Vec::new();
        for (arg, pty) in args.iter().zip(&case.payload) {
            let at = self.check_expr(arg, env, effects)?;
            if literal_pins_nothing(arg, pty, &vars) {
                deferred.push((pty, at.clone()));
            } else {
                self.bind_field(pty, &at, &vars, &mut map);
            }
            arg_tys.push((arg, at));
        }
        // Resolve the type arguments from the constructor's payload. A variable
        // no argument pins — a nullary case, or a case whose payload mentions no
        // type variable (`Spelling(str)` in a `Rule<K>`) — falls back, in order:
        //
        // 1. the enclosing function's expected return type, exactly as the
        //    generic *struct* path does (`ret_generic_args`). This is direct
        //    evidence, and `find_generic_args` looks through arrays and the other
        //    containers, so a nested `Then([Spelling("("), ..])` types from the
        //    outside in — every constructor in the body sees the same return type.
        // 2. the template's sole existing instance, when there is exactly one.
        //    That is a guess which merely happens to be unambiguous, so it is
        //    tried second — and it is why adding a *second* instance used to
        //    break the first one that relied on it.
        let unpinned = map.len() < tmpl.type_vars.len();
        let expected: Option<Vec<Type>> = if unpinned {
            self.ret_generic_args(base)
        } else {
            None
        };
        let sole: Option<Vec<Type>> = if unpinned && expected.is_none() {
            self.sole_instance(base)
                .and_then(|inst| self.instance_args(&inst).map(|(_, a)| a))
        } else {
            None
        };
        // 3. a deferred integer literal, last of all. `i64` is only the width a
        //    bare literal *defaults* to, so it is weaker evidence than either of
        //    the above — but it still settles a variable nothing else reached,
        //    which is what keeps a bare `Full(200)` working with no annotation
        //    and no existing instance in sight.
        let mut lit: HashMap<String, Type> = HashMap::new();
        for (pty, at) in deferred {
            self.bind_field(pty, &at, &vars, &mut lit);
        }
        let type_args: Vec<Type> = tmpl
            .type_vars
            .iter()
            .enumerate()
            .map(|(i, tv)| {
                map.get(&tv.name)
                    .cloned()
                    .or_else(|| expected.as_ref().and_then(|a| a.get(i).cloned()))
                    .or_else(|| sole.as_ref().and_then(|a| a.get(i).cloned()))
                    .or_else(|| lit.get(&tv.name).cloned())
                    .ok_or_else(|| {
                        Error::at(
                            format!(
                                "cannot infer type parameter {:?} of generic variant {base:?} \
                                 — a constructor argument or a single existing instance must \
                                 determine it",
                                tv.name
                            ),
                            span.clone(),
                        )
                    })
            })
            .collect::<Result<_, _>>()?;
        // Which instance this constructor makes came from the expected type as
        // often as from its own arguments; record it so the answer survives a
        // pass that moves the expression.
        //
        // Recorded as the *application* `Rule<Tok>` rather than the instance
        // `Rule$Tok`: the instance name only decomposes back into base + args
        // through a registry that is populated later, so an instance here is
        // opaque exactly where it needs to be read.
        self.lock_node(node, &Type::Generic(base.to_string(), type_args.clone()));
        // Inside a generic function the arguments may still be abstract —
        // `Many(r, 0)` where `r: Rule<K>` — and an abstract application has no
        // instance to name: monomorphization pins it once `K` is. Check the
        // arguments against the *template's* payload with the type arguments
        // substituted in, which stays abstract on both sides, and hand back the
        // application. Without this the construction resolved to a synthetic
        // `Rule$__typevar___K` that nothing else in the body agreed with. The
        // generic-*struct* path (`infer_generic_construct`) has always done this;
        // no generic variant was ever *constructed* in a generic body before the
        // parser library, only matched, which is why the two drifted apart.
        if type_args.iter().any(mentions_typevar) {
            let subst: HashMap<String, Type> = tmpl
                .type_vars
                .iter()
                .map(|tv| tv.name.clone())
                .zip(type_args.iter().cloned())
                .collect();
            for ((arg, at), pty) in arg_tys.iter().zip(&case.payload) {
                let target = crate::subst_type_params(pty, &subst);
                let at = self.flex_int(arg, at, &target)?;
                expect(
                    &at,
                    &target,
                    &format!("constructor {bare:?} argument"),
                    arg.span.clone(),
                )?;
            }
            return Ok(Type::Generic(base.to_string(), type_args));
        }
        let inst = self.instantiate_generic(base, &type_args)?;
        // Check each argument against the concrete (substituted) payload type.
        let cases = self.variant_cases(&inst).expect("just instantiated");
        let payload = cases
            .iter()
            .find(|(n, _)| n == bare)
            .map(|(_, p)| p.clone())
            .expect("ctor present in instance");
        for ((arg, at), pty) in arg_tys.iter().zip(&payload) {
            // A bare literal payload flexes to the width the instance settled
            // on, exactly as it does for a *concrete* variant's constructor
            // (see the `ctors` branch of `check_call`). Without this the
            // deferral above only got as far as resolving `Box<u8>` and then
            // rejected its own `200` for being an `i64`.
            let at = self.flex_int(arg, at, pty)?;
            expect(
                &at,
                pty,
                &format!("constructor {bare:?} argument"),
                arg.span.clone(),
            )?;
        }
        Ok(Type::Named(inst))
    }
}

/// Type-check `program`. The checker recovers at each item, so this returns
/// *every* error found — in source order — or `Ok` if every function is
/// well-formed.
/// Type-check `program`, returning it with the types of context-dependent
/// expressions stamped in (see [`Expr::ty`]).
///
/// The rewrite is why this returns a program rather than only errors: a bare
/// `none`, an empty `[]`, or a generic constructor takes its type from where it
/// sits, so a later pass that *moves* it — inlining, folding — would otherwise
/// change what it means. Recording the answer here, while the context is still
/// visible, is what makes those moves safe.
pub fn check(program: &Program) -> Result<Program, Vec<Error>> {
    // struct name → [(field_name, field_type, has_default)]
    let mut structs: HashMap<String, Vec<(String, Type, bool)>> = HashMap::new();
    let mut variants: HashMap<String, Vec<(String, Vec<Type>)>> = HashMap::new();
    let mut ctors: HashMap<String, String> = HashMap::new();
    // Generic templates are kept aside — they have no concrete layout; each use
    // is instantiated by inference (constructions) or lower_generics (annotations).
    let mut generic_structs: HashMap<String, StructDecl> = HashMap::new();
    let mut generic_variants: HashMap<String, VariantDecl> = HashMap::new();
    let mut sigs: HashMap<String, Signature> = HashMap::new();
    // Pass 1: collect declarations (templates and concrete decls). Constructor
    // registration is deferred to pass 2, since a *generic-variant instance*
    // (synthesized by lower_generics, so it appears before its template in the
    // item list) shares its constructors with the template and every sibling
    // instance — those are resolved by type, not through the unique `ctors` map.
    for item in &program.items {
        match item {
            Item::Struct(s) if s.is_generic() => {
                generic_structs.insert(s.name.clone(), s.clone());
            }
            Item::Struct(s) => {
                structs.insert(
                    s.name.clone(),
                    s.fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone(), f.default.is_some()))
                        .collect(),
                );
            }
            Item::Variant(v) if v.is_generic() => {
                generic_variants.insert(v.name.clone(), v.clone());
            }
            Item::Variant(v) => {
                variants.insert(
                    v.name.clone(),
                    v.cases
                        .iter()
                        .map(|c| (c.name.clone(), c.payload.clone()))
                        .collect(),
                );
            }
            Item::Fn(f) => {
                sigs.insert(f.name.clone(), f.sig.clone());
            }
            Item::Import(_) => {}
        }
    }
    // A generic-variant template's constructors: `ctor` → template base name. A
    // construction of one of these resolves to a specific instance by inference.
    let mut generic_ctors: HashMap<String, String> = HashMap::new();
    for (base, tmpl) in &generic_variants {
        for c in &tmpl.cases {
            // Keyed by the loader's variant-qualified name `Case@Template`, like
            // non-generic constructors — a generic constructor is addressable only
            // when brought into scope.
            generic_ctors.insert(format!("{}@{}", c.name, base), base.clone());
        }
    }
    // Pass 2: register the constructors of concrete, *non-instance* variants in
    // the unique `ctors` map. A generic-variant instance's constructors are
    // skipped (shared by name; resolved via `generic_ctors` + inference).
    for (vn, cases) in &variants {
        if is_variant_instance(vn, &generic_variants) {
            continue;
        }
        for (c, _) in cases {
            // Constructors are addressed only by their variant-qualified name:
            // the loader rewrites every in-scope reference to `Case@Variant`, so
            // bare case names may repeat across variants without colliding, and a
            // bare (unqualified) reference no longer resolves — it means the
            // constructor wasn't brought into scope.
            ctors.insert(format!("{c}@{vn}"), vn.clone());
        }
    }

    let cx = Cx {
        structs: &structs,
        syn_structs: std::cell::RefCell::new(HashMap::new()),
        variants: &variants,
        syn_variants: std::cell::RefCell::new(HashMap::new()),
        ctors: &ctors,
        generic_structs: &generic_structs,
        generic_variants: &generic_variants,
        generic_ctors: &generic_ctors,
        sigs: &sigs,
        current_ret: std::cell::RefCell::new(Type::Unit),
        current_fn: std::cell::RefCell::new(String::new()),
        current_type_params: std::cell::RefCell::new(Vec::new()),
        current_expected: std::cell::RefCell::new(None),
        locks: std::cell::RefCell::new(HashMap::new()),
        current_type_bounds: std::cell::RefCell::new(std::collections::HashMap::new()),
        resolving: std::cell::RefCell::new(HashSet::new()),
        try_errs: std::cell::RefCell::new(Vec::new()),
    };
    // Type-check struct field defaults in an empty environment (defaults are
    // evaluated at construction time with no local variables in scope).
    // Errors are collected per *item*, not thrown at the first one: each
    // function (and each struct-field default) is checked independently, so one
    // run reports every item that fails instead of only the earliest. Recovery
    // is at the item boundary — a function stops at its own first error, since
    // continuing inside one without a type for the offending expression would
    // produce cascades rather than findings.
    let mut errors: Vec<Error> = Vec::new();
    for item in &program.items {
        if let Item::Struct(s) = item {
            // A generic template's field types mention its type variables, so
            // its defaults can't be checked concretely; each instantiation's
            // (substituted) defaults ride along with the concrete field types.
            if s.is_generic() {
                continue;
            }
            for f in &s.fields {
                // The field's *type* is a written type like any other, so it
                // gets the same validation a parameter's does — including the
                // "that's a builtin, import it" hint for `Span`. Without this a
                // missing import surfaced only later, as a mismatch between two
                // types that render identically.
                if let Err(e) = cx.check_ty(
                    &f.ty,
                    &[],
                    &format!("struct {:?} field {:?}", s.name, f.name),
                ) {
                    errors.push(e);
                }
                if let Some(default) = &f.default {
                    let checked = cx.check_expr(default, &HashMap::new(), &[]).and_then(|dt| {
                        expect(
                            &dt,
                            &f.ty,
                            &format!("default for struct {:?} field {:?}", s.name, f.name),
                            default.span.clone(),
                        )
                    });
                    if let Err(e) = checked {
                        errors.push(e);
                    }
                }
            }
        }
    }
    for item in &program.items {
        if let Item::Variant(v) = item {
            // Same reasoning as a struct field. Until now an unimported `Span`
            // in a payload reached codegen and was reported as "payload type
            // Span is not supported", which points at the wrong problem.
            if v.is_generic() {
                continue;
            }
            for case in &v.cases {
                for ty in &case.payload {
                    if let Err(e) = cx.check_ty(
                        ty,
                        &[],
                        &format!("variant {:?} case {:?} payload", v.name, case.name),
                    ) {
                        errors.push(e);
                    }
                }
            }
        }
    }
    for item in &program.items {
        if let Item::Fn(f) = item {
            if let Err(e) = cx.check_fn(f) {
                errors.push(e);
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    // Stamp what checking learned onto a copy. A span that resolved two ways is
    // recorded as `None` and stamps nothing — see `Cx::locks`.
    let locks = cx.locks.borrow();
    let mut out = program.clone();
    // Walked in lockstep with the input: the clone is structurally identical, so
    // the two trees line up node for node, and each output node is stamped from
    // the *input* node's identity.
    for (src, item) in program.items.iter().zip(&mut out.items) {
        if let (Item::Fn(sf), Item::Fn(f)) = (src, item) {
            stamp_locks(&sf.body, &mut f.body, &locks);
            if let (Some(st), Some(t)) = (sf.test_body.as_ref(), f.test_body.as_mut()) {
                stamp_locks(st, t, &locks);
            }
        }
    }
    Ok(out)
}

/// The identity of an expression node — its address in the program being
/// checked. See [`Cx::locks`] for why this rather than its span.
fn node_id(e: &Expr) -> usize {
    e as *const Expr as usize
}

/// Apply [`Cx::locks`] to a body, in place.
fn stamp_locks(src: &Expr, out: &mut Expr, locks: &HashMap<usize, Option<Type>>) {
    if let Some(Some(ty)) = locks.get(&node_id(src)) {
        out.ty = Some(Box::new(ty.clone()));
    }
    for (s, o) in crate::children(src)
        .into_iter()
        .zip(crate::children_mut(out))
    {
        stamp_locks(s, o, locks);
    }
}

/// During checking, these types stand in for an as-yet-unknown scalar: the
/// `any` constraint, the bare-`none` inner marker, and an in-scope generic
/// type parameter. They're permitted wherever a concrete scalar primitive is
/// (set elements, dict keys, etc.).
fn is_abstract_scalar_ty(t: &Type, type_params: &[String]) -> bool {
    matches!(t, Type::Any | Type::NoneInner)
        // A variable of *this* signature. The anonymous one an `any` normalized
        // to carries an empty name and belongs to whatever signature holds it.
        || matches!(t, Type::TypeVar(n) if n.is_empty() || type_params.iter().any(|tp| tp == n))
}

impl Cx<'_> {
    fn check_fn(&self, f: &Function) -> Result<(), Error> {
        // Effects must be known and `mut` receivers well-formed regardless of
        // genericity.
        for e in &f.sig.effects {
            if !KNOWN_EFFECTS.contains(&e.as_str()) {
                return Err(Error::msg(format!(
                    "fn {:?} declares unknown effect \"!{e}\"",
                    f.name
                )));
            }
        }
        for (i, p) in f.sig.params.iter().enumerate() {
            if p.mutable && i != 0 {
                return Err(Error::msg(format!(
                    "fn {:?}: only the first parameter may be \"mut\"",
                    f.name
                )));
            }
        }
        if f.sig.is_mutating() {
            let self_p = &f.sig.params[0];
            if self_p.name != "self" {
                return Err(Error::msg(format!(
                    "fn {:?}: a \"mut\" receiver must be named \"self\"",
                    f.name
                )));
            }
            if f.sig.return_ty.is_some() {
                return Err(Error::msg(format!(
                    "fn {:?}: a mutating method cannot return a value",
                    f.name
                )));
            }
        }

        // Signature types must be valid (type parameters count as valid names).
        // A function type is allowed as a *parameter* (a lambda) but not as a
        // return type — there's no first-class function value to hand back.
        let type_var_names = f.sig.type_var_names();
        for p in &f.sig.params {
            self.check_ty(&p.ty, &type_var_names, &format!("fn {:?}", f.name))?;
        }
        if let Some(rt) = &f.sig.return_ty {
            if matches!(rt, Type::Fn(_, _)) {
                return Err(Error::msg(format!(
                    "fn {:?}: cannot return a function value ({})",
                    f.name,
                    tyname(rt)
                )));
            }
            self.check_ty(rt, &type_var_names, &format!("fn {:?}", f.name))?;
        }

        // Keyword-parameter defaults are checked like struct field defaults: in
        // an empty environment (a default can't reference other parameters —
        // it is spliced into *call sites*, where none are in scope). Effects
        // are the function's own declared set: every caller must cover the
        // callee's effects anyway, so a spliced default's effects are covered
        // wherever it lands.
        //
        // A default whose parameter type mentions a type variable (`pad: T = 0`)
        // has no answer *here*: whether `0` is a `T` depends on the
        // instantiation, and this check runs once for the template. So the
        // expression is still checked on its own — an undefined callee inside it
        // is an error now — but its *fit* against `T` waits for the call site,
        // which is exactly where the keyword-argument pass splices it, as an
        // ordinary argument of an ordinary generic call. Generic *struct* field
        // defaults have always deferred the same way, for the same reason (the
        // `s.is_generic()` skip in `check`); before this, the two disagreed and a
        // `T`-typed parameter default was simply unwritable.
        for p in &f.sig.params {
            if let Some(default) = &p.default {
                let dt = self.check_expr(default, &HashMap::new(), &f.sig.effects)?;
                let pty = subst_typevars(&p.ty, &type_var_names);
                if mentions_typevar(&pty) {
                    continue;
                }
                let dt = self.flex_int(default, &dt, &pty)?;
                expect(
                    &dt,
                    &pty,
                    &format!(
                        "default for fn {:?} parameter {:?}",
                        display(&f.name),
                        p.name
                    ),
                    default.span.clone(),
                )?;
            }
        }

        // Generic bodies are checked abstractly: each type variable (a declared
        // `<T>` or an anonymous `any`) is replaced by the permissive `__unknown__`
        // wildcard, so the body's *structural* type rules are still enforced (you
        // can't return a `T[]` where `i64` is declared) while operations whose
        // validity depends on the concrete instantiation stay permissive. For a
        // concrete function the substitution is the identity.
        let mut env: Env = HashMap::new();
        for p in &f.sig.params {
            env.insert(
                p.name.clone(),
                Binding {
                    ty: subst_typevars(&p.ty, &type_var_names),
                    mutable: p.mutable,
                },
            );
        }
        // A `mut self` method and a `()`-returning fn check their body as unit.
        let declared = subst_typevars(&f.sig.return_type(), &type_var_names);
        // Make the declared return type available to any `return value;` in the
        // body (functions are top-level, so this single slot can't nest), and
        // likewise the name/type params for any `let x: T = ..` annotation.
        *self.current_ret.borrow_mut() = declared.clone();
        *self.current_fn.borrow_mut() = f.name.clone();
        *self.current_type_params.borrow_mut() = type_var_names.clone();
        *self.current_type_bounds.borrow_mut() = f
            .sig
            .type_vars
            .iter()
            .map(|tp| (tp.name.clone(), tp.bound))
            .collect();
        let body_ty = self.check_expr(&f.body, &env, &f.sig.effects)?;
        // A bare-literal body flexes to a narrow-int return type (`fn g() -> u8
        // { 200 }`).
        let body_ty = self.flex_int(&f.body, &body_ty, &declared)?;
        // The return type is the body's context, and inlining is precisely what
        // takes it away.
        self.lock(&f.body, &declared);
        coerce(&body_ty, &declared).map_err(|()| {
            Error::at(
                format!(
                    "fn {:?}: body returns {}, but the declared return type is {}",
                    f.name,
                    tyname(&body_ty),
                    tyname(&declared)
                ),
                // The *value*'s span, not the body's: a block's span covers the
                // whole block, so reporting it underlined whichever statement
                // came first rather than the expression that produced the type.
                // Recorded by the parser — see `Expr::value_span`.
                f.body.value_span().clone(),
            )
        })
    }

    /// Validate that `t` names only known types (primitives, declared structs,
    /// in-scope type parameters) in valid positions.
    /// Validate a written type. `ctx` is the already-formatted subject of any
    /// message — `fn "lo"`, `struct "Tok" field "span"` — so the same checks
    /// serve a parameter, a struct field and a variant payload alike.
    fn check_ty(&self, t: &Type, type_params: &[String], ctx: &str) -> Result<(), Error> {
        match t {
            // Every primitive is a valid type in any general position.
            Type::Unit | Type::Primitive(_) => Ok(()),
            // The anonymous generic bound is valid anywhere a type-parameter
            // name is (that's what it desugars to during monomorphization), and
            // so is the parameter itself.
            Type::Any | Type::TypeVar(_) => Ok(()),
            // These are compiler-internal pseudo-types, never part of a
            // declared signature a user wrote — but `check_ty` also runs on
            // synthesized types (e.g. a struct field's inferred default), so
            // handle them permissively rather than asserting they can't occur.
            Type::NoneInner | Type::EmptyArrayArg | Type::NoneLiteralArg | Type::ConcatStr => {
                Ok(())
            }
            Type::Named(n) => {
                let ok = n == "Error"
                    || self.has_struct(n)
                    || self.variants.contains_key(n)
                    || type_params.iter().any(|tp| tp == n);
                if ok {
                    Ok(())
                } else {
                    let mut msg = format!("{ctx}: unknown type {n:?}");
                    if aipl_syntax::IMPORTABLE_BUILTIN_TYPES.contains(&n.as_str()) {
                        msg.push_str(&format!(
                            " — {n:?} is a builtin type; import it with `import {{ {n} }} from builtins;`"
                        ));
                    }
                    Err(Error::msg(msg))
                }
            }
            // Array/optional element types: a scalar, `str`, a nested array, or
            // an optional (`T?[]`, `T??`) — never a struct.
            Type::Array(inner) | Type::Optional(inner) => {
                self.check_elem_ty(inner, type_params, ctx)
            }
            // A set element: a scalar (i64/bool/char), `str`, or a type
            // parameter (pinned to one of those when monomorphized). No nested
            // containers, no struct/variant.
            Type::Set(inner) => {
                if is_set_elem(inner) || is_abstract_scalar_ty(inner, type_params) {
                    Ok(())
                } else {
                    Err(Error::msg(format!(
                        "{ctx}: a set element must be an integer (i8..i64, u8..u64), bool, char, or str, got {}",
                        tyname(inner)
                    )))
                }
            }
            // A dict `#{K: V}`: the key is a scalar/`str` (like a set element);
            // the value is any value type an array/optional element may be
            // (scalar, str, array, optional, struct, variant).
            Type::Dict(k, v) => {
                if !(is_dict_key(k) || is_abstract_scalar_ty(k, type_params)) {
                    return Err(Error::msg(format!(
                        "{ctx}: a dict key must be an integer (i8..i64, u8..u64), bool, char, or str, got {}",
                        tyname(k)
                    )));
                }
                self.check_elem_ty(v, type_params, ctx)
            }
            // A result `T!E`: either side may be *any* type that is valid on its
            // own, so this just recurses — which is also what applies each
            // payload's own rules (an array payload's elements are validated
            // like any array's, a dict payload's key like any dict's, a nested
            // result's sides like these). They all ride the same generic payload
            // machinery in codegen: sized by `elem_size_of`, written by
            // `store_array_elem`, refcounted by `emit_rc`. The Ok side may
            // additionally be unit — a void-result `!E` whose success carries no
            // value. A function is the one thing neither side can be: it's
            // erased by monomorphization, so there's no runtime value to carry.
            Type::Result(ok, err) => {
                for (p, side) in [(ok, "Ok"), (err, "Err")] {
                    if let Type::Fn(_, _) = &**p {
                        return Err(Error::msg(format!(
                            "{ctx}: a result {side} payload cannot be a function, got {}",
                            tyname(p)
                        )));
                    }
                }
                if !is_unit(ok) {
                    self.check_ty(ok, type_params, ctx)?;
                }
                self.check_ty(err, type_params, ctx)
            }
            // A function type (a lambda parameter): validate its argument and
            // return types. `check_fn` separately forbids it as a *return* type.
            Type::Fn(params, ret) => {
                for p in params {
                    self.check_ty(p, type_params, ctx)?;
                }
                self.check_ty(ret, type_params, ctx)
            }
            // Tuple types are lowered to Named by lower_tuples before check
            // runs, but handle them permissively in case one arrives.
            Type::Tuple(elems) => {
                for e in elems {
                    self.check_ty(e, type_params, ctx)?;
                }
                Ok(())
            }
            // Generic applications are lowered to Named by lower_generics before
            // check runs; validate the type arguments in case one arrives.
            Type::Generic(_, args) => {
                for a in args {
                    self.check_ty(a, type_params, ctx)?;
                }
                Ok(())
            }
        }
    }

    /// Type a `let`/`mut` binding's initializer and decide the binding's type —
    /// the half of `Let`/`LetMut` checking that doesn't depend on mutability.
    ///
    /// Unannotated, the binding simply takes the initializer's type. With a
    /// `let x: T = ..` annotation, `T` is the binding's type and the
    /// initializer is checked *against* it, so a bare integer literal flexes to
    /// it (`let n: u8 = 200;`) exactly as it would flowing into a parameter or
    /// a declared return type. The annotation is the only way to pin a narrow
    /// width on a binding whose initializer would otherwise infer `i64`.
    fn check_binding(
        &self,
        name: &str,
        ty: Option<&Type>,
        val: &Expr,
        body: &Expr,
        env: &Env,
        effects: &[String],
    ) -> Result<Type, Error> {
        // The annotation is the value's expected type, so it must be in scope
        // *while* the value is checked — a generic construction the value's own
        // arguments don't pin (`let r: Rule<Tok> = Spelling("(")`) has nothing
        // else to resolve from. Saved and restored rather than set, since
        // bindings nest.
        let prev = self
            .current_expected
            .replace(ty.map(|t| subst_typevars(t, &self.current_type_params.borrow())));
        let checked = self.check_expr(val, env, effects);
        *self.current_expected.borrow_mut() = prev;
        let vt = checked?;
        if is_unit(&vt) {
            return Err(Error::at(
                format!("cannot bind {name:?} to a value of type ()"),
                val.span.clone(),
            ));
        }
        check_result_inspected(name, &vt, body, val.span.clone())?;
        let Some(declared) = ty else {
            return Ok(vt);
        };
        // An annotation is a type in its own right: reject one that names an
        // unknown type or is illegal in this position, with the same
        // diagnostics a parameter's type gets.
        self.check_ty(
            declared,
            &self.current_type_params.borrow(),
            &format!("fn {:?}", self.current_fn.borrow()),
        )?;
        let declared = subst_typevars(declared, &self.current_type_params.borrow());
        // A written `Rule<Tok>` is a generic *application*; the value's type is
        // the instance it resolves to (`Rule$Tok`). Resolve the annotation the
        // same way before comparing, or the two spellings of one type fail to
        // coerce — and the message says a type is not itself. Left as-written if
        // it can't resolve (abstract inside a generic body), where `coerce`'s
        // typevar rule applies instead.
        let declared = self.resolve_generic_ty(&declared).unwrap_or(declared);
        self.lock(val, &declared);
        let vt = self.flex_int(val, &vt, &declared)?;
        // An annotated binding *converts* between integer widths rather than
        // merely checking: `let n: u8 = some_i64;` re-canonicalizes to 8 bits
        // (wrapping, exactly like the arithmetic operators), and `let n: i64 =
        // some_u8;` widens. This is the replacement for the removed `u8(..)`
        // conversion form — the annotation is now the one place a value changes
        // integer type, which is why it is allowed to be lossy here and nowhere
        // else. Non-integer pairs still have to coerce normally.
        if aipl_syntax::is_int_ty(&vt) && aipl_syntax::is_int_ty(&declared) {
            return Ok(declared);
        }
        coerce(&vt, &declared).map_err(|()| {
            // The binding the loader synthesizes for a `..base` struct spread
            // carries the target's type, so a cross-type spread lands here — but
            // the user wrote no `let`, and naming the internal binding in the
            // error would be nonsense. Report it as what they did write.
            let msg = if name.starts_with(aipl_syntax::SPREAD_BASE_PREFIX) {
                format!(
                    "cannot spread {} into {} — a \"..\" spread copies fields from \
                     another value of the same struct, and two structs that merely \
                     share field names are not interchangeable",
                    tyname(&vt),
                    tyname(&declared)
                )
            } else {
                format!(
                    "binding {name:?} is declared {}, but its value is {}",
                    tyname(&declared),
                    tyname(&vt)
                )
            };
            Error::at(msg, val.span.clone())
        })?;
        Ok(declared)
    }

    fn check_elem_ty(&self, t: &Type, type_params: &[String], ctx: &str) -> Result<(), Error> {
        match t {
            Type::Unit => Err(Error::msg("() is not allowed as an array/option element")),
            // A scalar primitive element: every integer width, plus
            // bool/char/str. An element slot is 8 bytes (`elem_size_of`) and a
            // value of a narrow type is *already* canonicalized to its width
            // before it's stored (see `canon_int`) — sign-extended for `i*`,
            // zero-extended for `u*` — so a full 8-byte store/load round-trips
            // it unchanged, exactly like an `i64`. Only the *interpretation*
            // differs, and everything that reads an element (rendering,
            // comparison, `sort`) dispatches on `int_bits`/`int_signed`.
            Type::Primitive(_) => Ok(()),
            // A type parameter, the anonymous generic bound, and the
            // bare-`none`/empty-container marker are abstract scalars — always a
            // valid element.
            Type::TypeVar(_) => Ok(()),
            Type::Any | Type::NoneInner | Type::EmptyArrayArg | Type::NoneLiteralArg => Ok(()),
            // A concat-str has the `str` runtime representation.
            Type::ConcatStr => Ok(()),
            Type::Named(n) => {
                if type_params.iter().any(|tp| tp == n)
                    || self.has_struct(n)
                    || self.variants.contains_key(n)
                {
                    Ok(()) // arrays and optionals of structs/variants are supported
                } else {
                    Err(Error::msg(format!("{ctx}: unknown type {n:?}")))
                }
            }
            // Nested arrays (`T[][]`) and nested optionals (`T??`) are allowed.
            Type::Array(inner) | Type::Optional(inner) => {
                self.check_elem_ty(inner, type_params, ctx)
            }
            // A set/dict/result can't (yet) be an array/optional element (or a
            // dict value) — they're not nestable in other containers in v1.
            Type::Set(_) | Type::Dict(_, _) | Type::Result(_, _) => Err(Error::msg(format!(
                "{ctx}: a set, dict, or result cannot be an array, optional, or dict element"
            ))),
            Type::Fn(_, _) => Err(Error::msg(format!(
                "{ctx}: arrays and optionals cannot contain function types"
            ))),
            Type::Tuple(_) => Err(Error::msg(format!(
                "{ctx}: tuple types cannot be array or optional elements"
            ))),
            // An (abstract) generic-struct/variant application `Token<K>` — a
            // valid element like any struct/variant (a concrete instance reaches
            // here as `Named`, checked once it's instantiated).
            Type::Generic(..) => Ok(()),
        }
    }

    /// Payload types of variant `vn`'s case `ctor`, if it exists.
    fn case_payload(&self, vn: &str, ctor: &str) -> Option<&[Type]> {
        self.variants
            .get(vn)?
            .iter()
            .find(|(c, _)| c == ctor)
            .map(|(_, p)| p.as_slice())
    }

    /// Whether `name` is the bare name of some variant's constructor (used only
    /// to hint, on an undefined-name error, that a constructor needs importing).
    fn is_ctor_name(&self, name: &str) -> bool {
        self.variants
            .values()
            .any(|cases| cases.iter().any(|(c, _)| c == name))
            || self
                .generic_variants
                .values()
                .any(|v| v.cases.iter().any(|c| c.name == name))
    }

    /// A bare-name (nullary) constructor must have an empty payload.
    fn expect_nullary_ctor(&self, ctor: &str, vn: &str, span: Span) -> Result<(), Error> {
        match self.case_payload(vn, ctor) {
            Some([]) => Ok(()),
            Some(p) => Err(Error::at(
                format!(
                    "constructor {ctor:?} takes {} argument(s); write {ctor}(..)",
                    p.len()
                ),
                span.clone(),
            )),
            None => Err(Error::at(
                format!("unknown constructor {ctor:?}"),
                span.clone(),
            )),
        }
    }

    /// Validate an array/`str` pattern's elements against element type `elem`,
    /// returning the types its **binder** elements introduce (one `elem` per
    /// bare-identifier element, in order — matching [`Pattern::bindings`]). A
    /// non-identifier element must be a literal, type-checked for equality
    /// against `elem`; it binds nothing. A binder name may not repeat within one
    /// pattern.
    fn array_pattern_bind_tys(
        &self,
        elems: &[Expr],
        elem: &Type,
        env: &Env,
        effects: &[String],
    ) -> Result<Vec<Type>, Error> {
        let mut tys = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for e in elems {
            if let ExprKind::Ident(name) = &e.kind {
                if seen.contains(&name.as_str()) {
                    return Err(Error::at(
                        format!("binder {name:?} appears more than once in this pattern"),
                        e.span.clone(),
                    ));
                }
                seen.push(name);
                tys.push(elem.clone());
            } else {
                if !is_pattern_literal(e) {
                    return Err(Error::at(
                        "array-pattern elements must be a binding name or a literal".to_string(),
                        e.span.clone(),
                    ));
                }
                let et = self.check_expr(e, env, effects)?;
                expect(&et, elem, "array-pattern element", e.span.clone())?;
            }
        }
        Ok(tys)
    }

    /// The types bound by `arm`'s pattern against scrutinee type `st`. Validates
    /// the constructor is legal for `st` and that the binding count matches.
    fn match_arm_bindings(
        &self,
        st: &Type,
        arm: &MatchArm,
        scrut_span: Span,
        env: &Env,
        effects: &[String],
    ) -> Result<Vec<Type>, Error> {
        // A `str` scrutinee matches string-literal arms (`"foo" => ...`), array
        // patterns (`[a, 'x'] => ...`, treating the str as a `char[]`), and a
        // wildcard (`_ => ...`). No constructor patterns.
        if is_str_repr(st) {
            return match &arm.pattern {
                Pattern::Str(_) | Pattern::Wildcard => Ok(vec![]),
                Pattern::Array(elems) => self.array_pattern_bind_tys(
                    elems,
                    &Type::Primitive(Primitive::Char),
                    env,
                    effects,
                ),
                Pattern::Ctor { .. } | Pattern::Char(_) => Err(Error::at(
                    "\"match\" on a str expects string literals, `[..]` patterns, or `_`"
                        .to_string(),
                    arm.span.clone(),
                )),
            };
        }
        // An array scrutinee matches array patterns (`[e0, ...] => ...`) and a
        // wildcard. Each element is a binder (bound to the element type) or a
        // literal (equality) whose type matches the element type. No constructor
        // patterns.
        if let Type::Array(elem) = st {
            return match &arm.pattern {
                Pattern::Array(elems) => self.array_pattern_bind_tys(elems, elem, env, effects),
                Pattern::Wildcard => Ok(vec![]),
                Pattern::Ctor { .. } | Pattern::Str(_) | Pattern::Char(_) => Err(Error::at(
                    "\"match\" on an array expects `[..]` patterns or `_`".to_string(),
                    arm.span.clone(),
                )),
            };
        }
        // A `char` scrutinee matches char-literal arms (`'a' => ...`) and a
        // wildcard. Like a `str` match it compares by value over an open domain,
        // so it binds nothing and needs a `_`.
        if matches!(st, Type::Primitive(Primitive::Char)) {
            return match &arm.pattern {
                Pattern::Char(_) | Pattern::Wildcard => Ok(vec![]),
                Pattern::Ctor { .. } | Pattern::Str(_) | Pattern::Array(_) => Err(Error::at(
                    "\"match\" on a char expects char literals or `_`".to_string(),
                    arm.span.clone(),
                )),
            };
        }
        // The non-constructor patterns only apply to a `str` / array / `char`
        // scrutinee.
        let (name, bindings, ignore_payload) = match &arm.pattern {
            Pattern::Ctor {
                name,
                bindings,
                ignore_payload,
            } => (name, bindings, *ignore_payload),
            Pattern::Str(_) => {
                return Err(Error::at(
                    format!("string-literal pattern matches a str, not {}", tyname(st)),
                    arm.span.clone(),
                ))
            }
            Pattern::Array(_) => {
                return Err(Error::at(
                    format!("array-literal pattern matches an array, not {}", tyname(st)),
                    arm.span.clone(),
                ))
            }
            Pattern::Char(_) => {
                return Err(Error::at(
                    format!("char-literal pattern matches a char, not {}", tyname(st)),
                    arm.span.clone(),
                ))
            }
            Pattern::Wildcard => {
                return Err(Error::at(
                    format!(
                        "wildcard `_` arms are only for a str/array match; a {} match must list \
                         every case",
                        tyname(st)
                    ),
                    arm.span.clone(),
                ))
            }
        };
        // A pattern constructor may be variant-qualified (`Case@Variant`); the
        // scrutinee's type fixes the variant, so resolve against the bare case.
        let bare: &str = name.split('@').next().unwrap_or(name);
        let payload: Vec<Type> = match st {
            Type::Optional(inner) => match bare {
                "some" => vec![(**inner).clone()],
                "none" => vec![],
                other => {
                    return Err(Error::at(
                        format!(
                            "\"match\" on an optional expects \"some\"/\"none\", got {other:?}"
                        ),
                        arm.span.clone(),
                    ))
                }
            },
            Type::Result(ok, err) => match bare {
                // A void-Ok result (`!E`) binds nothing in its `ok` arm.
                "ok" if is_unit(ok) => vec![],
                "ok" => vec![(**ok).clone()],
                "err" => vec![(**err).clone()],
                other => {
                    return Err(Error::at(
                        format!("\"match\" on a result expects \"ok\"/\"err\", got {other:?}"),
                        arm.span.clone(),
                    ))
                }
            },
            // A concrete named variant, or an (abstract) generic-variant
            // application `Emit<K>` inside a generic function — `cases_of`
            // resolves both.
            _ if self.cases_of(st).is_some() => {
                let cases = self.cases_of(st).expect("just checked");
                match cases.iter().find(|(c, _)| c.as_str() == bare) {
                    Some((_, p)) => p.clone(),
                    None => {
                        return Err(Error::at(
                            format!("{} has no constructor {bare:?}", tyname(st)),
                            arm.span.clone(),
                        ))
                    }
                }
            }
            other => {
                return Err(Error::at(
                    format!(
                        "\"match\" requires an optional or variant, got {}",
                        tyname(other)
                    ),
                    scrut_span,
                ))
            }
        };
        // `Ctor(..)` binds nothing on purpose, so there is no arity to agree
        // with — but it still has to be a case that *has* a payload, or it is
        // saying something untrue about the variant and the nullary `Ctor` is
        // the honest spelling.
        if ignore_payload {
            if payload.is_empty() {
                return Err(Error::at(
                    format!(
                        "constructor {name:?} carries no payload, so \"..\" ignores nothing — \
                         write {:?} instead",
                        name.split('@').next().unwrap_or(name),
                    ),
                    arm.span.clone(),
                ));
            }
            return Ok(payload);
        }
        if bindings.len() != payload.len() {
            let hint = if bindings.is_empty() && !payload.is_empty() {
                format!(
                    " (write \"{}(..)\" to match it without naming the payload)",
                    name.split('@').next().unwrap_or(name)
                )
            } else {
                String::new()
            };
            return Err(Error::at(
                format!(
                    "constructor {name:?} binds {} value(s), but {} given{hint}",
                    payload.len(),
                    bindings.len()
                ),
                arm.span.clone(),
            ));
        }
        Ok(payload)
    }

    /// Every constructor of the scrutinee's type must be matched exactly once.
    fn check_match_exhaustive(
        &self,
        st: &Type,
        arms: &[MatchArm],
        span: Span,
    ) -> Result<(), Error> {
        // A `str` / array match compares by exact equality and is open-domain, so
        // it must end with a wildcard `_` arm (the default). Arms are tried
        // top-to-bottom, so the `_` must be last (anything after it is
        // unreachable), and the literal patterns must be distinct (a duplicate is
        // the only way an earlier arm makes a later one unreachable under exact
        // matching).
        if is_str_repr(st) || matches!(st, Type::Array(_) | Type::Primitive(Primitive::Char)) {
            let noun = if is_str_repr(st) {
                "a str"
            } else if matches!(st, Type::Primitive(Primitive::Char)) {
                "a char"
            } else {
                "an array"
            };
            for (idx, arm) in arms.iter().enumerate() {
                // The `_` arm must be last.
                if matches!(arm.pattern, Pattern::Wildcard) && idx != arms.len() - 1 {
                    return Err(Error::at(
                        "the `_` arm must be last (arms after it are unreachable)".to_string(),
                        arm.span.clone(),
                    ));
                }
                // A duplicate literal pattern is dead code. (`Pattern: Eq`, so this
                // compares string literals and array-literal element lists alike.)
                if !matches!(arm.pattern, Pattern::Wildcard)
                    && arms[..idx].iter().any(|p| p.pattern == arm.pattern)
                {
                    let what = match &arm.pattern {
                        Pattern::Str(lit) => format!("duplicate {lit:?} arm"),
                        Pattern::Char(c) => {
                            format!("duplicate {:?} arm", char::from(*c))
                        }
                        _ => "duplicate match arm".to_string(),
                    };
                    return Err(Error::at(what, arm.span.clone()));
                }
            }
            if !matches!(arms.last(), Some(a) if matches!(a.pattern, Pattern::Wildcard)) {
                return Err(Error::at(
                    format!("non-exhaustive match on {noun}: add a `_` arm"),
                    span.clone(),
                ));
            }
            return Ok(());
        }
        let required: Vec<String> = match st {
            Type::Optional(_) => vec!["some".into(), "none".into()],
            Type::Result(_, _) => vec!["ok".into(), "err".into()],
            // A concrete or (abstract) generic variant — every case must appear.
            _ if self.cases_of(st).is_some() => self
                .cases_of(st)
                .expect("just checked")
                .into_iter()
                .map(|(c, _)| c)
                .collect(),
            // A non-matchable scrutinee already errored in `match_arm_bindings`.
            _ => return Ok(()),
        };
        let mut seen: HashSet<&str> = HashSet::new();
        for arm in arms {
            // Non-constructor patterns already errored in `match_arm_bindings`.
            let Pattern::Ctor { name, .. } = &arm.pattern else {
                continue;
            };
            // Patterns may be variant-qualified (`Case@Variant`); compare against
            // the bare case names the variant declares.
            let bare = name.split('@').next().unwrap_or(name);
            if !seen.insert(bare) {
                return Err(Error::at(
                    format!("duplicate \"{bare}\" arm"),
                    arm.span.clone(),
                ));
            }
        }
        let missing: Vec<&str> = required
            .iter()
            .map(String::as_str)
            .filter(|c| !seen.contains(c))
            .collect();
        if !missing.is_empty() {
            return Err(Error::at(
                format!("non-exhaustive match: missing {}", missing.join(", ")),
                span.clone(),
            ));
        }
        Ok(())
    }

    /// Effects produced by calling `name`. Read straight from its signature —
    /// builtins carry their effects (e.g. `print`'s `!prints`) in `sigs` like
    /// any other function.
    fn callee_effects(&self, name: &str) -> Vec<String> {
        self.sigs
            .get(name)
            .map(|s| s.effects.clone())
            .unwrap_or_default()
    }

    /// Check a `shim <effect> { op = f, .. } { body }` and return the body's
    /// type. Three rules, all stated in terms of the effect's operation table
    /// (`aipl_syntax::effect_operations`) rather than any particular effect:
    ///
    /// 1. **Coverage.** Every operation of `effect` must be bound exactly once,
    ///    and each bound function must match that operation's signature. With
    ///    full coverage the body provably cannot reach the real resource, which
    ///    is what makes rule 3 sound.
    /// 2. **The shim's own effects propagate.** Installing a shim means the
    ///    enclosing function may run it, so the enclosing function must declare
    ///    whatever the shim functions declare.
    /// 3. **The effect is discharged.** `effect` is added to the permitted set
    ///    for `body` only, so the body may call its operations (directly or at
    ///    any call depth) without the enclosing function declaring it.
    fn check_shim(
        &self,
        effect: &str,
        bindings: &[(String, String)],
        body: &Expr,
        env: &Env,
        effects: &[String],
        span: Span,
    ) -> Result<Type, Error> {
        let Some(ops) = aipl_syntax::effect_operations(effect) else {
            let known = aipl_syntax::SHIMMABLE_EFFECTS
                .iter()
                .map(|(n, _)| format!("\"!{n}\""))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::at(
                format!("effect \"!{effect}\" cannot be shimmed; shimmable effects are: {known}"),
                span,
            ));
        };
        // Each binding names an operation of this effect, at most once.
        let mut seen: Vec<&str> = Vec::new();
        for (op, _) in bindings {
            if !ops.contains(&op.as_str()) {
                return Err(Error::at(
                    format!(
                        "\"{op}\" is not an operation of effect \"!{effect}\"; its operations are: {}",
                        ops.join(", ")
                    ),
                    span,
                ));
            }
            if seen.contains(&op.as_str()) {
                return Err(Error::at(
                    format!("operation \"{op}\" is shimmed more than once"),
                    span,
                ));
            }
            seen.push(op);
        }
        // Coverage: a partial shim can't discharge the effect, because the
        // unshimmed operations would still reach the real resource.
        for op in ops {
            if !seen.contains(op) {
                return Err(Error::at(
                    format!(
                        "shim of \"!{effect}\" does not cover operation \"{op}\"; a partial shim \
                         cannot discharge the effect"
                    ),
                    span,
                ));
            }
        }
        for (op, f) in bindings {
            let Some(shim_sig) = self.sigs.get(f) else {
                return Err(Error::at(
                    format!("unknown function {:?} bound to \"{op}\"", display(f)),
                    span,
                ));
            };
            // The shim stands in for the operation at every call site, so it
            // must accept and return exactly what the operation does.
            let op_sig = self
                .sigs
                .get(&format!("__builtin_{op}"))
                .expect("a shimmable operation is a declared builtin");
            let show = |s: &Signature| {
                let ps = s
                    .param_types()
                    .iter()
                    .map(tyname)
                    .collect::<Vec<_>>()
                    .join(", ");
                let r = s.return_ty.as_ref().map_or("()".to_string(), tyname);
                format!("({ps}) -> {r}")
            };
            if shim_sig.param_types() != op_sig.param_types()
                || shim_sig.return_ty != op_sig.return_ty
            {
                return Err(Error::at(
                    format!(
                        "fn {:?} cannot shim \"{op}\": it is {}, but \"{op}\" is {}",
                        display(f),
                        show(shim_sig),
                        show(op_sig)
                    ),
                    span,
                ));
            }
            // Rule 2: running the shim is the enclosing function's business.
            for e in &shim_sig.effects {
                if !effects.contains(e) {
                    return Err(Error::at(
                        format!(
                            "shim fn {:?} has effect \"!{e}\" but the function installing the \
                             shim does not declare it",
                            display(f)
                        ),
                        span,
                    ));
                }
            }
        }
        // Rule 3: the body may use the effect; the enclosing function need not.
        let mut inner: Vec<String> = effects.to_vec();
        if !inner.iter().any(|e| e == effect) {
            inner.push(effect.to_string());
        }
        self.check_expr(body, env, &inner)
    }

    /// Check `expr` and return its type. `effects` is the enclosing function's
    /// declared effect set (callees must not exceed it).
    /// The result type of slicing a receiver of type `ot` (`recv[a..b]`, or
    /// the `recv[span]` Span-index sugar): a `str` slices to `str`, an array
    /// (including `char[]`) to its own type.
    fn slice_receiver_ty(&self, ot: &Type, span: Span) -> Result<Type, Error> {
        match ot {
            Type::Primitive(Primitive::Str) | Type::Array(_) => Ok(ot.clone()),
            other => Err(Error::at(
                format!("cannot slice a value of type {}", tyname(other)),
                span,
            )),
        }
    }

    fn check_expr(&self, expr: &Expr, env: &Env, effects: &[String]) -> Result<Type, Error> {
        self.check_expr_at(expr, env, effects, Pos::Value)
    }

    /// [`check_expr`], told whether this expression's value is *used*.
    ///
    /// Only `match` reads it (see [`Pos`]), but the answer has to be threaded
    /// rather than looked up, because "discarded" is a property of where an
    /// expression sits, not of the expression itself — and it reaches *through*
    /// the forms that merely sequence or choose a value. `match (x) { .. };`
    /// discards its arms' values, so an arm body is discarded too, and so is a
    /// `match` nested inside that arm.
    ///
    /// Every other arm recurses through plain [`check_expr`], which resets to
    /// [`Pos::Value`]. That is the safe polarity: a form added later treats its
    /// children as used — the stricter reading — rather than silently inheriting
    /// a `Discard` that lets an unchecked `match` through.
    fn check_expr_at(
        &self,
        expr: &Expr,
        env: &Env,
        effects: &[String],
        pos: Pos,
    ) -> Result<Type, Error> {
        let span = expr.span.clone();
        Ok(match &expr.kind {
            ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
            ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
            ExprKind::Unit => Type::Unit,
            ExprKind::Shim(effect, bindings, body) => {
                self.check_shim(effect, bindings, body, env, effects, span.clone())?
            }
            ExprKind::Num(_) => Type::Primitive(Primitive::I64),
            ExprKind::Bool(_) => Type::Primitive(Primitive::Bool),
            ExprKind::Str(_) => Type::Primitive(Primitive::Str),
            ExprKind::Char(_) => Type::Primitive(Primitive::Char),
            ExprKind::None => Type::Optional(Box::new(Type::NoneInner)),
            ExprKind::Ident(name) => {
                // A local binding shadows everything; otherwise a bare name may
                // be a nullary variant constructor (e.g. `Empty`), or a
                // function used as a value (`let f = inc;`).
                if let Some(b) = env.get(name) {
                    b.ty.clone()
                } else if let Some(vn) = self.ctors.get(name) {
                    let bare = name.split('@').next().unwrap_or(name);
                    self.expect_nullary_ctor(bare, vn, span.clone())?;
                    Type::Named(vn.clone())
                } else if self.generic_ctors.contains_key(name) {
                    // A nullary constructor of a generic variant (`Nothing`):
                    // its instance can't be inferred from a (missing) payload, so
                    // it's resolved from the expected type.
                    let base = &self.generic_ctors[name];
                    self.infer_generic_variant_ctor(
                        base,
                        name,
                        &[],
                        env,
                        effects,
                        span.clone(),
                        node_id(expr),
                    )?
                } else if let Some(sig) = self.sigs.get(name.as_str()) {
                    // A named function as a first-class value: its type is the
                    // corresponding `Type::Fn`. A runtime function value is a
                    // bare code address, so v1 restricts it to functions that
                    // need no closure and no effect accounting: generic
                    // functions (no single address) and effect-carrying ones
                    // (indirect calls can't be effect-checked at the call site)
                    // are rejected.
                    if sig.is_generic() {
                        return Err(Error::at(
                            format!(
                                "generic function {:?} cannot be used as a value \
                                 (a function value is a single concrete address)",
                                display(name)
                            ),
                            span.clone(),
                        ));
                    }
                    if !sig.effects.is_empty() {
                        return Err(Error::at(
                            format!(
                                "function {:?} has effects ({}), so it cannot be used as a value \
                                 (its effects couldn't be accounted for at an indirect call)",
                                display(name),
                                sig.effects
                                    .iter()
                                    .map(|e| format!("!{e}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            span.clone(),
                        ));
                    }
                    Type::Fn(sig.param_types(), Box::new(sig.return_type()))
                } else {
                    return Err(Error::at(
                        format!("unknown identifier {name:?}"),
                        span.clone(),
                    ));
                }
            }
            ExprKind::Neg(x) => {
                let t = self.check_expr(x, env, effects)?;
                expect(
                    &t,
                    &Type::Primitive(Primitive::I64),
                    "unary \"-\" operand",
                    x.span.clone(),
                )?;
                Type::Primitive(Primitive::I64)
            }
            ExprKind::Not(x) => {
                let t = self.check_expr(x, env, effects)?;
                expect(
                    &t,
                    &Type::Primitive(Primitive::Bool),
                    "\"!\" operand",
                    x.span.clone(),
                )?;
                Type::Primitive(Primitive::Bool)
            }
            ExprKind::Binop(l, op, r) => {
                let lt = self.check_expr(l, env, effects)?;
                let rt = self.check_expr(r, env, effects)?;
                // A bare integer literal flexes to the *other* operand's integer
                // type (if it fits), so `i8_val == 5` needs no explicit `i8(5)`.
                let rt2 = self.flex_int(r, &rt, &lt)?;
                let lt2 = self.flex_int(l, &lt, &rt)?;
                self.check_binop(
                    *op,
                    &lt2,
                    &rt2,
                    l.span.clone(),
                    r.span.clone(),
                    span.clone(),
                )?
            }
            ExprKind::If(c, t, e) => {
                let ct = self.check_expr(c, env, effects)?;
                expect(
                    &ct,
                    &Type::Primitive(Primitive::Bool),
                    "if condition",
                    c.span.clone(),
                )?;
                let tt = self.check_expr_at(t, env, effects, pos)?;
                let et = self.check_expr_at(e, env, effects, pos)?;
                // One branch a bare literal, the other a narrow int: the literal
                // takes the other's type (`if (b) { u8_val } else { 9 }`).
                let tt = self.flex_int(t, &tt, &et)?;
                let et = self.flex_int(e, &et, &tt)?;
                if coerce(&tt, &et).is_err() && coerce(&et, &tt).is_err() {
                    return Err(Error::at(
                        format!(
                            "if branches have mismatched types: {} vs {}",
                            tyname(&tt),
                            tyname(&et)
                        ),
                        span.clone(),
                    ));
                }
                merge(tt, et)
            }
            ExprKind::Seq(first, rest) => {
                // A mutating method call in statement position discards its
                // result, silently losing the mutation. Require the `set` form,
                // which writes the mutated value back into the receiver (or use
                // the returned value in an expression). A non-mutating method
                // called for effect (`x.print()`) is unaffected.
                if let ExprKind::Call(name, cargs, true) = &first.kind {
                    if self
                        .sigs
                        .get(name.as_str())
                        .is_some_and(|s| s.is_mutating())
                    {
                        let method = display(name);
                        let recv = match cargs.first().map(|a| &a.kind) {
                            Some(ExprKind::Ident(v)) => v.clone(),
                            _ => "recv".to_string(),
                        };
                        return Err(Error::at(
                            format!(
                                "the result of mutating method \"{method}\" is discarded, \
                                 losing the mutation; write \"set {recv}.{method}(..)\" to \
                                 mutate {recv} in place, or use the returned value"
                            ),
                            first.span.clone(),
                        ));
                    }
                }
                let ft = self.check_expr_at(first, env, effects, Pos::Discard)?;
                // A discarded statement whose value is a result would silently
                // drop its error — forbid it. The error must be handled: match on
                // it, or propagate with `?`. (Binding it with `let` and then never
                // reading the binding is rejected too — see `Let`/`LetMut`.)
                if matches!(ft, Type::Result(_, _)) {
                    return Err(Error::at(
                        "this result is discarded, ignoring its possible error; handle it \
                         with `match` or propagate it with `?`",
                        first.span.clone(),
                    ));
                }
                self.check_expr_at(rest, env, effects, pos)?
            }
            ExprKind::Return(value) => {
                // The returned value must match the enclosing function's declared
                // return type (a bare literal flexes to a narrow-int return).
                let vt = self.check_expr(value, env, effects)?;
                let declared = self.current_ret.borrow().clone();
                let vt = self.flex_int(value, &vt, &declared)?;
                coerce(&vt, &declared).map_err(|()| {
                    Error::at(
                        format!(
                            "return value is {}, but the function returns {}",
                            tyname(&vt),
                            tyname(&declared)
                        ),
                        span.clone(),
                    )
                })?;
                // `return` doesn't produce a value — it's a statement, like `set`.
                Type::Unit
            }
            // A lambda used as a *value* (bound to a local, stored in a struct
            // field, or a lowered payload constructor `Ctor`) becomes a
            // non-capturing top-level function whose address is the value. In
            // argument position the expected function type supplies parameter
            // types and captures are lifted (handled in `check_call`); here
            // there is neither, so every parameter must be explicitly typed, the
            // body must be effect-free (an indirect call can't be effect-checked
            // at the site), and it may not capture an enclosing local.
            ExprKind::Lambda(params, body) => {
                let mut ptys = Vec::with_capacity(params.len());
                let mut env2 = env.clone();
                for p in params {
                    let Some(ann) = &p.ty else {
                        return Err(Error::at(
                            format!(
                                "lambda parameter {:?} used as a value must be typed, \
                                 e.g. `|{}: i64| ...`",
                                p.name, p.name
                            ),
                            p.span.clone(),
                        ));
                    };
                    ptys.push(ann.clone());
                    env2.insert(
                        p.name.clone(),
                        Binding {
                            ty: ann.clone(),
                            mutable: false,
                        },
                    );
                }
                // Reject captures: any free identifier of the body that resolves
                // to an enclosing local (not a global function/type). Function
                // values carry no environment, so a capture can't be honored.
                let tenv: HashMap<String, Type> =
                    env.iter().map(|(k, b)| (k.clone(), b.ty.clone())).collect();
                if let Some((cap, _)) = super::free_vars(body, params, &tenv).into_iter().next() {
                    return Err(Error::at(
                        format!(
                            "a lambda used as a value cannot capture local {cap:?} \
                             (function values are non-capturing)"
                        ),
                        span.clone(),
                    ));
                }
                // Effect-free body: check with an empty effect context so any
                // effectful call inside is reported.
                let mark = self.try_errs.borrow().len();
                let body_ty = self.check_expr(body, &env2, &[])?;
                let body_ty = {
                    let errs = self.try_errs.borrow();
                    err_side_from_tries(body_ty, &errs[mark..])
                };
                self.try_errs.borrow_mut().truncate(mark);
                Type::Fn(ptys, Box::new(body_ty))
            }
            ExprKind::TupleLit(elems) => {
                let mut elem_tys: Vec<Type> = Vec::with_capacity(elems.len());
                for e in elems {
                    elem_tys.push(self.check_expr(e, env, effects)?);
                }
                let name = tuple_struct_name(&elem_tys);
                if !self.has_struct(&name) {
                    let fields: Vec<(String, Type, bool)> = elem_tys
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (format!("_{i}"), t.clone(), false))
                        .collect();
                    self.add_syn_struct(name.clone(), fields);
                }
                Type::Named(name)
            }
            ExprKind::Let(name, ty, val, body) => {
                let bt = self.check_binding(name, ty.as_ref(), val, body, env, effects)?;
                let mut env2 = env.clone();
                env2.insert(
                    name.clone(),
                    Binding {
                        ty: bt,
                        mutable: false,
                    },
                );
                self.check_expr_at(body, &env2, effects, pos)?
            }
            ExprKind::LetMut(name, ty, val, body) => {
                let bt = self.check_binding(name, ty.as_ref(), val, body, env, effects)?;
                let mut env2 = env.clone();
                env2.insert(
                    name.clone(),
                    Binding {
                        ty: bt,
                        mutable: true,
                    },
                );
                self.check_expr_at(body, &env2, effects, pos)?
            }
            ExprKind::Assign(lhs, val, body) => {
                let Some((name, path)) = ast::assign_target(lhs) else {
                    return Err(Error::at(
                        "set: assignment target must be a variable or a field of one".to_string(),
                        lhs.span.clone(),
                    ));
                };
                let binding = env.get(name).ok_or_else(|| {
                    Error::at(format!("set: undeclared variable {name:?}"), span.clone())
                })?;
                if !binding.mutable {
                    return Err(Error::at(
                        format!(
                            "set: cannot assign to immutable binding {name:?} (use \"let mut\")"
                        ),
                        span.clone(),
                    ));
                }
                // Walk the field path down to the stored-to place's type; every
                // step but the last must land on a struct.
                let mut expected = binding.ty.clone();
                for (i, field) in path.iter().enumerate() {
                    let target = ast::assign_target_display(name, &path, i);
                    let Type::Named(sn) = &expected else {
                        return Err(Error::at(
                            format!(
                                "set: field assignment target must be a struct, {target:?} \
                                 has type {}",
                                tyname(&expected)
                            ),
                            span.clone(),
                        ));
                    };
                    let fields = self.struct_fields(sn).ok_or_else(|| {
                        Error::at(
                            format!(
                                "set: field assignment target must be a struct, {target:?} \
                                 has type {}",
                                display(sn)
                            ),
                            span.clone(),
                        )
                    })?;
                    expected = fields
                        .iter()
                        .find(|(n, _, _)| n == *field)
                        .map(|(_, t, _)| t.clone())
                        .ok_or_else(|| {
                            Error::at(
                                format!("struct {:?} has no field {field:?}", display(sn)),
                                span.clone(),
                            )
                        })?;
                }
                let vt = self.check_expr(val, env, effects)?;
                // A bare literal takes the binding's (or field's) int type.
                let vt = self.flex_int(val, &vt, &expected)?;
                expect(&vt, &expected, "set", val.span.clone())?;
                self.check_expr_at(body, env, effects, pos)?
            }
            ExprKind::For(_var, iter, body) => {
                let it = self.check_expr(iter, env, effects)?;
                let elem = match &it {
                    Type::Array(inner) => (**inner).clone(),
                    t if *t == Type::Primitive(Primitive::Str) => Type::Primitive(Primitive::Char),
                    other => {
                        return Err(Error::at(
                            format!(
                                "for-loop iterable must be a str or array, got {}",
                                tyname(other)
                            ),
                            iter.span.clone(),
                        ));
                    }
                };
                let mut env2 = env.clone();
                env2.insert(
                    _var.clone(),
                    Binding {
                        ty: elem,
                        mutable: false,
                    },
                );
                self.check_expr_at(body, &env2, effects, Pos::Discard)?;
                Type::Primitive(Primitive::I64)
            }
            ExprKind::While(cond, body) => {
                let ct = self.check_expr(cond, env, effects)?;
                expect(
                    &ct,
                    &Type::Primitive(Primitive::Bool),
                    "while condition",
                    cond.span.clone(),
                )?;
                // The body sees the enclosing scope (no loop binding); a `mut`
                // tested/updated across iterations is declared before the loop.
                self.check_expr_at(body, env, effects, Pos::Discard)?;
                Type::Primitive(Primitive::I64)
            }
            ExprKind::ArrayLit(elems) => {
                let mut elem_ty = Type::NoneInner;
                for (i, e) in elems.iter().enumerate() {
                    let t = self.check_expr(e, env, effects)?;
                    if i == 0 {
                        elem_ty = t;
                    }
                }
                // A struct or variant element is valid too (must be declared); so
                // is an (abstract) generic-struct/variant application `Token<K>`.
                // `has_variant`, not `self.variants`: a *synthesized* generic
                // instance (`Rule$Kind`, from `Rule<Kind>`) lives in the syn map,
                // and one of those is as valid an element as any other variant.
                let elem_ok = is_valid_elem(&elem_ty)
                    || matches!(&elem_ty, Type::Named(n)
                        if self.has_struct(n) || self.has_variant(n))
                    || matches!(&elem_ty, Type::Generic(..));
                if !elems.is_empty() && !elem_ok {
                    return Err(Error::at(
                        format!(
                            "array elements must be an integer (i8..i64, u8..u64), bool, char, \
                             str, an array, an optional, or a struct, got {}",
                            tyname(&elem_ty)
                        ),
                        span.clone(),
                    ));
                }
                Type::Array(Box::new(elem_ty))
            }
            ExprKind::SetLit(elems) => {
                // Elements share one type (i64/bool/char/str); an empty `#{}` is
                // `__none__` (coerces to any `T{}`). Dups dropped at runtime.
                let mut elem_ty = Type::NoneInner;
                for (i, e) in elems.iter().enumerate() {
                    let t = self.check_expr(e, env, effects)?;
                    if i == 0 {
                        elem_ty = t;
                    } else {
                        expect(&t, &elem_ty, "set element", e.span.clone())?;
                    }
                }
                if !elems.is_empty() && !is_set_elem(&elem_ty) {
                    return Err(Error::at(
                        format!(
                            "set elements must be an integer (i8..i64, u8..u64), bool, char, or str, got {}",
                            tyname(&elem_ty)
                        ),
                        span.clone(),
                    ));
                }
                Type::Set(Box::new(elem_ty))
            }
            ExprKind::DictLit(pairs) => {
                // Keys share one scalar/str type; values share one value type.
                // An empty `#{:}` is `#{__none__: __none__}` (coerces to any
                // `#{K: V}`). Duplicate keys keep the last binding (at runtime).
                let mut key_ty = Type::NoneInner;
                let mut val_ty = Type::NoneInner;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    let kt = self.check_expr(k, env, effects)?;
                    let vt = self.check_expr(v, env, effects)?;
                    if i == 0 {
                        key_ty = kt;
                        val_ty = vt;
                    } else {
                        expect(&kt, &key_ty, "dict key", k.span.clone())?;
                        expect(&vt, &val_ty, "dict value", v.span.clone())?;
                    }
                }
                if !pairs.is_empty() {
                    if !is_dict_key(&key_ty) {
                        return Err(Error::at(
                            format!(
                                "dict keys must be an integer (i8..i64, u8..u64), bool, char, or str, got {}",
                                tyname(&key_ty)
                            ),
                            span.clone(),
                        ));
                    }
                    let val_ok = is_valid_elem(&val_ty)
                        || matches!(&val_ty, Type::Named(n)
                            if self.has_struct(n) || self.variants.contains_key(n));
                    if !val_ok {
                        return Err(Error::at(
                            format!(
                                "dict values must be an integer (i8..i64, u8..u64), bool, char, str, an array, an optional, \
                                 or a struct, got {}",
                                tyname(&val_ty)
                            ),
                            span.clone(),
                        ));
                    }
                }
                Type::Dict(Box::new(key_ty), Box::new(val_ty))
            }
            ExprKind::Index(obj, idx) => {
                let ot = self.check_expr(obj, env, effects)?;
                let it = self.check_expr(idx, env, effects)?;
                // `s[span]` — a `Span` index is slice sugar for
                // `s[span.start..span.end]`, so it takes the slice rules:
                // a `str` or array receiver, sliced to its own type.
                if matches!(&it, Type::Named(n) if n == "__builtin_Span") {
                    return self.slice_receiver_ty(&ot, obj.span.clone());
                }
                expect_len_operand(&it, "array index", idx.span.clone())?;
                let elem = match ot {
                    Type::Array(inner) => *inner,
                    // `s[i]` on a `str` is the byte at `i` as a `char?`.
                    Type::Primitive(Primitive::Str) => Type::Primitive(Primitive::Char),
                    other => {
                        return Err(Error::at(
                            format!("cannot index a value of type {}", tyname(&other)),
                            obj.span.clone(),
                        ));
                    }
                };
                // Indexing yields `elem?` — for a `T?[]` that's a genuine `T??`.
                Type::Optional(Box::new(elem))
            }
            ExprKind::Slice(obj, start, end) => {
                let ot = self.check_expr(obj, env, effects)?;
                let result = self.slice_receiver_ty(&ot, obj.span.clone())?;
                let st = self.check_expr(start, env, effects)?;
                expect_len_operand(&st, "slice start", start.span.clone())?;
                // An open-ended `recv[start..]` has no end expression — it runs to
                // the receiver's length.
                if let Some(end) = end {
                    let et = self.check_expr(end, env, effects)?;
                    expect_len_operand(&et, "slice end", end.span.clone())?;
                }

                result
            }
            ExprKind::Try(inner) => {
                // `expr?` unwraps a result `T!E` (yielding `T`) or an optional
                // `T?` (yielding `T`). The constraint that the enclosing fn
                // returns `_!E` / an optional (so the early-returned Err / `none`
                // fits) is enforced in codegen, where the return type is in scope.
                let it = self.check_expr(inner, env, effects)?;
                match it {
                    Type::Result(ok, e) => {
                        // What this `?` propagates. A lambda body reads these
                        // back to learn its own error type — see
                        // `err_side_from_tries`.
                        self.try_errs.borrow_mut().push((*e).clone());
                        (*ok).clone()
                    }
                    Type::Optional(inner) => (*inner).clone(),
                    other => {
                        return Err(Error::at(
                            format!(
                                "\"?\" requires a result (T!E) or an optional (T?), got {}",
                                tyname(&other)
                            ),
                            span.clone(),
                        ));
                    }
                }
            }
            ExprKind::Field(obj, fname) => {
                let ot = self.check_expr(obj, env, effects)?;
                let fields = self.fields_of(&ot).ok_or_else(|| {
                    Error::at(
                        format!("field access on non-struct value of type {}", tyname(&ot)),
                        obj.span.clone(),
                    )
                })?;
                fields
                    .iter()
                    .find(|(n, _, _)| n == fname)
                    .map(|(_, t, _)| t.clone())
                    .ok_or_else(|| {
                        Error::at(
                            format!("struct {} has no field {fname:?}", tyname(&ot)),
                            span.clone(),
                        )
                    })?
            }
            ExprKind::Construct(name, inits) => {
                // A construction of a generic struct template (`Box { value: 5 }`)
                // infers its type arguments from the field values.
                if self.generic_structs.contains_key(name) {
                    return self.infer_generic_construct(name, inits, env, effects, span.clone());
                }
                let fields = self.structs.get(name).cloned().ok_or_else(|| {
                    let mut msg = format!("unknown struct {name:?}");
                    if aipl_syntax::IMPORTABLE_BUILTIN_TYPES.contains(&name.as_str()) {
                        msg.push_str(&format!(
                            " — {name:?} is a builtin type; import it with `import {{ {name} }} from builtins;`"
                        ));
                    }
                    Error::at(msg, span.clone())
                })?;
                // Each provided init must name a real field with a compatible type.
                for fi in inits {
                    let (_, expected, _) = fields
                        .iter()
                        .find(|(n, _, _)| *n == fi.name)
                        .ok_or_else(|| {
                            Error::at(
                                format!("struct {:?} has no field {:?}", display(name), fi.name),
                                fi.value.span.clone(),
                            )
                        })?;
                    let vt = self.check_expr(&fi.value, env, effects)?;
                    let ctx = format!("struct {:?} field {:?}", display(name), fi.name);
                    // `start..end` desugars to a `__builtin_Span` construction, so
                    // its two fields are slice bounds by another name — accept
                    // either signedness there, exactly as `xs[a..b]` does.
                    if name == "__builtin_Span" {
                        expect_len_operand(&vt, &ctx, fi.value.span.clone())?;
                    } else {
                        // A bare literal takes the field's int type.
                        let vt = self.flex_int(&fi.value, &vt, expected)?;
                        expect(&vt, expected, &ctx, fi.value.span.clone())?;
                    }
                }
                // Every field without a default must be provided.
                for (fname, _, has_default) in &fields {
                    if !has_default && !inits.iter().any(|i| &i.name == fname) {
                        return Err(Error::at(
                            format!(
                                "struct {:?} field {fname:?} has no default and was not provided",
                                display(name)
                            ),
                            span.clone(),
                        ));
                    }
                }
                Type::Named(name.clone())
            }
            ExprKind::Match(scrut, arms) => {
                let st = self.check_expr(scrut, env, effects)?;
                // The scrutinee's type decides the legal patterns: `some`/`none`
                // for an optional, the declared cases for a variant.
                // `merged` carries the arm that produced it, so a bare-literal arm
                // can flex to a *later* narrow-int arm as well as the other way
                // round — the same courtesy `if` branches get. Without it,
                // `match (w) { WNil => 0, WC(n, ..) => n }` with a `u64` payload
                // has no spelling at all now that `u64(0)` is gone.
                let mut merged: Option<(Type, &Expr)> = None;
                for arm in arms {
                    let bind_tys =
                        self.match_arm_bindings(&st, arm, scrut.span.clone(), env, effects)?;
                    let mut env2 = env.clone();
                    for (name, ty) in arm.pattern.bindings().iter().zip(bind_tys) {
                        env2.insert(name.clone(), Binding { ty, mutable: false });
                    }
                    let t = self.check_expr_at(&arm.body, &env2, effects, pos)?;
                    merged = Some(match merged {
                        None => (t, &arm.body),
                        Some((prev, prev_body)) => {
                            let t = self.flex_int(&arm.body, &t, &prev)?;
                            let prev = self.flex_int(prev_body, &prev, &t)?;
                            (merge(prev, t), &arm.body)
                        }
                    });
                }
                let merged = merged.map(|(t, _)| t);
                self.check_match_exhaustive(&st, arms, span.clone())?;
                let ty = merged.unwrap_or(Type::Primitive(Primitive::I64));
                self.check_match_kind(&ty, arms, pos, span.clone())?;
                ty
            }
            ExprKind::Call(name, args, method_style) => {
                // For a method call the receiver is `args[0]`, and two rules
                // apply that a free call is exempt from. (`check_call` then
                // handles arity/types/effects uniformly for both forms.)
                if *method_style {
                    let recv = &args[0];
                    // A mutating method in *expression* position copies its
                    // receiver (copy-and-modify), so it doesn't require a mutable
                    // receiver. The in-place writeback form `set recv.f(args)`
                    // does — that's enforced by the `Assign` check ("cannot assign
                    // to immutable binding"), which fires on the target directly.
                    // A user function called as a method must declare a `self` receiver.
                    if let Some(s) = self.sigs.get(name.as_str()) {
                        if !s.is_method() {
                            return Err(Error::at(
                                format!(
                                    "fn {:?} cannot be called as a method (its first parameter must be named \"self\")",
                                    display(name)
                                ),
                                recv.span.clone(),
                            ));
                        }
                    }
                }
                self.check_call(name, args, env, effects, span.clone(), node_id(expr))?
            }
        })
    }

    /// Enforce that a `match` is *either* a statement or an expression, never
    /// silently both. See [`Pos`] for the rule; this is where it is decided.
    ///
    /// The kind comes from the arms: a `match` whose arms produce no value is a
    /// statement, one whose arms produce a value is an expression. Each then has
    /// one obligation, and they are deliberately complementary — every `match`
    /// satisfies exactly one, so there is no shape that quietly does both.
    fn check_match_kind(
        &self,
        ty: &Type,
        arms: &[ast::MatchArm],
        pos: Pos,
        span: Span,
    ) -> Result<(), Error> {
        match (ty, pos) {
            // A statement `match` used as a statement — the whole point of the
            // form, and the only shape allowed to assign to anything outside
            // itself.
            (Type::Unit, Pos::Discard) => Ok(()),
            // A statement `match` in expression position: its arms yield
            // nothing, so whatever is reading it gets nothing to read.
            (Type::Unit, Pos::Value) => Err(Error::at(
                "this `match` produces no value, so it is a statement — end it with \";\" \
                 (to use it as an expression, every arm must produce a value)"
                    .to_string(),
                span,
            )),
            // An expression `match` whose value is thrown away. The arms went to
            // the trouble of producing one, so dropping it is a mistake — either
            // use it, or make the arms statements.
            (_, Pos::Discard) => Err(Error::at(
                format!(
                    "this `match` produces {}, but its value is unused — bind it with \
                     \"let\", return it, or make every arm a statement so the `match` is \
                     a statement too",
                    tyname(ty)
                ),
                span,
            )),
            // An expression `match` used as one: its arms must read as pure
            // value-producers, so a `set` reaching outside is out. A statement
            // `match` is where that belongs.
            (_, Pos::Value) => {
                for arm in arms {
                    let declared: HashSet<String> = arm.pattern.bindings().into_iter().collect();
                    if let Some((name, at)) = outer_assign(&arm.body, &declared) {
                        return Err(Error::at(
                            format!(
                                "this arm assigns to {name:?}, which is declared outside \
                                 the `match` — an expression `match` produces a value and \
                                 may not also mutate. Make every arm a statement (ending \
                                 the `match` with \";\"), or lift the assignment out and \
                                 use the `match`'s value"
                            ),
                            at,
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    fn check_call(
        &self,
        name: &str,
        args: &[Expr],
        env: &Env,
        effects: &[String],
        span: Span,
        // Identity of the call expression, for `Cx::lock_node`.
        node: usize,
    ) -> Result<Type, Error> {
        // A variant constructor `Ctor(a, b, ...)` (unless shadowed by a local
        // function-typed binding, handled below): check each argument against
        // the case's payload type; the result is the variant type.
        if !env.contains_key(name) {
            if let Some(vn) = self.ctors.get(name) {
                // A constructor reference may be variant-qualified (`Case@Variant`);
                // the case's payload is keyed by the bare case name.
                let bare = name.split('@').next().unwrap_or(name);
                let payload = self.case_payload(vn, bare).unwrap_or(&[]).to_vec();
                if args.len() != payload.len() {
                    return Err(Error::at(
                        format!(
                            "constructor {bare:?} expects {} argument(s), got {}",
                            payload.len(),
                            args.len()
                        ),
                        span.clone(),
                    ));
                }
                for (arg, pty) in args.iter().zip(&payload) {
                    let at = self.check_expr(arg, env, effects)?;
                    let at = self.flex_int(arg, &at, pty)?;
                    expect(
                        &at,
                        pty,
                        &format!("constructor {bare:?} argument"),
                        arg.span.clone(),
                    )?;
                }
                return Ok(Type::Named(vn.clone()));
            }
            // A constructor of a generic variant template: resolve to a concrete
            // instance by inferring the type arguments.
            if let Some(base) = self.generic_ctors.get(name) {
                return self.infer_generic_variant_ctor(base, name, args, env, effects, span, node);
            }
        }
        // The integer conversion builtins `i8(x)`/`u64(x)`/… are gone. A typed
        // binding does the job — and it is the *only* place a value changes
        // integer type, so a lossy narrowing is always written down. A bare
        // literal needs nothing at all: it takes whatever integer type its
        // context expects. Recognized here only to say so, since otherwise this
        // reads as a call to an undefined function.
        if !env.contains_key(name) && aipl_syntax::int_bits(name).is_some() {
            return Err(Error::at(
                format!(
                    "{name}(..) conversions were removed — bind with the type instead \
                     (`let n: {name} = ..;`), or drop the conversion entirely if the \
                     argument is a literal, which takes the type its context expects"
                ),
                span.clone(),
            ));
        }
        // `ok(x)` / `err(e)` — result constructors, like `some`/`none`. Each
        // pins one side from its argument; the other side is `__none__`, left
        // for the expected result type to resolve by coercion (e.g. `ok(5)` is
        // `i64!__none__`, coercing to a declared `i64!str`).
        if !env.contains_key(name) && (name == "ok" || name == "err") {
            // `ok()` with no argument is the void success of a `!E` result.
            if name == "ok" && args.is_empty() {
                return Ok(Type::Result(
                    Box::new(Type::Unit),
                    Box::new(Type::NoneInner),
                ));
            }
            if args.len() != 1 {
                return Err(Error::at(
                    format!("{name:?} expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let t = self.check_expr(&args[0], env, effects)?;
            let none = || Box::new(Type::NoneInner);
            return Ok(if name == "ok" {
                Type::Result(Box::new(t), none())
            } else {
                Type::Result(none(), Box::new(t))
            });
        }
        // `s.len()` / `len(s)` — and its `is_nonempty` companion — on a set,
        // dict, or string: both signatures are `(self: T[]) -> ..`, which
        // doesn't unify with `#{T}` / `#{K: V}` / `str`, so dispatch those
        // receivers here. (An array receiver falls through to the generic
        // signature below.) For a string `len` is the byte length, and
        // `is_nonempty` asks whether there is any byte at all.
        if matches!(name, "__builtin_len" | "__builtin_is_nonempty") && args.len() == 1 {
            let t = self.check_expr(&args[0], env, effects)?;
            if matches!(t, Type::Set(_) | Type::Dict(_, _)) || is_str_repr(&t) {
                return Ok(if name == "__builtin_len" {
                    Type::Primitive(Primitive::U64)
                } else {
                    Type::Primitive(Primitive::Bool)
                });
            }
        }
        // `a + b` / `a - b` resolve (in the loader) to a call to the file's bound
        // `+`/`-` implementation — `__builtin_{wrapping,saturating}_{add,sub}`.
        // Each is integer arithmetic (the flavors differ only in overflow codegen),
        // typed here exactly like the primitive Binop: same-width integers, with a
        // bare literal operand flexing to the other's width. Reserved, not imported.
        if let Some(op) = match name {
            "__builtin_wrapping_add" | "__builtin_saturating_add" => Some("+"),
            "__builtin_wrapping_sub" | "__builtin_saturating_sub" => Some("-"),
            "__builtin_wrapping_mul" => Some("*"),
            _ => None,
        } {
            if args.len() == 2 {
                let lt = self.check_expr(&args[0], env, effects)?;
                let rt = self.check_expr(&args[1], env, effects)?;
                let rt2 = self.flex_int(&args[1], &rt, &lt)?;
                let lt2 = self.flex_int(&args[0], &lt, &rt)?;
                return self.check_int_arith(
                    op,
                    &lt2,
                    &rt2,
                    args[0].span.clone(),
                    args[1].span.clone(),
                );
            }
        }
        // `s.starts_with(p)` / `s.ends_with(p)` / `s.contains(n)` /
        // `s.starts_with_at(p, i)`: the pattern/needle is variadic, so it
        // accepts the sequence, a single element, or an optional element. A
        // `str` receiver takes a `char*` pattern (a `str`, a `char`, or a
        // `char?`); a `T[]` receiver takes a `T*` pattern (a `T[]`, a `T`, or a
        // `T?`). Fully dispatched here rather than through the generic
        // signature. `starts_with_at` carries one extra argument, the offset to
        // match at — a slice bound in every respect, so it is checked as one.
        let at_arity = usize::from(name == "__builtin_starts_with_at");
        if matches!(
            name,
            "__builtin_starts_with"
                | "__builtin_ends_with"
                | "__builtin_contains"
                | "__builtin_starts_with_at"
        ) && args.len() == 2 + at_arity
        {
            let recv = self.check_expr(&args[0], env, effects)?;
            let pat = self.check_expr(&args[1], env, effects)?;
            // The variadic sequence type per receiver: `str` for a string,
            // `T[]` for an array (its own type).
            let seq = if is_str_repr(&recv) {
                Some(Type::Primitive(Primitive::Str))
            } else if matches!(recv, Type::Array(_)) {
                Some(recv.clone())
            } else {
                None
            };
            if let Some(seq) = seq {
                // A bare literal pattern flexes to the sequence's element type,
                // so `i8_array.contains(-128)` needs no conversion on the literal.
                let pat = self.flex_int(&args[1], &pat, &variadic_elem(&seq))?;
                if !variadic_accepts(&pat, &seq) {
                    let elem = variadic_elem(&seq);
                    return Err(Error::at(
                        format!(
                            "{:?} pattern expects {}, {}, or {}?, got {}",
                            display(name),
                            tyname(&seq),
                            tyname(&elem),
                            tyname(&elem),
                            tyname(&pat)
                        ),
                        args[1].span.clone(),
                    ));
                }
                // `starts_with_at`'s offset: the same operand a slice start is,
                // checked the same way so `xs[i..].starts_with(p)` and its fused
                // form accept exactly the same `i`.
                if at_arity == 1 {
                    let at = self.check_expr(&args[2], env, effects)?;
                    expect_len_operand(&at, "starts_with_at offset", args[2].span.clone())?;
                }
                return Ok(Type::Primitive(Primitive::Bool));
            }
            // Set membership is its own builtin — point at it rather than
            // reporting a confusing mismatch against the `T[]` signature.
            if name == "__builtin_contains" && matches!(recv, Type::Set(_)) {
                return Err(Error::at(
                    "\"contains\" takes an array or str receiver; for set membership use \"has\"",
                    args[0].span.clone(),
                ));
            }
            // A non-str/array receiver: fall through to report the mismatch
            // against the generic `T[]` signature.
        }
        // A call *through* a function-typed binding (a lambda parameter or a
        // local bound to one): `f(x)`. Check arity and arguments against the
        // function type and yield its return type. No effect check — the Fn
        // type carries no effects; a lambda's effects are charged to the site
        // that supplies it (see `check_lambda`).
        if let Some(b) = env.get(name) {
            let Type::Fn(ptys, ret) = &b.ty else {
                return Err(Error::at(
                    format!("{name:?} is not a function and cannot be called"),
                    span.clone(),
                ));
            };
            if ptys.len() != args.len() {
                return Err(Error::at(
                    format!("{name:?} expects {} arg(s), got {}", ptys.len(), args.len()),
                    span.clone(),
                ));
            }
            for (i, (arg, pty)) in args.iter().zip(ptys).enumerate() {
                self.check_arg(
                    arg,
                    Some(pty),
                    env,
                    effects,
                    &format!("call to {name:?} arg {i}"),
                )?;
            }
            return Ok((**ret).clone());
        }

        // Effect discipline: a callee's declared effects must be covered by the
        // caller's. Builtins carry their effects in `sigs` like user functions.
        for e in self.callee_effects(name) {
            if !effects.contains(&e) {
                return Err(Error::at(
                    format!(
                        "fn {:?} has effect \"!{e}\" but the calling function does not declare it",
                        display(name)
                    ),
                    span.clone(),
                ));
            }
        }

        // From here a call resolves through a function *signature* — builtin or
        // user-defined, indistinguishable: both live in `sigs`. An unresolved
        // name that *is* a builtin almost always means a forgotten import, so
        // point at the fix.
        let Some(sig) = self.sigs.get(name) else {
            // A call through a function-valued struct field: `recv.f(rest)`
            // (stored as `f(recv, rest)`) where `name` isn't a function but
            // `recv`'s struct has a field `name` holding a function value.
            // Consulted only after function/method resolution fails, so a real
            // fn never loses to a same-named field. Type-check the remaining
            // args against the field's function type and yield its return type;
            // codegen loads the field and `call_indirect`s through it.
            if !args.is_empty() {
                if let Ok(Type::Named(sn)) = self.check_expr(&args[0], env, effects) {
                    if let Some((ptys, ret)) = self.struct_fn_field(&sn, name) {
                        let cargs = &args[1..];
                        if ptys.len() != cargs.len() {
                            return Err(Error::at(
                                format!(
                                    "field {name:?} is a function value expecting {} arg(s), got {}",
                                    ptys.len(),
                                    cargs.len()
                                ),
                                span.clone(),
                            ));
                        }
                        for (i, (arg, pty)) in cargs.iter().zip(&ptys).enumerate() {
                            self.check_arg(
                                arg,
                                Some(pty),
                                env,
                                effects,
                                &format!("call to field {name:?} arg {i}"),
                            )?;
                        }
                        return Ok(ret);
                    }
                }
            }
            let mut msg = format!("call to undefined fn {:?}", display(name));
            if aipl_syntax::IMPORTABLE_BUILTINS.contains(&name) {
                msg.push_str(&format!(
                    " — \"{name}\" is a builtin; import it with `import {{ {name} }} from builtins;`"
                ));
            } else if self.is_ctor_name(name) {
                msg.push_str(&format!(
                    " — \"{name}\" is a variant constructor that isn't in scope; import it with \
                     `import {{ {name} }} from` its defining file, or write it qualified as \
                     `Variant.{name}`"
                ));
            }
            return Err(Error::at(msg, span.clone()));
        };
        if sig.params.len() != args.len() {
            return Err(Error::at(
                format!(
                    "fn {:?} expects {} arg(s), got {}",
                    display(name),
                    sig.params.len(),
                    args.len()
                ),
                span.clone(),
            ));
        }

        if !sig.is_generic() {
            // Concrete signature: check each argument against its declared
            // parameter type (pushing the expected type into a lambda/fn-ref).
            let params = sig.params.clone();
            let mut atys = Vec::with_capacity(args.len());
            for (i, (arg, p)) in args.iter().zip(&params).enumerate() {
                let pty = &p.ty;
                if p.variadic {
                    // A variadic `T*` parameter accepts its sequence type, a
                    // single element, or an optional element — codegen
                    // normalizes whichever form to the sequence. Synthesize the
                    // argument's type, then accept any of the three shapes.
                    let aty = self.check_arg(arg, None, env, effects, "variadic argument")?;
                    if !variadic_accepts(&aty, pty) {
                        let elem = variadic_elem(pty);
                        return Err(Error::at(
                            format!(
                                "fn {:?} arg {i}: variadic parameter expects {}, {}, or {}?, got {}",
                                display(name),
                                tyname(pty),
                                tyname(&elem),
                                tyname(&elem),
                                tyname(&aty)
                            ),
                            arg.span.clone(),
                        ));
                    }
                    atys.push(aty);
                } else {
                    atys.push(self.check_arg(
                        arg,
                        Some(pty),
                        env,
                        effects,
                        &format!("fn {:?} arg {i}", display(name)),
                    )?);
                }
            }
            return Ok(self.return_ty_of(name, &atys));
        }

        // Generic signature: infer the named type variables from the non-function
        // arguments, then check each function-typed argument (a lambda or a
        // named-function value) against the *substituted* parameter type — so a
        // lambda passed to `map`/`filter`/any generic HOF is checked against the
        // concrete element type. Non-function arguments are checked by synthesis
        // only: an `any[]` parameter's element type varies per call and isn't
        // pinned here (codegen settles the concrete fit), so coercing against it
        // would be unsound. The result type is the substituted return type, with
        // any still-unresolved variable left permissive (`__unknown__`).
        let vars: HashSet<&str> = sig.type_vars.iter().map(|tp| tp.name.as_str()).collect();
        let params = sig.param_types();
        let return_ty = sig.return_type();
        let is_mutating = sig.is_mutating();
        let mut map: HashMap<String, Type> = HashMap::new();
        let mut atys: Vec<Type> = vec![Type::Unit; args.len()];
        // Pass 1: non-function arguments — type them and collect type-var bindings.
        for (i, (arg, pty)) in args.iter().zip(&params).enumerate() {
            if matches!(pty, Type::Fn(_, _)) {
                continue;
            }
            let aty = self.check_expr(arg, env, effects)?;
            self.bind_field(pty, &aty, &vars, &mut map);
            atys[i] = aty;
        }
        // Pass 2: function-typed arguments — check against the substituted type.
        for (i, (arg, pty)) in args.iter().zip(&params).enumerate() {
            if !matches!(pty, Type::Fn(_, _)) {
                continue;
            }
            let expected = subst_vars(pty, &map, &vars);
            atys[i] = self.check_arg(
                arg,
                Some(&expected),
                env,
                effects,
                &format!("fn {:?} arg {i}", display(name)),
            )?;
            // The lambda's inferred type can pin a variable that appears only in
            // this function-typed parameter — e.g. `U` in `map<T, U>(self: T[], f:
            // (T) -> U)`, learned from the lambda's body return type.
            self.bind_field(pty, &atys[i], &vars, &mut map);
        }
        // A bound-constrained type variable (e.g. `<T: ord>`) must resolve to a
        // type that satisfies its bound — the unification above is purely
        // structural and knows nothing about bounds.
        for tp in &sig.type_vars {
            if let Some(bound_ty) = map.get(&tp.name) {
                if !self.bound_satisfied(tp.bound, bound_ty) {
                    // Inside a generic body the argument is the *enclosing*
                    // function's own type variable. Saying "inferred as type
                    // parameter T" there would print the same name twice (the
                    // callee's `T` and the caller's) and explain nothing — name
                    // the enclosing variable's declared bound instead, which is
                    // the thing that has to change.
                    let detail = match typevar_name(bound_ty) {
                        Some(v) => {
                            let have = self.current_type_bounds.borrow().get(v).copied();
                            format!(
                                "but the enclosing {:?} is declared \"{}\"",
                                v,
                                have.unwrap_or(Bound::Any).name()
                            )
                        }
                        None => format!("but was inferred as {}", tyname(bound_ty)),
                    };
                    return Err(Error::at(
                        format!(
                            "fn {:?}: type parameter {:?} requires \"{}\", {}",
                            display(name),
                            tp.name,
                            tp.bound.name(),
                            detail
                        ),
                        span.clone(),
                    ));
                }
            }
        }
        // A mutating method yields its (mutated) receiver; otherwise the
        // substituted return type.
        if is_mutating {
            Ok(atys
                .into_iter()
                .next()
                .unwrap_or(Type::Primitive(Primitive::I64)))
        } else {
            // A `T[] -> T[]` signature (e.g. `reverse`) called on a `str`
            // substitutes to `char[]` (since `str` pins `T = char` — see
            // `collect_var_bindings`), not `str` — but `coerce` treats the two
            // as freely interchangeable (see its `is_char_array` rule), so no
            // per-builtin override is needed here to keep e.g. `fn f(s: str)
            // -> str { s.reverse() }` type-checking.
            let ret = subst_vars(&return_ty, &map, &vars);
            // A generic-struct/variant return (`fn wrap<T>(..) -> Box<T>`) came
            // back as a `Type::Generic`; once its arguments are concrete at the
            // call site, resolve it to the synthesized named instance so the rest
            // of the checker sees an ordinary struct. An abstract one (the call
            // is inside another generic function) stays generic.
            if mentions_typevar(&ret) {
                Ok(ret)
            } else {
                self.resolve_generic_ty(&ret)
            }
        }
    }

    /// Check one call argument against its expected parameter type (when known).
    /// A lambda argument is checked against the expected function type; any
    /// other argument is checked by synthesis. Returns the argument's type.
    fn check_arg(
        &self,
        arg: &Expr,
        expected: Option<&Type>,
        env: &Env,
        effects: &[String],
        ctx: &str,
    ) -> Result<Type, Error> {
        // The parameter type is this argument's context — the only place a bare
        // `none` or `[]` learns what it is.
        if let Some(exp) = expected {
            self.lock(arg, exp);
        }
        if let ExprKind::Lambda(params, body) = &arg.kind {
            let Some(Type::Fn(ptys, ret)) = expected else {
                return Err(Error::at(
                    "a lambda can only be passed where a function-typed parameter is expected"
                        .to_string(),
                    arg.span.clone(),
                ));
            };
            // Report the lambda's *inferred* type (actual body return), not the
            // expected one, so a generic HOF can pin an only-in-the-lambda return
            // variable (`U` in `map<T, U>`).
            let body_ty =
                self.check_lambda(params, body, ptys, ret, env, effects, arg.span.clone())?;
            return Ok(Type::Fn(ptys.clone(), Box::new(body_ty)));
        }
        // A bare function *name* passed where a function value is expected: a
        // named function (or imported builtin) used as a value. It isn't a local
        // binding, so it doesn't resolve through `env` — validate its signature.
        if let (Some(Type::Fn(ptys, ret)), ExprKind::Ident(g)) = (expected, &arg.kind) {
            if !env.contains_key(g) {
                // Report the function's *declared* return, not the expected one,
                // for the same reason the lambda branch above does: it may pin a
                // variable that appears only here (`F` in `map_err`'s `(E) -> F`).
                let actual_ret = self.check_fn_ref(g, ptys, ret, effects, arg.span.clone())?;
                return Ok(Type::Fn(ptys.clone(), Box::new(actual_ret)));
            }
        }
        let aty = self.check_expr(arg, env, effects)?;
        if let Some(e) = expected {
            // A bare literal argument flexes to a narrow-int parameter type.
            let aty = self.flex_int(arg, &aty, e)?;
            expect(&aty, e, ctx, arg.span.clone())?;
            return Ok(aty);
        }
        Ok(aty)
    }

    /// Check a lambda literal against an expected function signature: parameter
    /// count and any explicit annotations must match, and the body (checked with
    /// the lambda's parameters added to the enclosing environment, so captures
    /// resolve) must produce the expected return type. The body's effects are
    /// charged to `effects` — the enclosing function that supplies the lambda.
    fn check_lambda(
        &self,
        params: &[LambdaParam],
        body: &Expr,
        expected_params: &[Type],
        expected_ret: &Type,
        env: &Env,
        effects: &[String],
        span: Span,
    ) -> Result<Type, Error> {
        if params.len() != expected_params.len() {
            return Err(Error::at(
                format!(
                    "lambda has {} parameter(s), but {} was expected",
                    params.len(),
                    tyname(&Type::Fn(
                        expected_params.to_vec(),
                        Box::new(expected_ret.clone())
                    ))
                ),
                span.clone(),
            ));
        }
        let mut env2 = env.clone();
        for (p, pty) in params.iter().zip(expected_params) {
            if let Some(ann) = &p.ty {
                if ann != pty {
                    return Err(Error::at(
                        format!(
                            "lambda parameter {:?} is annotated {}, but {} was expected",
                            p.name,
                            tyname(ann),
                            tyname(pty)
                        ),
                        p.span.clone(),
                    ));
                }
            }
            env2.insert(
                p.name.clone(),
                Binding {
                    ty: pty.clone(),
                    mutable: false,
                },
            );
        }
        let mark = self.try_errs.borrow().len();
        let body_ty = self.check_expr(body, &env2, effects)?;
        let body_ty = {
            let errs = self.try_errs.borrow();
            err_side_from_tries(body_ty, &errs[mark..])
        };
        self.try_errs.borrow_mut().truncate(mark);
        coerce(&body_ty, expected_ret).map_err(|()| {
            Error::at(
                format!(
                    "lambda body returns {}, but {} was expected",
                    tyname(&body_ty),
                    tyname(expected_ret)
                ),
                // The value's span — see the twin in `check_fn`. A lambda whose
                // body is a single expression records none, so this is that
                // expression either way.
                body.value_span().clone(),
            )
        })?;
        // Return the body's *actual* type: a generic HOF (`map<T, U>`) infers `U`
        // from this (the expected return is often the still-unresolved `U`), so a
        // chained `xs.map(|x| ..).minimum()` knows the mapped element type.
        Ok(body_ty)
    }

    /// The signature of a function referenced *by name* as a value (passed to a
    /// higher-order function): its parameter types, return type, and effects.
    /// Resolves any function — builtin or user — through `sigs`. A *generic*
    /// function is instantiated against the `expected_params` first (e.g. the
    /// builtin `to_str<T>` passed where `(i64) -> str` is expected resolves to
    /// `(i64) -> str`), with any still-unresolved variable left permissive.
    /// `None` for an unknown name.
    fn fn_ref_sig(
        &self,
        name: &str,
        expected_params: &[Type],
    ) -> Option<(Vec<Type>, Type, Vec<String>)> {
        let sig = self.sigs.get(name)?;
        if !sig.is_generic() || sig.params.len() != expected_params.len() {
            // Concrete (or an arity mismatch the caller will report against the
            // un-substituted signature).
            return Some((sig.param_types(), sig.return_type(), sig.effects.clone()));
        }
        let vars: HashSet<&str> = sig.type_vars.iter().map(|tp| tp.name.as_str()).collect();
        let mut map: HashMap<String, Type> = HashMap::new();
        let param_types = sig.param_types();
        for (pty, ety) in param_types.iter().zip(expected_params) {
            self.bind_field(pty, ety, &vars, &mut map);
        }
        let params = param_types
            .iter()
            .map(|p| subst_vars(p, &map, &vars))
            .collect();
        let ret = subst_vars(&sig.return_type(), &map, &vars);
        Some((params, ret, sig.effects.clone()))
    }

    /// Validate that the function named `name` can be passed where a
    /// `(expected_params) -> expected_ret` value is expected: arity and types
    /// must line up (parameters contravariantly, the result covariantly), and
    /// its effects must be covered by the supplying function's `effects`.
    /// Validates `name` against the expected signature and yields its *declared*
    /// return type — which may be more specific than `expected_ret` when that is
    /// still an unresolved type variable, and is what lets a generic HOF pin a
    /// variable appearing only in this parameter's result (the named-function
    /// counterpart of the lambda case in [`Self::check_arg`]).
    fn check_fn_ref(
        &self,
        name: &str,
        expected_params: &[Type],
        expected_ret: &Type,
        effects: &[String],
        span: Span,
    ) -> Result<Type, Error> {
        let (params, ret, fx) = self.fn_ref_sig(name, expected_params).ok_or_else(|| {
            Error::at(
                format!(
                    "{:?} cannot be passed as a function value (it is not a function, \
                     or it is generic — passing generic functions is not supported)",
                    display(name)
                ),
                span.clone(),
            )
        })?;
        if params.len() != expected_params.len() {
            return Err(Error::at(
                format!(
                    "fn {:?} takes {} parameter(s), but a function taking {} is expected here",
                    display(name),
                    params.len(),
                    expected_params.len()
                ),
                span.clone(),
            ));
        }
        for (provided, declared) in expected_params.iter().zip(&params) {
            if coerce(provided, declared).is_err() {
                return Err(Error::at(
                    format!(
                        "fn {:?} expects a {} argument, but will be called with {} here",
                        display(name),
                        tyname(declared),
                        tyname(provided)
                    ),
                    span.clone(),
                ));
            }
        }
        if coerce(&ret, expected_ret).is_err() {
            return Err(Error::at(
                format!(
                    "fn {:?} returns {}, but {} is expected here",
                    display(name),
                    tyname(&ret),
                    tyname(expected_ret)
                ),
                span.clone(),
            ));
        }
        for e in &fx {
            if !effects.iter().any(|d| d == e) {
                return Err(Error::at(
                    format!(
                        "fn {:?} has effect \"!{e}\" but the calling function does not declare it",
                        display(name)
                    ),
                    span.clone(),
                ));
            }
        }
        Ok(ret)
    }

    /// The return type of a non-generic call (the generic path substitutes its
    /// own). A mutating method yields its (mutated) receiver.
    fn return_ty_of(&self, name: &str, atys: &[Type]) -> Type {
        let Some(sig) = self.sigs.get(name) else {
            return Type::Primitive(Primitive::I64);
        };
        if sig.is_mutating() {
            return atys
                .first()
                .cloned()
                .unwrap_or(Type::Primitive(Primitive::I64));
        }
        sig.return_type()
    }

    /// Flexibly retype a bare integer literal `e` (currently `ety`) to a target
    /// integer type `other` when it fits — so a literal can meet a narrow int
    /// without an explicit conversion (`i8_val + 1`, `f(200)` where `f` takes a
    /// `u8`, `fn g() -> u8 { 200 }`). A literal that doesn't fit is an error.
    /// Non-literals and non-integer targets are returned unchanged.
    ///
    /// "Literal" reaches through the value-passing constructs (a block's tail,
    /// an `if`/`match` whose arms are all literals) — see
    /// [`aipl_syntax::flex_int_values`].
    fn flex_int(&self, e: &Expr, ety: &Type, other: &Type) -> Result<Type, Error> {
        match aipl_syntax::flex_fit(e, ety, other) {
            Ok(Some(t)) => Ok(t),
            Ok(None) => Ok(ety.clone()),
            Err((v, name)) => Err(Error::at(
                format!("integer literal {v} does not fit in {name}"),
                e.span.clone(),
            )),
        }
    }

    /// Type of an integer addition — the `+` operator and the `wrapping_add` /
    /// `saturating_add` builtins it resolves to. Both operands must be the *same*
    /// integer width (convert explicitly with `i32(x)` etc.; no implicit mixing);
    /// `i64` is the common default. An unresolved generic operand stays permissive.
    /// Non-integers are rejected — `+` is integer-only (string concat is `+++`).
    /// Type of an integer add/subtract — the `+`/`-` operators and the
    /// `wrapping_*`/`saturating_*` builtins they resolve to. `op` is the spelling
    /// (`"+"` or `"-"`), used only for diagnostics. Both operands must be the same
    /// integer width; an unresolved generic operand stays permissive; non-integers
    /// are rejected (with a `+++`-concat hint for a string given to `+`).
    fn check_int_arith(
        &self,
        op: &str,
        lt: &Type,
        rt: &Type,
        lspan: Span,
        rspan: Span,
    ) -> Result<Type, Error> {
        if is_unknown(lt) || is_unknown(rt) {
            return Ok(unknown_ty());
        }
        if aipl_syntax::is_int_ty(lt) && lt == rt {
            return Ok(lt.clone());
        }
        // A string operand is the common mistake now that `+`/`-` are integer-only.
        // For `+`, point at `+++` (string concatenation).
        if is_str_repr(lt) || is_str_repr(rt) {
            let (bad, span) = if is_str_repr(lt) {
                (lt, lspan)
            } else {
                (rt, rspan)
            };
            let verb = if op == "+" { "addition" } else { "subtraction" };
            let hint = if op == "+" {
                "; use \"+++\" to concatenate strings"
            } else {
                ""
            };
            return Err(Error::at(
                format!(
                    "\"{op}\" is integer {verb}, but this operand is {}{hint}",
                    tyname(bad)
                ),
                span,
            ));
        }
        expect(
            lt,
            &Type::Primitive(Primitive::I64),
            "arithmetic operand",
            lspan,
        )?;
        expect(
            rt,
            &Type::Primitive(Primitive::I64),
            "arithmetic operand",
            rspan,
        )?;
        Ok(Type::Primitive(Primitive::I64))
    }

    fn check_binop(
        &self,
        op: BinOp,
        lt: &Type,
        rt: &Type,
        lspan: Span,
        rspan: Span,
        span: Span,
    ) -> Result<Type, Error> {
        // Arithmetic/comparison operate within a single integer type — both
        // operands must be the *same* width and signedness (convert explicitly
        // with `i32(x)` etc.; no implicit mixing). `i64` is the common default.
        let same_int = aipl_syntax::is_int_ty(lt) && lt == rt;
        match op {
            // Integer add only — the increment sugar `set n++` lowers to a
            // primitive add. A user's `+`/`-` resolves (in the loader) to a call
            // to its bound `wrapping_*`/`saturating_*`/user fn instead; those
            // calls reuse `check_int_arith` too. Concatenation is its own
            // operator, below.
            BinOp::Add => self.check_int_arith("+", lt, rt, lspan, rspan),
            // Concatenation. `Error` concatenates like `str`; the result is a
            // plain str. An unresolved generic result stays permissive.
            BinOp::Concat => {
                if is_unknown(lt) || is_unknown(rt) {
                    Ok(unknown_ty())
                } else if (is_str_repr(lt) || is_char_array(lt))
                    && (is_str_repr(rt) || is_char_array(rt))
                {
                    // `char[]` is the same representation as `str` (see
                    // `is_char_array`), and generic returns can hand back the
                    // `char[]` spelling — `join`'s `-> T[]` at `T = char` is
                    // exactly that. Accept either spelling on either side.
                    Ok(Type::Primitive(Primitive::Str))
                } else {
                    Err(Error::at(
                        "\"+++\" concatenates strings: both sides must be str".to_string(),
                        span.clone(),
                    ))
                }
            }
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if same_int {
                    return Ok(lt.clone());
                }
                expect(
                    lt,
                    &Type::Primitive(Primitive::I64),
                    "arithmetic operand",
                    lspan,
                )?;
                expect(
                    rt,
                    &Type::Primitive(Primitive::I64),
                    "arithmetic operand",
                    rspan,
                )?;
                Ok(Type::Primitive(Primitive::I64))
            }
            BinOp::Eq | BinOp::Ne => {
                // Equality works for any two values of the *same* type — scalars,
                // str, optionals, arrays, sets, structs, variants — compared
                // structurally (sets order-independently). The two sides must be
                // the same type up to the usual `none`/empty-collection coercion
                // (so `x == none`, `xs == []`, `s == #{}` are allowed), and an
                // unresolved generic result stays permissive. Function values
                // have no runtime identity, so they're rejected.
                if matches!(lt, Type::Fn(_, _)) || matches!(rt, Type::Fn(_, _)) {
                    return Err(Error::at(
                        format!(
                            "{:?} is not supported for function values",
                            binop_spelling(op)
                        ),
                        span.clone(),
                    ));
                }
                let comparable = is_unknown(lt)
                    || is_unknown(rt)
                    || coerce(lt, rt).is_ok()
                    || coerce(rt, lt).is_ok();
                if !comparable {
                    return Err(Error::at(
                        format!(
                            "{:?} between {} and {}: both sides must be the same type",
                            binop_spelling(op),
                            tyname(lt),
                            tyname(rt)
                        ),
                        span.clone(),
                    ));
                }
                Ok(Type::Primitive(Primitive::Bool))
            }
            BinOp::And | BinOp::Or => {
                expect(
                    lt,
                    &Type::Primitive(Primitive::Bool),
                    "logical operand",
                    lspan,
                )?;
                expect(
                    rt,
                    &Type::Primitive(Primitive::Bool),
                    "logical operand",
                    rspan,
                )?;
                Ok(Type::Primitive(Primitive::Bool))
            }
            // Ordering comparisons: same-int operands → bool. Spelled out
            // rather than left to a `_`, so a new operator has to decide here
            // instead of silently being typed as a comparison.
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                if same_int {
                    return Ok(Type::Primitive(Primitive::Bool));
                }
                // `str` orders lexicographically by bytes (see `aipl_str_cmp`),
                // matching the order `sort` imposes on a `str[]`.
                if is_str_repr(lt) && is_str_repr(rt) {
                    return Ok(Type::Primitive(Primitive::Bool));
                }
                expect(
                    lt,
                    &Type::Primitive(Primitive::I64),
                    "comparison operand",
                    lspan,
                )?;
                expect(
                    rt,
                    &Type::Primitive(Primitive::I64),
                    "comparison operand",
                    rspan,
                )?;
                Ok(Type::Primitive(Primitive::Bool))
            }
            // The loader lowers `++` to an add before the checker runs.
            BinOp::Incr => unreachable!("`++` is lowered to `+` by the loader"),
        }
    }
}

/// Strip the internal `__builtin_` prefix for diagnostics.
fn display(name: &str) -> &str {
    name.strip_prefix("__builtin_").unwrap_or(name)
}

/// Strip a single internal name-mangling prefix for display: the reserved
/// `__builtin_`, or the loader's per-file module prefix `__m<index>__` (added to
/// every non-root file's top-level names). Neither can appear in a user-written
/// identifier, so this only ever strips compiler-internal decoration.
fn strip_mangle_prefix(s: &str) -> &str {
    let s = s.strip_prefix("__builtin_").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("__m") {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with("__") {
            return &rest[digits + 2..];
        }
    }
    s
}

/// Render a (possibly mangled) named type for diagnostics. Strips the module /
/// `__builtin_` prefixes (see [`strip_mangle_prefix`]) and turns a generic
/// instance's mangled name back into source-like form: `Box$i64` → `Box<i64>`,
/// `Pair$i64$str` → `Pair<i64, str>`, a synthetic tuple `__tuple$i64$str` →
/// `(i64, str)`. Nested instances were already flattened to `_` when mangled
/// (see [`mangle_type`]), so those don't fully round-trip, but the common
/// single-level case reads cleanly.
fn demangle_named(n: &str) -> String {
    let mut parts = n.split('$');
    let base = strip_mangle_prefix(parts.next().unwrap_or(n));
    let args: Vec<&str> = parts.map(strip_mangle_prefix).collect();
    if args.is_empty() {
        base.to_string()
    } else if base == "__tuple" {
        format!("({})", args.join(", "))
    } else {
        format!("{base}<{}>", args.join(", "))
    }
}

/// A type the checker can't pin down (e.g. a generic call's type-variable
/// result that we don't instantiate here). It coerces with anything, so the
/// checker stays permissive rather than reporting a false mismatch.
fn unknown_ty() -> Type {
    Type::Named("__unknown__".to_string())
}

fn is_unknown(t: &Type) -> bool {
    matches!(t, Type::Named(n) if n == "__unknown__")
}

/// An abstract type variable in a generic body. Unlike `__unknown__` it is *not*
/// a wildcard: it coerces only with itself, so the structural rules still bite
/// (a `T` doesn't fit an `i64`, you can't `+`/`<`/`*` two `T`s — `T: any` makes
/// no such promise) while `==`, container ops, binding, and `return T` work.
/// The sentinel carries the variable's *name* (`__typevar__$T`) so a bound can
/// still be looked up at an inner call site — without it a generic body knows
/// only "some type parameter" and no bound can ever be satisfied (see
/// `bound_satisfied`). An anonymous `any` has no name and gets the bare prefix.
/// Whether an expression's type comes from where it *sits* rather than from what
/// it says — so moving it (inlining, folding) would change its meaning unless the
/// type was recorded first. See [`Expr::ty`].
///
/// Deliberately the same set the inliner used to refuse to move, plus generic
/// constructions: `none` and the empty collection literals carry only a
/// placeholder element type, `ok`/`err` leave their *other* side a placeholder,
/// and a generic constructor whose arguments don't pin every variable takes them
/// from the expected type.
fn needs_lock(k: &ExprKind) -> bool {
    match k {
        ExprKind::None => true,
        ExprKind::ArrayLit(v) | ExprKind::SetLit(v) => v.is_empty(),
        ExprKind::DictLit(v) => v.is_empty(),
        ExprKind::Call(n, _, _) => n == "ok" || n == "err" || n == "some",
        _ => false,
    }
}

/// Whether `t` still contains one of the checker's placeholder types — a type
/// that means "decided by context", which is exactly what must not be recorded
/// as an answer.
fn mentions_placeholder(t: &Type) -> bool {
    match t {
        Type::NoneInner | Type::EmptyArrayArg | Type::NoneLiteralArg | Type::Any => true,
        Type::Optional(i) | Type::Array(i) | Type::Set(i) => mentions_placeholder(i),
        Type::Dict(k, v) => mentions_placeholder(k) || mentions_placeholder(v),
        Type::Result(a, b) => mentions_placeholder(a) || mentions_placeholder(b),
        Type::Fn(ps, r) => ps.iter().any(mentions_placeholder) || mentions_placeholder(r),
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(mentions_placeholder),
        _ => false,
    }
}

fn typevar_ty(name: &str) -> Type {
    Type::TypeVar(name.to_string())
}

const TYPEVAR: &str = "__typevar__$";

fn is_typevar(t: &Type) -> bool {
    matches!(t, Type::TypeVar(_))
}

/// The variable a typevar sentinel came from, or `None` for an anonymous `any`.
fn typevar_name(t: &Type) -> Option<&str> {
    match t {
        Type::TypeVar(n) => Some(n.as_str()).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// Whether `t` contains the abstract `__typevar__` sentinel anywhere — i.e. it's
/// not fully concrete. Used to decide whether a generic instantiation can be
/// pinned to a synthetic named instance now, or must stay a `Type::Generic` (an
/// abstract application inside a generic function, resolved at monomorphization).
fn mentions_typevar(t: &Type) -> bool {
    match t {
        Type::TypeVar(_) => true,
        Type::Optional(i) | Type::Array(i) | Type::Set(i) => mentions_typevar(i),
        Type::Dict(k, v) => mentions_typevar(k) || mentions_typevar(v),
        Type::Result(a, b) => mentions_typevar(a) || mentions_typevar(b),
        Type::Fn(ps, r) => ps.iter().any(mentions_typevar) || mentions_typevar(r),
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(mentions_typevar),
        Type::Unit
        | Type::Primitive(_)
        | Type::Named(_)
        | Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => false,
    }
}

/// Valid element of an array literal: a scalar, `str`, a nested array, an
/// optional (`T?[]`), or an (abstract) type variable — never a struct.
/// `none`/`__unknown__` are accepted (they coerce). Used in body position.
fn is_valid_elem(t: &Type) -> bool {
    is_array_elem(t)
        || is_none_inner(t)
        || is_unknown(t)
        || is_typevar(t)
        || matches!(t, Type::Optional(_))
}

/// Replace every type variable in `t` — a declared `<T>` (in `type_params`) or
/// an anonymous `any` — with the abstract `__typevar__`, recursing through
/// arrays and optionals. Lets a generic body be checked abstractly: concrete
/// structure is preserved (`i64`, `str`, `T[]` → `__typevar__[]`) while the bare
/// type variables coerce only with themselves. Identity for a concrete signature.
fn subst_typevars(t: &Type, type_params: &[String]) -> Type {
    match t {
        Type::Any => typevar_ty(""),
        // A declared parameter is already a `TypeVar` (`promote_type_vars`, at
        // the end of parsing); `type_params` now only decides which variables
        // belong to *this* signature, and every variable in a body does.
        Type::TypeVar(n) if n.is_empty() || type_params.iter().any(|p| p == n) => t.clone(),
        Type::TypeVar(_)
        | Type::Primitive(_)
        | Type::Named(_)
        | Type::Unit
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => t.clone(),
        Type::Array(inner) => Type::Array(Box::new(subst_typevars(inner, type_params))),
        Type::Set(inner) => Type::Set(Box::new(subst_typevars(inner, type_params))),
        Type::Dict(k, v) => Type::Dict(
            Box::new(subst_typevars(k, type_params)),
            Box::new(subst_typevars(v, type_params)),
        ),
        Type::Optional(inner) => Type::Optional(Box::new(subst_typevars(inner, type_params))),
        Type::Result(ok, err) => Type::Result(
            Box::new(subst_typevars(ok, type_params)),
            Box::new(subst_typevars(err, type_params)),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|p| subst_typevars(p, type_params))
                .collect(),
            Box::new(subst_typevars(ret, type_params)),
        ),
        Type::Tuple(elems) => Type::Tuple(
            elems
                .iter()
                .map(|e| subst_typevars(e, type_params))
                .collect(),
        ),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter()
                .map(|a| subst_typevars(a, type_params))
                .collect(),
        ),
    }
}

/// Like `type_name`, but renders the checker's internal sentinels as human
/// phrases instead of leaking them: the abstract `__typevar__` as "a type
/// parameter", and the unresolved-generic `__unknown__` wildcard as `_`.
/// Recurses so a sentinel nested in a function/array/optional type is rendered
/// too (e.g. an inferred `(i64) -> _` from a partly-resolved generic).
fn tyname(t: &Type) -> String {
    match t {
        // The sentinel carries its variable's name (see `typevar_ty`); name it in
        // the message rather than leaking the mangling, and stay generic for an
        // anonymous `any`, which has no name to give.
        Type::TypeVar(_) => match typevar_name(t) {
            Some(v) => format!("type parameter {v:?}"),
            None => "a type parameter".to_string(),
        },
        Type::Optional(inner) if is_typevar(inner) => "an optional type parameter".to_string(),
        Type::Array(inner) if is_typevar(inner) => "an array of a type parameter".to_string(),
        Type::Set(inner) if is_typevar(inner) => "a set of a type parameter".to_string(),
        Type::Named(n) if n == "__unknown__" => "_".to_string(),
        // A builtin type (`Span`), a per-file name (`__m1__LexError`), or a
        // generic instance (`Token$AiplTok`) carries internal mangling — render
        // it back to source-like form for diagnostics.
        Type::Named(n) => demangle_named(n),
        Type::Optional(inner) => format!("{}?", tyname(inner)),
        Type::Array(inner) => format!("{}[]", tyname(inner)),
        Type::Set(inner) => format!("#{{{}}}", tyname(inner)),
        Type::Dict(k, v) => format!("#{{{}: {}}}", tyname(k), tyname(v)),
        Type::Result(ok, err) => format!("{}!{}", tyname(ok), tyname(err)),
        Type::Fn(params, ret) => {
            let ps = params.iter().map(tyname).collect::<Vec<_>>().join(", ");
            format!("({ps}) -> {}", tyname(ret))
        }
        _ => type_name(t),
    }
}

fn is_unit(t: &Type) -> bool {
    *t == Type::Unit
}

/// The element type `T` of a variadic parameter's sequence type — the inverse of
/// the parser's `T → seq(T)` mapping: `str → char` (an AIPL string is the char
/// sequence), `T[] → T`.
fn variadic_elem(seq: &Type) -> Type {
    match seq {
        Type::Primitive(Primitive::Str) => Type::Primitive(Primitive::Char),
        Type::Array(e) => (**e).clone(),
        // The parser only builds `str` / `T[]` sequence types; fall back to the
        // seq itself so acceptance still type-checks for any stray shape.
        other => other.clone(),
    }
}

/// Whether `arg` is acceptable for a variadic parameter whose sequence type is
/// `seq`: the sequence itself, a single element, or an optional element.
fn variadic_accepts(arg: &Type, seq: &Type) -> bool {
    let elem = variadic_elem(seq);
    coerce(arg, seq).is_ok()
        || coerce(arg, &elem).is_ok()
        || coerce(arg, &Type::Optional(Box::new(elem))).is_ok()
}

/// `actual` fits `expected`, applying the same `none`/empty-array coercions as
/// codegen's `expect_type`. `__unknown__` (an unresolved generic result) fits
/// anything.
/// Whether `e` is a literal usable as an array-pattern element: a scalar/string
/// literal, or a nested array literal of such. Restricting patterns to literals
/// keeps them self-contained — no bindings, free variables, or calls — so the
/// loader/mono/codegen consumers can treat a `Pattern::Array` as inert data.
fn is_pattern_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Char(_) | ExprKind::Str(_) => true,
        ExprKind::ArrayLit(elems) => elems.iter().all(is_pattern_literal),
        _ => false,
    }
}

/// Whether `t` is `char[]`. This element specialization shares `str`'s
/// runtime representation entirely (see `is_char_array` in `aipl-codegen`,
/// its codegen-side counterpart) — a `char` is a single byte and `str`'s
/// content is just packed bytes — which is what makes the `coerce` rule below
/// sound: an actual `char[]` value and an actual `str` value are
/// bit-identical, so treating the two types as freely interchangeable never
/// mismatches a value's real layout.
pub(crate) fn is_char_array(t: &Type) -> bool {
    matches!(t, Type::Array(inner) if **inner == Type::Primitive(Primitive::Char))
}

fn coerce(actual: &Type, expected: &Type) -> Result<(), ()> {
    if actual == expected || is_unknown(actual) || is_unknown(expected) {
        return Ok(());
    }
    // Type variables coerce with each other regardless of which variable they
    // came from. Naming the sentinel (see `typevar_ty`) would otherwise have
    // silently *tightened* this — `T` and `U` used to erase to one value and so
    // compared equal above. Distinguishing them is a separate change with its
    // own fanout; this keeps the naming purely additive.
    if is_typevar(actual) && is_typevar(expected) {
        return Ok(());
    }
    // A bare `none` / empty `[]` carries the placeholder element `__none__`,
    // which fits any element type in either direction — and at any depth: peel
    // matching optional/array layers and apply the same rule to the cores, so
    // e.g. `some(some(none))` (`__none__???`) fits `i64???`.
    if is_none_inner(actual) || is_none_inner(expected) {
        return Ok(());
    }
    // `Error` is `str` under the hood (for now), so the two coerce freely either
    // way: a string error message makes an `Error`, and an `Error` is usable
    // anywhere a `str` is (e.g. `print(e)`).
    if (is_error(actual) && *expected == Type::Primitive(Primitive::Str))
        || (*actual == Type::Primitive(Primitive::Str) && is_error(expected))
    {
        return Ok(());
    }
    // `str` functions as an alias of `char[]` (see `is_char_array`): a generic
    // `T[]`-shaped builtin (e.g. `reverse`) called on a `str` unifies `T =
    // char` (see `collect_var_bindings`) and its substituted `char[]` return
    // type must still be usable as `str` — and, symmetrically, a real
    // `char[]` value is usable wherever `str` is expected.
    if (is_char_array(actual) && *expected == Type::Primitive(Primitive::Str))
        || (*actual == Type::Primitive(Primitive::Str) && is_char_array(expected))
    {
        return Ok(());
    }
    match (actual, expected) {
        (Type::Optional(a), Type::Optional(b)) => coerce(a, b),
        (Type::Array(a), Type::Array(b)) => coerce(a, b),
        (Type::Set(a), Type::Set(b)) => coerce(a, b),
        (Type::Dict(ak, av), Type::Dict(bk, bv)) => coerce(ak, bk).and_then(|()| coerce(av, bv)),
        (Type::Result(ao, ae), Type::Result(bo, be)) => {
            coerce(ao, bo).and_then(|()| coerce(ae, be))
        }
        _ => Err(()),
    }
}

/// A result captured in a binding must actually be read somewhere in the
/// binding's body — leaving it unused would silently drop its error, exactly
/// like a bare discard. (Reassigning the binding doesn't count as a read: only
/// `Ident` references do, which is what `count_ident` tallies.) `span.clone()` points at
/// the bound value.
fn check_result_inspected(name: &str, vt: &Type, body: &Expr, span: Span) -> Result<(), Error> {
    if matches!(vt, Type::Result(_, _)) && crate::count_ident(name, body) == 0 {
        return Err(Error::at(
            format!(
                "the result bound to {name:?} is never used, ignoring its possible error; \
                 inspect it with `match` or `?`"
            ),
            span.clone(),
        ));
    }
    Ok(())
}

fn expect(actual: &Type, expected: &Type, ctx: &str, span: Span) -> Result<(), Error> {
    coerce(actual, expected)
        .map_err(|()| Error::at(mismatch_msg(actual, expected, ctx), span.clone()))
}

/// "expected X, got Y" — and, when the two render *identically*, enough extra to
/// tell them apart.
///
/// Distinct types can share a rendering: an unimported `Span` is the unknown
/// `Named("Span")` while the builtin is `Named("__builtin_Span")`, and both
/// display as `Span`; a written `Rule<Tok>` and its instance `Rule$Tok` do the
/// same. Reporting "expected Span, got Span" says a type is not itself, which
/// sends the reader looking for the wrong problem — so fall back to the internal
/// names, which are ugly but at least differ.
fn mismatch_msg(actual: &Type, expected: &Type, ctx: &str) -> String {
    let (e, a) = (tyname(expected), tyname(actual));
    if e != a {
        return format!("{ctx}: expected {e}, got {a}");
    }
    format!(
        "{ctx}: expected {e}, got a different type that is also written {a:?} \
         (internally {expected:?} vs {actual:?}) — most often a builtin type used \
         without importing it"
    )
}

/// A length-like operand — an index, a slice bound, a `Span` field, or a
/// capacity — accepted as `i64` *or* `u64`. Both signednesses are accepted
/// because the two natural sources disagree — an integer literal or a loop
/// counter is `i64`, while `len()` is `u64` — and requiring one would force a
/// conversion on every `xs[xs.len() - 1]` or every `xs[i]`. Codegen clamps
/// bounds to `[0, len]` regardless, so a negative `i64` and a huge `u64` are
/// already handled identically; the width is the same 64-bit register either
/// way, so nothing is lost by accepting both. Any narrower integer still needs
/// an explicit conversion, as before.
fn expect_len_operand(actual: &Type, ctx: &str, span: Span) -> Result<(), Error> {
    if matches!(
        actual,
        Type::Primitive(Primitive::I64) | Type::Primitive(Primitive::U64)
    ) {
        return Ok(());
    }
    expect(actual, &Type::Primitive(Primitive::I64), ctx, span)
}

/// Merge two branch/arm types with the same coercions (permissive). A
/// `__none__` element on either side takes the other's, recursively through
/// matching optional/array layers.
fn merge(a: Type, b: Type) -> Type {
    if a == b || is_none_inner(&a) {
        return b;
    }
    if is_none_inner(&b) {
        return a;
    }
    match (&a, &b) {
        (Type::Optional(x), Type::Optional(y)) => {
            Type::Optional(Box::new(merge((**x).clone(), (**y).clone())))
        }
        (Type::Array(x), Type::Array(y)) => {
            Type::Array(Box::new(merge((**x).clone(), (**y).clone())))
        }
        (Type::Set(x), Type::Set(y)) => Type::Set(Box::new(merge((**x).clone(), (**y).clone()))),
        (Type::Dict(xk, xv), Type::Dict(yk, yv)) => Type::Dict(
            Box::new(merge((**xk).clone(), (**yk).clone())),
            Box::new(merge((**xv).clone(), (**yv).clone())),
        ),
        (Type::Result(xo, xe), Type::Result(yo, ye)) => Type::Result(
            Box::new(merge((**xo).clone(), (**yo).clone())),
            Box::new(merge((**xe).clone(), (**ye).clone())),
        ),
        _ => a,
    }
}

/// Permissively bind the type variables in `vars` that appear in `param_ty`, by
/// matching it structurally against `arg_ty`. Best-effort: unmatched structure
/// and conflicts are ignored (the first binding wins), keeping the checker
/// permissive and leaving the concrete fit to codegen.
/// Whether `name` is a synthesized generic-*variant* instance (e.g. `Opt$i64`):
/// its base (before the first `$`) is a generic-variant template. `$` can't
/// appear in a source name, so this reliably distinguishes instances from
/// ordinary variants.
pub(crate) fn is_variant_instance(
    name: &str,
    generic_variants: &HashMap<String, VariantDecl>,
) -> bool {
    name.contains('$')
        && name
            .split('$')
            .next()
            .is_some_and(|base| generic_variants.contains_key(base))
}

/// Map a template's type variables to an application's arguments (positionally).
pub(crate) fn zip_type_args(
    type_vars: &[aipl_syntax::ast::TypeParam],
    args: &[Type],
) -> HashMap<String, Type> {
    type_vars
        .iter()
        .map(|tv| tv.name.clone())
        .zip(args.iter().cloned())
        .collect()
}

/// Every type variable in declaration order, resolved from `map`, or `None` if
/// any is still unbound.
pub(crate) fn collect_args(
    type_vars: &[aipl_syntax::ast::TypeParam],
    map: &HashMap<String, Type>,
) -> Option<Vec<Type>> {
    type_vars
        .iter()
        .map(|tv| map.get(&tv.name).cloned())
        .collect()
}

/// Whether `arg` is a bare integer literal landing in a slot that is *exactly* a
/// type variable — the shape whose width the literal must not be the one to
/// decide.
///
/// The same bow-out [`crate::Mono::instantiate_types`] makes for a generic call,
/// applied to a generic constructor's payload. A literal types as `i64` but
/// flexes to whatever its context wants, so binding it here would pin the
/// variable to `i64` against no competition at all: `let b: Box<u8> = Full(200)`
/// resolved to a `Box<i64>` and was then rejected against its own annotation.
/// Deferring it lets the expected type have first refusal, and the literal still
/// pins any variable nothing else reached.
///
/// Only the bare-variable slot qualifies. A `T[]` or `T?` payload never receives
/// a bare literal, and a concrete payload (`Spelling(str)`) pins nothing anyway.
pub(crate) fn literal_pins_nothing(arg: &Expr, pty: &Type, vars: &HashSet<&str>) -> bool {
    matches!(pty, Type::TypeVar(v) if vars.contains(v.as_str()))
        && aipl_syntax::const_int(arg).is_some()
}

pub(crate) fn collect_var_bindings(
    param_ty: &Type,
    arg_ty: &Type,
    vars: &HashSet<&str>,
    map: &mut HashMap<String, Type>,
) {
    match (param_ty, arg_ty) {
        (Type::TypeVar(v), a) if vars.contains(v.as_str()) => {
            map.entry(v.clone()).or_insert_with(|| a.clone());
        }
        // A bare `none`/empty `[]` argument carries no element type (`__none__`),
        // so it can't pin the variable — leave it for another argument to fix.
        (Type::Array(p), Type::Array(a)) if !is_none_inner(a) => {
            collect_var_bindings(p, a, vars, map)
        }
        // `str` is usable as `char[]` — pin the element variable to `char`.
        (Type::Array(p), Type::Primitive(Primitive::Str)) => {
            collect_var_bindings(p, &Type::Primitive(Primitive::Char), vars, map)
        }
        (Type::Set(p), Type::Set(a)) if !is_none_inner(a) => collect_var_bindings(p, a, vars, map),
        (Type::Dict(pk, pv), Type::Dict(ak, av)) => {
            // Bind from whichever side carries concrete structure; an empty
            // `#{:}` has `__none__` key/value and pins nothing.
            if !is_none_inner(ak) {
                collect_var_bindings(pk, ak, vars, map);
            }
            if !is_none_inner(av) {
                collect_var_bindings(pv, av, vars, map);
            }
        }
        (Type::Optional(p), Type::Optional(a)) if !is_none_inner(a) => {
            collect_var_bindings(p, a, vars, map)
        }
        (Type::Result(po, pe), Type::Result(ao, ae)) => {
            // Bind from whichever side carries concrete structure; an `ok`/`err`
            // pins one side and leaves the other `__none__`.
            if !is_none_inner(ao) {
                collect_var_bindings(po, ao, vars, map);
            }
            if !is_none_inner(ae) {
                collect_var_bindings(pe, ae, vars, map);
            }
        }
        (Type::Fn(ps, pr), Type::Fn(as_, ar)) => {
            for (p, a) in ps.iter().zip(as_) {
                collect_var_bindings(p, a, vars, map);
            }
            collect_var_bindings(pr, ar, vars, map);
        }
        _ => {}
    }
}

/// Substitute the type variables in `vars` within `t`: a bound variable becomes
/// its inferred type, an *un*bound one becomes the permissive `__unknown__`
/// wildcard (so an only-partly-inferred signature still type-checks). Names not
/// in `vars` (concrete types, anonymous `any`) are left as-is.
fn subst_vars(t: &Type, map: &HashMap<String, Type>, vars: &HashSet<&str>) -> Type {
    match t {
        Type::TypeVar(v) if vars.contains(v.as_str()) => {
            map.get(v).cloned().unwrap_or_else(unknown_ty)
        }
        Type::Primitive(_)
        | Type::Named(_)
        | Type::TypeVar(_)
        | Type::Unit
        | Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => t.clone(),
        Type::Array(inner) => Type::Array(Box::new(subst_vars(inner, map, vars))),
        Type::Set(inner) => Type::Set(Box::new(subst_vars(inner, map, vars))),
        Type::Dict(k, v) => Type::Dict(
            Box::new(subst_vars(k, map, vars)),
            Box::new(subst_vars(v, map, vars)),
        ),
        Type::Optional(inner) => Type::Optional(Box::new(subst_vars(inner, map, vars))),
        Type::Result(ok, err) => Type::Result(
            Box::new(subst_vars(ok, map, vars)),
            Box::new(subst_vars(err, map, vars)),
        ),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter().map(|p| subst_vars(p, map, vars)).collect(),
            Box::new(subst_vars(r, map, vars)),
        ),
        Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| subst_vars(e, map, vars)).collect()),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter().map(|a| subst_vars(a, map, vars)).collect(),
        ),
    }
}
