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

use aipl_syntax::ast::{
    Expr, ExprKind, Function, Item, LambdaParam, MatchArm, Pattern, Primitive, Program, Signature,
    Type,
};
use aipl_syntax::{
    is_array_elem, is_dict_key, is_error, is_none_inner, is_set_elem, is_str_repr, none_inner_ty,
    type_name, Error, Span,
};

/// Generate the canonical synthetic-struct name for a tuple with the given
/// element types. Matches the name produced by mono's `lower_tuples`.
pub(crate) fn tuple_struct_name(elems: &[Type]) -> String {
    fn mangle(ty: &Type) -> String {
        match ty {
            Type::Unit => panic!("Tuple members cannot be unit"),
            // A tuple type is parsed straight from source syntax (`(A, B)`);
            // these are compiler-internal pseudo-types that never appear there.
            Type::Any
            | Type::NoneInner
            | Type::EmptyArrayArg
            | Type::NoneLiteralArg
            | Type::ConcatStr => {
                panic!("Tuple members cannot be a compiler pseudo-type")
            }
            Type::Primitive(p) => p.name().into(),
            Type::Named(n) => n.replace(['$', '!'], "_"),
            Type::Array(e) => format!("arr_{}", mangle(e)),
            Type::Optional(e) => format!("opt_{}", mangle(e)),
            Type::Set(e) => format!("set_{}", mangle(e)),
            Type::Dict(k, v) => format!("dict_{}_{}", mangle(k), mangle(v)),
            Type::Result(ok, err) => format!("res_{}_{}", mangle(ok), mangle(err)),
            Type::Fn(ps, ret) => {
                let args = ps.iter().map(mangle).collect::<Vec<_>>().join("_");
                format!("fn_{}_{}", args, mangle(ret))
            }
            Type::Tuple(es) => {
                format!(
                    "tuple_{}",
                    es.iter().map(mangle).collect::<Vec<_>>().join("_")
                )
            }
        }
    }
    format!(
        "__tuple${}",
        elems.iter().map(mangle).collect::<Vec<_>>().join("$")
    )
}

/// Effects the language recognizes. `prints` = writes to stdout; `read_files` =
/// reads from the filesystem; `write_files` = writes to the filesystem;
/// `execute_program` = spawns a child process.
const KNOWN_EFFECTS: &[&str] = &["prints", "read_files", "write_files", "execute_program"];

/// A bound name's type and whether it's reassignable (`let mut` / `mut self`).
#[derive(Clone)]
struct Binding {
    ty: Type,
    mutable: bool,
}

type Env = HashMap<String, Binding>;

struct Cx<'a> {
    structs: &'a HashMap<String, Vec<(String, Type, bool)>>,
    /// Synthetic struct layouts created on-the-fly when a `TupleLit` is seen
    /// during checking. Looked up alongside `structs` by `struct_fields`.
    syn_structs: std::cell::RefCell<HashMap<String, Vec<(String, Type, bool)>>>,
    /// Variant (sum) types: name → ordered cases `(ctor, payload types)`.
    variants: &'a HashMap<String, Vec<(String, Vec<Type>)>>,
    /// Constructor name → the variant it belongs to (for typing `Ctor(..)`).
    ctors: &'a HashMap<String, String>,
    sigs: &'a HashMap<String, Signature>,
    /// The declared return type of the function currently being checked (with
    /// type-vars substituted), so a `return value;` can be checked against it.
    /// Functions are top-level (never nested), so a single slot suffices.
    current_ret: std::cell::RefCell<Type>,
}

impl<'a> Cx<'a> {
    fn struct_fields(&self, name: &str) -> Option<Vec<(String, Type, bool)>> {
        self.structs
            .get(name)
            .cloned()
            .or_else(|| self.syn_structs.borrow().get(name).cloned())
    }
    fn has_struct(&self, name: &str) -> bool {
        self.structs.contains_key(name) || self.syn_structs.borrow().contains_key(name)
    }
    fn add_syn_struct(&self, name: String, fields: Vec<(String, Type, bool)>) {
        self.syn_structs.borrow_mut().insert(name, fields);
    }
}

