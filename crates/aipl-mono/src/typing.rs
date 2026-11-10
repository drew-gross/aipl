//! What the pinner (`pin.rs`) and monomorphization (`lib.rs`) share about types: how a type is
//! named and mangled, which type coerces to which, whether a written type is allowed to appear
//! where it was written, the rule that an expression which yields a value may not also assign, and
//! the signatures of a few builtins whose receiver changes the shape of their parameters.

use std::collections::{HashMap, HashSet};

use aipl_syntax::ast;
use aipl_syntax::ast::{
    BinOp, Callee, Exclusion, Expr, ExprKind, MatchArm, Pattern, Primitive, Signature, StructDecl,
    Type, VariantDecl,
};
use aipl_syntax::{
    binop_spelling, is_dict_key, is_error, is_set_elem, type_name, Error, Span, SPAN_TYPE,
};

/// Mangle a type into a fragment usable inside a synthetic struct/variant name
/// (all `$`/`!`/nesting flattened to `_`). Shared by [`tuple_struct_name`] and
/// [`generic_instance_name`] so the two naming schemes agree on how a type is
/// spelled.
pub(crate) fn mangle_type(ty: &Type) -> String {
    match ty {
        Type::Case(v) => format!("case_{}", mangle_type(v)),
        Type::Unit => panic!("Synthetic-type members cannot be unit"),
        // Tuple/generic members are parsed straight from source syntax; these
        // are compiler-internal pseudo-types that never appear there.
        Type::Any | Type::ConcatStr => {
            panic!("Synthetic-type members cannot be a compiler pseudo-type")
        }
        // Reachable, unlike the pseudo-types above: a generic tuple instantiated
        // with a variable nothing pinned mangles its synthetic struct name
        // through here (`tests/cases/tuples/generic_tuple.aipl` — measured with
        // a `panic!` in this arm). So it keeps the spelling the old
        // `Named("__unknown__")` sentinel mangled to, for the same reason
        // `TypeVar` below keeps its own: an instance name generated before and
        // after this split has to be the same name.
        Type::Unknown => "__unknown__".into(),
        Type::Primitive(p) => p.name().into(),
        Type::Named(n) => n.replace(['$', '!'], "_"),
        // Spelled exactly as the old `Named("__typevar__$T")` sentinel mangled,
        // so instance names generated before and after this split agree.
        Type::TypeVar(v) => format!("{}{v}", TYPEVAR.replace('$', "_")),
        Type::Array(e) => format!("arr_{}", mangle_type(e)),
        // A refinement shares its base's layout, so a synthetic type holding
        // one is the synthetic type holding the base.
        Type::Without(base, _) => mangle_type(base),
        Type::Optional(e) => format!("opt_{}", mangle_type(e)),
        Type::Set(e, o) => format!("{}_{}", set_mangle(*o), mangle_type(e)),
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
    // A tuple reached through the generic path lands on the same struct as one
    // written concretely, or the same tuple type would be two incompatible
    // structs — `(Rule<Tok>, Rule<Tok>)` against `(Rule_Tok, Rule_Tok)`, which is
    // exactly what a generic function handing a tuple to a concrete caller
    // produces otherwise. The generic base carries its arity (`__tuple2`) so
    // each arity can have a template of its own; the *instance* is named by its
    // element types alone, which is what `tuple_struct_name` already spells.
    if base.starts_with(TUPLE_TEMPLATE) {
        return tuple_struct_name(args);
    }
    format!(
        "{base}${}",
        args.iter().map(mangle_type).collect::<Vec<_>>().join("$")
    )
}

/// Prefix of the synthetic *template* a tuple with abstract elements lowers to:
/// `__tuple2`, `__tuple3`, … one per arity. A tuple whose element types are all
/// concrete skips this and is lowered straight to its instance
/// ([`tuple_struct_name`]); only one that mentions a type variable needs a
/// template for monomorphization to instantiate.
///
/// Deliberately a prefix of neither more nor less than `__tuple`, so that
/// [`generic_instance_name`] can recognize it and hand back the very name the
/// concrete route produces.
pub(crate) const TUPLE_TEMPLATE: &str = "__tuple";

/// The diagnostic for a call to `callee`, which declares `effect`, from a function
/// that declares `caller_effects` and not that one.
///
/// Two cases are not the general discipline but a consequence of what `!measure`
/// is. The effect is internal, declared by `measure` and nothing a program can
/// reach except from a `.benchmark` block, whose synthesized function declares
/// it *and no other effect*. So a missing `!measure` means a call to `measure`
/// from anywhere else, and a caller that has `!measure` is a `.benchmark`
/// block that reached for an effect — which is not supported there yet.
pub(crate) fn undeclared_effect_message(
    callee: &str,
    effect: &str,
    caller_effects: &[String],
) -> String {
    if effect == "measure" {
        format!("fn {callee:?} can only be called from a `.benchmark` block")
    } else if caller_effects.iter().any(|e| e == "measure") {
        format!(
            "effects are not supported in a `.benchmark` block yet: fn {callee:?} has effect \"!{effect}\""
        )
    } else {
        format!(
            "fn {callee:?} has effect \"!{effect}\" but the calling function does not declare it"
        )
    }
}

/// Whether an expression's value is used by whatever encloses it.
///
/// Every construct that holds a block — `match`, `if`, `if let`, `shim` — is
/// *either* a statement or an expression, and this is the half of that decision
/// its position makes:
///
/// - in [`Pos::Value`] it is an **expression**, and its blocks may not assign
///   to anything declared outside it ([`no_outer_assign`]), so that reading it
///   is enough to know what it does;
/// - in [`Pos::Discard`] it is a **statement**, and may assign freely.
///
/// `match` carries the rule twice over, because its arms also say which kind it
/// is: arms that produce a value make it an expression whose value must be used,
/// arms that produce none make it a statement that must sit where its value is
/// discarded. The practical effect is that a `match` run purely for effect is
/// written `match (x) { .. };` — with the semicolon that makes it a statement,
/// and the same semicolon is what makes a mutating `if` or `shim` one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pos {
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
        ExprKind::For(var, index, iter, rest) => outer_assign(iter, declared).or_else(|| {
            let mut inner = declared.clone();
            inner.insert(var.clone());
            if let Some(i) = index {
                inner.insert(i.clone());
            }
            outer_assign(rest, &inner)
        }),
        ExprKind::Match(scrut, arms) => outer_assign(scrut, declared).or_else(|| {
            arms.iter().find_map(|a| {
                let mut inner = declared.clone();
                inner.extend(a.pattern.bindings());
                outer_assign(&a.body, &inner)
            })
        }),
        ExprKind::IfLet(arm, scrut, else_b) => outer_assign(scrut, declared)
            .or_else(|| {
                let mut inner = declared.clone();
                inner.extend(arm.pattern.bindings());
                outer_assign(&arm.body, &inner)
            })
            .or_else(|| outer_assign(else_b, declared)),
        // A lambda body is a separate function; its captures are checked there.
        ExprKind::Lambda(_, _) => None,
        _ => crate::children(body)
            .into_iter()
            .find_map(|c| outer_assign(c, declared)),
    }
}

/// A block-holding construct, as the "may not also mutate" diagnostic names it.
///
/// `if let` is a [`ValueForm::If`]: the keyword a reader sees is `if`, and it is
/// made a statement the same way.
pub(crate) enum ValueForm {
    Match,
    If,
    Shim,
}

impl ValueForm {
    /// What to call the block a hit sits in, the construct's keyword, and how
    /// *this* construct is written as a statement — which is the form a `set`
    /// belongs in. Each reaches statement position its own way, and naming the
    /// way is most of what the diagnostic is worth.
    fn words(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            ValueForm::Match => ("arm", "match", "Make every arm a statement"),
            ValueForm::If => ("branch", "if", "Make every branch a statement"),
            ValueForm::Shim => ("body", "shim", "Make the body a statement"),
        }
    }
}

/// Reject an assignment in `body` reaching a binding declared outside it.
///
/// This is the half of the statement-or-expression rule that applies to a
/// construct used as an *expression* (see [`Pos`]): it produces a value, so it
/// may not also mutate, and `let v = <construct>` therefore tells you its whole
/// effect. A construct run for effect is written as a statement instead, where a
/// reader expects effects.
///
/// `declared` is what the block itself binds before its first statement — a
/// pattern's binders, or nothing — and [`outer_assign`] grows it as it walks, so
/// a block driving its own `mut` to compute its value is untouched.
pub(crate) fn no_outer_assign(
    body: &Expr,
    declared: &HashSet<String>,
    form: ValueForm,
) -> Result<(), Error> {
    let (part, kw, make_stmt) = form.words();
    match outer_assign(body, declared) {
        None => Ok(()),
        Some((name, at)) => Err(Error::at(
            format!(
                "this {part} assigns to {name:?}, which is declared outside the `{kw}` — an \
                 expression `{kw}` produces a value and may not also mutate. {make_stmt} \
                 (ending the `{kw}` with \";\"), or lift the assignment out and use the \
                 `{kw}`'s value"
            ),
            at,
        )),
    }
}

/// Validates the types a program writes against the declarations it makes.
struct TypeValidator<'a> {
    /// Struct name → its fields `(name, type, has default)`.
    structs: &'a HashMap<String, Vec<(String, Type, bool)>>,
    /// Variant (sum) types: name → ordered cases `(ctor, payload types)`.
    variants: &'a HashMap<String, Vec<(String, Vec<Type>)>>,
    /// Generic struct templates by name (`Box` → its `StructDecl`).
    generic_structs: &'a HashMap<String, StructDecl>,
    /// Generic variant templates by name (`Opt` → its `VariantDecl`).
    generic_variants: &'a HashMap<String, VariantDecl>,
}

impl<'a> TypeValidator<'a> {
    fn struct_fields(&self, name: &str) -> Option<Vec<(String, Type, bool)>> {
        self.structs.get(name).cloned()
    }
    fn has_struct(&self, name: &str) -> bool {
        self.structs.contains_key(name)
    }

    /// The cases of a variant-typed value: a concrete named variant, or an
    /// (abstract) generic-variant application `Emit<K>` (template cases with its
    /// type variables substituted). Lets a `match` on such a value resolve.
    /// Whether `t` can be a set's element: a scalar or `str`, or a struct or
    /// variant, whose elements are told apart by its structural `==`.
    fn is_set_member(&self, t: &Type) -> bool {
        is_set_elem(t)
            || match t {
                Type::Named(n) => {
                    self.struct_fields(n).is_some() || self.variant_cases(n).is_some()
                }
                Type::Generic(base, _) => {
                    self.generic_structs.contains_key(base)
                        || self.generic_variants.contains_key(base)
                }
                _ => false,
            }
    }