/// Type-check `program`. Returns the first error found, or `Ok` if every
/// function is well-formed.
pub fn check(program: &Program) -> Result<(), Error> {
    // struct name → [(field_name, field_type, has_default)]
    let mut structs: HashMap<String, Vec<(String, Type, bool)>> = HashMap::new();
    let mut variants: HashMap<String, Vec<(String, Vec<Type>)>> = HashMap::new();
    let mut ctors: HashMap<String, String> = HashMap::new();
    let mut sigs: HashMap<String, Signature> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Struct(s) => {
                structs.insert(
                    s.name.clone(),
                    s.fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone(), f.default.is_some()))
                        .collect(),
                );
            }
            Item::Variant(v) => {
                for c in &v.cases {
                    if ctors.insert(c.name.clone(), v.name.clone()).is_some() {
                        return Err(Error::msg(format!(
                            "duplicate variant constructor {:?}",
                            c.name
                        )));
                    }
                }
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

    let cx = Cx {
        structs: &structs,
        syn_structs: std::cell::RefCell::new(HashMap::new()),
        variants: &variants,
        ctors: &ctors,
        sigs: &sigs,
        current_ret: std::cell::RefCell::new(Type::Unit),
    };
    // Type-check struct field defaults in an empty environment (defaults are
    // evaluated at construction time with no local variables in scope).
    for item in &program.items {
        if let Item::Struct(s) = item {
            for f in &s.fields {
                if let Some(default) = &f.default {
                    let dt = cx.check_expr(default, &HashMap::new(), &[])?;
                    expect(
                        &dt,
                        &f.ty,
                        &format!("default for struct {:?} field {:?}", s.name, f.name),
                        default.span.clone(),
                    )?;
                }
            }
        }
    }
    for item in &program.items {
        if let Item::Fn(f) = item {
            cx.check_fn(f)?;
        }
    }
    Ok(())
}