    fn cases_of(&self, ty: &Type) -> Option<Vec<(String, Vec<Type>)>> {
        match ty {
            // `V without C` has the cases of `V` less `C`. This is what makes a
            // `match` on it need no arm for `C` and refuse one.
            Type::Without(base, Exclusion::NamedCase(excluded)) => {
                let mut cases = self.cases_of(base)?;
                cases.retain(|(c, _)| c != excluded);
                Some(cases)
            }
            Type::Without(base, Exclusion::EmptyArray) => self.cases_of(base),
            Type::Named(n) => self.variant_cases(n),
            Type::Generic(base, args) => {
                let tmpl = self.generic_variants.get(base)?;
                let map = zip_type_args(&tmpl.type_vars, args);
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
    /// Cases of variant `name` (a source variant or a synthesized generic
    /// instance).
    fn variant_cases(&self, name: &str) -> Option<Vec<(String, Vec<Type>)>> {
        self.variants.get(name).cloned()
    }
}

/// The refusal of a default on the type parameter of a declaration other than a
/// function: only a call has an inference for a default to stand in for.
pub(crate) fn defaults_only_on_functions(kind: &str, name: &aipl_syntax::SpanStr) -> Error {
    Error::at(
        format!(
            "{kind} {:?}: a type parameter's default is for a function, where a call can leave \
             the type for inference; a {kind}'s type arguments are always written",
            name.text
        ),
        name.span.clone(),
    )
}

/// Tag `e` with the file the function named `name` came from, so the renderer
/// points its caret into *that* source rather than whatever the caller happens
/// to hold.
///
/// This is the post-load half of the loader's `tag_origin`. The loader tags the
/// diagnostics it raises while reading each file, but checking runs on one
/// merged program, where the only surviving trace of a function's origin is the
/// `__m<N>__` its name was mangled with. A span from an imported file rendered
/// against the entry file's source lands at a meaningless offset — far past the
/// end for a short file, and, worse, at a plausible but wrong line for a long
/// one.
///
/// Leaves the error alone when the name carries no file index (a root-file item,
/// which the caller already renders correctly, or a builtin declaration) or the
/// program has no source map (a single parsed file). `Error::in_file` keeps the
/// innermost attribution, so an error already tagged by the loader is untouched.
pub fn attribute(e: Error, name: &str, sources: &[aipl_syntax::ast::FileSource]) -> Error {
    let Some(idx) = aipl_syntax::mangled_file_index(name) else {
        return e;
    };
    match sources.get(idx as usize) {
        Some(f) if !f.source.is_empty() => e.in_file(&f.label, &f.source),
        _ => e,
    }
}

/// Whether `sig` has a function-typed parameter whose result mentions a type
/// variable. A call's arguments may leave such a variable unpinned: a lambda has no
/// type until its parameters are known, and a function-typed parameter of the
/// caller is not a lambda at all.
pub(crate) fn has_function_result_var(sig: &Signature) -> bool {
    sig.is_generic()
        && sig.param_types().iter().any(|p| match p {
            Type::Fn(_, ret) => sig
                .type_vars
                .iter()
                .any(|tp| super::ty_mentions(ret, &tp.name)),
            _ => false,
        })
}

/// Whether `sig` has a type variable that its return type mentions and no
/// parameter does — one a call's arguments cannot pin.
pub(crate) fn has_return_only_var(sig: &Signature) -> bool {
    let return_ty = sig.return_type();
    let params = sig.param_types();
    sig.type_vars.iter().any(|tp| {
        super::ty_mentions(&return_ty, &tp.name)
            && !params.iter().any(|p| super::ty_mentions(p, &tp.name))
    })
}

/// During checking, these types stand in for an as-yet-unknown scalar: the
/// `any` constraint, the bare-`none` inner marker, and an in-scope generic
/// type parameter. They're permitted wherever a concrete scalar primitive is
/// (set elements, dict keys, etc.).
fn is_abstract_scalar_ty(t: &Type, type_params: &[String]) -> bool {
    matches!(t, Type::Any)
        // A variable of *this* signature. The anonymous one an `any` normalized
        // to carries an empty name and belongs to whatever signature holds it.
        || matches!(t, Type::TypeVar(n) if n.is_empty() || type_params.iter().any(|tp| tp == n))
}

impl TypeValidator<'_> {
    /// Validate that `t` names only known types (primitives, declared structs,
    /// in-scope type parameters) in valid positions.
    /// Validate a written type. `ctx` is the already-formatted subject of any
    /// message — `fn "lo"`, `struct "Tok" field "span"` — so the same checks
    /// serve a parameter, a struct field and a variant payload alike.
    fn check_ty(&self, t: &Type, type_params: &[String], ctx: &str) -> Result<(), Error> {
        match t {
            // `Case<V>` is a case of the *variant* `V`, so `V` has to be one —
            // `Case<i64>` names nothing. A type parameter is allowed through:
            // whether it lands on a variant is settled at instantiation, where
            // the same check runs against the substituted type.
            Type::Case(v) => match &**v {
                Type::TypeVar(_) => Ok(()),
                Type::Named(n) if self.variant_cases(n).is_some() => Ok(()),
                other => Err(Error::msg(format!(
                    "{ctx}: Case<{}> — a case belongs to a variant, and {} is not one",
                    aipl_syntax::type_name(other),
                    aipl_syntax::type_name(other)
                ))),
            },
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
            Type::Unknown | Type::ConcatStr => Ok(()),
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
            // An array element may additionally be a set or a dict, which an
            // optional core and a dict value may not — see `check_array_elem_ty`.
            Type::Array(inner) => self.check_array_elem_ty(inner, type_params, ctx),
            // `T without X`: the base is a type in its own right, and the
            // exclusion has to be a value the base could hold — `[]` excludes
            // something only from an array. A type parameter is let through as
            // it is for `Case<V>`: whether it lands on an array is settled at
            // instantiation.
            Type::Without(base, exclusion) => {
                self.check_ty(base, type_params, ctx)?;
                match (exclusion, aipl_syntax::unrefined(base)) {
                    (Exclusion::EmptyArray, Type::Array(_) | Type::TypeVar(_)) => Ok(()),
                    (Exclusion::EmptyArray, other) => Err(Error::msg(format!(
                        "{ctx}: `without []` excludes the empty array, and {} is not an array type",
                        tyname(other)
                    ))),
                    // `V without C`: `C` has to be a case `V` still has. A
                    // type parameter is let through for the same reason as
                    // above, and an exclusion already made leaves nothing to
                    // exclude twice.
                    (Exclusion::NamedCase(_), Type::TypeVar(_)) => Ok(()),
                    (Exclusion::NamedCase(case), other) => match self.cases_of(base) {
                        Some(cases) if cases.iter().any(|(c, _)| c == case) => Ok(()),
                        Some(_) => Err(Error::msg(format!(
                            "{ctx}: `without {case}` excludes a case of {}, which has no case {case:?}",
                            tyname(base)
                        ))),
                        None => Err(Error::msg(format!(
                            "{ctx}: `without {case}` excludes a case of a variant, and {} is not one",
                            tyname(other)
                        ))),
                    },
                }
            }
            // Optional core types: a scalar, `str`, a nested array, or an
            // optional (`T??`) — never a struct.
            Type::Optional(inner) => self.check_optional_core_ty(inner, type_params, ctx),
            // A set element: a scalar (i64/bool/char), `str`, or a type
            // parameter (pinned to one of those when monomorphized). No nested
            // containers, no struct/variant.
            Type::Set(inner, order) => {
                if self.is_set_member(inner) || is_abstract_scalar_ty(inner, type_params) {
                    check_ordered_elem(inner, *order, type_params, ctx)
                } else {
                    Err(Error::msg(format!(
                        "{ctx}: a set element must be an integer (i8..i64, u8..u64), bool, char, str, struct or variant, got {}",
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

    /// An *array* element type. Everything [`Self::check_elem_ty`] allows, plus a
    /// set or a dict: both are heap blocks addressed by an 8-byte pointer, the
    /// same shape a nested array element already has, so `T[][]`'s machinery
    /// carries them unchanged (`aipl_arr_drop_arr` decs each element block, and a
    /// set or dict block is what it is given).
    ///
    /// A set is also an optional's core ([`Self::check_optional_core_ty`]), which
    /// stores it inline, but a dict is not, and neither is a dict value. Those are
    /// separate pieces of work rather than the same one.
    fn check_array_elem_ty(
        &self,
        t: &Type,
        type_params: &[String],
        ctx: &str,
    ) -> Result<(), Error> {
        match t {
            // Validate the set/dict on its own terms — its element type, or its
            // key and value — which is exactly what `check_ty` does for one
            // written anywhere else.
            Type::Set(..) | Type::Dict(_, _) => self.check_ty(t, type_params, ctx),
            _ => self.check_elem_ty(t, type_params, ctx),
        }
    }

    /// An optional's core type: everything [`Self::check_elem_ty`] allows, plus a
    /// set. A set is one heap pointer, which an optional stores inline as it does
    /// an array's, so the optional's retain, drop and comparison carry it.
    /// A dict is not yet taken: it is a separate piece of work.
    fn check_optional_core_ty(
        &self,
        t: &Type,
        type_params: &[String],
        ctx: &str,
    ) -> Result<(), Error> {
        match t {
            Type::Set(..) => self.check_ty(t, type_params, ctx),
            _ => self.check_elem_ty(t, type_params, ctx),
        }
    }

    fn check_elem_ty(&self, t: &Type, type_params: &[String], ctx: &str) -> Result<(), Error> {
        match t {
            // A tag in a register, so it fits an element slot like any scalar.
            Type::Case(_) => self.check_ty(t, type_params, ctx),
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
            Type::Any | Type::Unknown => Ok(()),
            // A concat-str has the `str` runtime representation.
            Type::ConcatStr => Ok(()),
            // A refined element is its base laid out in the slot — `check_ty`
            // is what validates the refinement itself.
            Type::Without(base, _) => {
                self.check_ty(t, type_params, ctx)?;
                self.check_elem_ty(base, type_params, ctx)
            }
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
            Type::Array(inner) => self.check_array_elem_ty(inner, type_params, ctx),
            Type::Optional(inner) => self.check_optional_core_ty(inner, type_params, ctx),
            // A set is allowed as an *array* element or an optional's core, which
            // reach here through `check_array_elem_ty` and `check_optional_core_ty`;
            // a dict value is neither, and this is that refusal.
            Type::Set(..) => Err(Error::msg(format!(
                "{ctx}: a set can be an array or optional element but not a dict value"
            ))),
            // A dict is allowed as an *array* element only.
            Type::Dict(_, _) => Err(Error::msg(format!(
                "{ctx}: a dict can be an array element but not an optional or dict element"
            ))),
            Type::Result(_, _) => Err(Error::msg(format!(
                "{ctx}: a result cannot be an array, optional, or dict element"
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
}

/// Whether `ty` is one of the IEEE 754 float types.
pub(crate) fn is_float_ty(ty: &Type) -> bool {
    matches!(ty, Type::Primitive(p) if p.is_float())
}

/// The float builtin that provides the operator `op` — `float_add` for `+` — as the
/// import a diagnostic can suggest.
fn float_builtin_for(op: BinOp) -> Option<&'static str> {
    aipl_syntax::operator_named_forms(binop_spelling(op))
        .into_iter()
        .find(|name| name.starts_with("float_"))
}

/// An error unless `ty` is something the integer operators take: a float operand to one of
/// them is a mistake whose fix is the float operation, so the message names the import.
pub(crate) fn reject_float_operand(op: BinOp, ty: &Type, span: &Span) -> Result<(), Error> {
    if !is_float_ty(ty) {
        return Ok(());
    }
    let spelling = binop_spelling(op);
    let hint = match float_builtin_for(op) {
        Some(name) => format!("; import `{name} as {spelling}` to {spelling} floats"),
        None => String::new(),
    };
    Err(Error::at(
        format!(
            "\"{spelling}\" does not take {}: floats never mix with the integer operations{hint}",
            tyname(ty)
        ),
        span.clone(),
    ))
}

/// `map`'s signature over an optional receiver: the declared array form with
/// the receiver's `T[]` and the result's `U[]` read as `T?` and `U?`. Derived
/// from the declaration rather than written out, so the parameter names, the
/// type variables and the lambda's shape stay whatever the declaration says.
pub(crate) fn optional_map_signature(array_form: &Signature) -> Signature {
    let as_optional = |t: &Type| match t {
        Type::Array(inner) => Type::Optional(inner.clone()),
        other => other.clone(),
    };
    let mut sig = array_form.clone();
    if let Some(receiver) = sig.params.first_mut() {
        receiver.ty = as_optional(&receiver.ty);
    }
    sig.return_ty = sig.return_ty.as_ref().map(as_optional);
    sig
}

/// `map` over a [`Span`]: the array form with the span as the receiver and the
/// `u64` index as the element, keeping the array result.
///
/// A `Span` is a *range of positions*, so mapping it yields one element per
/// position — `0..3` gives three — and the element handed to the lambda is the
/// index itself, `u64` like every other length and bound in the language. The
/// result stays an array: positions are a sequence, unlike the optional form,
/// where the receiver's shape is what the result has to mirror.
///
/// [`Span`]: aipl_syntax::Span
pub(crate) fn span_map_signature(array_form: &Signature) -> Signature {
    let mut sig = array_form.clone();
    if let Some(receiver) = sig.params.first_mut() {
        receiver.ty = Type::Named(SPAN_TYPE.to_string());
    }
    // `f: (T) -> U` becomes `f: (u64) -> U`: the index replaces the element, and
    // `U` stays the variable the return type is built from.
    if let Some(f) = sig.params.get_mut(1) {
        if let Type::Fn(params, _) = &mut f.ty {
            if let Some(p) = params.first_mut() {
                *p = Type::Primitive(Primitive::U64);
            }
        }
    }
    sig
}

/// Which tuple pattern the loader read a temporary's slots for — what a value
/// that is not a tuple was being taken apart by.
#[derive(Clone, Copy)]
pub(crate) enum TuplePattern {
    /// `let (a, b) = v;`
    Let,
    /// `for (let (a, b) : xs)`, where the value is each element.
    For,
}

impl TuplePattern {
    pub(crate) fn not_a_tuple(self, ty: &str) -> String {
        match self {
            TuplePattern::Let => format!(
                "cannot destructure {ty} as a tuple — a pattern like \"(a, b)\" takes apart a tuple value"
            ),
            TuplePattern::For => format!(
                "cannot destructure the elements of this loop as tuples — they are {ty}. \
                 To bind a dict's key and value write \"for (let k, v : d)\"; for an \
                 array's position and element, \"for (let i, x : xs)\""
            ),
        }
    }
}

/// Strip the internal name-mangling prefix for diagnostics: the reserved
/// `__builtin_`, or the loader's per-file module prefix `__m<index>__` (added to
/// every non-root file's top-level names). Neither can appear in a user-written
/// identifier, so this only ever strips compiler-internal decoration.
pub(crate) fn display(name: &str) -> &str {
    aipl_syntax::strip_mangle_prefix(name)
}

/// `t` less its outer refinements, and what they exclude.
fn peel_refinements(t: &Type) -> (&Type, Vec<&Exclusion>) {
    let mut excluded = Vec::new();
    let mut base = t;
    while let Type::Without(inner, x) = base {
        excluded.push(x);
        base = inner;
    }
    (base, excluded)
}

fn is_unknown(t: &Type) -> bool {
    matches!(t, Type::Unknown)
}

/// An abstract type variable in a generic body. Unlike `Type::Unknown` it is *not*
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
pub(crate) fn needs_lock(k: &ExprKind) -> bool {
    match k {
        ExprKind::None => true,
        ExprKind::ArrayLit(v) | ExprKind::SetLit(v, _) => v.is_empty(),
        ExprKind::DictLit(v) => v.is_empty(),
        // `to_set` leaves its *order* to context (see `SetOrder::Context`), and
        // `narrow` its whole target type.
        ExprKind::Call(n, _, _) => {
            matches!(
                n,
                Callee::Ok
                    | Callee::Err
                    | Callee::Some
                    | Callee::ToSet
                    | Callee::Narrow
                    | Callee::CollectArms
                    | Callee::AsCase
            )
        }
        _ => false,
    }
}

const TYPEVAR: &str = "__typevar__$";

pub(crate) fn is_typevar(t: &Type) -> bool {
    matches!(t, Type::TypeVar(_))
}

/// The variable a typevar sentinel came from, or `None` for an anonymous `any`.
fn typevar_name(t: &Type) -> Option<&str> {
    match t {
        Type::TypeVar(n) => Some(n.as_str()).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// The name fragment a set contributes to a synthetic type name: `set_` for
/// the unordered set, and one per ordered form, since they are distinct types.
pub fn set_mangle(order: aipl_syntax::ast::SetOrder) -> &'static str {
    use aipl_syntax::ast::SetOrder;
    match order {
        // A placeholder order names no instance of its own: it is what a
        // `to_set` call is typed before context decides, and it becomes the
        // unordered set when nothing does.
        SetOrder::Unordered | SetOrder::Context => "set",
        SetOrder::Desc => "setdesc",
        SetOrder::Asc => "setasc",
    }
}

/// An ordered set (`#>{T}` / `#<{T}`) sorts its elements, so its element must
/// be `ord` — an integer, `char`, or `str`. A type parameter passes: its bound
/// is checked where it is declared, and its instance where it is used.
fn check_ordered_elem(
    inner: &Type,
    order: aipl_syntax::ast::SetOrder,
    type_params: &[String],
    ctx: &str,
) -> Result<(), Error> {
    if !order.is_ordered()
        || is_abstract_scalar_ty(inner, type_params)
        || aipl_syntax::ast::Bound::Ord.accepts(inner, &|_| false)
    {
        return Ok(());
    }
    Err(Error::msg(format!(
        "{ctx}: an ordered set (`#>{{..}}` / `#<{{..}}`) sorts its elements, so its element must \
         be an integer, char, or str, got {}",
        tyname(inner)
    )))
}

/// Whether `t` names a type called one of `names`, anywhere inside it.
pub(crate) fn names_a_type_named(t: &Type, names: &[&str]) -> bool {
    match t {
        Type::Named(n) => names.contains(&n.as_str()),
        Type::Case(inner)
        | Type::Optional(inner)
        | Type::Array(inner)
        | Type::Set(inner, _)
        | Type::Without(inner, _) => names_a_type_named(inner, names),
        Type::Dict(k, v) => names_a_type_named(k, names) || names_a_type_named(v, names),
        Type::Result(ok, err) => names_a_type_named(ok, names) || names_a_type_named(err, names),
        Type::Fn(ps, ret) => {
            ps.iter().any(|p| names_a_type_named(p, names)) || names_a_type_named(ret, names)
        }
        Type::Tuple(elems) => elems.iter().any(|e| names_a_type_named(e, names)),
        Type::Generic(n, args) => {
            names.contains(&n.as_str()) || args.iter().any(|a| names_a_type_named(a, names))
        }
        _ => false,
    }
}

/// Whether `t` names the type variable `v`.
pub(crate) fn mentions_type_named(t: &Type, v: &str) -> bool {
    match t {
        Type::TypeVar(n) => n == v,
        Type::Case(i)
        | Type::Optional(i)
        | Type::Array(i)
        | Type::Set(i, _)
        | Type::Without(i, _) => mentions_type_named(i, v),
        Type::Dict(a, b) | Type::Result(a, b) => {
            mentions_type_named(a, v) || mentions_type_named(b, v)
        }
        Type::Fn(ps, r) => {
            ps.iter().any(|p| mentions_type_named(p, v)) || mentions_type_named(r, v)
        }
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(|e| mentions_type_named(e, v)),
        _ => false,
    }
}

pub(crate) fn mentions_typevar(t: &Type) -> bool {
    match t {
        Type::TypeVar(_) => true,
        Type::Case(v) => mentions_typevar(v),
        Type::Optional(i) | Type::Array(i) | Type::Set(i, _) | Type::Without(i, _) => {
            mentions_typevar(i)
        }
        Type::Dict(k, v) => mentions_typevar(k) || mentions_typevar(v),
        Type::Result(a, b) => mentions_typevar(a) || mentions_typevar(b),
        Type::Fn(ps, r) => ps.iter().any(mentions_typevar) || mentions_typevar(r),
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(mentions_typevar),
        Type::Unit
        | Type::Primitive(_)
        | Type::Named(_)
        | Type::Any
        | Type::Unknown
        | Type::ConcatStr => false,
    }
}

pub(crate) fn tyname(t: &Type) -> String {
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
        Type::Set(inner, _) if is_typevar(inner) => "a set of a type parameter".to_string(),
        // A builtin type (`Span`), a per-file name (`__m1__LexError`), or a
        // generic instance (`Token$AiplTok`) carries internal mangling — render
        // it back to source-like form for diagnostics.
        Type::Named(n) => aipl_syntax::demangle_named(n),
        Type::Optional(inner) => format!("{}?", tyname(inner)),
        Type::Array(inner) => format!("{}[]", tyname(inner)),
        Type::Set(inner, o) => format!("#{}{{{}}}", o.spelling(), tyname(inner)),
        Type::Dict(k, v) => format!("#{{{}: {}}}", tyname(k), tyname(v)),
        Type::Result(ok, err) => format!("{}!{}", tyname(ok), tyname(err)),
        Type::Fn(params, ret) => {
            let ps = params.iter().map(tyname).collect::<Vec<_>>().join(", ");
            format!("({ps}) -> {}", tyname(ret))
        }
        _ => type_name(t),
    }
}

pub(crate) fn is_unit(t: &Type) -> bool {
    *t == Type::Unit
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

pub(crate) fn coerce(actual: &Type, expected: &Type) -> Result<(), ()> {
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
    // A `Case<V>` *is* its tag, and mono lowers a constructor reference to that
    // tag as a plain integer. So the two are one value in two spellings once
    // lowering has run, and a second pass over the lowered program must not read
    // that as a type error. Introducing a case is still governed by `flex_fit`,
    // which accepts only a constructor reference — a bare integer never becomes
    // a case in source.
    let tag_like = |t: &Type| {
        matches!(t, Type::Case(_))
            || matches!(t, Type::Primitive(p) if p.name() == "i64" || p.name() == "u64")
    };
    if (matches!(actual, Type::Case(_)) || matches!(expected, Type::Case(_)))
        && tag_like(actual)
        && tag_like(expected)
    {
        return Ok(());
    }
    // A refinement (`T[] without []`) is its base at runtime, so a refined value
    // goes wherever the base does. The other direction is where the refinement
    // *means* something, and there is no coercion into it: the value has to be
    // shown not to be the excluded one, which only a literal (`flex_fit`) or a
    // runtime test (`ensure_nonempty`) does. Two refinements agree when they
    // exclude the same thing from bases that coerce.
    // Refinements are a set: a value fits a type that excludes no more than it
    // does, whichever order they were stacked in (`(V without A) without B` is
    // `(V without B) without A`).
    if matches!(actual, Type::Without(..)) || matches!(expected, Type::Without(..)) {
        let (actual_base, actual_excluded) = peel_refinements(actual);
        let (expected_base, expected_excluded) = peel_refinements(expected);
        return if expected_excluded
            .iter()
            .all(|x| actual_excluded.contains(x))
        {
            coerce(actual_base, expected_base)
        } else {
            Err(())
        };
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
    // The same alias against a `T[]` whose element is still open — a type
    // variable, or the `Type::Unknown` a generic call's expected type carries
    // for one not yet pinned: a `str` is the `char[]` that pins `T = char`
    // (`collect_var_bindings`), so it fits before the binding is made too.
    // `map_join`'s `f: (T) -> U[]` meets a `str`-valued `f` this way.
    let open_elem_array = |t: &Type| matches!(t, Type::Array(e) if is_typevar(e) || is_unknown(e));
    if (*actual == Type::Primitive(Primitive::Str) && open_elem_array(expected))
        || (open_elem_array(actual) && *expected == Type::Primitive(Primitive::Str))
    {
        return Ok(());
    }
    match (actual, expected) {
        (Type::Optional(a), Type::Optional(b)) => coerce(a, b),
        (Type::Array(a), Type::Array(b)) => coerce(a, b),
        // An empty `#{}` is any set, ordered or not, on either side (a binding
        // seeded with one has no order yet), and so is a `to_set` call whose
        // order context is deciding right here; anything else has to agree on
        // the order, since the two are different types over one runtime.
        (Type::Set(a, oa), Type::Set(b, ob))
            if *oa == aipl_syntax::ast::SetOrder::Context
                || *ob == aipl_syntax::ast::SetOrder::Context =>
        {
            coerce(a, b)
        }
        (Type::Set(a, oa), Type::Set(b, ob)) if oa == ob => coerce(a, b),
        (Type::Dict(ak, av), Type::Dict(bk, bv)) => coerce(ak, bk).and_then(|()| coerce(av, bv)),
        // Two instances of one generic type (a tuple is `__tuple2<A, B>`) agree
        // when their arguments do, which is how a position that is still open
        // (`(K, V)` before `K` and `V` are pinned) accepts a settled one.
        (Type::Generic(an, aa), Type::Generic(bn, ba)) if an == bn && aa.len() == ba.len() => {
            aa.iter().zip(ba).try_for_each(|(a, b)| coerce(a, b))
        }
        (Type::Result(ao, ae), Type::Result(bo, be)) => {
            coerce(ao, bo).and_then(|()| coerce(ae, be))
        }
        // A tuple with settled elements is the struct named for them
        // (`tuple_struct_name`), and one with an element still open is an
        // instance of its arity's template. The settled one fits when the open
        // ones are all there is to compare, or when the names agree.
        (Type::Named(an), Type::Generic(bn, ba))
            if an.starts_with("__tuple$") && bn.starts_with(TUPLE_TEMPLATE) =>
        {
            let open = |t: &Type| is_typevar(t) || is_unknown(t);
            if ba.iter().any(open) || *an == tuple_struct_name(ba) {
                Ok(())
            } else {
                Err(())
            }
        }
        // A function value goes where a function of the same shape is wanted. What
        // it is given has to fit what it takes, and what it returns has to fit
        // what is wanted back — which is how a generic caller's still-open return
        // (`map`'s `U`) accepts a function parameter whose return is known.
        (Type::Fn(ap, ar), Type::Fn(bp, br)) if ap.len() == bp.len() => ap
            .iter()
            .zip(bp)
            .try_for_each(|(a, b)| coerce(b, a))
            .and_then(|()| coerce(ar, br)),
        _ => Err(()),
    }
}

/// A result captured in a binding must actually be read somewhere in the
/// binding's body — leaving it unused would silently drop its error, exactly
/// like a bare discard. (Reassigning the binding doesn't count as a read: only
/// `Ident` references do, which is what `count_ident` tallies.) `span.clone()` points at
/// the bound value.
pub(crate) fn check_result_inspected(
    name: &str,
    vt: &Type,
    body: &Expr,
    span: Span,
) -> Result<(), Error> {
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

/// A tuple written out of integer literals, which flexes slot by slot as a bare
/// literal does.
pub(crate) fn is_int_pair(arg: &Expr) -> bool {
    matches!(&arg.kind, ExprKind::TupleLit(slots) if slots.iter().all(|s| aipl_syntax::const_int(s).is_some()))
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
/// a bare literal, and a concrete payload (`Literal(str)`) pins nothing anyway.
pub(crate) fn literal_pins_nothing(arg: &Expr, pty: &Type, vars: &HashSet<&str>) -> bool {
    matches!(pty, Type::TypeVar(v) if vars.contains(v.as_str()))
        && (aipl_syntax::const_int(arg).is_some() || is_int_pair(arg))
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
        // A refinement carries no variables of its own: `T[] without []` pins
        // `T` exactly as `T[]` does, from either side.
        (Type::Without(p, _), a) => collect_var_bindings(p, a, vars, map),
        (p, Type::Without(a, _)) => collect_var_bindings(p, a, vars, map),
        (Type::Case(p), Type::Case(a)) => collect_var_bindings(p, a, vars, map),
        // A bare `none`/empty `[]` argument carries no element type (`__none__`),
        // so it can't pin the variable — leave it for another argument to fix.
        (Type::Array(p), Type::Array(a)) => collect_var_bindings(p, a, vars, map),
        // `str` is usable as `char[]` — pin the element variable to `char`.
        (Type::Array(p), Type::Primitive(Primitive::Str)) => {
            collect_var_bindings(p, &Type::Primitive(Primitive::Char), vars, map)
        }
        // A set is usable as a `T[]` too: it shares the array's layout and its
        // walk, so a generic that only reads its sequence (`all`, `find_if`,
        // `map_join`) takes one, keeping the set's own type in the instance
        // (mono's `set_kept`). The element pins `T` exactly as an array's would.
        (Type::Array(p), Type::Set(a, _)) => collect_var_bindings(p, a, vars, map),
        (Type::Set(p, _), Type::Set(a, _)) => collect_var_bindings(p, a, vars, map),
        (Type::Dict(pk, pv), Type::Dict(ak, av)) => {
            // Bind from whichever side carries concrete structure; an empty
            // An empty dict literal has `__none__` key/value and pins nothing.
            collect_var_bindings(pk, ak, vars, map);
            collect_var_bindings(pv, av, vars, map);
        }
        (Type::Optional(p), Type::Optional(a)) => collect_var_bindings(p, a, vars, map),
        (Type::Result(po, pe), Type::Result(ao, ae)) => {
            // Bind from whichever side carries concrete structure; an `ok`/`err`
            // pins one side and leaves the other `__none__`.
            collect_var_bindings(po, ao, vars, map);
            collect_var_bindings(pe, ae, vars, map);
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

/// Whether every arm takes a tuple scrutinee apart — a tuple pattern of that
/// arity, or `_` — so the tuple need never exist as a value. A binder for the
/// whole tuple would need one, and takes the ordinary path.
pub(crate) fn matches_elementwise(arms: &[MatchArm], arity: usize) -> bool {
    arms.iter().all(|a| {
        matches!(&a.pattern, Pattern::Tuple(ps) if ps.len() == arity)
            || matches!(a.pattern, Pattern::Wildcard)
    })
}

/// The declaration tables a written type is validated against.
pub(crate) struct DeclTables<'a> {
    pub structs: &'a HashMap<String, Vec<(String, Type, bool)>>,
    pub variants: &'a HashMap<String, Vec<(String, Vec<Type>)>>,
    pub generic_structs: &'a HashMap<String, StructDecl>,
    pub generic_variants: &'a HashMap<String, VariantDecl>,
}

/// Whether `t`, as a user wrote it, is a type that may appear there: it names a type that
/// exists, an element is something an array/optional/set/dict may hold, a refinement excludes
/// something its base has. `type_params` are the type variables in scope; `ctx` begins the
/// message.
pub(crate) fn validate_type(
    d: &DeclTables,
    t: &Type,
    type_params: &[String],
    ctx: &str,
) -> Result<(), Error> {
    let cx = TypeValidator {
        structs: d.structs,
        variants: d.variants,
        generic_structs: d.generic_structs,
        generic_variants: d.generic_variants,
    };
    cx.check_ty(t, type_params, ctx)
}