/// During checking, these types stand in for an as-yet-unknown scalar: the
/// `any` constraint, the bare-`none` inner marker, and an in-scope generic
/// type parameter. They're permitted wherever a concrete scalar primitive is
/// (set elements, dict keys, etc.).
fn is_abstract_scalar_ty(t: &Type, type_params: &[String]) -> bool {
    matches!(t, Type::Any | Type::NoneInner)
        || matches!(t, Type::Named(n) if type_params.iter().any(|tp| tp == n))
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
            self.check_ty(&p.ty, &type_var_names, &f.name)?;
        }
        if let Some(rt) = &f.sig.return_ty {
            if matches!(rt, Type::Fn(_, _)) {
                return Err(Error::msg(format!(
                    "fn {:?}: cannot return a function value ({})",
                    f.name,
                    tyname(rt)
                )));
            }
            self.check_ty(rt, &type_var_names, &f.name)?;
        }

        // Keyword-parameter defaults are checked like struct field defaults: in
        // an empty environment (a default can't reference other parameters —
        // it is spliced into *call sites*, where none are in scope). Effects
        // are the function's own declared set: every caller must cover the
        // callee's effects anyway, so a spliced default's effects are covered
        // wherever it lands. The expected type gets the same type-variable
        // substitution as the body check, so a generic function stays checkable.
        for p in &f.sig.params {
            if let Some(default) = &p.default {
                let dt = self.check_expr(default, &HashMap::new(), &f.sig.effects)?;
                let pty = subst_typevars(&p.ty, &type_var_names);
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
        // body (functions are top-level, so this single slot can't nest).
        *self.current_ret.borrow_mut() = declared.clone();
        let body_ty = self.check_expr(&f.body, &env, &f.sig.effects)?;
        // A bare-literal body flexes to a narrow-int return type (`fn g() -> u8
        // { 200 }`).
        let body_ty = self.flex_int(&f.body, &body_ty, &declared)?;
        coerce(&body_ty, &declared).map_err(|()| {
            Error::at(
                format!(
                    "fn {:?}: body returns {}, but the declared return type is {}",
                    f.name,
                    tyname(&body_ty),
                    tyname(&declared)
                ),
                f.body.span.clone(),
            )
        })
    }

    /// Validate that `t` names only known types (primitives, declared structs,
    /// in-scope type parameters) in valid positions.
    fn check_ty(&self, t: &Type, type_params: &[String], fname: &str) -> Result<(), Error> {
        match t {
            // Every primitive is a valid type in any general position.
            Type::Unit | Type::Primitive(_) => Ok(()),
            // The anonymous generic bound is valid anywhere a type-parameter
            // name is (that's what it desugars to during monomorphization).
            Type::Any => Ok(()),
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
                    let mut msg = format!("fn {fname:?}: unknown type {n:?}");
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
                self.check_elem_ty(inner, type_params, fname)
            }
            // A set element: a scalar (i64/bool/char), `str`, or a type
            // parameter (pinned to one of those when monomorphized). No nested
            // containers, no struct/variant.
            Type::Set(inner) => {
                if is_set_elem(inner) || is_abstract_scalar_ty(inner, type_params) {
                    Ok(())
                } else {
                    Err(Error::msg(format!(
                        "fn {fname:?}: a set element must be i64, bool, char, or str, got {}",
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
                        "fn {fname:?}: a dict key must be i64, bool, char, or str, got {}",
                        tyname(k)
                    )));
                }
                self.check_elem_ty(v, type_params, fname)
            }
            // A result `T!E`: the Ok and Err payloads are scalar/`str`/a
            // struct (or a type parameter pinned to a scalar). Variants aren't
            // supported as a payload yet. The Ok side may also be unit — a
            // void-result `!E` whose success carries no value.
            Type::Result(ok, err) => {
                let payload_ok = |p: &Type| {
                    is_set_elem(p) // i64/bool/char/str
                        || is_error(p)
                        || is_abstract_scalar_ty(p, type_params)
                        || matches!(p, Type::Named(n) if self.has_struct(n))
                };
                if !payload_ok(ok) && !is_unit(ok) {
                    return Err(Error::msg(format!(
                        "fn {fname:?}: a result Ok payload must be i64, bool, char, str, a struct, or unit (\"!E\"), got {}",
                        tyname(ok)
                    )));
                }
                if !payload_ok(err) {
                    return Err(Error::msg(format!(
                        "fn {fname:?}: a result Err payload must be i64, bool, char, str, Error, or a struct, got {}",
                        tyname(err)
                    )));
                }
                Ok(())
            }
            // A function type (a lambda parameter): validate its argument and
            // return types. `check_fn` separately forbids it as a *return* type.
            Type::Fn(params, ret) => {
                for p in params {
                    self.check_ty(p, type_params, fname)?;
                }
                self.check_ty(ret, type_params, fname)
            }
            // Tuple types are lowered to Named by lower_tuples before check
            // runs, but handle them permissively in case one arrives.
            Type::Tuple(elems) => {
                for e in elems {
                    self.check_ty(e, type_params, fname)?;
                }
                Ok(())
            }
        }
    }

    fn check_elem_ty(&self, t: &Type, type_params: &[String], fname: &str) -> Result<(), Error> {
        match t {
            Type::Unit => Err(Error::msg("() is not allowed as an array/option element")),
            // A scalar primitive element: i64/bool/char/str are stored; the
            // narrow integer widths aren't stored in composites yet (only as
            // scalar values).
            Type::Primitive(p) => {
                if matches!(
                    p,
                    Primitive::I64 | Primitive::Bool | Primitive::Char | Primitive::Str
                ) {
                    Ok(())
                } else {
                    Err(Error::msg(format!(
                        "fn {fname:?}: {} is not yet supported as an array/optional element \
                         (only as a scalar value)",
                        p.name()
                    )))
                }
            }
            // The anonymous generic bound and the bare-`none`/empty-container
            // marker are abstract scalars — always a valid element.
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
                    Err(Error::msg(format!("fn {fname:?}: unknown type {n:?}")))
                }
            }
            // Nested arrays (`T[][]`) and nested optionals (`T??`) are allowed.
            Type::Array(inner) | Type::Optional(inner) => {
                self.check_elem_ty(inner, type_params, fname)
            }
            // A set/dict/result can't (yet) be an array/optional element (or a
            // dict value) — they're not nestable in other containers in v1.
            Type::Set(_) | Type::Dict(_, _) | Type::Result(_, _) => Err(Error::msg(format!(
                "fn {fname:?}: a set, dict, or result cannot be an array, optional, or dict element"
            ))),
            Type::Fn(_, _) => Err(Error::msg(format!(
                "fn {fname:?}: arrays and optionals cannot contain function types"
            ))),
            Type::Tuple(_) => Err(Error::msg(format!(
                "fn {fname:?}: tuple types cannot be array or optional elements"
            ))),
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
        // A `str` scrutinee matches string-literal arms (`"foo" => ...`) and a
        // wildcard (`_ => ...`); neither binds anything. No constructor patterns.
        if is_str_repr(st) {
            return match &arm.pattern {
                Pattern::Str(_) | Pattern::Wildcard => Ok(vec![]),
                Pattern::Ctor { .. } | Pattern::Array(_) => Err(Error::at(
                    "\"match\" on a str expects string literals or `_`".to_string(),
                    arm.span.clone(),
                )),
            };
        }
        // An array scrutinee matches array-literal arms (`[e0, ...] => ...`) and a
        // wildcard; neither binds anything. Each element must be a literal whose
        // type matches the scrutinee's element type. No constructor patterns.
        if let Type::Array(elem) = st {
            return match &arm.pattern {
                Pattern::Array(elems) => {
                    for e in elems {
                        if !is_pattern_literal(e) {
                            return Err(Error::at(
                                "array-pattern elements must be literals".to_string(),
                                e.span.clone(),
                            ));
                        }
                        let et = self.check_expr(e, env, effects)?;
                        expect(&et, elem, "array-pattern element", e.span.clone())?;
                    }
                    Ok(vec![])
                }
                Pattern::Wildcard => Ok(vec![]),
                Pattern::Ctor { .. } | Pattern::Str(_) => Err(Error::at(
                    "\"match\" on an array expects array literals or `_`".to_string(),
                    arm.span.clone(),
                )),
            };
        }
        // The non-constructor patterns only apply to a `str` / array scrutinee.
        let (name, bindings) = match &arm.pattern {
            Pattern::Ctor { name, bindings } => (name, bindings),
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
        let payload: Vec<Type> = match st {
            Type::Optional(inner) => match name.as_str() {
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
            Type::Result(ok, err) => match name.as_str() {
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
            Type::Named(n) if self.variants.contains_key(n) => match self.case_payload(n, name) {
                Some(p) => p.to_vec(),
                None => {
                    return Err(Error::at(
                        format!("{n} has no constructor {name:?}"),
                        arm.span.clone(),
                    ))
                }
            },
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
        if bindings.len() != payload.len() {
            return Err(Error::at(
                format!(
                    "constructor {name:?} binds {} value(s), but {} given",
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
        if is_str_repr(st) || matches!(st, Type::Array(_)) {
            let noun = if is_str_repr(st) { "a str" } else { "an array" };
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
            Type::Named(n) if self.variants.contains_key(n) => {
                self.variants[n].iter().map(|(c, _)| c.clone()).collect()
            }
            // A non-matchable scrutinee already errored in `match_arm_bindings`.
            _ => return Ok(()),
        };
        let mut seen: HashSet<&str> = HashSet::new();
        for arm in arms {
            // Non-constructor patterns already errored in `match_arm_bindings`.
            let Pattern::Ctor { name, .. } = &arm.pattern else {
                continue;
            };
            if !seen.insert(name.as_str()) {
                return Err(Error::at(
                    format!("duplicate \"{name}\" arm"),
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

    /// Check `expr` and return its type. `effects` is the enclosing function's
    /// declared effect set (callees must not exceed it).
    fn check_expr(&self, expr: &Expr, env: &Env, effects: &[String]) -> Result<Type, Error> {
        let span = expr.span.clone();
        Ok(match &expr.kind {
            ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
            ExprKind::Unit => Type::Unit,
            ExprKind::Num(_) => Type::Primitive(Primitive::I64),
            ExprKind::Bool(_) => Type::Primitive(Primitive::Bool),
            ExprKind::Str(_) => Type::Primitive(Primitive::Str),
            ExprKind::Char(_) => Type::Primitive(Primitive::Char),
            ExprKind::None => Type::Optional(Box::new(none_inner_ty())),
            ExprKind::Ident(name) => {
                // A local binding shadows everything; otherwise a bare name may
                // be a nullary variant constructor (e.g. `Empty`).
                if let Some(b) = env.get(name) {
                    b.ty.clone()
                } else if let Some(vn) = self.ctors.get(name) {
                    self.expect_nullary_ctor(name, vn, span.clone())?;
                    Type::Named(vn.clone())
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
                let tt = self.check_expr(t, env, effects)?;
                let et = self.check_expr(e, env, effects)?;
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
                let ft = self.check_expr(first, env, effects)?;
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
                self.check_expr(rest, env, effects)?
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
            // A lambda is only valid as a call argument, where the expected
            // function type is known (`check_call` handles it). Anywhere else
            // there's no type to infer its parameters against.
            ExprKind::Lambda(_, _) => {
                return Err(Error::at(
                    "a lambda is only valid as an argument to a function with a matching \
                     function-typed parameter"
                        .to_string(),
                    span.clone(),
                ));
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
            ExprKind::Let(name, val, body) => {
                let vt = self.check_expr(val, env, effects)?;
                if is_unit(&vt) {
                    return Err(Error::at(
                        format!("cannot bind {name:?} to a value of type ()"),
                        val.span.clone(),
                    ));
                }
                check_result_inspected(name, &vt, body, val.span.clone())?;
                let mut env2 = env.clone();
                env2.insert(
                    name.clone(),
                    Binding {
                        ty: vt,
                        mutable: false,
                    },
                );
                self.check_expr(body, &env2, effects)?
            }
            ExprKind::LetMut(name, val, body) => {
                let vt = self.check_expr(val, env, effects)?;
                if is_unit(&vt) {
                    return Err(Error::at(
                        format!("cannot bind {name:?} to a value of type ()"),
                        val.span.clone(),
                    ));
                }
                check_result_inspected(name, &vt, body, val.span.clone())?;
                let mut env2 = env.clone();
                env2.insert(
                    name.clone(),
                    Binding {
                        ty: vt,
                        mutable: true,
                    },
                );
                self.check_expr(body, &env2, effects)?
            }
            ExprKind::Assign(name, val, body) => {
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
                let expected = binding.ty.clone();
                let vt = self.check_expr(val, env, effects)?;
                expect(&vt, &expected, "set", val.span.clone())?;
                self.check_expr(body, env, effects)?
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
                self.check_expr(body, &env2, effects)?;
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
                self.check_expr(body, env, effects)?;
                Type::Primitive(Primitive::I64)
            }
            ExprKind::ArrayLit(elems) => {
                let mut elem_ty = none_inner_ty();
                for (i, e) in elems.iter().enumerate() {
                    let t = self.check_expr(e, env, effects)?;
                    if i == 0 {
                        elem_ty = t;
                    }
                }
                // A struct or variant element is valid too (must be declared).
                let elem_ok = is_valid_elem(&elem_ty)
                    || matches!(&elem_ty, Type::Named(n)
                        if self.has_struct(n) || self.variants.contains_key(n));
                if !elems.is_empty() && !elem_ok {
                    return Err(Error::at(
                        format!(
                            "array elements must be i64, bool, char, str, an array, an optional, \
                             or a struct, got {}",
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
                let mut elem_ty = none_inner_ty();
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
                            "set elements must be i64, bool, char, or str, got {}",
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
                let mut key_ty = none_inner_ty();
                let mut val_ty = none_inner_ty();
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
                                "dict keys must be i64, bool, char, or str, got {}",
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
                                "dict values must be i64, bool, char, str, an array, an optional, \
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
                expect(
                    &it,
                    &Type::Primitive(Primitive::I64),
                    "array index",
                    idx.span.clone(),
                )?;
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
                expect(
                    &ot,
                    &Type::Primitive(Primitive::Str),
                    "slice receiver",
                    obj.span.clone(),
                )?;
                let st = self.check_expr(start, env, effects)?;
                expect(
                    &st,
                    &Type::Primitive(Primitive::I64),
                    "slice start",
                    start.span.clone(),
                )?;
                // An open-ended `recv[start..]` has no end expression — it runs to
                // the receiver's length.
                if let Some(end) = end {
                    let et = self.check_expr(end, env, effects)?;
                    expect(
                        &et,
                        &Type::Primitive(Primitive::I64),
                        "slice end",
                        end.span.clone(),
                    )?;
                }
                Type::Primitive(Primitive::Str)
            }
            ExprKind::Try(inner) => {
                // `expr?` requires a result `T!E` and yields the Ok type `T`. The
                // constraint that the enclosing fn returns `_!E` (so the
                // early-returned Err fits) is enforced in codegen, where the
                // return type is in scope.
                let it = self.check_expr(inner, env, effects)?;
                match it {
                    Type::Result(ok, _) => (*ok).clone(),
                    other => {
                        return Err(Error::at(
                            format!("\"?\" requires a result (T!E), got {}", tyname(&other)),
                            span.clone(),
                        ));
                    }
                }
            }
            ExprKind::Field(obj, fname) => {
                let ot = self.check_expr(obj, env, effects)?;
                let Type::Named(sn) = &ot else {
                    return Err(Error::at(
                        format!("field access on non-struct value of type {}", tyname(&ot)),
                        obj.span.clone(),
                    ));
                };
                let fields = self.struct_fields(sn).ok_or_else(|| {
                    Error::at(
                        format!("field access on non-struct value of type {}", display(sn)),
                        obj.span.clone(),
                    )
                })?;
                fields
                    .iter()
                    .find(|(n, _, _)| n == fname)
                    .map(|(_, t, _)| t.clone())
                    .ok_or_else(|| {
                        Error::at(
                            format!("struct {:?} has no field {fname:?}", display(sn)),
                            span.clone(),
                        )
                    })?
            }
            ExprKind::Construct(name, inits) => {
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
                    expect(
                        &vt,
                        expected,
                        &format!("struct {:?} field {:?}", display(name), fi.name),
                        fi.value.span.clone(),
                    )?;
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
                let mut merged: Option<Type> = None;
                for arm in arms {
                    let bind_tys =
                        self.match_arm_bindings(&st, arm, scrut.span.clone(), env, effects)?;
                    let mut env2 = env.clone();
                    for (name, ty) in arm.pattern.bindings().iter().zip(bind_tys) {
                        env2.insert(name.clone(), Binding { ty, mutable: false });
                    }
                    let t = self.check_expr(&arm.body, &env2, effects)?;
                    merged = Some(match merged {
                        None => t,
                        Some(prev) => merge(prev, t),
                    });
                }
                self.check_match_exhaustive(&st, arms, span.clone())?;
                merged.unwrap_or(Type::Primitive(Primitive::I64))
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
                self.check_call(name, args, env, effects, span.clone())?
            }
        })
    }

    fn check_call(
        &self,
        name: &str,
        args: &[Expr],
        env: &Env,
        effects: &[String],
        span: Span,
    ) -> Result<Type, Error> {
        // A variant constructor `Ctor(a, b, ...)` (unless shadowed by a local
        // function-typed binding, handled below): check each argument against
        // the case's payload type; the result is the variant type.
        if !env.contains_key(name) {
            if let Some(vn) = self.ctors.get(name) {
                let payload = self.case_payload(vn, name).unwrap_or(&[]).to_vec();
                if args.len() != payload.len() {
                    return Err(Error::at(
                        format!(
                            "constructor {name:?} expects {} argument(s), got {}",
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
                        &format!("constructor {name:?} argument"),
                        arg.span.clone(),
                    )?;
                }
                return Ok(Type::Named(vn.clone()));
            }
        }
        // Integer conversion builtins `i8(x)`/`i32(x)`/`u64(x)`/… — like the
        // result/optional constructors, these are special-cased (not imported)
        // and reserved. They convert any integer to the named width (wrapping /
        // sign- or zero-extending), so the result type is the named type.
        if !env.contains_key(name) && aipl_syntax::int_bits(name).is_some() {
            if args.len() != 1 {
                return Err(Error::at(
                    format!("{name:?} conversion expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let at = self.check_expr(&args[0], env, effects)?;
            if !aipl_syntax::is_int_ty(&at) {
                return Err(Error::at(
                    format!(
                        "{name:?} converts an integer, but its argument is {}",
                        tyname(&at)
                    ),
                    args[0].span.clone(),
                ));
            }
            // `int_bits` matched, so the name is a known integer primitive.
            return Ok(Type::Primitive(
                Primitive::from_name(name).expect("integer conversion name is a primitive"),
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
                    Box::new(none_inner_ty()),
                ));
            }
            if args.len() != 1 {
                return Err(Error::at(
                    format!("{name:?} expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let t = self.check_expr(&args[0], env, effects)?;
            let none = || Box::new(none_inner_ty());
            return Ok(if name == "ok" {
                Type::Result(Box::new(t), none())
            } else {
                Type::Result(none(), Box::new(t))
            });
        }
        // `s.len()` / `len(s)` on a set, dict, or string: the builtin `len`
        // signature is `(self: T[]) -> i64`, which doesn't unify with `#{T}` /
        // `#{K: V}` / `str`, so dispatch those receivers here. (An array receiver
        // falls through to the generic signature below.) For a string `len` is the
        // byte length.
        if name == "__builtin_len" && args.len() == 1 {
            let t = self.check_expr(&args[0], env, effects)?;
            if matches!(t, Type::Set(_) | Type::Dict(_, _)) || is_str_repr(&t) {
                return Ok(Type::Primitive(Primitive::I64));
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
        // `s.starts_with(p)` / `s.ends_with(p)`: the pattern is variadic, so it
        // accepts the sequence, a single element, or an optional element. A
        // `str` receiver takes a `char*` pattern (a `str`, a `char`, or a
        // `char?`); a `T[]` receiver takes a `T*` pattern (a `T[]`, a `T`, or a
        // `T?`). Fully dispatched here rather than through the generic signature.
        if (name == "__builtin_starts_with" || name == "__builtin_ends_with") && args.len() == 2 {
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
                return Ok(Type::Primitive(Primitive::Bool));
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
            let mut msg = format!("call to undefined fn {:?}", display(name));
            if aipl_syntax::IMPORTABLE_BUILTINS.contains(&name) {
                msg.push_str(&format!(
                    " — \"{name}\" is a builtin; import it with `import {{ {name} }} from builtins;`"
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
            collect_var_bindings(pty, &aty, &vars, &mut map);
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
            collect_var_bindings(pty, &atys[i], &vars, &mut map);
        }
        // A bound-constrained type variable (e.g. `<T: ord>`) must resolve to a
        // type that satisfies its bound — the unification above is purely
        // structural and knows nothing about bounds.
        for tp in &sig.type_vars {
            if let Some(bound_ty) = map.get(&tp.name) {
                if !tp.bound.accepts(bound_ty) {
                    return Err(Error::at(
                        format!(
                            "fn {:?}: type parameter {:?} requires \"{}\", but was inferred as {}",
                            display(name),
                            tp.name,
                            tp.bound.name(),
                            tyname(bound_ty)
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
        } else if name == "__builtin_enumerate" {
            // The return type `(i64, T)[]` is lowered to `Named("__tuple$i64$T")[]` by
            // lower_tuples. `subst_vars` can't substitute `T` inside a mangled struct name,
            // so we compute the concrete tuple name directly from the element type.
            let elem_ty = match atys.first() {
                Some(Type::Array(inner)) => *inner.clone(),
                Some(Type::Primitive(Primitive::Str)) => Type::Primitive(Primitive::Char),
                _ => map.get("T").cloned().unwrap_or_else(unknown_ty),
            };
            let tuple_name = tuple_struct_name(&[Type::Primitive(Primitive::I64), elem_ty.clone()]);
            // Register the concrete struct so field access on the return value type-checks.
            if !self.has_struct(&tuple_name) {
                self.add_syn_struct(
                    tuple_name.clone(),
                    vec![
                        ("_0".to_string(), Type::Primitive(Primitive::I64), false),
                        ("_1".to_string(), elem_ty, false),
                    ],
                );
            }
            Ok(Type::Array(Box::new(Type::Named(tuple_name))))
        } else {
            // A `T[] -> T[]` signature (e.g. `reverse`) called on a `str`
            // substitutes to `char[]` (since `str` pins `T = char` — see
            // `collect_var_bindings`), not `str` — but `coerce` treats the two
            // as freely interchangeable (see its `is_char_array` rule), so no
            // per-builtin override is needed here to keep e.g. `fn f(s: str)
            // -> str { s.reverse() }` type-checking.
            Ok(subst_vars(&return_ty, &map, &vars))
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
                self.check_fn_ref(g, ptys, ret, effects, arg.span.clone())?;
                return Ok(Type::Fn(ptys.clone(), ret.clone()));
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
        let body_ty = self.check_expr(body, &env2, effects)?;
        coerce(&body_ty, expected_ret).map_err(|()| {
            Error::at(
                format!(
                    "lambda body returns {}, but {} was expected",
                    tyname(&body_ty),
                    tyname(expected_ret)
                ),
                body.span.clone(),
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
            collect_var_bindings(pty, ety, &vars, &mut map);
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
    fn check_fn_ref(
        &self,
        name: &str,
        expected_params: &[Type],
        expected_ret: &Type,
        effects: &[String],
        span: Span,
    ) -> Result<(), Error> {
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
        Ok(())
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
    fn flex_int(&self, e: &Expr, ety: &Type, other: &Type) -> Result<Type, Error> {
        if let Type::Primitive(p) = other {
            if p.is_int() && ety != other {
                if let Some(v) = aipl_syntax::const_int(e) {
                    if aipl_syntax::int_fits(v, p.name()) {
                        return Ok(other.clone());
                    }
                    return Err(Error::at(
                        format!("integer literal {v} does not fit in {}", p.name()),
                        e.span.clone(),
                    ));
                }
            }
        }
        Ok(ety.clone())
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
        op: char,
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
            // `+` is integer add only — the increment sugar `set n++` lowers to a
            // primitive `+`. User `+`/`-` resolve (in the loader) to a call to their
            // bound `wrapping_*`/`saturating_*`/user fn instead; those calls reuse
            // `check_int_arith` too. String concatenation is `+++` (`'C'`).
            '+' => self.check_int_arith("+", lt, rt, lspan, rspan),
            // `+++` — string concatenation. `Error` concatenates like `str`; the
            // result is a plain str. An unresolved generic result stays permissive.
            'C' => {
                if is_unknown(lt) || is_unknown(rt) {
                    Ok(unknown_ty())
                } else if is_str_repr(lt) && is_str_repr(rt) {
                    Ok(Type::Primitive(Primitive::Str))
                } else {
                    Err(Error::at(
                        "\"+++\" concatenates strings: both sides must be str".to_string(),
                        span.clone(),
                    ))
                }
            }
            '-' | '*' | '/' | '%' => {
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
            'E' | 'N' => {
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
                            if op == 'E' { "==" } else { "!=" }
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
                            if op == 'E' { "==" } else { "!=" },
                            tyname(lt),
                            tyname(rt)
                        ),
                        span.clone(),
                    ));
                }
                Ok(Type::Primitive(Primitive::Bool))
            }
            'A' | 'O' => {
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
            // Ordering comparisons (`<`, `>`, `<=`, `>=`): same-int operands → bool.
            _ => {
                if same_int {
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
        }
    }
}

/// Strip the internal `__builtin_` prefix for diagnostics.
fn display(name: &str) -> &str {
    name.strip_prefix("__builtin_").unwrap_or(name)
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
fn typevar_ty() -> Type {
    Type::Named("__typevar__".to_string())
}

fn is_typevar(t: &Type) -> bool {
    matches!(t, Type::Named(n) if n == "__typevar__")
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
        Type::Any => typevar_ty(),
        Type::Named(n) if type_params.iter().any(|p| p == n) => typevar_ty(),
        Type::Primitive(_)
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
    }
}

/// Like `type_name`, but renders the checker's internal sentinels as human
/// phrases instead of leaking them: the abstract `__typevar__` as "a type
/// parameter", and the unresolved-generic `__unknown__` wildcard as `_`.
/// Recurses so a sentinel nested in a function/array/optional type is rendered
/// too (e.g. an inferred `(i64) -> _` from a partly-resolved generic).
fn tyname(t: &Type) -> String {
    match t {
        Type::Named(n) if n == "__typevar__" => "a type parameter".to_string(),
        Type::Optional(inner) if is_typevar(inner) => "an optional type parameter".to_string(),
        Type::Array(inner) if is_typevar(inner) => "an array of a type parameter".to_string(),
        Type::Set(inner) if is_typevar(inner) => "a set of a type parameter".to_string(),
        Type::Named(n) if n == "__unknown__" => "_".to_string(),
        // A builtin type (e.g. `Span`) is internally named with the reserved
        // `__builtin_` prefix (see `display`) so a user's own type can never
        // collide with it — strip it back off for diagnostics.
        Type::Named(n) => display(n).to_string(),
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
fn is_char_array(t: &Type) -> bool {
    matches!(t, Type::Array(inner) if **inner == Type::Primitive(Primitive::Char))
}

fn coerce(actual: &Type, expected: &Type) -> Result<(), ()> {
    if actual == expected || is_unknown(actual) || is_unknown(expected) {
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
    coerce(actual, expected).map_err(|()| {
        Error::at(
            format!(
                "{ctx}: expected {}, got {}",
                tyname(expected),
                tyname(actual)
            ),
            span.clone(),
        )
    })
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
fn collect_var_bindings(
    param_ty: &Type,
    arg_ty: &Type,
    vars: &HashSet<&str>,
    map: &mut HashMap<String, Type>,
) {
    match (param_ty, arg_ty) {
        (Type::Named(v), a) if vars.contains(v.as_str()) => {
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
        Type::Named(v) if vars.contains(v.as_str()) => {
            map.get(v).cloned().unwrap_or_else(unknown_ty)
        }
        Type::Primitive(_)
        | Type::Named(_)
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
    }
}
