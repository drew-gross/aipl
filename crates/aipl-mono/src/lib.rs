//! Monomorphization of generic functions over `any`.
//!
//! A function is generic when a parameter carries the `any` type variable as
//! either an array element (`any[]`) or an optional inner type (`any?`). `any`
//! may appear *only* in those parameter forms (never bare, never in a return
//! type), so a generic call's result type is always concrete and `any` never
//! propagates into a caller's types. Each `any`-bearing parameter is its own
//! independent type variable, inferred separately from its argument — so
//! `fn same_length(v1: any[], v2: any[])` works on `i64[], char[]`, and an
//! instance is specialized on the *tuple* of element types.
//!
//! This pass runs after the loader and before codegen. It walks every concrete
//! function (and, transitively, every instance it creates), and at each call to
//! a generic function infers the concrete type of each `any`-bearing parameter
//! from the arguments, emits/looks-up a specialized copy named with all of them
//! (e.g. `same_length$i64$char`), and rewrites the call to target it. Because it
//! substitutes the concrete types in before analyzing an instance, the whole
//! pass — and its type inference — operates on concrete types only.
//!
//! `$` is not a legal identifier character, so mangled names can never collide
//! with user functions. Uninstantiated generic templates are simply dropped.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    slice::from_ref,
    sync::OnceLock,
};

mod check;
pub use check::check;

use aipl_syntax::{
    ast::{
        is_unit, Expr, ExprKind, FieldDecl, FieldInit, Function, Item, LambdaParam, MatchArm,
        Param, Pattern, Primitive, Program, StructDecl, Type, VariantCase, VariantDecl,
    },
    concat_str_ty, empty_array_arg_ty, is_array_elem, is_concat_str, is_empty_array_arg, is_error,
    is_none_inner, is_none_literal_arg, is_str_repr, none_inner_ty, none_literal_arg_ty, type_name,
    DebugOptions, Error, Span, BUILTIN_SIGNATURES,
};

/// Hard cap on the number of generic instances monomorphization will emit.
/// For well-behaved generics the reachable instance set is finite (type
/// arguments are primitives), so blowing past this means a generic is
/// instantiating itself with an ever-growing type — a non-terminating loop.
/// Turning that into an error rather than hanging is what makes the bug
/// findable; `--debug` then prints the exact chain of growing instances.
const INSTANTIATION_LIMIT: usize = 10_000;

/// Lower all `Type::Tuple` annotations in `program` to synthetic named structs.
///
/// Every `(A, B, C)` type annotation is replaced by `Type::Named("__tuple$A$B$C")` and
/// a corresponding `struct __tuple$A$B$C { _0: A; _1: B; _2: C; }` declaration is
/// prepended to the program. This runs before `check` so the checker and codegen
/// only ever see named struct types, never raw tuples. Expression-level `TupleLit`
/// nodes are left for mono's `infer()` to lower.
pub fn lower_tuples(program: &Program) -> Program {
    let mut fields_map: HashMap<String, Vec<FieldDecl>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    let mut new_items: Vec<Item> = program
        .items
        .iter()
        .map(|item| match item {
            Item::Fn(f) => {
                let new_params: Vec<Param> = f
                    .params
                    .iter()
                    .map(|p| Param {
                        ty: lt_ty(&p.ty, &mut fields_map, &mut order),
                        ..p.clone()
                    })
                    .collect();
                let new_ret = f
                    .return_ty
                    .as_ref()
                    .map(|t| lt_ty(t, &mut fields_map, &mut order));
                let new_body = lt_expr(&f.body, &mut fields_map, &mut order);
                let new_test = f
                    .test_body
                    .as_ref()
                    .map(|tb| lt_expr(tb, &mut fields_map, &mut order));
                Item::Fn(Function {
                    params: new_params,
                    return_ty: new_ret,
                    body: new_body,
                    test_body: new_test,
                    ..f.clone()
                })
            }
            Item::Struct(s) => Item::Struct(StructDecl {
                name: s.name.clone(),
                fields: s
                    .fields
                    .iter()
                    .map(|fd| FieldDecl {
                        ty: lt_ty(&fd.ty, &mut fields_map, &mut order),
                        ..fd.clone()
                    })
                    .collect(),
            }),
            Item::Variant(v) => Item::Variant(VariantDecl {
                name: v.name.clone(),
                cases: v
                    .cases
                    .iter()
                    .map(|c| VariantCase {
                        name: c.name.clone(),
                        payload: c
                            .payload
                            .iter()
                            .map(|t| lt_ty(t, &mut fields_map, &mut order))
                            .collect(),
                    })
                    .collect(),
            }),
            Item::Import(_) => item.clone(),
        })
        .collect();

    let mut synth: Vec<Item> = order
        .into_iter()
        .map(|name| {
            Item::Struct(StructDecl {
                fields: fields_map.remove(&name).unwrap(),
                name,
            })
        })
        .collect();
    synth.append(&mut new_items);
    Program { items: synth }
}

/// Lower a type, registering any new synthetic tuple-struct in `fields_map`/`order`.
fn lt_ty(
    t: &Type,
    fields_map: &mut HashMap<String, Vec<FieldDecl>>,
    order: &mut Vec<String>,
) -> Type {
    match t {
        Type::Tuple(elems) => {
            let lowered: Vec<Type> = elems.iter().map(|e| lt_ty(e, fields_map, order)).collect();
            let name = check::tuple_struct_name(&lowered);
            if !fields_map.contains_key(&name) {
                order.push(name.clone());
                let fs: Vec<FieldDecl> = lowered
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| FieldDecl {
                        name: format!("_{i}"),
                        ty: ty.clone(),
                        default: None,
                    })
                    .collect();
                fields_map.insert(name.clone(), fs);
            }
            Type::Named(name)
        }
        Type::Array(inner) => Type::Array(Box::new(lt_ty(inner, fields_map, order))),
        Type::Set(inner) => Type::Set(Box::new(lt_ty(inner, fields_map, order))),
        Type::Optional(inner) => Type::Optional(Box::new(lt_ty(inner, fields_map, order))),
        Type::Dict(k, v) => Type::Dict(
            Box::new(lt_ty(k, fields_map, order)),
            Box::new(lt_ty(v, fields_map, order)),
        ),
        Type::Result(ok, err) => Type::Result(
            Box::new(lt_ty(ok, fields_map, order)),
            Box::new(lt_ty(err, fields_map, order)),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params.iter().map(|p| lt_ty(p, fields_map, order)).collect(),
            Box::new(lt_ty(ret, fields_map, order)),
        ),
        Type::Named(_)
        | Type::Primitive(_)
        | Type::Unit
        | Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => t.clone(),
    }
}

/// Walk an expression, lowering any `Type::Tuple` that appears in lambda-param
/// type annotations. All other expression structure is preserved unchanged.
fn lt_expr(e: &Expr, fm: &mut HashMap<String, Vec<FieldDecl>>, ord: &mut Vec<String>) -> Expr {
    let kind = match &e.kind {
        ExprKind::Lambda(params, body) => {
            let new_params: Vec<LambdaParam> = params
                .iter()
                .map(|p| LambdaParam {
                    ty: p.ty.as_ref().map(|t| lt_ty(t, fm, ord)),
                    ..p.clone()
                })
                .collect();
            ExprKind::Lambda(new_params, Box::new(lt_expr(body, fm, ord)))
        }
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit
        | ExprKind::Ident(_) => e.kind.clone(),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(lt_expr(x, fm, ord))),
        ExprKind::Not(x) => ExprKind::Not(Box::new(lt_expr(x, fm, ord))),
        ExprKind::Field(x, f) => ExprKind::Field(Box::new(lt_expr(x, fm, ord)), f.clone()),
        ExprKind::Try(x) => ExprKind::Try(Box::new(lt_expr(x, fm, ord))),
        ExprKind::Return(x) => ExprKind::Return(Box::new(lt_expr(x, fm, ord))),
        ExprKind::Binop(a, op, b) => ExprKind::Binop(
            Box::new(lt_expr(a, fm, ord)),
            *op,
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::Seq(a, b) => {
            ExprKind::Seq(Box::new(lt_expr(a, fm, ord)), Box::new(lt_expr(b, fm, ord)))
        }
        ExprKind::Index(a, b) => {
            ExprKind::Index(Box::new(lt_expr(a, fm, ord)), Box::new(lt_expr(b, fm, ord)))
        }
        ExprKind::While(a, b) => {
            ExprKind::While(Box::new(lt_expr(a, fm, ord)), Box::new(lt_expr(b, fm, ord)))
        }
        ExprKind::If(a, b, c) => ExprKind::If(
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
            Box::new(lt_expr(c, fm, ord)),
        ),
        ExprKind::Slice(a, b, c) => ExprKind::Slice(
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
            c.as_ref().map(|c| Box::new(lt_expr(c, fm, ord))),
        ),
        ExprKind::Let(n, a, b) => ExprKind::Let(
            n.clone(),
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::LetMut(n, a, b) => ExprKind::LetMut(
            n.clone(),
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::Assign(n, a, b) => ExprKind::Assign(
            n.clone(),
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::For(v, iter, body) => ExprKind::For(
            v.clone(),
            Box::new(lt_expr(iter, fm, ord)),
            Box::new(lt_expr(body, fm, ord)),
        ),
        ExprKind::Call(name, args, ms) => ExprKind::Call(
            name.clone(),
            args.iter().map(|a| lt_expr(a, fm, ord)).collect(),
            *ms,
        ),
        ExprKind::ArrayLit(elems) => {
            ExprKind::ArrayLit(elems.iter().map(|a| lt_expr(a, fm, ord)).collect())
        }
        ExprKind::SetLit(elems) => {
            ExprKind::SetLit(elems.iter().map(|a| lt_expr(a, fm, ord)).collect())
        }
        ExprKind::TupleLit(elems) => {
            ExprKind::TupleLit(elems.iter().map(|a| lt_expr(a, fm, ord)).collect())
        }
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (lt_expr(k, fm, ord), lt_expr(v, fm, ord)))
                .collect(),
        ),
        ExprKind::Construct(name, inits) => ExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|fi| FieldInit {
                    name: fi.name.clone(),
                    value: lt_expr(&fi.value, fm, ord),
                })
                .collect(),
        ),
        ExprKind::Match(s, arms) => ExprKind::Match(
            Box::new(lt_expr(s, fm, ord)),
            arms.iter()
                .map(|arm| MatchArm {
                    pattern: arm.pattern.clone(),
                    body: lt_expr(&arm.body, fm, ord),
                    span: arm.span.clone(),
                })
                .collect(),
        ),
    };
    Expr::new(kind, e.span.clone())
}

/// Rewrite `program` so it contains no type variables: every generic function
/// is replaced by zero or more concrete instances, one per distinct tuple of
/// type arguments it's called with.
pub fn monomorphize(program: &Program, dbg: DebugOptions) -> Result<MonoProgram, Error> {
    // Field types per struct (for typing `Field`/`Construct`).
    // struct name → [(field_name, field_type, default_expr)]
    let mut structs: HashMap<String, Vec<(String, Type, Option<Expr>)>> = HashMap::new();
    // Variant (sum) types: name → ordered cases `(ctor, payload)`; and the
    // reverse ctor → variant map for typing `Ctor(..)` and nullary `Ctor`.
    let mut variants: HashMap<String, Vec<(String, Vec<Type>)>> = HashMap::new();
    let mut ctors: HashMap<String, String> = HashMap::new();
    // Return type of each *concrete* user fn (for typing non-generic calls).
    let mut fn_returns: HashMap<String, Type> = HashMap::new();
    // Names of mutating functions (first param is `mut self`), generic or not.
    // The call form `foo(b, ..)` of one of these desugars to copy-and-modify.
    let mut mutating: HashSet<String> = HashSet::new();
    let mut generics: HashMap<String, Generic> = HashMap::new();
    let mut concrete_fns: Vec<&Function> = Vec::new();
    // Structs (and any stray imports) pass through unchanged.
    let mut passthrough: Vec<Item> = Vec::new();

    for item in &program.items {
        match item {
            Item::Struct(s) => {
                structs.insert(
                    s.name.clone(),
                    s.fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone(), f.default.clone()))
                        .collect(),
                );
                passthrough.push(item.clone());
            }
            Item::Variant(v) => {
                for c in &v.cases {
                    ctors.insert(c.name.clone(), v.name.clone());
                }
                variants.insert(
                    v.name.clone(),
                    v.cases
                        .iter()
                        .map(|c| (c.name.clone(), c.payload.clone()))
                        .collect(),
                );
                passthrough.push(item.clone());
            }
            Item::Import(_) => passthrough.push(item.clone()),
            Item::Fn(f) => {
                if f.params.first().is_some_and(|p| p.mutable) {
                    mutating.insert(f.name.clone());
                }
                if is_generic(f) {
                    generics.insert(f.name.clone(), normalize(f)?);
                } else {
                    // A non-generic fn must not mention `any` in its return.
                    if f.return_ty.as_ref().is_some_and(|t| ty_mentions(t, "any")) {
                        return Err(Error::msg(format!(
                            "fn \"{}\": \"any\" is only allowed in a parameter (\"any[]\", \"any?\") \
                             or via a declared type parameter \"<T: any>\"",
                            f.name
                        )));
                    }
                    fn_returns.insert(
                        f.name.clone(),
                        f.return_ty
                            .clone()
                            .unwrap_or(Type::Primitive(Primitive::I64)),
                    );
                    concrete_fns.push(f);
                }
            }
        }
    }

    // Own a copy of each concrete function so the demand-driven driver can pull
    // reachable ones without juggling borrows of `program`. Source functions are
    // already fully resolved (no type parameters) — they just haven't been
    // specialized to a particular call site yet (variadic resolution, ownership,
    // concat representation), which is what registering them as a
    // `ConcreteTemplate` defers.
    let concrete: HashMap<String, ConcreteTemplate> = concrete_fns
        .iter()
        .map(|f| {
            (
                f.name.clone(),
                ConcreteTemplate {
                    params: f.params.clone(),
                    effects: f.effects.clone(),
                    return_ty: f.return_ty.clone(),
                    body: f.body.clone(),
                },
            )
        })
        .collect();

    let mut mono = Mono {
        generics: &generics,
        concrete,
        fn_returns: &fn_returns,
        mutating: &mutating,
        structs: &structs,
        syn_structs: HashMap::new(),
        variants: &variants,
        ctors: &ctors,
        emitted: HashSet::new(),
        queue: VecDeque::new(),
        synth: 0,
        cur_effects: Vec::new(),
        cur_lenv: HashMap::new(),
        lambda_envs: HashMap::new(),
        spec_memo: HashMap::new(),
        dbg,
    };

    // Seed lazily: a program is reached from `main`. A `main`-less *library*
    // checked via the `.test` runner is reached from its synthesized
    // `__test_main` driver, which transitively pulls in the code under test —
    // specializing any higher-order function through its lambda call sites.
    // (Seeding every concrete fn there would try to emit an HOF *template*
    // directly, which has no lowering: its function-typed parameter is resolved
    // only by specialization.) A fragment with neither (e.g. a codegen unit test
    // compiling one function, or an FFI engine loaded for its `pub` entry points)
    // falls back to every concrete fn — except non-generic HOF templates (those
    // with a function-typed parameter), which can't be lowered directly and are
    // reached only through a specialized call site.
    let mut seeds: Vec<String> = if mono.concrete.contains_key("main") {
        vec!["main".to_string()]
    } else if mono.concrete.contains_key("__test_main") {
        vec!["__test_main".to_string()]
    } else {
        concrete_fns
            .iter()
            .filter(|f| !f.params.iter().any(|p| matches!(p.ty, Type::Fn(_, _))))
            .map(|f| f.name.clone())
            .collect()
    };
    // The `check` command synthesizes a `__test_main` driver (calling each
    // `__test$<fn>` body); seed it too so it survives even when the program also
    // has a `main`. It transitively reaches every test and the code under test.
    if mono.concrete.contains_key("__test_main") && !seeds.iter().any(|s| s == "__test_main") {
        seeds.push("__test_main".to_string());
    }
    for s in &seeds {
        mono.enqueue_concrete(s);
    }

    dbg.trace(
        "mono",
        format_args!(
            "seeding {} concrete fn(s) lazily, {} generic template(s)",
            seeds.len(),
            generics.len()
        ),
    );

    let mut out_fns: Vec<ConcreteFn> = Vec::new();
    // Drain the work-list until nothing new is discovered. Processing a body
    // enqueues the instances (concrete callees, generic specializations, and
    // their owned forms) it calls, so the reachable set grows transitively from
    // the seeds. Each instance substitutes its type arguments (if any) into the
    // template signature and records which parameters it owns.
    let mut instantiated = 0usize;
    while let Some(inst) = mono.queue.pop_front() {
        let (params, effects, return_ty, body) =
            if let Some(Generic { sig, body }) = generics.get(&inst.template) {
                // Generic specialization: limit-check (a runaway here means a
                // self-growing instance), then substitute type arguments.
                instantiated += 1;
                if instantiated > INSTANTIATION_LIMIT {
                    return Err(Error::msg(format!(
                    "monomorphization exceeded {INSTANTIATION_LIMIT} generic instances without \
                     terminating — a generic function is most likely instantiating itself with an \
                     ever-growing type. The most recent instance was `{}`. Re-run with `--debug` \
                     to see the full chain of instantiations.",
                    inst.mangled
                )));
                }
                let map: HashMap<String, Type> = sig
                    .type_vars
                    .iter()
                    .cloned()
                    .zip(inst.specs.type_args.iter().cloned())
                    .collect();
                let chars = Type::Array(Box::new(Type::Primitive(Primitive::Char)));
                let params: Vec<Param> = sig
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let ty = subst_vars(&p.ty, &map);
                        // A `str`-as-`char[]` parameter keeps its `str` type: the body
                        // operates on the str directly, no materialization.
                        let str_kept = inst.specs.params.get(i).is_some_and(|p| p.str_kept);
                        let ty = if str_kept && ty == chars {
                            Type::Primitive(Primitive::Str)
                        } else {
                            ty
                        };
                        Param {
                            name: p.name.clone(),
                            ty,
                            mutable: p.mutable,
                            variadic: p.variadic,
                        }
                    })
                    .collect();
                let return_ty = sig.return_ty.as_ref().map(|t| subst_vars(t, &map));
                (params, sig.effects.clone(), return_ty, body.clone())
            } else {
                // Concrete function: its signature is already concrete.
                let f = mono.concrete[&inst.template].clone();
                (f.params, f.effects, f.return_ty, f.body)
            };
        // A concat-specialized instance retypes its marked `str` parameters to
        // the internal concat-str sentinel, so the body (and codegen) carry the
        // representation. Mirrors the `str_params` `char[] -> str` retype above.
        let params: Vec<Param> = params
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let concat = inst.specs.params.get(i).is_some_and(|s| s.concat);
                if concat && is_str_repr(&p.ty) {
                    Param {
                        ty: aipl_syntax::concat_str_ty(),
                        ..p
                    }
                } else {
                    p
                }
            })
            .collect();
        // Resolve any variadic parameters into their concrete shape: retype each
        // and prepend a prologue rebuilding the sequence the body iterates, so
        // `process` (and codegen) see only concrete, non-variadic parameters.
        let (params, body) = specialize_variadic(
            params,
            body,
            &inst.specs.indices(|p| p.variadic == VShape::Elem),
            &inst.specs.indices(|p| p.variadic == VShape::Opt),
        );
        let owned_params = inst.specs.indices(|p| p.owned);
        mono.dbg.trace(
            "mono",
            format_args!(
                "process `{}` (owns {:?}, {} still queued)",
                inst.mangled,
                owned_params,
                mono.queue.len()
            ),
        );
        let mut out = mono.process(&inst.mangled, &params, &effects, &return_ty, &body)?;
        out.owned_params = owned_params;
        out.concat_params = inst.specs.indices(|p| p.concat);
        out_fns.push(out);
    }

    let syn_structs: Vec<StructDecl> = mono
        .syn_structs
        .drain()
        .map(|(name, fields)| StructDecl {
            name,
            fields: fields
                .into_iter()
                .map(|(fname, ty, _)| aipl_syntax::ast::FieldDecl {
                    name: fname,
                    ty,
                    default: None,
                })
                .collect(),
        })
        .collect();
    let mut structs: Vec<StructDecl> = Vec::new();
    let mut variants_out: Vec<VariantDecl> = Vec::new();
    for item in passthrough {
        match item {
            Item::Struct(s) => structs.push(s),
            Item::Variant(v) => variants_out.push(v),
            // Imports are resolved by the loader — codegen never sees them.
            Item::Import(_) => {}
            Item::Fn(_) => unreachable!("passthrough never carries a fn item"),
        }
    }
    structs.extend(syn_structs);
    Ok(MonoProgram {
        structs,
        variants: variants_out,
        fns: out_fns,
    })
}

/// The shape a variadic (`T*`) argument takes at a call site: the sequence
/// itself, a single element, or an optional element. Each maps to a distinct
/// specialization. `Seq` (the default) is the plain instance; a non-variadic
/// parameter is always `Seq`.
#[derive(Clone, Copy, PartialEq, Default)]
enum VShape {
    #[default]
    Seq,
    Elem,
    Opt,
}

/// Classify a variadic argument of type `arg_ty` against the parameter's
/// sequence type `seq_ty`. An optional is always the optional shape; otherwise
/// the argument is the sequence if it matches the sequence representation
/// (`str`/`Error` for a `str` sequence, any array for a `T[]` one), else a
/// single element.
fn variadic_shape(arg_ty: &Type, seq_ty: &Type) -> VShape {
    if matches!(arg_ty, Type::Optional(_)) {
        VShape::Opt
    } else if matches!(seq_ty, Type::Primitive(Primitive::Str)) {
        if is_str_repr(arg_ty) {
            VShape::Seq
        } else {
            VShape::Elem
        }
    } else if matches!(arg_ty, Type::Array(_)) {
        VShape::Seq
    } else {
        VShape::Elem
    }
}

/// The element type `T` of a variadic parameter's sequence type — the inverse of
/// the parser's `T → seq(T)` mapping: `str → char` (an AIPL string is the char
/// sequence), `T[] → T`.
fn variadic_elem_ty(seq: &Type) -> Type {
    match seq {
        Type::Primitive(Primitive::Str) => Type::Primitive(Primitive::Char),
        Type::Array(e) => (**e).clone(),
        other => other.clone(),
    }
}

/// Resolve an instance's variadic parameters to their per-call shape. A
/// parameter listed in `variadic_elem` / `variadic_opt` is retyped to the
/// element / optional type and renamed (`x` → `x$v`), and a `let x = <rebuild>;`
/// prologue is prepended so the body still sees the sequence it was written
/// against (`[x$v]` / `match x$v { .. }`, or `__char_to_str(..)` / `""` for a
/// `char*`-backed `str` sequence). Any other variadic parameter is the sequence
/// form and only has its `variadic` flag cleared. After this, params and body
/// are fully concrete — codegen never sees a variadic parameter.
fn specialize_variadic(
    mut params: Vec<Param>,
    body: Expr,
    variadic_elem: &[usize],
    variadic_opt: &[usize],
) -> (Vec<Param>, Expr) {
    let span = body.span.clone();
    // (original name, expression rebuilding the sequence) in parameter order.
    let mut prologues: Vec<(String, Expr)> = Vec::new();
    for (i, p) in params.iter_mut().enumerate() {
        if !p.variadic {
            continue;
        }
        p.variadic = false;
        let elem = variadic_elem_ty(&p.ty);
        let is_char = elem == Type::Primitive(Primitive::Char);
        let orig = p.name.clone();
        if variadic_elem.contains(&i) {
            let pv = format!("{orig}$v");
            let pv_id = Expr::new(ExprKind::Ident(pv.clone()), span.clone());
            let convert = if is_char {
                Expr::new(
                    ExprKind::Call("__char_to_str".into(), vec![pv_id], false),
                    span.clone(),
                )
            } else {
                Expr::new(ExprKind::ArrayLit(vec![pv_id]), span.clone())
            };
            p.name = pv;
            p.ty = elem;
            prologues.push((orig, convert));
        } else if variadic_opt.contains(&i) {
            let pv = format!("{orig}$v");
            let xn = format!("{orig}$x");
            let x_id = Expr::new(ExprKind::Ident(xn.clone()), span.clone());
            let some_body = if is_char {
                Expr::new(
                    ExprKind::Call("__char_to_str".into(), vec![x_id], false),
                    span.clone(),
                )
            } else {
                Expr::new(ExprKind::ArrayLit(vec![x_id]), span.clone())
            };
            let none_body = if is_char {
                Expr::new(ExprKind::Str(String::new()), span.clone())
            } else {
                Expr::new(ExprKind::ArrayLit(Vec::new()), span.clone())
            };
            let m = Expr::new(
                ExprKind::Match(
                    Box::new(Expr::new(ExprKind::Ident(pv.clone()), span.clone())),
                    vec![
                        MatchArm {
                            pattern: Pattern::Ctor {
                                name: "some".into(),
                                bindings: vec![xn],
                            },
                            body: some_body,
                            span: span.clone(),
                        },
                        MatchArm {
                            pattern: Pattern::Ctor {
                                name: "none".into(),
                                bindings: Vec::new(),
                            },
                            body: none_body,
                            span: span.clone(),
                        },
                    ],
                ),
                span.clone(),
            );
            p.name = pv;
            p.ty = Type::Optional(Box::new(elem));
            prologues.push((orig, m));
        }
        // else: the sequence form — keep the name and (sequence) type.
    }
    let mut new_body = body;
    for (orig, convert) in prologues.into_iter().rev() {
        new_body = Expr::new(
            ExprKind::Let(orig, Box::new(convert), Box::new(new_body)),
            span.clone(),
        );
    }
    (params, new_body)
}

/// A function's shape apart from its body, after normalization: its ordered
/// type variables (declared `<T, ..>` first, then a synthetic name per
/// anonymous `any[]`/`any?` parameter), the params/return/effects where every
/// type variable is a named reference. [`builtin_sigs`] only needs this —
/// builtins are never enqueued/specialized like a real generic (see
/// [`Generic`]), so there's no body to carry.
#[derive(Clone)]
struct Signature {
    type_vars: Vec<String>,
    params: Vec<Param>,
    return_ty: Option<Type>,
    effects: Vec<String>,
}

impl Signature {
    /// Substitute `type_args` (positional, matching `type_vars` order) into
    /// this signature's params and return type, yielding the concrete
    /// (monomorphic) params/return-type pair a specialized instance has.
    /// Used by [`Mono::concrete_signature`] for ownership eligibility — the
    /// substituted params/return must be concrete heap types for
    /// `owned_eligible` to recognize them, so a raw type variable left
    /// unsubstituted here would silently disable the owned optimization.
    fn make_concrete(&self, type_args: &[Type]) -> (Vec<Param>, Option<Type>) {
        let map: HashMap<String, Type> = self
            .type_vars
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect();
        let params = self
            .params
            .iter()
            .map(|p| Param {
                name: p.name.clone(),
                ty: subst_vars(&p.ty, &map),
                mutable: p.mutable,
                variadic: p.variadic,
            })
            .collect();
        let return_ty = self.return_ty.as_ref().map(|t| subst_vars(t, &map));
        (params, return_ty)
    }
}

/// A generic function template ready to monomorphize: a [`Signature`] plus the
/// body to substitute into when specializing a call.
#[derive(Clone)]
struct Generic {
    sig: Signature,
    body: Expr,
}

/// A fully-resolved function: every type variable has been substituted (there
/// are none left) and ownership/representation specialization decisions are
/// final, so it's ready for codegen. Distinct from [`Function`] (the AST type),
/// which additionally carries source-only concerns — visibility, declared type
/// parameters, an attached `.test`/`.doc` — that have no meaning once
/// monomorphization is done; conversely `owned_params`/`concat_params` (below)
/// have no meaning *before* monomorphization, since specialization is the only
/// thing that ever sets them. Reusing one struct for both ends of the pass
/// meant every source function and every synthesized instance carried four
/// dead fields; splitting them apart makes each representation self-explanatory
/// and lets the compiler catch a stray reference to the wrong one.
#[derive(Clone)]
pub struct ConcreteFn {
    pub name: String,
    pub params: Vec<ConcreteParam>,
    pub effects: Vec<String>,
    pub return_ty: Option<Type>,
    pub body: Expr,
    /// Indices of parameters this instance *takes ownership of*: the caller
    /// transfers its sole reference instead of retaining, and the callee is
    /// responsible for consuming it (so it isn't dropped on entry-scope exit).
    /// Set when a call passes a fresh, uniquely-owned heap argument; empty for
    /// a plain borrow instance.
    pub owned_params: Vec<usize>,
    /// Indices of `str` parameters this instance receives in the
    /// *concatenated-string* representation (a lazy concat node — see
    /// [`aipl_syntax::concat_str_ty`]). Set for a concat-specialized instance
    /// (`$c{i}`); empty for a plain-`str` instance. The parameter's `ty` is
    /// retyped to the concat sentinel in such an instance, so codegen still
    /// sees a str-repr parameter; this list records *which* for repr-aware
    /// passes.
    pub concat_params: Vec<usize>,
}

/// A fully-resolved parameter: `variadic` doesn't appear here because by the
/// time a [`ConcreteFn`] exists, every variadic parameter has already been
/// resolved to its per-call shape by `specialize_variadic` (its `ty` retyped to
/// the element/optional/sequence form as appropriate) — codegen never sees a
/// parameter still in variadic form. Distinct from [`Param`], whose `variadic`
/// is exactly the declaration `specialize_variadic` resolves away.
#[derive(Clone)]
pub struct ConcreteParam {
    pub name: String,
    pub ty: Type,
    pub mutable: bool,
}

impl From<Param> for ConcreteParam {
    fn from(p: Param) -> Self {
        ConcreteParam {
            name: p.name,
            ty: p.ty,
            mutable: p.mutable,
        }
    }
}

/// A concrete (non-generic) function *registered* with the monomorphizer but
/// not yet specialized to a particular call site: its parameters may still be
/// declared `variadic` (unresolved until a call's argument shapes are known —
/// see `specialize_variadic`), and it carries no ownership/concat-representation
/// decisions (those are per-*instance*, decided when a call is enqueued, not
/// per-registration). Looked up by name from `Mono::concrete` (so it carries no
/// `name` of its own — the map key is the name) and turned into one or more
/// [`ConcreteFn`] instances as calls to it are discovered. Distinct from
/// [`Generic`], which additionally carries type variables to substitute — a
/// `ConcreteTemplate` has none; its shape is already fully concrete modulo
/// variadic resolution.
#[derive(Clone)]
struct ConcreteTemplate {
    params: Vec<Param>,
    effects: Vec<String>,
    return_ty: Option<Type>,
    body: Expr,
}

/// The output of [`monomorphize`]: struct/variant declarations pass through
/// unchanged, and every function is a [`ConcreteFn`] instance ready for
/// codegen. Replaces a whole [`Program`] (whose `Item::Fn` would otherwise
/// force codegen to share `Function` with the pre-mono source representation).
#[derive(Clone)]
pub struct MonoProgram {
    pub structs: Vec<StructDecl>,
    pub variants: Vec<VariantDecl>,
    pub fns: Vec<ConcreteFn>,
}

/// How a *single parameter* is specialized in an instance — the per-parameter
/// markers the mangled name encodes. A parameter can carry more than one at once
/// (e.g. a single owned `char[]`-kept-as-`str` parameter is both `owned` and
/// `str_kept`), so these are independent flags rather than one tag.
///
/// - `owned`: the caller moves its sole ref in (`$own{i}`).
/// - `str_kept`: a `char[]` parameter passed a `str`, kept as `str` — not
///   materialized; the body operates on the str directly (`$s{i}`).
/// - `concat`: a `str` parameter passed a *concat-str* argument (see
///   [`aipl_syntax::CONCAT_STR`]); retyped to the concat sentinel, producing a
///   distinct concat-specialized instance (`$c{i}`).
/// - `variadic`: a variadic (`T*`) parameter's argument shape — `Elem` / `Opt`
///   specialize to a single element / an optional element (`$ve{i}` / `$vo{i}`),
///   while `Seq` (the default, and every non-variadic parameter) takes the
///   sequence form. The processing loop retypes an `Elem`/`Opt` parameter and
///   prepends a prologue rebuilding the sequence the body iterates, so codegen
///   sees only concrete, non-variadic parameters.
#[derive(Default, Clone)]
struct ParamSpec {
    owned: bool,
    str_kept: bool,
    concat: bool,
    variadic: VShape,
}

/// How an instance specializes its template's signature — everything that
/// distinguishes it from a plain borrow instance, and that the mangled name
/// encodes. Bundled so `enqueue_full` (and `Instance`) take one argument.
/// `type_args` is per *type variable* (in `type_vars` order; empty for a concrete
/// fn); `params` is per *parameter* (indexed by position — see [`ParamSpec`]).
#[derive(Default, Clone)]
struct ParamSpecs {
    type_args: Vec<Type>,
    params: Vec<ParamSpec>,
}

impl ParamSpecs {
    /// The parameter indices whose [`ParamSpec`] satisfies `pred`, ascending.
    fn indices(&self, pred: impl Fn(&ParamSpec) -> bool) -> Vec<usize> {
        self.params
            .iter()
            .enumerate()
            .filter(|(_, p)| pred(p))
            .map(|(i, _)| i)
            .collect()
    }
}

/// A pending specialization: the `template` (a concrete fn or a generic), how it
/// specializes (`specs`), and the `mangled` name it'll be emitted under.
struct Instance {
    template: String,
    specs: ParamSpecs,
    mangled: String,
}

/// What a function-typed binding (a lambda parameter of a specialized callee)
/// resolves to: the synthesized lambda function and the capture-argument
/// expressions to forward to it (capture parameters in the current scope).
#[derive(Clone)]
struct LambdaBinding {
    fn_name: String,
    captures: Vec<Expr>,
}

type LambdaEnv = HashMap<String, LambdaBinding>;

struct Mono<'a> {
    generics: &'a HashMap<String, Generic>,
    /// Every concrete (non-generic) user function, by name. The reachable subset
    /// is discovered lazily from the seeds and drained from `concrete_queue`.
    concrete: HashMap<String, ConcreteTemplate>,
    /// Return type of each concrete user fn (for typing non-generic calls).
    fn_returns: &'a HashMap<String, Type>,
    /// Names of mutating functions (`mut self` receiver), generic or not.
    mutating: &'a HashSet<String>,
    structs: &'a HashMap<String, Vec<(String, Type, Option<Expr>)>>,
    /// Synthetic struct definitions created on the fly for tuple literals.
    syn_structs: HashMap<String, Vec<(String, Type, Option<Expr>)>>,
    /// Variant types (name → cases) and the ctor → variant reverse map.
    variants: &'a HashMap<String, Vec<(String, Vec<Type>)>>,
    ctors: &'a HashMap<String, String>,
    /// Mangled names already queued, so each instance is processed exactly once.
    emitted: HashSet<String>,
    /// Reachable instances (concrete fns and generic specializations, in their
    /// borrow and owned forms) awaiting processing, in discovery order. Lambda
    /// specializations and synthesized lambda functions are inserted into
    /// `concrete` and queued here too.
    queue: VecDeque<Instance>,
    /// Counter for unique synthesized names (lambda functions and lambda-
    /// specialized callees).
    synth: usize,
    /// Declared effects of the function currently being processed. A lambda's
    /// synthesized function and the lambda-specialized callee inherit these:
    /// the checker guarantees the enclosing function's effects cover the
    /// lambda's, so this is a sound (and per-call-site precise) over-approximation.
    cur_effects: Vec<String>,
    /// Lambda bindings (function-typed parameter → its synthesized function +
    /// capture forwards) for the function currently being processed.
    cur_lenv: LambdaEnv,
    /// Lambda environment for each synthesized lambda-specialized callee, set as
    /// `cur_lenv` when that callee is processed.
    lambda_envs: HashMap<String, LambdaEnv>,
    /// Memo of lambda specializations: (callee, the synthesized function each of
    /// its function-typed parameters maps to) → the specialized callee's name.
    /// Lets a forwarded lambda reuse a specialization, so recursive higher-order
    /// functions terminate.
    spec_memo: HashMap<(String, Vec<String>), String>,
    dbg: DebugOptions,
}

type Env = HashMap<String, Type>;

impl Mono<'_> {
    /// Process one (already concrete) function: type the body and rewrite every
    /// generic call within it to its mangled instance name.
    fn process(
        &mut self,
        name: &str,
        params: &[Param],
        effects: &[String],
        return_ty: &Option<Type>,
        body: &Expr,
    ) -> Result<ConcreteFn, Error> {
        let mut env: Env = HashMap::new();
        for p in params {
            env.insert(p.name.clone(), p.ty.clone());
        }
        // Lambdas synthesized while processing this body inherit its effects.
        self.cur_effects = effects.to_vec();
        // A lambda-specialized callee carries its function-typed parameters'
        // bindings; an ordinary function has none.
        self.cur_lenv = self.lambda_envs.get(name).cloned().unwrap_or_default();
        let (body, _) = self.infer(body, &env)?;
        Ok(ConcreteFn {
            name: name.to_string(),
            // By now `specialize_variadic` has already resolved every
            // parameter's shape, so `variadic` is always false — dropped here.
            params: params.iter().cloned().map(ConcreteParam::from).collect(),
            effects: effects.to_vec(),
            return_ty: return_ty.clone(),
            body,
            // The driver sets these from the instance's ownership / concat-repr
            // decisions.
            owned_params: Vec::new(),
            concat_params: Vec::new(),
        })
    }

    /// Specialize a higher-order call for its lambda arguments. Each
    /// function-typed argument — a literal lambda or a forwarded function-typed
    /// binding — resolves to a synthesized function (newly lifted, or reused
    /// from the enclosing lambda environment). The callee is cloned into a
    /// specialization where each function-typed parameter is dropped, its
    /// captures become ordinary parameters, and its lambda environment maps the
    /// dropped parameter to the synthesized function (so the body's direct calls
    /// and onward passes resolve during processing). Captured values are
    /// forwarded at the call site. Specializations are memoized by (callee,
    /// synthesized functions) so recursive higher-order functions terminate.
    /// Specialize a higher-order call for its lambda / function-ref arguments:
    /// lift each lambda to a synthesized function, drop the function-typed
    /// parameters, thread captures in as ordinary parameters, and emit one
    /// specialized concrete callee. `base_name` seeds the specialized name and the
    /// reuse-memo key (the original callee for a concrete fn, or its type-args
    /// instance for a generic one); `template` is the concrete (already
    /// type-substituted, if generic) function and `ret` its concrete return type.
    fn specialize_call_with(
        &mut self,
        base_name: &str,
        template: &ConcreteTemplate,
        ret: Type,
        args: &[Expr],
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let name = base_name;
        let effects = self.cur_effects.clone();

        // Pass 1: per argument, build the forwarded call argument(s); for a
        // function-typed parameter, also resolve which synthesized function it
        // binds to and the capture types (to shape the specialized callee).
        let mut new_args: Vec<Expr> = Vec::new();
        let mut targets: Vec<Option<(String, Vec<Type>)>> = Vec::new();
        for (arg, param) in args.iter().zip(&template.params) {
            if let Type::Fn(ptys, lret) = &param.ty {
                let (fn_name, forwards): (String, Vec<Expr>) = match &arg.kind {
                    ExprKind::Lambda(lparams, lbody) => {
                        let captures = free_vars(lbody, lparams, env);
                        let fn_name =
                            self.synth_lambda(lparams, lbody, ptys, lret, &captures, &effects);
                        let forwards = captures
                            .iter()
                            .map(|(cn, _)| Expr::new(ExprKind::Ident(cn.clone()), arg.span.clone()))
                            .collect();
                        (fn_name, forwards)
                    }
                    ExprKind::Ident(g) if self.cur_lenv.contains_key(g) => {
                        let lb = self.cur_lenv[g].clone();
                        (lb.fn_name, lb.captures)
                    }
                    // A named function (or builtin) passed by name: it *is* the
                    // target — no lifting and no captures. Ensure it's emitted
                    // (a no-op for builtins, which aren't in `concrete`).
                    ExprKind::Ident(g) if self.is_fn_ref(g, env) => {
                        self.enqueue_concrete(g);
                        (g.clone(), Vec::new())
                    }
                    _ => {
                        return Err(Error::at(
                            format!(
                                "argument to a function parameter of \"{name}\" must be a lambda, \
                                 a named function, or a forwarded function parameter"
                            ),
                            arg.span.clone(),
                        ));
                    }
                };
                let mut cap_types = Vec::with_capacity(forwards.len());
                for fe in forwards {
                    let (fr, ft) = self.infer(&fe, env)?;
                    cap_types.push(ft);
                    new_args.push(fr);
                }
                targets.push(Some((fn_name, cap_types)));
            } else {
                let (ra, _) = self.infer(arg, env)?;
                new_args.push(ra);
                targets.push(None);
            }
        }

        // Reuse an existing specialization for the same (callee, functions).
        let key = (
            name.to_string(),
            targets
                .iter()
                .filter_map(|t| t.as_ref().map(|(f, _)| f.clone()))
                .collect::<Vec<_>>(),
        );
        if let Some(spec) = self.spec_memo.get(&key) {
            return Ok((
                Expr::new(ExprKind::Call(spec.clone(), new_args, false), span.clone()),
                ret,
            ));
        }

        // Build the specialized callee: drop each function-typed parameter, add
        // a parameter per capture, and record its lambda environment.
        let mut new_params: Vec<Param> = Vec::new();
        let mut lenv: LambdaEnv = HashMap::new();
        for (param, target) in template.params.iter().zip(&targets) {
            match target {
                Some((fn_name, cap_types)) => {
                    let mut cap_idents = Vec::with_capacity(cap_types.len());
                    for ct in cap_types {
                        let cap = format!("$cap{}", self.synth);
                        self.synth += 1;
                        new_params.push(Param {
                            name: cap.clone(),
                            ty: ct.clone(),
                            mutable: false,
                            variadic: false,
                        });
                        cap_idents.push(Expr::new(ExprKind::Ident(cap), span.clone()));
                    }
                    lenv.insert(
                        param.name.clone(),
                        LambdaBinding {
                            fn_name: fn_name.clone(),
                            captures: cap_idents,
                        },
                    );
                }
                None => new_params.push(param.clone()),
            }
        }

        let spec_name = format!("{name}$lam{}", self.synth);
        self.synth += 1;
        self.concrete.insert(
            spec_name.clone(),
            ConcreteTemplate {
                params: new_params,
                effects,
                return_ty: template.return_ty.clone(),
                body: template.body.clone(),
            },
        );
        self.lambda_envs.insert(spec_name.clone(), lenv);
        self.spec_memo.insert(key, spec_name.clone());
        self.enqueue_concrete(&spec_name);
        Ok((
            Expr::new(ExprKind::Call(spec_name, new_args, false), span.clone()),
            ret,
        ))
    }

    /// Specialize a call to a *generic* higher-order function (e.g. a
    /// `count_while<T>(self: T[], pred: (T) -> bool)`): infer the type variables
    /// from the non-lambda arguments, substitute them into a concrete template
    /// (whose function-typed parameter is now concrete), then hand off to
    /// `specialize_call_with` to lift the lambda over that instance.
    fn specialize_generic_call(
        &mut self,
        gname: &str,
        args: &[Expr],
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let Generic { sig, body } = self.generics[gname].clone();
        let var_set: HashSet<&str> = sig.type_vars.iter().map(String::as_str).collect();
        // A lambda carries no type, so the type variables must be pinned by the
        // other (non-function) parameters' arguments.
        let mut map: HashMap<String, Type> = HashMap::new();
        // A `str` passed to a `T[]` parameter specializes on the str
        // representation *directly* — the body iterates/indexes the str itself,
        // with no `char[]` materialization. `T` is still pinned to `char` (so the
        // element type and any lambda are right), but this parameter's concrete
        // type stays `str`. Record which parameters that applies to.
        let mut str_arg = vec![false; sig.params.len()];
        for (i, (param, arg)) in sig.params.iter().zip(args).enumerate() {
            if matches!(param.ty, Type::Fn(_, _)) {
                continue;
            }
            let (_, aty) = self.infer(arg, env)?;
            collect_bindings(&param.ty, &aty, &var_set, &mut map, gname, span.clone())?;
            str_arg[i] = aty == Type::Primitive(Primitive::Str);
        }
        let mut type_args = Vec::with_capacity(sig.type_vars.len());
        for v in &sig.type_vars {
            let t = map.get(v).cloned().ok_or_else(|| {
                Error::at(
                    format!(
                        "cannot infer a type for \"{}\" in generic \"{gname}\" — it appears only \
                         in a function-typed parameter, so it can't be pinned by a lambda argument",
                        display_var(v)
                    ),
                    span.clone(),
                )
            })?;
            type_args.push(t);
        }
        // Concrete (type-substituted) template; its function-typed parameter is now
        // concrete (e.g. `(i64) -> bool`), which `specialize_call_with` will lift.
        // A `str`-as-`char[]` parameter keeps its `str` type rather than the
        // substituted `char[]`.
        let chars = Type::Array(Box::new(Type::Primitive(Primitive::Char)));
        let params: Vec<Param> = sig
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let ty = subst_vars(&p.ty, &map);
                let ty = if str_arg[i] && ty == chars {
                    Type::Primitive(Primitive::Str)
                } else {
                    ty
                };
                Param {
                    name: p.name.clone(),
                    ty,
                    mutable: p.mutable,
                    variadic: p.variadic,
                }
            })
            .collect();
        let ret = match &sig.return_ty {
            Some(t) => subst_vars(t, &map),
            None => Type::Unit,
        };
        let template = ConcreteTemplate {
            params,
            effects: sig.effects.clone(),
            return_ty: Some(ret.clone()),
            body: body.clone(),
        };
        // Name the instance by its type args so different `T`s don't collide in
        // the specialization memo; a `str`-specialized parameter is marked too, so
        // the str instance doesn't collide with the `char[]` one (both have
        // `T = char`).
        let mut base = gname.to_string();
        for t in &type_args {
            base.push('$');
            base.push_str(&type_name(t));
        }
        for (i, is_str) in str_arg.iter().enumerate() {
            if *is_str {
                base.push_str(&format!("$s{i}"));
            }
        }
        self.specialize_call_with(&base, &template, ret, args, env, span.clone())
    }

    /// Synthesize a top-level function from a lambda: its parameters (typed from
    /// the expected function type) followed by one parameter per captured
    /// variable. Returns the function's name (it's inserted and queued).
    fn synth_lambda(
        &mut self,
        lparams: &[LambdaParam],
        lbody: &Expr,
        ptys: &[Type],
        lret: &Type,
        captures: &[(String, Type)],
        effects: &[String],
    ) -> String {
        let fn_name = format!("__lambda_{}", self.synth);
        self.synth += 1;
        let mut params: Vec<Param> = lparams
            .iter()
            .zip(ptys)
            .map(|(lp, pty)| Param {
                name: lp.name.clone(),
                ty: pty.clone(),
                mutable: false,
                variadic: false,
            })
            .collect();
        for (cn, ct) in captures {
            params.push(Param {
                name: cn.clone(),
                ty: ct.clone(),
                mutable: false,
                variadic: false,
            });
        }
        self.concrete.insert(
            fn_name.clone(),
            ConcreteTemplate {
                params,
                effects: effects.to_vec(),
                return_ty: Some(lret.clone()),
                body: lbody.clone(),
            },
        );
        self.enqueue_concrete(&fn_name);
        fn_name
    }

    /// Expand the builtin `arr.map(|x| body)` (or `map(arr, |x| body)`) into a
    /// synthesized mapping function with a proper declared return type `U[]`
    /// (where `U` is the lambda body's type). The lambda is lifted like any
    /// other (its captures become parameters threaded through), and the mapping
    /// function holds the `mut out = []; for (..) { out.push(..) } out` loop.
    fn expand_map(
        &mut self,
        arr: &Expr,
        lambda: &Expr,
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let (rarr, arr_ty) = self.infer(arr, env)?;
        let Type::Array(elem) = &arr_ty else {
            return Err(Error::at(
                format!("map expects an array, got {}", type_name(&arr_ty)),
                arr.span.clone(),
            ));
        };
        let elem = (**elem).clone();
        let effects = self.cur_effects.clone();
        // Resolve the per-element mapping function. A lambda literal is lifted
        // (its captures threaded through as extra parameters); a bare function
        // name is used directly (no captures). Either way this yields the
        // function to call per element, the captures to forward, and the result
        // element type `U`.
        let (map_fn, captures, u): (String, Vec<(String, Type)>, Type) = match &lambda.kind {
            ExprKind::Lambda(params, body) => {
                if params.len() != 1 {
                    return Err(Error::at(
                        format!("map's lambda takes 1 parameter, got {}", params.len()),
                        lambda.span.clone(),
                    ));
                }
                let captures = free_vars(body, params, env);
                // `U` is the lambda body's type with its parameter bound to `T`.
                let mut benv = env.clone();
                benv.insert(params[0].name.clone(), elem.clone());
                let (_, u) = self.infer(body, &benv)?;
                // Lift the lambda to `__lambda(x: T, captures..) -> U`.
                let fname =
                    self.synth_lambda(params, body, from_ref(&elem), &u, &captures, &effects);
                (fname, captures, u)
            }
            ExprKind::Ident(g) if self.is_fn_ref(g, env) => {
                // A named function (or builtin): `U` is its return type for an
                // element-typed argument. Ensure it's emitted (no-op for builtins).
                let u = self.ref_return(g, from_ref(&elem));
                self.enqueue_concrete(g);
                (g.clone(), Vec::new(), u)
            }
            _ => {
                return Err(Error::at(
                    "map expects a lambda or a function name, e.g. \"xs.map(|x| ..)\" or \
                     \"xs.map(f)\""
                        .to_string(),
                    lambda.span.clone(),
                ));
            }
        };

        // When the source array is a fresh, uniquely-owned heap value, map can
        // overwrite each slot in place and reuse the same allocation — as long as
        // each mapped `U` fits where a `T` was (`size(U) <= size(T)`) and neither
        // is a composite. Both `T` and `U` non-composite means both are 8-byte
        // (scalars, `str`, arrays), so `U` fits in `T`'s slot and the stored
        // element drop-fn (which differs when `T != U`) is patched per slot. A
        // composite *input* is excluded because the loop variable would be an
        // interior pointer into the buffer that the in-place write could clobber
        // before it's read; a composite *output* (an optional/struct, 16+ bytes)
        // wouldn't fit an 8-byte slot anyway. A `bool` array (in or out) is
        // excluded too — it's bit-packed, so the per-element byte-stride writes
        // the in-place intrinsics emit don't apply.
        let reusable = |t: &Type| {
            !matches!(t, Type::Optional(_))
                && !matches!(t, Type::Primitive(Primitive::Bool))
                && !matches!(t, Type::Named(n) if self.structs.contains_key(n) || self.syn_structs.contains_key(n))
        };
        let in_place = is_fresh_heap(&rarr, &arr_ty) && reusable(&elem) && reusable(&u);

        // The mapping function: `(xs: T[], captures..) -> U[]`.
        let mut map_params = vec![Param {
            name: "$arr".to_string(),
            ty: Type::Array(Box::new(elem.clone())),
            mutable: false,
            variadic: false,
        }];
        let mut call_args = vec![rarr];
        let mut cap_idents: Vec<Expr> = Vec::new();
        for (cn, ct) in &captures {
            let cap = format!("$cap{}", self.synth);
            self.synth += 1;
            map_params.push(Param {
                name: cap.clone(),
                ty: ct.clone(),
                mutable: false,
                variadic: false,
            });
            cap_idents.push(Expr::new(ExprKind::Ident(cap), span.clone()));
            call_args.push(
                self.infer(&Expr::new(ExprKind::Ident(cn.clone()), span.clone()), env)?
                    .0,
            );
        }
        let id = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), span.clone());
        let mut lam_args = vec![id("$e")];
        lam_args.extend(cap_idents);
        let call = Expr::new(ExprKind::Call(map_fn, lam_args, false), span.clone());

        let map_body = if in_place {
            // In-place: overwrite each slot with its mapped value and reuse the
            // allocation. body: `mut $a = $arr;
            //        mut $i = 0;
            //        for (let $e : $a) { __map_set($a, $i, f($e, caps..), $e); set $i = $i + 1; }
            //        __map_result($a)`
            // The length is unchanged, so each slot is read once by the loop and
            // written once; `__map_result` hands the reused buffer back as `U[]`.
            // `__map_set(arr, i, new, old)` stores `new` (a `U`) at slot `i`,
            // taking co-ownership, releases the overwritten `old` (a `T`), and
            // patches the array's stored element drop-fn to `U`'s (it was `T`'s).
            let map_set = Expr::new(
                ExprKind::Call(
                    "__map_set".to_string(),
                    vec![id("$a"), id("$i"), call, id("$e")],
                    false,
                ),
                span.clone(),
            );
            let incr = Expr::new(
                ExprKind::Assign(
                    "$i".to_string(),
                    Box::new(Expr::new(
                        ExprKind::Binop(
                            Box::new(id("$i")),
                            '+',
                            Box::new(Expr::new(ExprKind::Num(1), span.clone())),
                        ),
                        span.clone(),
                    )),
                    Box::new(Expr::new(ExprKind::Unit, span.clone())),
                ),
                span.clone(),
            );
            let for_body = Expr::new(
                ExprKind::Seq(Box::new(map_set), Box::new(incr)),
                span.clone(),
            );
            let loop_ = Expr::new(
                ExprKind::For("$e".to_string(), Box::new(id("$a")), Box::new(for_body)),
                span.clone(),
            );
            // The reused buffer now holds `U` elements, but `$a`'s static type is
            // `T[]`. `__map_result` hands it back reinterpreted as the function's
            // declared return type (`U[]`) — a no-op at runtime (same pointer).
            let result = Expr::new(
                ExprKind::Call("__map_result".to_string(), vec![id("$a")], false),
                span.clone(),
            );
            let inner = Expr::new(
                ExprKind::LetMut(
                    "$i".to_string(),
                    Box::new(Expr::new(ExprKind::Num(0), span.clone())),
                    Box::new(Expr::new(
                        ExprKind::Seq(Box::new(loop_), Box::new(result)),
                        span.clone(),
                    )),
                ),
                span.clone(),
            );
            Expr::new(
                ExprKind::LetMut("$a".to_string(), Box::new(id("$arr")), Box::new(inner)),
                span.clone(),
            )
        } else {
            // Copying path: `mut $out = with_capacity(len($arr));
            //                for (let $e : $arr) { $out.push(f($e, caps..)); } $out`
            // `$out` is pre-sized to the source length: the result has exactly
            // one element per input, so reserving up front turns the `push`
            // loop's doubling reallocations into plain stores into spare capacity.
            let push = Expr::new(
                ExprKind::Call("__builtin_push".to_string(), vec![id("$out"), call], true),
                span.clone(),
            );
            let loop_ = Expr::new(
                ExprKind::For("$e".to_string(), Box::new(id("$arr")), Box::new(push)),
                span.clone(),
            );
            // `with_capacity` pre-sizes only 8-byte-element buffers; an optional
            // (16-byte) or bit-packed `bool` output starts from an empty `[]`
            // that the first push sizes correctly for its representation.
            let out_init = if matches!(&u, Type::Optional(_))
                || matches!(&u, Type::Primitive(Primitive::Bool))
            {
                Expr::new(ExprKind::ArrayLit(Vec::new()), span.clone())
            } else {
                Expr::new(
                    ExprKind::Call(
                        "__builtin_with_capacity".to_string(),
                        vec![Expr::new(
                            ExprKind::Call("__builtin_len".to_string(), vec![id("$arr")], false),
                            span.clone(),
                        )],
                        false,
                    ),
                    span.clone(),
                )
            };
            Expr::new(
                ExprKind::LetMut(
                    "$out".to_string(),
                    Box::new(out_init),
                    Box::new(Expr::new(
                        ExprKind::Seq(Box::new(loop_), Box::new(id("$out"))),
                        span.clone(),
                    )),
                ),
                span.clone(),
            )
        };
        let map_name = format!("__map{}", self.synth);
        self.synth += 1;
        let ret = Type::Array(Box::new(decay_concat(u)));
        self.concrete.insert(
            map_name.clone(),
            ConcreteTemplate {
                params: map_params,
                effects,
                return_ty: Some(ret.clone()),
                body: map_body,
            },
        );
        // The in-place body consumes its array parameter, so emit the *owned*
        // instance (the fresh argument is moved in, not retained). The copying
        // body just borrows it.
        let mangled = if in_place {
            self.enqueue(&map_name, &[], &[0])
        } else {
            self.enqueue_concrete(&map_name);
            map_name
        };
        Ok((
            Expr::new(ExprKind::Call(mangled, call_args, false), span.clone()),
            ret,
        ))
    }

    /// Expand the builtin `zip_with(a, b, f)` into a synthesized function
    /// `($a: T[], $b: U[], captures..) -> R[]` that pairs elements positionally
    /// and applies `f` (a lambda — including an operator passed as a value,
    /// `zip_with(a, b, +)` — or a named function). The result has one element
    /// per pair up to the shorter input (Python `zip` / Haskell `zipWith`):
    /// `[f(a[0], b[0]), f(a[1], b[1]), ...]`.
    ///
    /// The synthesized body relies entirely on existing constructs — no new
    /// codegen. Bounds-checked indexing (`$b[$i]` yields `U?`) gives the
    /// shorter-length cutoff for free: iterating `$a` while `$b[$i]` is `some`
    /// stops as soon as `$b` runs out.
    ///
    /// ```text
    /// mut $out = [];
    /// mut $i = 0;
    /// for (let $e : $a) {
    ///     match ($b[$i]) {
    ///         some($v) => $out.push(f($e, $v, captures..)),
    ///         none => (),
    ///     };
    ///     set $i = $i + 1;
    /// }
    /// $out
    /// ```
    ///
    /// Like `map`, when an input is a fresh, uniquely-owned heap value it is
    /// moved in and its buffer reused for the result (see `in_place_body` in
    /// the function body for the shape). With *both* inputs fresh, both are
    /// moved in and the longer one — only known at runtime — is reused:
    /// `if (len($a) < len($b)) { ..reuse $b.. } else { ..reuse $a.. }`, the
    /// non-reused input released after the loop.
    fn expand_zip_with(
        &mut self,
        arr_a: &Expr,
        arr_b: &Expr,
        f: &Expr,
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let (rarr_a, ty_a) = self.infer(arr_a, env)?;
        let (rarr_b, ty_b) = self.infer(arr_b, env)?;
        let Type::Array(elem_a) = &ty_a else {
            return Err(Error::at(
                format!("zip_with expects an array, got {}", type_name(&ty_a)),
                arr_a.span.clone(),
            ));
        };
        let Type::Array(elem_b) = &ty_b else {
            return Err(Error::at(
                format!("zip_with expects an array, got {}", type_name(&ty_b)),
                arr_b.span.clone(),
            ));
        };
        let elem_a = (**elem_a).clone();
        let elem_b = (**elem_b).clone();
        let effects = self.cur_effects.clone();
        // Resolve the pairing function, mirroring `expand_map`: a lambda literal
        // (including an operator-value's desugared `|lhs, rhs| lhs <op> rhs`) is
        // lifted with its captures threaded as extra params; a bare function
        // name is used directly. Either way this yields the function to call per
        // pair, the captures to forward, and the result element type `R`.
        let (zip_fn, captures, r): (String, Vec<(String, Type)>, Type) = match &f.kind {
            ExprKind::Lambda(params, body) => {
                if params.len() != 2 {
                    return Err(Error::at(
                        format!(
                            "zip_with's function takes 2 parameters, got {}",
                            params.len()
                        ),
                        f.span.clone(),
                    ));
                }
                let captures = free_vars(body, params, env);
                // `R` is the body's type with the two params bound to `T`/`U`.
                let mut benv = env.clone();
                benv.insert(params[0].name.clone(), elem_a.clone());
                benv.insert(params[1].name.clone(), elem_b.clone());
                let (_, r) = self.infer(body, &benv)?;
                let fname = self.synth_lambda(
                    params,
                    body,
                    &[elem_a.clone(), elem_b.clone()],
                    &r,
                    &captures,
                    &effects,
                );
                (fname, captures, r)
            }
            ExprKind::Ident(g) if self.is_fn_ref(g, env) => {
                let r = self.ref_return(g, &[elem_a.clone(), elem_b.clone()]);
                self.enqueue_concrete(g);
                (g.clone(), Vec::new(), r)
            }
            _ => {
                return Err(Error::at(
                    "zip_with expects a binary function or operator, e.g. \
                     \"zip_with(xs, ys, +)\" or \"zip_with(xs, ys, |x, y| ..)\""
                        .to_string(),
                    f.span.clone(),
                ));
            }
        };

        // Like `map`, zip_with can consume a fresh, uniquely-owned input and
        // overwrite its slots in place instead of allocating a fresh output. The
        // reused array is both iterated and overwritten, so its element type and
        // the result type must be 8-byte non-composites (see `expand_map` for
        // the full rationale); the *other* array is only indexed, so its element
        // type is unconstrained.
        let reusable = |t: &Type| {
            !matches!(t, Type::Optional(_))
                && !matches!(t, Type::Primitive(Primitive::Bool))
                && !matches!(t, Type::Named(n) if self.structs.contains_key(n) || self.syn_structs.contains_key(n))
        };
        let own_a = is_fresh_heap(&rarr_a, &ty_a) && reusable(&elem_a) && reusable(&r);
        let own_b = is_fresh_heap(&rarr_b, &ty_b) && reusable(&elem_b) && reusable(&r);

        // The synthesized function: `($a: T[], $b: U[], captures..) -> R[]`.
        let mut zip_params = vec![
            Param {
                name: "$a".to_string(),
                ty: Type::Array(Box::new(elem_a.clone())),
                mutable: false,
                variadic: false,
            },
            Param {
                name: "$b".to_string(),
                ty: Type::Array(Box::new(elem_b.clone())),
                mutable: false,
                variadic: false,
            },
        ];
        let mut call_args = vec![rarr_a, rarr_b];
        let mut cap_idents: Vec<Expr> = Vec::new();
        for (cn, ct) in &captures {
            let cap = format!("$cap{}", self.synth);
            self.synth += 1;
            zip_params.push(Param {
                name: cap.clone(),
                ty: ct.clone(),
                mutable: false,
                variadic: false,
            });
            cap_idents.push(Expr::new(ExprKind::Ident(cap), span.clone()));
            call_args.push(
                self.infer(&Expr::new(ExprKind::Ident(cn.clone()), span.clone()), env)?
                    .0,
            );
        }

        let id = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), span.clone());
        // f($e, $v, captures..) — `$e` is always the `$a`-element and `$v` the
        // `$b`-element, whichever of the two arrays the loop iterates.
        let mut lam_args = vec![id("$e"), id("$v")];
        lam_args.extend(cap_idents);
        let call = Expr::new(ExprKind::Call(zip_fn, lam_args, false), span.clone());
        // set <n> = <n> + 1;
        let incr = |n: &str| {
            Expr::new(
                ExprKind::Assign(
                    n.to_string(),
                    Box::new(Expr::new(
                        ExprKind::Binop(
                            Box::new(id(n)),
                            '+',
                            Box::new(Expr::new(ExprKind::Num(1), span.clone())),
                        ),
                        span.clone(),
                    )),
                    Box::new(Expr::new(ExprKind::Unit, span.clone())),
                ),
                span.clone(),
            )
        };

        // The in-place body, reusing whichever input is owned:
        //
        //   mut $z = <reused>;
        //   mut $i = 0;
        //   mut $w = 0;
        //   for (let <re> : $z) {
        //       match (<other>[$i]) {
        //           some(<oe>) => { __map_set($z, $i, f($e, $v, ..), <re>); set $w = $w + 1; },
        //           none => __filter_drop(<re>),
        //       };
        //       set $i = $i + 1;
        //   }
        //   __filter_drop(<other>);   // only when <other> was moved in too
        //   __filter_truncate($z, $w);
        //   __map_result($z)
        //
        // The loop runs over the *reused* array — each slot is read (`<re>`)
        // before `__map_set` overwrites it, exactly map's discipline — while the
        // other array is indexed, so the shorter-input cutoff works from either
        // side. Once the indexed side runs out, the reused array's remaining
        // elements are released (`__filter_drop`), and the length is truncated
        // to the written count `$w` (a no-op when the reused array was the
        // shorter one).
        let in_place_body = |reuse_a: bool, drop_other: bool, call: Expr| -> Expr {
            // Iterating `$a` binds `$e` and the match binds `$v`; iterating `$b`
            // is the mirror image. Either way `f($e, $v, ..)` sees `$e`/`$v` in
            // the right order.
            let (reused, other, reused_el, other_el) = if reuse_a {
                ("$a", "$b", "$e", "$v")
            } else {
                ("$b", "$a", "$v", "$e")
            };
            let map_set = Expr::new(
                ExprKind::Call(
                    "__map_set".to_string(),
                    vec![id("$z"), id("$i"), call, id(reused_el)],
                    false,
                ),
                span.clone(),
            );
            let some_body = Expr::new(
                ExprKind::Seq(Box::new(map_set), Box::new(incr("$w"))),
                span.clone(),
            );
            let none_body = Expr::new(
                ExprKind::Call("__filter_drop".to_string(), vec![id(reused_el)], false),
                span.clone(),
            );
            let index = Expr::new(
                ExprKind::Index(Box::new(id(other)), Box::new(id("$i"))),
                span.clone(),
            );
            let match_expr = Expr::new(
                ExprKind::Match(
                    Box::new(index),
                    vec![
                        MatchArm {
                            pattern: Pattern::Ctor {
                                name: "some".to_string(),
                                bindings: vec![other_el.to_string()],
                            },
                            body: some_body,
                            span: span.clone(),
                        },
                        MatchArm {
                            pattern: Pattern::Ctor {
                                name: "none".to_string(),
                                bindings: Vec::new(),
                            },
                            body: none_body,
                            span: span.clone(),
                        },
                    ],
                ),
                span.clone(),
            );
            let for_body = Expr::new(
                ExprKind::Seq(Box::new(match_expr), Box::new(incr("$i"))),
                span.clone(),
            );
            let loop_ = Expr::new(
                ExprKind::For(
                    reused_el.to_string(),
                    Box::new(id("$z")),
                    Box::new(for_body),
                ),
                span.clone(),
            );
            let trunc = Expr::new(
                ExprKind::Call(
                    "__filter_truncate".to_string(),
                    vec![id("$z"), id("$w")],
                    false,
                ),
                span.clone(),
            );
            let result = Expr::new(
                ExprKind::Call("__map_result".to_string(), vec![id("$z")], false),
                span.clone(),
            );
            let mut after = Expr::new(
                ExprKind::Seq(Box::new(trunc), Box::new(result)),
                span.clone(),
            );
            if drop_other {
                // Both inputs were moved in: release the one not reused (its
                // elements were only borrowed by `f`).
                let drop_o = Expr::new(
                    ExprKind::Call("__filter_drop".to_string(), vec![id(other)], false),
                    span.clone(),
                );
                after = Expr::new(
                    ExprKind::Seq(Box::new(drop_o), Box::new(after)),
                    span.clone(),
                );
            }
            let inner = Expr::new(
                ExprKind::Seq(Box::new(loop_), Box::new(after)),
                span.clone(),
            );
            let with_w = Expr::new(
                ExprKind::LetMut(
                    "$w".to_string(),
                    Box::new(Expr::new(ExprKind::Num(0), span.clone())),
                    Box::new(inner),
                ),
                span.clone(),
            );
            let with_i = Expr::new(
                ExprKind::LetMut(
                    "$i".to_string(),
                    Box::new(Expr::new(ExprKind::Num(0), span.clone())),
                    Box::new(with_w),
                ),
                span.clone(),
            );
            Expr::new(
                ExprKind::LetMut("$z".to_string(), Box::new(id(reused)), Box::new(with_i)),
                span.clone(),
            )
        };

        let body = if own_a && own_b {
            // Both inputs are fresh: move both in and reuse the *longer* one —
            // known only at runtime, so the body branches on the lengths. Either
            // buffer fits the result (its length is the min); keeping the longer
            // one preserves the larger capacity, and the shorter is freed.
            let len_of = |n: &str| {
                Expr::new(
                    ExprKind::Call("__builtin_len".to_string(), vec![id(n)], false),
                    span.clone(),
                )
            };
            let cond = Expr::new(
                ExprKind::Binop(Box::new(len_of("$a")), '<', Box::new(len_of("$b"))),
                span.clone(),
            );
            Expr::new(
                ExprKind::If(
                    Box::new(cond),
                    Box::new(in_place_body(false, true, call.clone())),
                    Box::new(in_place_body(true, true, call.clone())),
                ),
                span.clone(),
            )
        } else if own_a {
            in_place_body(true, false, call)
        } else if own_b {
            in_place_body(false, false, call)
        } else {
            // Copying path:
            //   mut $out = [];
            //   mut $i = 0;
            //   for (let $e : $a) {
            //       match ($b[$i]) { some($v) => $out.push(f($e, $v, ..)), none => () };
            //       set $i = $i + 1;
            //   }
            //   $out
            let push = Expr::new(
                ExprKind::Call("__builtin_push".to_string(), vec![id("$out"), call], true),
                span.clone(),
            );
            let index = Expr::new(
                ExprKind::Index(Box::new(id("$b")), Box::new(id("$i"))),
                span.clone(),
            );
            let match_expr = Expr::new(
                ExprKind::Match(
                    Box::new(index),
                    vec![
                        MatchArm {
                            pattern: Pattern::Ctor {
                                name: "some".to_string(),
                                bindings: vec!["$v".to_string()],
                            },
                            body: push,
                            span: span.clone(),
                        },
                        MatchArm {
                            pattern: Pattern::Ctor {
                                name: "none".to_string(),
                                bindings: Vec::new(),
                            },
                            body: Expr::new(ExprKind::Unit, span.clone()),
                            span: span.clone(),
                        },
                    ],
                ),
                span.clone(),
            );
            let for_body = Expr::new(
                ExprKind::Seq(Box::new(match_expr), Box::new(incr("$i"))),
                span.clone(),
            );
            let loop_ = Expr::new(
                ExprKind::For("$e".to_string(), Box::new(id("$a")), Box::new(for_body)),
                span.clone(),
            );
            // mut $i = 0; { loop; $out }
            let with_i = Expr::new(
                ExprKind::LetMut(
                    "$i".to_string(),
                    Box::new(Expr::new(ExprKind::Num(0), span.clone())),
                    Box::new(Expr::new(
                        ExprKind::Seq(Box::new(loop_), Box::new(id("$out"))),
                        span.clone(),
                    )),
                ),
                span.clone(),
            );
            // mut $out = []; <with_i>
            Expr::new(
                ExprKind::LetMut(
                    "$out".to_string(),
                    Box::new(Expr::new(ExprKind::ArrayLit(Vec::new()), span.clone())),
                    Box::new(with_i),
                ),
                span.clone(),
            )
        };

        let zip_name = format!("__zip{}", self.synth);
        self.synth += 1;
        let ret = Type::Array(Box::new(r));
        self.concrete.insert(
            zip_name.clone(),
            ConcreteTemplate {
                params: zip_params,
                effects,
                return_ty: Some(ret.clone()),
                body,
            },
        );
        // The in-place bodies consume the fresh input(s), so emit the instance
        // that owns them (the arguments are moved in, not retained); the copying
        // body just borrows both.
        let owned: Vec<usize> = [(own_a, 0), (own_b, 1)]
            .iter()
            .filter(|(own, _)| *own)
            .map(|&(_, i)| i)
            .collect();
        let mangled = if owned.is_empty() {
            self.enqueue_concrete(&zip_name);
            zip_name
        } else {
            self.enqueue(&zip_name, &[], &owned)
        };
        Ok((
            Expr::new(ExprKind::Call(mangled, call_args, false), span.clone()),
            ret,
        ))
    }

    /// Expand the builtin `arr.all(|x| pred)` (or `all(arr, |x| pred)`) into a
    /// synthesized function `(xs: T[], captures..) -> bool` that returns `false`
    /// at the first element failing `pred` and `true` otherwise (so an empty
    /// array is vacuously `true`). The predicate is lifted like any lambda (its
    /// captures threaded through as parameters), or used directly when it's a
    /// named function.
    fn expand_all(
        &mut self,
        arr: &Expr,
        pred: &Expr,
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let (rarr, arr_ty) = self.infer(arr, env)?;
        // The element type, and the parameter type for the synthesized function.
        // A `str` is iterated as its bytes (`char`s) directly — keeping the `str`
        // representation, no `char[]` materialization — mirroring the generic
        // str-as-`char[]` rule.
        let (elem, arr_param_ty) = match &arr_ty {
            Type::Array(inner) => ((**inner).clone(), arr_ty.clone()),
            Type::Primitive(Primitive::Str) => (
                Type::Primitive(Primitive::Char),
                Type::Primitive(Primitive::Str),
            ),
            _ => {
                return Err(Error::at(
                    format!("all expects an array or string, got {}", type_name(&arr_ty)),
                    arr.span.clone(),
                ))
            }
        };
        let effects = self.cur_effects.clone();
        // Resolve the predicate `(T) -> bool`: lift a lambda (threading captures
        // through), or use a named function directly.
        let (pred_fn, captures): (String, Vec<(String, Type)>) = match &pred.kind {
            ExprKind::Lambda(params, body) => {
                if params.len() != 1 {
                    return Err(Error::at(
                        format!("all's lambda takes 1 parameter, got {}", params.len()),
                        pred.span.clone(),
                    ));
                }
                let captures = free_vars(body, params, env);
                let fname = self.synth_lambda(
                    params,
                    body,
                    from_ref(&elem),
                    &Type::Primitive(Primitive::Bool),
                    &captures,
                    &effects,
                );
                (fname, captures)
            }
            ExprKind::Ident(g) if self.is_fn_ref(g, env) => {
                self.enqueue_concrete(g);
                (g.clone(), Vec::new())
            }
            _ => {
                return Err(Error::at(
                    "all expects a lambda or a function name, e.g. \"xs.all(|x| ..)\" or \
                     \"xs.all(f)\""
                        .to_string(),
                    pred.span.clone(),
                ));
            }
        };

        // The `all` function: `(xs: T[], captures..) -> bool`. Params/args mirror
        // `expand_filter`: the array, then one parameter per capture.
        let id = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), span.clone());
        let mut all_params = vec![Param {
            name: "$arr".to_string(),
            ty: arr_param_ty,
            mutable: false,
            variadic: false,
        }];
        let mut call_args = vec![rarr];
        let mut pred_args = vec![id("$e")];
        for (cn, ct) in &captures {
            let cap = format!("$cap{}", self.synth);
            self.synth += 1;
            all_params.push(Param {
                name: cap.clone(),
                ty: ct.clone(),
                mutable: false,
                variadic: false,
            });
            pred_args.push(id(&cap));
            call_args.push(self.infer(&id(cn), env)?.0);
        }
        // body: `for (let $e : $arr) { if (!pred($e, caps..)) { return false; }; } true`
        let cond = Expr::new(
            ExprKind::Not(Box::new(Expr::new(
                ExprKind::Call(pred_fn, pred_args, false),
                span.clone(),
            ))),
            span.clone(),
        );
        let ret_false = Expr::new(
            ExprKind::Return(Box::new(Expr::new(ExprKind::Bool(false), span.clone()))),
            span.clone(),
        );
        let guarded = Expr::new(
            ExprKind::If(
                Box::new(cond),
                Box::new(ret_false),
                Box::new(Expr::new(ExprKind::Unit, span.clone())),
            ),
            span.clone(),
        );
        let loop_ = Expr::new(
            ExprKind::For("$e".to_string(), Box::new(id("$arr")), Box::new(guarded)),
            span.clone(),
        );
        let body = Expr::new(
            ExprKind::Seq(
                Box::new(loop_),
                Box::new(Expr::new(ExprKind::Bool(true), span.clone())),
            ),
            span.clone(),
        );

        let all_name = format!("__all{}", self.synth);
        self.synth += 1;
        let ret = Type::Primitive(Primitive::Bool);
        self.concrete.insert(
            all_name.clone(),
            ConcreteTemplate {
                params: all_params,
                effects,
                return_ty: Some(ret.clone()),
                body,
            },
        );
        self.enqueue_concrete(&all_name);
        Ok((
            Expr::new(ExprKind::Call(all_name, call_args, false), span.clone()),
            ret,
        ))
    }

    /// Expand the builtin `arr.filter(|x| pred)` (or `filter(arr, |x| pred)`)
    /// into a synthesized function `(xs: T[], captures..) -> T[]` that keeps each
    /// element for which the predicate holds. The predicate is lifted like any
    /// lambda (its captures threaded through as parameters), or used directly
    /// when it's a named function. The element type is preserved.
    fn expand_filter(
        &mut self,
        arr: &Expr,
        pred: &Expr,
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let (rarr, arr_ty) = self.infer(arr, env)?;
        let Type::Array(elem) = &arr_ty else {
            return Err(Error::at(
                format!("filter expects an array, got {}", type_name(&arr_ty)),
                arr.span.clone(),
            ));
        };
        let elem = (**elem).clone();
        let effects = self.cur_effects.clone();
        // When the source array is a fresh, uniquely-owned heap value (an array
        // literal or a call result like `xs.map(..)`), it's safe to consume it
        // and filter *in place* — compacting kept elements toward the front and
        // reusing the same allocation — instead of building a fresh output.
        // Optional elements are excluded: the in-place intrinsics move 8-byte
        // slots, but a `T?` element is 16 bytes. `bool` is excluded too — it's
        // bit-packed, so per-element byte moves don't apply.
        let in_place = !matches!(&elem, Type::Optional(_))
            && !matches!(&elem, Type::Primitive(Primitive::Bool))
            && is_fresh_heap(&rarr, &arr_ty);
        // Resolve the predicate `(T) -> bool`: lift a lambda (threading its
        // captures through), or use a named function directly (no captures).
        let (pred_fn, captures): (String, Vec<(String, Type)>) = match &pred.kind {
            ExprKind::Lambda(params, body) => {
                if params.len() != 1 {
                    return Err(Error::at(
                        format!("filter's lambda takes 1 parameter, got {}", params.len()),
                        pred.span.clone(),
                    ));
                }
                let captures = free_vars(body, params, env);
                // Lift to `__lambda(x: T, captures..) -> bool`.
                let fname = self.synth_lambda(
                    params,
                    body,
                    from_ref(&elem),
                    &Type::Primitive(Primitive::Bool),
                    &captures,
                    &effects,
                );
                (fname, captures)
            }
            ExprKind::Ident(g) if self.is_fn_ref(g, env) => {
                self.enqueue_concrete(g);
                (g.clone(), Vec::new())
            }
            _ => {
                return Err(Error::at(
                    "filter expects a lambda or a function name, e.g. \"xs.filter(|x| ..)\" or \
                     \"xs.filter(f)\""
                        .to_string(),
                    pred.span.clone(),
                ));
            }
        };

        // The filter function: `(xs: T[], captures..) -> T[]`.
        let mut filter_params = vec![Param {
            name: "$arr".to_string(),
            ty: Type::Array(Box::new(elem.clone())),
            mutable: false,
            variadic: false,
        }];
        let mut call_args = vec![rarr];
        let mut cap_idents: Vec<Expr> = Vec::new();
        for (cn, ct) in &captures {
            let cap = format!("$cap{}", self.synth);
            self.synth += 1;
            filter_params.push(Param {
                name: cap.clone(),
                ty: ct.clone(),
                mutable: false,
                variadic: false,
            });
            cap_idents.push(Expr::new(ExprKind::Ident(cap), span.clone()));
            call_args.push(
                self.infer(&Expr::new(ExprKind::Ident(cn.clone()), span.clone()), env)?
                    .0,
            );
        }
        let id = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), span.clone());
        let mut pred_args = vec![id("$e")];
        pred_args.extend(cap_idents);
        let cond = Expr::new(ExprKind::Call(pred_fn, pred_args, false), span.clone());

        let filter_body = if in_place {
            // body: `mut $a = $arr;
            //        mut $w = 0;
            //        for (let $e : $a) {
            //            if (pred($e, caps..)) { __filter_keep($a, $w, $e); set $w = $w + 1; }
            //            else { __filter_drop($e); }
            //        }
            //        __filter_truncate($a, $w); $a`
            // Two-pointer compaction: `$w` is the write cursor, `$e` the read.
            // `__filter_keep` moves a kept element to slot `$w` (raw pointer copy
            // — ownership relocates, no refcount change). `__filter_drop` releases
            // a filtered-out element. `__filter_truncate` sets the length to `$w`;
            // the now-dead tail is never released. The `for` loop reads `len` each
            // step, so the length must stay fixed until after the loop. Moves only
            // ever write a slot `<= ` the current read index, so unread elements
            // are never clobbered.
            let keep = Expr::new(
                ExprKind::Call(
                    "__filter_keep".to_string(),
                    vec![id("$a"), id("$w"), id("$e")],
                    false,
                ),
                span.clone(),
            );
            let incr = Expr::new(
                ExprKind::Assign(
                    "$w".to_string(),
                    Box::new(Expr::new(
                        ExprKind::Binop(
                            Box::new(id("$w")),
                            '+',
                            Box::new(Expr::new(ExprKind::Num(1), span.clone())),
                        ),
                        span.clone(),
                    )),
                    Box::new(Expr::new(ExprKind::Unit, span.clone())),
                ),
                span.clone(),
            );
            let then_branch =
                Expr::new(ExprKind::Seq(Box::new(keep), Box::new(incr)), span.clone());
            let drop_e = Expr::new(
                ExprKind::Call("__filter_drop".to_string(), vec![id("$e")], false),
                span.clone(),
            );
            let guarded = Expr::new(
                ExprKind::If(Box::new(cond), Box::new(then_branch), Box::new(drop_e)),
                span.clone(),
            );
            let loop_ = Expr::new(
                ExprKind::For("$e".to_string(), Box::new(id("$a")), Box::new(guarded)),
                span.clone(),
            );
            let trunc = Expr::new(
                ExprKind::Call(
                    "__filter_truncate".to_string(),
                    vec![id("$a"), id("$w")],
                    false,
                ),
                span.clone(),
            );
            let after = Expr::new(
                ExprKind::Seq(Box::new(trunc), Box::new(id("$a"))),
                span.clone(),
            );
            let inner = Expr::new(
                ExprKind::LetMut(
                    "$w".to_string(),
                    Box::new(Expr::new(ExprKind::Num(0), span.clone())),
                    Box::new(Expr::new(
                        ExprKind::Seq(Box::new(loop_), Box::new(after)),
                        span.clone(),
                    )),
                ),
                span.clone(),
            );
            Expr::new(
                ExprKind::LetMut("$a".to_string(), Box::new(id("$arr")), Box::new(inner)),
                span.clone(),
            )
        } else {
            // Copying path: `mut $out = with_capacity(len($arr));
            //                for (let $e : $arr) { if (pred($e, caps..)) { $out.push($e); } else {} }
            //                $out`
            // `$out` is reserved to the source length — an upper bound, since
            // filter keeps at most every element — so the `push` loop never
            // reallocates. `push` yields unit, so the empty `else` matches.
            let push = Expr::new(
                ExprKind::Call(
                    "__builtin_push".to_string(),
                    vec![id("$out"), id("$e")],
                    true,
                ),
                span.clone(),
            );
            let guarded = Expr::new(
                ExprKind::If(
                    Box::new(cond),
                    Box::new(push),
                    Box::new(Expr::new(ExprKind::Unit, span.clone())),
                ),
                span.clone(),
            );
            let loop_ = Expr::new(
                ExprKind::For("$e".to_string(), Box::new(id("$arr")), Box::new(guarded)),
                span.clone(),
            );
            // `with_capacity` pre-sizes only 8-byte-element buffers; an optional
            // (16-byte) or bit-packed `bool` output starts from an empty `[]`
            // that the first push sizes correctly for its representation.
            let out_init = if matches!(&elem, Type::Optional(_))
                || matches!(&elem, Type::Primitive(Primitive::Bool))
            {
                Expr::new(ExprKind::ArrayLit(Vec::new()), span.clone())
            } else {
                Expr::new(
                    ExprKind::Call(
                        "__builtin_with_capacity".to_string(),
                        vec![Expr::new(
                            ExprKind::Call("__builtin_len".to_string(), vec![id("$arr")], false),
                            span.clone(),
                        )],
                        false,
                    ),
                    span.clone(),
                )
            };
            Expr::new(
                ExprKind::LetMut(
                    "$out".to_string(),
                    Box::new(out_init),
                    Box::new(Expr::new(
                        ExprKind::Seq(Box::new(loop_), Box::new(id("$out"))),
                        span.clone(),
                    )),
                ),
                span.clone(),
            )
        };

        let filter_name = format!("__filter{}", self.synth);
        self.synth += 1;
        let ret = Type::Array(Box::new(elem));
        self.concrete.insert(
            filter_name.clone(),
            ConcreteTemplate {
                params: filter_params,
                effects,
                return_ty: Some(ret.clone()),
                body: filter_body,
            },
        );
        // The in-place body consumes its array parameter, so emit the *owned*
        // instance (the fresh argument is moved in, not retained). The copying
        // body just borrows it.
        let mangled = if in_place {
            self.enqueue(&filter_name, &[], &[0])
        } else {
            self.enqueue_concrete(&filter_name);
            filter_name
        };
        Ok((
            Expr::new(ExprKind::Call(mangled, call_args, false), span.clone()),
            ret,
        ))
    }

    /// Expand the builtin `arr.enumerate()` into a synthesized function
    /// `($arr: T[]) -> (i64, T)[]` that pairs each element with its 0-based
    /// index: `[a, b, c].enumerate()` → `[(0,a),(1,b),(2,c)]`.
    fn expand_enumerate(
        &mut self,
        arr: &Expr,
        env: &Env,
        span: Span,
    ) -> Result<(Expr, Type), Error> {
        let (rarr, arr_ty) = self.infer(arr, env)?;
        let (elem, param_ty) = match &arr_ty {
            Type::Array(inner) => ((**inner).clone(), arr_ty.clone()),
            Type::Primitive(Primitive::Str) => (Type::Primitive(Primitive::Char), arr_ty.clone()),
            _ => {
                return Err(Error::at(
                    format!(
                        "enumerate expects an array or str, got {}",
                        type_name(&arr_ty)
                    ),
                    arr.span.clone(),
                ))
            }
        };
        let effects = self.cur_effects.clone();

        let id = |n: &str| Expr::new(ExprKind::Ident(n.to_string()), span.clone());

        // Body:
        //   mut $i = 0;
        //   mut $out = [];
        //   for (let $e : $arr) { $out.push(($i, $e)); set $i = $i + 1; }
        //   $out
        let tuple = Expr::new(ExprKind::TupleLit(vec![id("$i"), id("$e")]), span.clone());
        let push = Expr::new(
            ExprKind::Call("__builtin_push".to_string(), vec![id("$out"), tuple], true),
            span.clone(),
        );
        let incr = Expr::new(
            ExprKind::Assign(
                "$i".to_string(),
                Box::new(Expr::new(
                    ExprKind::Binop(
                        Box::new(id("$i")),
                        '+',
                        Box::new(Expr::new(ExprKind::Num(1), span.clone())),
                    ),
                    span.clone(),
                )),
                Box::new(Expr::new(ExprKind::Unit, span.clone())),
            ),
            span.clone(),
        );
        let for_body = Expr::new(ExprKind::Seq(Box::new(push), Box::new(incr)), span.clone());
        let loop_ = Expr::new(
            ExprKind::For("$e".to_string(), Box::new(id("$arr")), Box::new(for_body)),
            span.clone(),
        );
        let enumerate_body = Expr::new(
            ExprKind::LetMut(
                "$i".to_string(),
                Box::new(Expr::new(ExprKind::Num(0), span.clone())),
                Box::new(Expr::new(
                    ExprKind::LetMut(
                        "$out".to_string(),
                        Box::new(Expr::new(ExprKind::ArrayLit(Vec::new()), span.clone())),
                        Box::new(Expr::new(
                            ExprKind::Seq(Box::new(loop_), Box::new(id("$out"))),
                            span.clone(),
                        )),
                    ),
                    span.clone(),
                )),
            ),
            span.clone(),
        );

        let enumerate_name = format!("__enumerate{}", self.synth);
        self.synth += 1;
        // Return type: (i64, T)[]. The tuple struct is registered by TupleLit
        // inference when the synthesized body is compiled.
        let tuple_name = check::tuple_struct_name(&[Type::Primitive(Primitive::I64), elem.clone()]);
        let ret = Type::Array(Box::new(Type::Named(tuple_name)));
        self.concrete.insert(
            enumerate_name.clone(),
            ConcreteTemplate {
                params: vec![Param {
                    name: "$arr".to_string(),
                    ty: param_ty,
                    mutable: false,
                    variadic: false,
                }],
                effects,
                return_ty: Some(ret.clone()),
                body: enumerate_body,
            },
        );
        self.enqueue_concrete(&enumerate_name);
        Ok((
            Expr::new(
                ExprKind::Call(enumerate_name, vec![rarr], false),
                span.clone(),
            ),
            ret,
        ))
    }

    /// Mark a concrete function reachable in its plain borrow form, queueing it
    /// the first time it's seen. A non-concrete name (builtin, generic,
    /// undeclared) is ignored — generics go through `instantiate_types`, and
    /// codegen reports any undefined fn.
    fn enqueue_concrete(&mut self, name: &str) {
        if self.concrete.contains_key(name) {
            self.enqueue(name, &[], &[]);
        }
    }

    /// Queue an instance if its mangled name hasn't been seen, returning the
    /// name. The mangle encodes the concrete type of every `any`-bearing
    /// parameter (`same_length$i64$char`) and, for an owned instance, an `$own`
    /// suffix per moved-in parameter (`extended$own0`) — so the borrow and owned
    /// forms are distinct functions.
    fn enqueue(&mut self, template: &str, type_args: &[Type], owned_params: &[usize]) -> String {
        let n = owned_params.iter().copied().max().map_or(0, |m| m + 1);
        let mut params = vec![ParamSpec::default(); n];
        for &i in owned_params {
            params[i].owned = true;
        }
        self.enqueue_full(
            template,
            ParamSpecs {
                type_args: type_args.to_vec(),
                params,
            },
        )
    }

    /// Queue a specialized instance (see [`ParamSpecs`]) if its mangled name
    /// hasn't been seen, returning the name. Each specialization marker is folded
    /// into the mangled name so distinct specializations are distinct functions:
    /// `$<type>` per type arg, then `$own{i}` (owned), `$s{i}` (`char[]`-kept-as-
    /// `str`), `$c{i}` (concat-str), `$ve{i}`/`$vo{i}` (variadic element/optional)
    /// — grouped by marker, ascending parameter index within each group.
    fn enqueue_full(&mut self, template: &str, specs: ParamSpecs) -> String {
        let mut mangled = template.to_string();
        for t in &specs.type_args {
            mangled.push('$');
            mangled.push_str(&type_name(t));
        }
        for i in specs.indices(|p| p.owned) {
            mangled.push_str(&format!("$own{i}"));
        }
        for i in specs.indices(|p| p.str_kept) {
            mangled.push_str(&format!("$s{i}"));
        }
        for i in specs.indices(|p| p.concat) {
            mangled.push_str(&format!("$c{i}"));
        }
        for i in specs.indices(|p| p.variadic == VShape::Elem) {
            mangled.push_str(&format!("$ve{i}"));
        }
        for i in specs.indices(|p| p.variadic == VShape::Opt) {
            mangled.push_str(&format!("$vo{i}"));
        }
        if self.emitted.insert(mangled.clone()) {
            self.dbg
                .trace("mono", format_args!("enqueue instance `{mangled}`"));
            self.queue.push_back(Instance {
                template: template.to_string(),
                specs,
                mangled: mangled.clone(),
            });
        }
        mangled
    }

    /// Resolve a call to generic `gname`: infer the concrete type of each type
    /// variable from the argument types (unifying repeated variables), returning
    /// the type arguments and the concrete return type. Does not queue — the
    /// caller decides ownership and enqueues the right instance.
    fn instantiate_types(
        &mut self,
        gname: &str,
        arg_tys: &[Type],
        span: Span,
    ) -> Result<(Vec<Type>, Type), Error> {
        // Copy the bits we need so we don't borrow `self.generics` across the
        // `&mut self` enqueue.
        let (type_vars, param_tys, return_ty) = {
            let Generic { sig, .. } = &self.generics[gname];
            (
                sig.type_vars.clone(),
                sig.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>(),
                sig.return_ty.clone(),
            )
        };
        let var_set: HashSet<&str> = type_vars.iter().map(String::as_str).collect();
        let mut map: HashMap<String, Type> = HashMap::new();
        for (pty, aty) in param_tys.iter().zip(arg_tys) {
            collect_bindings(pty, aty, &var_set, &mut map, gname, span.clone())?;
        }
        // Fallback pass: any type variable that no concrete arg pinned can
        // still be inferred from an empty-array or bare-`none` argument — the
        // resulting instance accepts the corresponding pseudo-type
        // (`EmptyArray` / `NoneLiteral`), substituted to `Array(__none__)` /
        // `Optional(__none__)` for codegen.
        for v in &type_vars {
            if map.contains_key(v) {
                continue;
            }
            for (pty, aty) in param_tys.iter().zip(arg_tys) {
                if let Some(marker) = pseudo_marker(pty, aty, v) {
                    map.insert(v.clone(), marker);
                    break;
                }
            }
        }
        let mut targs = Vec::with_capacity(type_vars.len());
        for v in &type_vars {
            let t = map.get(v).ok_or_else(|| {
                Error::at(
                    format!(
                        "cannot infer a type for \"{}\" in generic \"{gname}\"",
                        display_var(v)
                    ),
                    span.clone(),
                )
            })?;
            targs.push(t.clone());
        }
        let ret = match &return_ty {
            Some(t) => subst_vars(t, &map),
            None => Type::Primitive(Primitive::I64),
        };
        Ok((targs, ret))
    }

    /// Which parameters the call to `template` (with the given concrete
    /// `type_args`) takes ownership of: a parameter the template is
    /// owned-eligible for, whose argument is a fresh, uniquely-owned heap value.
    /// Empty unless an owned instance is warranted.
    fn owned_for_call(
        &self,
        template: &str,
        type_args: &[Type],
        args: &[Expr],
        arg_tys: &[Type],
    ) -> Vec<usize> {
        let (params, return_ty, body) = self.concrete_signature(template, type_args);
        match owned_eligible(template, &params, &return_ty, &body) {
            Some(i) if i < args.len() && is_fresh_heap(&args[i], &arg_tys[i]) => vec![i],
            _ => vec![],
        }
    }

    /// The concrete signature (params, return type, body) of `template` under
    /// `type_args` — substituting a generic's type variables, or returning a
    /// concrete fn's signature directly. Used for ownership eligibility.
    fn concrete_signature(
        &self,
        template: &str,
        type_args: &[Type],
    ) -> (Vec<Param>, Option<Type>, Expr) {
        if let Some(Generic { sig, body }) = self.generics.get(template) {
            let (params, return_ty) = sig.make_concrete(type_args);
            (params, return_ty, body.clone())
        } else if let Some(f) = self.concrete.get(template) {
            (f.params.clone(), f.return_ty.clone(), f.body.clone())
        } else {
            // Not a user function (e.g. a builtin) — never owned-eligible.
            (Vec::new(), None, Expr::new(ExprKind::Unit, 0..0))
        }
    }

    /// Resolve the concrete return type of a non-generic call (builtin or user
    /// fn). Permissive: an unknown callee falls back to Unit — codegen issues
    /// the real "undefined fn"/missing-import diagnostic.
    fn call_return(&self, name: &str, arg_tys: &[Type]) -> Type {
        if let Some(t) = builtin_return(name, arg_tys) {
            return t;
        }
        // A variant constructor `Ctor(..)` yields its variant type.
        if let Some(vn) = self.ctors.get(name) {
            return Type::Named(vn.clone());
        }
        self.fn_returns.get(name).cloned().unwrap_or(Type::Unit)
    }

    /// Payload types a `match` arm binds: the optional's element for `some`, a
    /// variant case's payload positionally, else a permissive `i64` fill (the
    /// checker has already validated the pattern).
    fn match_payload_tys(&self, scrut: &Type, ctor: &str, n: usize) -> Vec<Type> {
        match scrut {
            Type::Optional(inner) if ctor == "some" => vec![(**inner).clone()],
            // A void-Ok (`!E`) binds nothing in its `ok` arm.
            Type::Result(ok, _) if ctor == "ok" && is_unit(ok) => vec![],
            Type::Result(ok, _) if ctor == "ok" => vec![(**ok).clone()],
            Type::Result(_, err) if ctor == "err" => vec![(**err).clone()],
            Type::Named(name) => self
                .variants
                .get(name)
                .and_then(|cases| cases.iter().find(|(c, _)| c == ctor))
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| vec![Type::Primitive(Primitive::I64); n]),
            _ => vec![Type::Primitive(Primitive::I64); n],
        }
    }

    /// Is `name` a reference to a top-level function usable as a *value* (passed
    /// to a higher-order function)? True for a concrete user function or a
    /// builtin, but not for a name bound as a local in `env` (which shadows any
    /// global) nor a generic (the checker rejects generic functions as values).
    fn is_fn_ref(&self, name: &str, env: &Env) -> bool {
        !env.contains_key(name)
            && (self.concrete.contains_key(name) || name.starts_with("__builtin_"))
    }

    /// The declared return type of a resolved function value `name` called with
    /// `arg_tys`: a concrete/synthesized function's declared return, else a
    /// builtin's computed return, else unit.
    fn ref_return(&self, name: &str, arg_tys: &[Type]) -> Type {
        self.concrete
            .get(name)
            .and_then(|f| f.return_ty.clone())
            .or_else(|| builtin_return(name, arg_tys))
            .unwrap_or(Type::Unit)
    }

    /// Infer `expr`'s concrete type while rewriting any generic call names to
    /// their mangled instances. Returns the rewritten expression and its type.
    fn infer(&mut self, expr: &Expr, env: &Env) -> Result<(Expr, Type), Error> {
        let span = expr.span.clone();
        let node = |kind| Expr::new(kind, span.clone());
        Ok(match &expr.kind {
            ExprKind::Unit => (expr.clone(), Type::Unit),
            ExprKind::Num(_) => (expr.clone(), Type::Primitive(Primitive::I64)),
            ExprKind::Bool(_) => (expr.clone(), Type::Primitive(Primitive::Bool)),
            ExprKind::Str(_) => (expr.clone(), Type::Primitive(Primitive::Str)),
            ExprKind::Char(_) => (expr.clone(), Type::Primitive(Primitive::Char)),
            ExprKind::None => (expr.clone(), Type::Optional(Box::new(none_inner_ty()))),
            ExprKind::Ident(name) => {
                // A bare name is a local binding, or — if unbound — a nullary
                // variant constructor (e.g. `Empty`), whose type is its variant.
                let ty = env.get(name).cloned().unwrap_or_else(|| {
                    self.ctors
                        .get(name)
                        .map(|vn| Type::Named(vn.clone()))
                        .unwrap_or_else(|| Type::Primitive(Primitive::I64))
                });
                (expr.clone(), ty)
            }
            ExprKind::Neg(inner) => {
                let (ri, _) = self.infer(inner, env)?;
                (
                    node(ExprKind::Neg(Box::new(ri))),
                    Type::Primitive(Primitive::I64),
                )
            }
            ExprKind::Not(inner) => {
                let (ri, _) = self.infer(inner, env)?;
                (
                    node(ExprKind::Not(Box::new(ri))),
                    Type::Primitive(Primitive::Bool),
                )
            }
            ExprKind::Binop(l, op, r) => {
                let (rl, lt) = self.infer(l, env)?;
                let (rr, rt) = self.infer(r, env)?;
                // A bare literal operand flexes to the other's integer type
                // (the checker verified the fit); keeps mono's types consistent
                // with codegen so e.g. `i8_val + 1` infers as `i8`.
                let lt = aipl_syntax::flex_int_ty(&rl, &lt, &rt);
                let rt = aipl_syntax::flex_int_ty(&rr, &rt, &lt);
                let ty = match op {
                    // String concatenation builds a lazy concat node (see
                    // `aipl_concat_lazy`), so its result carries the *concat-str*
                    // representation — which a downstream `fn(s: str)` call uses to
                    // select a concat-specialized instance. (`Error` concatenates
                    // like `str` too.)
                    '+' if is_str_repr(&lt) && is_str_repr(&rt) => concat_str_ty(),
                    // Same-integer-type arithmetic keeps that width/signedness.
                    '+' | '-' | '*' | '/' | '%' if aipl_syntax::is_int_ty(&lt) && lt == rt => {
                        lt.clone()
                    }
                    '+' | '-' | '*' | '/' | '%' => Type::Primitive(Primitive::I64),
                    _ => Type::Primitive(Primitive::Bool), // comparison / logical
                };
                (node(ExprKind::Binop(Box::new(rl), *op, Box::new(rr))), ty)
            }
            ExprKind::ArrayLit(elems) => {
                let mut relems = Vec::with_capacity(elems.len());
                let mut elem_ty = none_inner_ty();
                for (i, e) in elems.iter().enumerate() {
                    let (re, t) = self.infer(e, env)?;
                    if i == 0 {
                        elem_ty = t;
                    }
                    relems.push(re);
                }
                (
                    node(ExprKind::ArrayLit(relems)),
                    Type::Array(Box::new(decay_concat(elem_ty))),
                )
            }
            ExprKind::TupleLit(elems) => {
                let mut elem_tys: Vec<Type> = Vec::with_capacity(elems.len());
                let mut relems: Vec<Expr> = Vec::with_capacity(elems.len());
                for e in elems {
                    let (re, t) = self.infer(e, env)?;
                    elem_tys.push(decay_concat(t));
                    relems.push(re);
                }
                let name = check::tuple_struct_name(&elem_tys);
                // Only add to syn_structs if not already injected by lower_tuples.
                if !self.structs.contains_key(&name) && !self.syn_structs.contains_key(&name) {
                    let fields: Vec<(String, Type, Option<Expr>)> = elem_tys
                        .iter()
                        .enumerate()
                        .map(|(i, t)| (format!("_{i}"), t.clone(), None))
                        .collect();
                    self.syn_structs.insert(name.clone(), fields);
                }
                let inits: Vec<aipl_syntax::ast::FieldInit> = relems
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| aipl_syntax::ast::FieldInit {
                        name: format!("_{i}"),
                        value: v,
                    })
                    .collect();
                (
                    node(ExprKind::Construct(name.clone(), inits)),
                    Type::Named(name),
                )
            }
            ExprKind::SetLit(elems) => {
                let mut relems = Vec::with_capacity(elems.len());
                let mut elem_ty = none_inner_ty();
                for (i, e) in elems.iter().enumerate() {
                    let (re, t) = self.infer(e, env)?;
                    if i == 0 {
                        elem_ty = t;
                    }
                    relems.push(re);
                }
                (
                    node(ExprKind::SetLit(relems)),
                    Type::Set(Box::new(decay_concat(elem_ty))),
                )
            }
            ExprKind::DictLit(pairs) => {
                let mut rpairs = Vec::with_capacity(pairs.len());
                let mut key_ty = none_inner_ty();
                let mut val_ty = none_inner_ty();
                for (i, (k, v)) in pairs.iter().enumerate() {
                    let (rk, kt) = self.infer(k, env)?;
                    let (rv, vt) = self.infer(v, env)?;
                    if i == 0 {
                        key_ty = kt;
                        val_ty = vt;
                    }
                    rpairs.push((rk, rv));
                }
                (
                    node(ExprKind::DictLit(rpairs)),
                    Type::Dict(
                        Box::new(decay_concat(key_ty)),
                        Box::new(decay_concat(val_ty)),
                    ),
                )
            }
            ExprKind::Index(obj, idx) => {
                let (ro, ot) = self.infer(obj, env)?;
                let (ridx, _) = self.infer(idx, env)?;
                let elem = match ot {
                    Type::Array(inner) => *inner,
                    // `s[i]` on a `str` is the byte at `i` as a `char?`.
                    Type::Primitive(Primitive::Str) => Type::Primitive(Primitive::Char),
                    _ => none_inner_ty(),
                };
                // Indexing yields `elem?` — for a `T?[]` that's a genuine `T??`.
                (
                    node(ExprKind::Index(Box::new(ro), Box::new(ridx))),
                    Type::Optional(Box::new(elem)),
                )
            }
            ExprKind::Slice(obj, start, end) => {
                let (ro, _) = self.infer(obj, env)?;
                let (rs, _) = self.infer(start, env)?;
                let re = match end {
                    Some(e) => Some(Box::new(self.infer(e, env)?.0)),
                    None => None,
                };
                (
                    node(ExprKind::Slice(Box::new(ro), Box::new(rs), re)),
                    Type::Primitive(Primitive::Str),
                )
            }
            ExprKind::Try(inner) => {
                // `expr?` yields the Ok type of `expr`'s result; codegen emits
                // the unwrap / early-return-Err.
                let (rin, it) = self.infer(inner, env)?;
                let ok = match it {
                    Type::Result(ok, _) => *ok,
                    _ => none_inner_ty(),
                };
                (node(ExprKind::Try(Box::new(rin))), ok)
            }
            ExprKind::Field(obj, fname) => {
                let (ro, ot) = self.infer(obj, env)?;
                let fty = match &ot {
                    Type::Named(sn) => self
                        .structs
                        .get(sn)
                        .or_else(|| self.syn_structs.get(sn))
                        .and_then(|fs| fs.iter().find(|(n, _, _)| n == fname))
                        .map(|(_, t, _)| t.clone())
                        .unwrap_or_else(|| Type::Primitive(Primitive::I64)),
                    _ => Type::Primitive(Primitive::I64),
                };
                (node(ExprKind::Field(Box::new(ro), fname.clone())), fty)
            }
            ExprKind::If(c, t, e) => {
                let (rc, _) = self.infer(c, env)?;
                let (rt, tt) = self.infer(t, env)?;
                let (re, et) = self.infer(e, env)?;
                let ty = merge(tt, et);
                (
                    node(ExprKind::If(Box::new(rc), Box::new(rt), Box::new(re))),
                    ty,
                )
            }
            ExprKind::Seq(first, rest) => {
                // Evaluate `first` for effect (no binding), then yield `rest`.
                let (rf, _) = self.infer(first, env)?;
                let (rr, rt) = self.infer(rest, env)?;
                (node(ExprKind::Seq(Box::new(rf), Box::new(rr))), rt)
            }
            ExprKind::Return(value) => {
                // Rewrite the returned value; the `return` itself yields unit.
                let (rv, _) = self.infer(value, env)?;
                (node(ExprKind::Return(Box::new(rv))), Type::Unit)
            }
            // Lambdas as arguments to plain (Call-form) higher-order functions
            // are handled by `specialize_call`. Reaching here means a lambda in
            // a position that isn't supported yet (e.g. a method-call receiver
            // or argument).
            ExprKind::Lambda(_, _) => {
                return Err(Error::at(
                    "a lambda is only supported as an argument to a plain higher-order function \
                     call, e.g. `map(xs, |x| ..)`"
                        .to_string(),
                    span.clone(),
                ));
            }
            ExprKind::Let(name, val, body) => {
                let (rv, vt) = self.infer(val, env)?;
                let mut env2 = env.clone();
                env2.insert(name.clone(), vt);
                let (rb, bt) = self.infer(body, &env2)?;
                (
                    node(ExprKind::Let(name.clone(), Box::new(rv), Box::new(rb))),
                    bt,
                )
            }
            ExprKind::LetMut(name, val, body) => {
                let (rv, vt) = self.infer(val, env)?;
                let mut env2 = env.clone();
                env2.insert(name.clone(), vt);
                let (rb, bt) = self.infer(body, &env2)?;
                (
                    node(ExprKind::LetMut(name.clone(), Box::new(rv), Box::new(rb))),
                    bt,
                )
            }
            ExprKind::Assign(name, val, body) => {
                let (rv, _) = self.infer(val, env)?;
                // Codegen compiles the body under the unchanged env.
                let (rb, bt) = self.infer(body, env)?;
                (
                    node(ExprKind::Assign(name.clone(), Box::new(rv), Box::new(rb))),
                    bt,
                )
            }
            ExprKind::For(var, iter, body) => {
                let (ri, it) = self.infer(iter, env)?;
                let elem = match it {
                    Type::Array(inner) => *inner,
                    _ => Type::Primitive(Primitive::Char), // str iteration binds char
                };
                let mut env2 = env.clone();
                env2.insert(var.clone(), elem);
                let (rb, _) = self.infer(body, &env2)?;
                (
                    node(ExprKind::For(var.clone(), Box::new(ri), Box::new(rb))),
                    Type::Primitive(Primitive::I64),
                )
            }
            ExprKind::While(cond, body) => {
                // Condition type (must be bool) is enforced in codegen, like the
                // `if` condition; here we just infer both sides. The loop yields
                // i64 0 (its value is discarded), matching `for`.
                let (rc, _) = self.infer(cond, env)?;
                let (rb, _) = self.infer(body, env)?;
                (
                    node(ExprKind::While(Box::new(rc), Box::new(rb))),
                    Type::Primitive(Primitive::I64),
                )
            }
            ExprKind::Match(scrut, arms) => {
                let (rs, st) = self.infer(scrut, env)?;
                let mut rarms = Vec::with_capacity(arms.len());
                let mut merged: Option<Type> = None;
                for arm in arms {
                    // Bind the arm's payload from the scrutinee's type: the
                    // optional's element for `some`, or the variant case's
                    // payload types positionally. String-literal / wildcard arms
                    // bind nothing.
                    let bind_tys = match &arm.pattern {
                        Pattern::Ctor { name, bindings } => {
                            self.match_payload_tys(&st, name, bindings.len())
                        }
                        Pattern::Str(_) | Pattern::Array(_) | Pattern::Wildcard => Vec::new(),
                    };
                    let mut env2 = env.clone();
                    for (name, ty) in arm.pattern.bindings().iter().zip(bind_tys) {
                        env2.insert(name.clone(), ty);
                    }
                    let (rb, t) = self.infer(&arm.body, &env2)?;
                    rarms.push(MatchArm {
                        pattern: arm.pattern.clone(),
                        body: rb,
                        span: arm.span.clone(),
                    });
                    merged = Some(match merged {
                        None => t,
                        Some(prev) => merge(prev, t),
                    });
                }
                (
                    node(ExprKind::Match(Box::new(rs), rarms)),
                    merged.unwrap_or(Type::Primitive(Primitive::I64)),
                )
            }
            ExprKind::Construct(name, inits) => {
                // Expand to a complete field list in struct-definition order,
                // filling missing fields from their declared defaults.
                let field_defs = self
                    .structs
                    .get(name.as_str())
                    .or_else(|| self.syn_structs.get(name.as_str()))
                    .cloned()
                    .unwrap_or_default();
                let mut rfields = Vec::with_capacity(field_defs.len());
                for (fname, _, fdefault) in &field_defs {
                    let src = if let Some(fi) = inits.iter().find(|i| &i.name == fname) {
                        fi.value.clone()
                    } else if let Some(def) = fdefault {
                        def.clone()
                    } else {
                        // Checker already caught this; surface a clean error just in case.
                        return Err(Error::at(
                            format!("struct {name:?} field {fname:?} has no default and was not provided"),
                            span.clone(),
                        ));
                    };
                    let (rv, _) = self.infer(&src, env)?;
                    rfields.push(aipl_syntax::ast::FieldInit {
                        name: fname.clone(),
                        value: rv,
                    });
                }
                (
                    node(ExprKind::Construct(name.clone(), rfields)),
                    Type::Named(name.clone()),
                )
            }
            ExprKind::Call(name, args, method_style) => {
                let method_style = *method_style;
                // Builtin `map`/`filter`: `map(arr, f)` and `arr.map(f)` fold to
                // the same arg list `[arr, f]`, so one path serves both forms.
                if name == "__builtin_map" {
                    if args.len() != 2 {
                        return Err(Error::at(
                            format!(
                                "map takes an array and a lambda, got {} argument(s)",
                                args.len()
                            ),
                            span.clone(),
                        ));
                    }
                    return self.expand_map(&args[0], &args[1], env, span.clone());
                }
                if name == "__builtin_filter" {
                    if args.len() != 2 {
                        return Err(Error::at(
                            format!(
                                "filter takes an array and a predicate, got {} argument(s)",
                                args.len()
                            ),
                            span.clone(),
                        ));
                    }
                    return self.expand_filter(&args[0], &args[1], env, span.clone());
                }
                if name == "__builtin_all" {
                    if args.len() != 2 {
                        return Err(Error::at(
                            format!(
                                "all takes an array and a predicate, got {} argument(s)",
                                args.len()
                            ),
                            span.clone(),
                        ));
                    }
                    return self.expand_all(&args[0], &args[1], env, span.clone());
                }
                if name == "__builtin_zip_with" {
                    if args.len() != 3 {
                        return Err(Error::at(
                            format!(
                                "zip_with takes two arrays and a binary function, got {} \
                                 argument(s)",
                                args.len()
                            ),
                            span.clone(),
                        ));
                    }
                    return self.expand_zip_with(&args[0], &args[1], &args[2], env, span.clone());
                }
                if name == "__builtin_enumerate" {
                    if args.len() != 1 {
                        return Err(Error::at(
                            format!("enumerate takes an array, got {} argument(s)", args.len()),
                            span.clone(),
                        ));
                    }
                    return self.expand_enumerate(&args[0], env, span.clone());
                }
                // A direct call through a function-typed binding: `f(x)` where
                // `f` is a lambda parameter of this (specialized) function.
                // Rewrite to call the synthesized function, forwarding captures.
                // (Free-call form only — a method name is never a lambda binding.)
                if !method_style {
                    if let Some(lb) = self.cur_lenv.get(name).cloned() {
                        let mut rargs = Vec::with_capacity(args.len() + lb.captures.len());
                        let mut atys = Vec::with_capacity(args.len());
                        for a in args {
                            let (ra, t) = self.infer(a, env)?;
                            rargs.push(ra);
                            atys.push(t);
                        }
                        // The bound function may be a synthesized lambda (concrete),
                        // a named user function, or a builtin — `ref_return` covers
                        // all three. (Captures aren't part of the builtin's args.)
                        let ret = self.ref_return(&lb.fn_name, &atys);
                        rargs.extend(lb.captures);
                        return Ok((node(ExprKind::Call(lb.fn_name, rargs, false)), ret));
                    }
                }
                // The free-call form `foo(b, rest..)` of a mutating function is
                // sugar for copy-and-modify: copy `b`, mutate the copy, and yield
                // it, leaving the original `b` untouched. Desugar to
                // `{ mut __mut_copy = b; __mut_copy.foo(rest..); __mut_copy }` and
                // re-infer — the method form below then handles the mutation (and
                // any generic mangling). An explicit `b.foo(..)` skips this: it
                // mutates `b` in place.
                if !method_style && self.mutating.contains(name) && !args.is_empty() {
                    let tmp = "__mut_copy".to_string();
                    let recv = || Expr::new(ExprKind::Ident(tmp.clone()), span.clone());
                    let mut margs = Vec::with_capacity(args.len());
                    margs.push(recv());
                    margs.extend(args[1..].iter().cloned());
                    let method = node(ExprKind::Call(name.clone(), margs, true));
                    let seq = node(ExprKind::Seq(Box::new(method), Box::new(recv())));
                    let desugared = node(ExprKind::LetMut(
                        tmp.clone(),
                        Box::new(args[0].clone()),
                        Box::new(seq),
                    ));
                    return self.infer(&desugared, env);
                }
                // A higher-order call passing lambda (or forwarded) arguments is
                // specialized for those lambdas (lifting/reusing their functions
                // and threading captures in). The receiver is already `args[0]`
                // for the method form, so the folded arg list specializes
                // identically. Mutating methods are excluded — their in-place ABI
                // isn't a plain call (the free form was desugared just above).
                let has_fn_arg = args.iter().any(|a| match &a.kind {
                    ExprKind::Lambda(_, _) => true,
                    ExprKind::Ident(n) => self.cur_lenv.contains_key(n) || self.is_fn_ref(n, env),
                    _ => false,
                });
                if self.concrete.contains_key(name) && !self.mutating.contains(name) && has_fn_arg {
                    let template = self.concrete[name].clone();
                    let ret = self.fn_returns.get(name).cloned().unwrap_or(Type::Unit);
                    return self.specialize_call_with(
                        name,
                        &template,
                        ret,
                        args,
                        env,
                        span.clone(),
                    );
                }
                // A *generic* higher-order call (e.g. `xs.count_while(|x| ..)` over
                // `T[]`): infer the type variables from the non-lambda arguments,
                // substitute them into a concrete template, then specialize that for
                // the lambdas — the union of generic monomorphization and lambda
                // lifting.
                if self.generics.contains_key(name) && !self.mutating.contains(name) && has_fn_arg {
                    return self.specialize_generic_call(name, args, env, span.clone());
                }
                let mut rargs = Vec::with_capacity(args.len());
                let mut atys = Vec::with_capacity(args.len());
                for a in args {
                    let (ra, t) = self.infer(a, env)?;
                    rargs.push(ra);
                    atys.push(t);
                }
                // A method call's args aren't move-eligible (a mutating method's
                // store-back path owns the receiver); only free calls compute the
                // owned-parameter set used for the move optimization.
                if self.generics.contains_key(name) {
                    let (type_args, ret) = self.instantiate_types(name, &atys, span.clone())?;
                    let owned = if method_style {
                        Vec::new()
                    } else {
                        self.owned_for_call(name, &type_args, args, &atys)
                    };
                    // Per-parameter specialization: each parameter records whether
                    // it's moved in (`owned`) and whether a `str` argument hit a
                    // `char[]`/`T[]` parameter (`str_kept` — specialize on the str
                    // directly, no `char[]` materialization; `T` is still `char`,
                    // so only the `char[]`-substituted parameter is marked).
                    let params: Vec<ParamSpec> = {
                        let chars = Type::Array(Box::new(Type::Primitive(Primitive::Char)));
                        let Generic { sig, .. } = &self.generics[name];
                        let tmap: HashMap<String, Type> = sig
                            .type_vars
                            .iter()
                            .cloned()
                            .zip(type_args.iter().cloned())
                            .collect();
                        sig.params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| ParamSpec {
                                owned: owned.contains(&i),
                                str_kept: atys.get(i) == Some(&Type::Primitive(Primitive::Str))
                                    && subst_vars(&p.ty, &tmap) == chars,
                                ..ParamSpec::default()
                            })
                            .collect()
                    };
                    let mangled = self.enqueue_full(name, ParamSpecs { type_args, params });
                    (node(ExprKind::Call(mangled, rargs, method_style)), ret)
                } else if self.concrete.contains_key(name) {
                    // A function-typed parameter must be supplied with a lambda,
                    // a named function, or a forwarded function parameter (all of
                    // which route to `specialize_call` above). Reaching here with
                    // a `Fn` parameter (free-call form) means it was none of those.
                    if !method_style
                        && self.concrete[name]
                            .params
                            .iter()
                            .any(|p| matches!(p.ty, Type::Fn(_, _)))
                    {
                        return Err(Error::at(
                            format!(
                                "fn \"{name}\" takes a function parameter; pass a lambda or a \
                                 function by name"
                            ),
                            span.clone(),
                        ));
                    }
                    // Variadic (`T*`) parameters specialize per argument shape: a
                    // sequence keeps the plain instance, a single element / optional
                    // gets a distinct instance whose body rebuilds the sequence (see
                    // `specialize_variadic`). Non-variadic parameters are `Seq`.
                    let shapes: Vec<VShape> = self.concrete[name]
                        .params
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            if p.variadic {
                                atys.get(i)
                                    .map_or(VShape::Seq, |a| variadic_shape(a, &p.ty))
                            } else {
                                VShape::Seq
                            }
                        })
                        .collect();
                    let mut owned = if method_style {
                        Vec::new()
                    } else {
                        self.owned_for_call(name, &[], args, &atys)
                    };
                    // A specialized (element/optional) variadic parameter is
                    // borrowed — its prologue retains the value into the rebuilt
                    // sequence — so it never moves in.
                    owned.retain(|i| shapes[*i] == VShape::Seq);
                    // Per-parameter specialization: a moved-in (`owned`) parameter,
                    // a variadic specialized to an element/optional, and a `str`
                    // parameter passed a *concat-str* argument (`concat` — retyped
                    // to the concat sentinel in the processing loop).
                    let params: Vec<ParamSpec> = self.concrete[name]
                        .params
                        .iter()
                        .enumerate()
                        .map(|(i, p)| ParamSpec {
                            owned: owned.contains(&i),
                            concat: matches!(p.ty, Type::Primitive(Primitive::Str))
                                && atys.get(i).is_some_and(is_concat_str),
                            variadic: shapes[i],
                            ..ParamSpec::default()
                        })
                        .collect();
                    let mangled = self.enqueue_full(
                        name,
                        ParamSpecs {
                            type_args: Vec::new(),
                            params,
                        },
                    );
                    let ret = self.call_return(name, &atys);
                    (node(ExprKind::Call(mangled, rargs, method_style)), ret)
                } else if (name == "__builtin_starts_with" || name == "__builtin_ends_with")
                    && atys.len() == 2
                {
                    // The variadic pattern of `starts_with`/`ends_with` is resolved
                    // by shape into a distinct builtin (codegen implements each):
                    // a `str` receiver takes a `char*` pattern, an array a `T*` one.
                    let seq_ty = if is_str_repr(&atys[0]) {
                        Type::Primitive(Primitive::Str)
                    } else {
                        atys[0].clone()
                    };
                    let resolved = match variadic_shape(&atys[1], &seq_ty) {
                        VShape::Seq => name.clone(),
                        VShape::Elem => format!("{name}$ve"),
                        VShape::Opt => format!("{name}$vo"),
                    };
                    let ret = self.call_return(name, &atys);
                    (node(ExprKind::Call(resolved, rargs, method_style)), ret)
                } else {
                    // Builtin (`push`, `len`, …) or undefined (codegen reports
                    // the latter).
                    let ret = self.call_return(name, &atys);
                    (node(ExprKind::Call(name.clone(), rargs, method_style)), ret)
                }
            }
        })
    }
}

/// Match a (normalized) parameter type against an argument type, recording the
/// concrete type each type variable binds to. Repeated variables must unify.
///
/// An argument that carries no information for a variable — a bare `none` for a
/// `T?` parameter, an empty array for a `T[]` one, or a shape that doesn't match
/// the parameter — is silently skipped: the variable may still be pinned by
/// another parameter, and any genuine shape mismatch is reported by codegen
/// against the resulting concrete instance. Only a variable left unbound after
/// all parameters yields a "cannot infer" error (in `instantiate_call`).
fn collect_bindings(
    param_ty: &Type,
    arg_ty: &Type,
    vars: &HashSet<&str>,
    map: &mut HashMap<String, Type>,
    gname: &str,
    span: Span,
) -> Result<(), Error> {
    match param_ty {
        Type::Named(v) if vars.contains(v.as_str()) => bind(v, arg_ty, map, gname, span.clone()),
        Type::Optional(inner) if ty_contains_var(inner, vars) => match arg_ty {
            Type::Optional(a) if !is_none_inner(a) => {
                collect_bindings(inner, a, vars, map, gname, span.clone())
            }
            _ => Ok(()),
        },
        Type::Array(inner) if ty_contains_var(inner, vars) => match arg_ty {
            Type::Array(a) if !is_none_inner(a) => {
                collect_bindings(inner, a, vars, map, gname, span.clone())
            }
            // `str` is usable as `char[]` — pin the element variable to `char`.
            Type::Primitive(Primitive::Str) => collect_bindings(
                inner,
                &Type::Primitive(Primitive::Char),
                vars,
                map,
                gname,
                span.clone(),
            ),
            _ => Ok(()),
        },
        Type::Set(inner) if ty_contains_var(inner, vars) => match arg_ty {
            Type::Set(a) if !is_none_inner(a) => {
                collect_bindings(inner, a, vars, map, gname, span.clone())
            }
            _ => Ok(()),
        },
        Type::Dict(pk, pv) if ty_contains_var(pk, vars) || ty_contains_var(pv, vars) => {
            match arg_ty {
                Type::Dict(ak, av) => {
                    if !is_none_inner(ak) {
                        collect_bindings(pk, ak, vars, map, gname, span.clone())?;
                    }
                    if !is_none_inner(av) {
                        collect_bindings(pv, av, vars, map, gname, span.clone())?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        Type::Result(po, pe) if ty_contains_var(po, vars) || ty_contains_var(pe, vars) => {
            match arg_ty {
                Type::Result(ao, ae) => {
                    if !is_none_inner(ao) {
                        collect_bindings(po, ao, vars, map, gname, span.clone())?;
                    }
                    if !is_none_inner(ae) {
                        collect_bindings(pe, ae, vars, map, gname, span.clone())?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        // No type variable in this parameter — codegen checks the concrete fit.
        _ => Ok(()),
    }
}

/// Bind type variable `v` to `ty`, enforcing a valid primitive and that all
/// uses of `v` agree.
fn bind(
    v: &str,
    ty: &Type,
    map: &mut HashMap<String, Type>,
    gname: &str,
    span: Span,
) -> Result<(), Error> {
    if !is_array_elem(ty) {
        return Err(Error::at(
            format!(
                "\"{}\" in \"{gname}\" resolved to {}, which is not a valid type argument (i64, bool, or char)",
                display_var(v),
                type_name(ty)
            ),
            span.clone(),
        ));
    }
    match map.get(v) {
        None => {
            map.insert(v.to_string(), ty.clone());
            Ok(())
        }
        Some(prev) if prev == ty => Ok(()),
        Some(prev) => Err(Error::at(
            format!(
                "conflicting types for \"{}\" in \"{gname}\": {} vs {}",
                display_var(v),
                type_name(prev),
                type_name(ty)
            ),
            span.clone(),
        )),
    }
}

/// If `param_ty` mentions `v` exactly inside `T[]` (with `arg_ty` the empty
/// array literal) or inside `T?` (with `arg_ty` bare `none`), return the
/// pseudo marker to bind `v` to. Returns `None` when no such pairing applies.
fn pseudo_marker(param_ty: &Type, arg_ty: &Type, v: &str) -> Option<Type> {
    match (param_ty, arg_ty) {
        (Type::Array(inner), Type::Array(a))
            if matches!(inner.as_ref(), Type::Named(n) if n == v) && is_none_inner(a) =>
        {
            Some(empty_array_arg_ty())
        }
        (Type::Optional(inner), Type::Optional(a))
            if matches!(inner.as_ref(), Type::Named(n) if n == v) && is_none_inner(a) =>
        {
            Some(none_literal_arg_ty())
        }
        _ => None,
    }
}

/// [`aipl_syntax::BUILTIN_SIGNATURES`] parsed once and indexed by name, each
/// normalized (via [`normalize`]) into a [`Signature`] — the same declarations
/// the checker resolves builtin calls against, reused here so mono's own
/// return-type inference doesn't hand-duplicate them. A builtin is never
/// enqueued/specialized like a real [`Generic`], so only its `Signature` is
/// kept (the body `normalize` produces alongside it is discarded).
fn builtin_sigs() -> &'static HashMap<String, Signature> {
    static SIGS: OnceLock<HashMap<String, Signature>> = OnceLock::new();
    SIGS.get_or_init(|| {
        let program =
            aipl_parser::parse(BUILTIN_SIGNATURES).expect("builtin signatures are valid AIPL");
        program
            .items
            .into_iter()
            .filter_map(|item| match item {
                Item::Fn(f) => {
                    let name = f.name.clone();
                    let g = normalize(&f).expect("builtin signatures are valid AIPL");
                    Some((name, g.sig))
                }
                _ => None,
            })
            .collect()
    })
}

/// If `param_ty` binds type variable `v` (directly, or nested in a container:
/// `T[]`, `T?`, `#{T}`, a dict's key/value, or a result's ok/err side),
/// return the type `v` resolves to given `arg_ty` in the matching position —
/// even if that's the `__none__` marker (an empty array's element, or the
/// bare `none` literal's inner). Unlike [`collect_bindings`] (which backs real
/// monomorphization, restricting a bound variable to a specializable element
/// type and erroring on a mismatch), this only feeds a *return type*
/// computation for mono's own inference: any type is a valid binding, and a
/// shape that doesn't match the parameter yields `None` (the variable may
/// still be pinned by a different parameter — see [`declared_builtin_return`]).
fn bind_builtin_var(param_ty: &Type, arg_ty: &Type, v: &str) -> Option<Type> {
    match param_ty {
        Type::Named(p) if p == v => Some(arg_ty.clone()),
        Type::Optional(inner) if ty_mentions(inner, v) => match arg_ty {
            Type::Optional(a) => bind_builtin_var(inner, a, v),
            _ => None,
        },
        Type::Array(inner) if ty_mentions(inner, v) => match arg_ty {
            Type::Array(a) => bind_builtin_var(inner, a, v),
            // `str` is usable as `char[]` — pin the element variable to `char`.
            Type::Primitive(Primitive::Str) => Some(Type::Primitive(Primitive::Char)),
            _ => None,
        },
        Type::Set(inner) if ty_mentions(inner, v) => match arg_ty {
            Type::Set(a) => bind_builtin_var(inner, a, v),
            _ => None,
        },
        Type::Dict(pk, pv) if ty_mentions(pk, v) || ty_mentions(pv, v) => match arg_ty {
            Type::Dict(ak, av) if ty_mentions(pk, v) => bind_builtin_var(pk, ak, v),
            Type::Dict(_, av) => bind_builtin_var(pv, av, v),
            _ => None,
        },
        Type::Result(po, pe) if ty_mentions(po, v) || ty_mentions(pe, v) => match arg_ty {
            Type::Result(ao, _) if ty_mentions(po, v) => bind_builtin_var(po, ao, v),
            Type::Result(_, ae) => bind_builtin_var(pe, ae, v),
            _ => None,
        },
        _ => None,
    }
}

/// Return type of a builtin declared in [`aipl_syntax::BUILTIN_SIGNATURES`],
/// substituting its type variables from `arg_tys`. `None` if `name` isn't
/// declared there (an internal/synthetic name, or one of `builtin_return`'s
/// own special cases).
fn declared_builtin_return(name: &str, arg_tys: &[Type]) -> Option<Type> {
    let sig = builtin_sigs().get(name)?;
    let mut map: HashMap<String, Type> = HashMap::new();
    for v in &sig.type_vars {
        // Prefer the first parameter whose argument concretely pins `v`;
        // a `__none__` binding (an empty-array/bare-`none` argument, e.g.
        // `value_or`'s uninformative `self` side) only wins if nothing else
        // does, so a later, more informative parameter still takes over.
        let mut none_ish: Option<Type> = None;
        for (p, aty) in sig.params.iter().zip(arg_tys) {
            match bind_builtin_var(&p.ty, aty, v) {
                Some(t) if !is_none_inner(&t) => {
                    map.insert(v.clone(), t);
                    break;
                }
                Some(t) => {
                    none_ish.get_or_insert(t);
                }
                None => {}
            }
        }
        map.entry(v.clone())
            .or_insert_with(|| none_ish.unwrap_or(Type::Primitive(Primitive::I64)));
    }
    Some(match &sig.return_ty {
        Some(t) => subst_vars(t, &map),
        None => Type::Unit,
    })
}

/// Return type of a builtin call, or `None` if `name` isn't a builtin. Most
/// builtins substitute directly from their [`aipl_syntax::BUILTIN_SIGNATURES`]
/// declaration via [`declared_builtin_return`]; the cases here are the ones
/// where mono's own inference genuinely diverges from that signature (a
/// different result than the declared one, or a synthetic/internal name not
/// declared there at all).
fn builtin_return(name: &str, arg_tys: &[Type]) -> Option<Type> {
    // Integer conversion builtins `i8(x)`/`u32(x)`/… yield the named width.
    if let Some(p) = Primitive::from_name(name).filter(|p| p.is_int()) {
        return Some(Type::Primitive(p));
    }
    match name {
        // Internal: an empty array reserved to a given capacity (`map`'s output).
        // Untyped element (`__none__`) like `[]`; refined by the first `push`.
        "__builtin_with_capacity" => return Some(Type::Array(Box::new(none_inner_ty()))),
        // Declared void (it's a statement); mono needs an `i64` value for the
        // expression it emits (see the `expr`-position uses of `print`).
        "__builtin_print" => return Some(Type::Primitive(Primitive::I64)),
        // Internal: a single `char` to a one-char `str`, emitted by variadic
        // `char*` specialization (see `specialize_variadic`).
        "__char_to_str" => return Some(Type::Primitive(Primitive::Str)),
        // `xs.reverse() -> T[]` / `s.reverse() -> str` — same type as the input.
        // The declared signature is `T[] -> T[]`; a `str` receiver (which
        // `collect_bindings`-style unification would bind as `char[]`) instead
        // dispatches to a `str` result, so this stays hand-written.
        "__builtin_reverse" => {
            return Some(match arg_tys.first() {
                Some(t) if is_str_repr(t) => Type::Primitive(Primitive::Str),
                Some(Type::Array(inner)) => Type::Array(inner.clone()),
                _ => Type::Array(Box::new(none_inner_ty())),
            })
        }
        // Declared void (it mutates `self` in place); mono treats the call as
        // yielding the array it was given (first effective arg).
        "__builtin_push" => {
            return Some(
                arg_tys
                    .first()
                    .cloned()
                    .unwrap_or(Type::Primitive(Primitive::I64)),
            )
        }
        // some(x) wraps the value's type. Not expressible via plain signature
        // substitution: a concat-str argument must decay to plain `str` before
        // it's wrapped (see `decay_concat`).
        "some" => {
            return Some(Type::Optional(Box::new(decay_concat(
                arg_tys
                    .first()
                    .cloned()
                    .unwrap_or(Type::Primitive(Primitive::I64)),
            ))))
        }
        // ok(x)/err(e) pin one side of a result; the other is `__none__`,
        // resolved by the expected result type via coercion (like `none`).
        // With an arg, `ok(x)` pins the Ok type to `x`; with none, `ok()` is the
        // void success of a `!E` result (Ok side is unit). Not declared in
        // `BUILTIN_SIGNATURES` (the checker special-cases them the same way).
        "ok" => {
            return Some(Type::Result(
                Box::new(decay_concat(arg_tys.first().cloned().unwrap_or(Type::Unit))),
                Box::new(none_inner_ty()),
            ))
        }
        "err" => {
            return Some(Type::Result(
                Box::new(none_inner_ty()),
                Box::new(decay_concat(
                    arg_tys
                        .first()
                        .cloned()
                        .unwrap_or(Type::Primitive(Primitive::I64)),
                )),
            ))
        }
        // Internal in-place-filter intrinsics (statements; see `expand_filter`).
        "__filter_keep" | "__filter_drop" | "__filter_truncate" => return Some(Type::Unit),
        // Internal in-place-map intrinsic (a statement; see `expand_map`).
        "__map_set" => return Some(Type::Unit),
        // Internal: reinterpret the reused buffer as the result element type.
        // Its real result type is fixed by codegen (the enclosing fn's return);
        // here it just borrows the input array type so mono inference proceeds.
        "__map_result" => {
            return Some(
                arg_tys
                    .first()
                    .cloned()
                    .unwrap_or(Type::Primitive(Primitive::I64)),
            )
        }
        _ => {}
    }
    declared_builtin_return(name, arg_tys)
}

/// Merge two branch/arm types, applying the same `none`/empty-array coercions
/// codegen uses. Permissive: a genuine mismatch is left for codegen to report.
/// Drop the internal concat-str representation back to plain `str`. The concat
/// marker (see [`aipl_syntax::CONCAT_STR`]) is only meaningful as the type of a
/// *scalar value* flowing to a `str` parameter; once a value is placed into a
/// compound type (array element, set/dict member, optional inner, struct field,
/// result payload, …) the container is a homogeneous `str` container, so the
/// marker is dropped — keeping the sentinel out of derived types (where codegen
/// keys element drop/retain/heap-ness on an exact `str`).
fn decay_concat(t: Type) -> Type {
    if is_concat_str(&t) {
        Type::Primitive(Primitive::Str)
    } else {
        t
    }
}

fn merge(a: Type, b: Type) -> Type {
    if a == b || is_none_inner(&a) {
        return b;
    }
    if is_none_inner(&b) {
        return a;
    }
    // A concat-str merged with another str-repr (plain `str`/`Error`) degrades to
    // that general representation: a mixed-provenance value isn't guaranteed to be
    // a concat node, so it must not drive concat specialization. (Two concats took
    // the `a == b` path above.)
    if is_concat_str(&a) && is_str_repr(&b) {
        return b;
    }
    if is_concat_str(&b) && is_str_repr(&a) {
        return a;
    }
    // Recurse through matching layers so a `__none__` core deep in a chain (e.g.
    // `some(some(none))`) takes the other branch's concrete core.
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

/// Names that can't be used as a type-parameter name.
const RESERVED_TYPE_NAMES: &[&str] = &["i64", "bool", "char", "str", "any"];

/// A function is generic if it declares type parameters or uses anonymous
/// `any` in a parameter.
fn is_generic(f: &Function) -> bool {
    !f.type_params.is_empty() || f.params.iter().any(|p| ty_mentions(&p.ty, "any"))
}

/// Validate a generic function's signature and normalize it: collect its type
/// variables (declared names, then one synthetic per anonymous `any[]`/`any?`
/// parameter) and rewrite anonymous `any` to those synthetic names.
fn normalize(f: &Function) -> Result<Generic, Error> {
    let mut type_vars: Vec<String> = Vec::new();
    let mut declared: HashSet<String> = HashSet::new();
    for tp in &f.type_params {
        if RESERVED_TYPE_NAMES.contains(&tp.as_str()) {
            return Err(Error::msg(format!(
                "fn \"{}\": \"{tp}\" is not a valid type parameter name (reserved)",
                f.name
            )));
        }
        if !declared.insert(tp.clone()) {
            return Err(Error::msg(format!(
                "fn \"{}\": duplicate type parameter \"{tp}\"",
                f.name
            )));
        }
        type_vars.push(tp.clone());
    }

    // Normalize parameter types: anonymous `any` → a fresh synthetic variable.
    let mut counter = 0usize;
    let mut params = Vec::with_capacity(f.params.len());
    for p in &f.params {
        let ty = normalize_param_ty(&p.ty, &mut type_vars, &mut counter, &f.name)?;
        params.push(Param {
            name: p.name.clone(),
            ty,
            mutable: p.mutable,
            variadic: p.variadic,
        });
    }

    // The return type may reference declared type parameters, but never the
    // anonymous `any` keyword (it has no name to bind).
    if f.return_ty.as_ref().is_some_and(|t| ty_mentions(t, "any")) {
        return Err(Error::msg(format!(
            "fn \"{}\": bare \"any\" is not allowed in a return type; declare \"<T: any>\" and return \"T\"",
            f.name
        )));
    }

    // Every declared type parameter must appear in a parameter, or it could
    // never be inferred from a call.
    for tp in &f.type_params {
        if !params.iter().any(|p| ty_mentions(&p.ty, tp)) {
            return Err(Error::msg(format!(
                "fn \"{}\": type parameter \"{tp}\" is not used by any parameter, so it can't be inferred",
                f.name
            )));
        }
    }

    Ok(Generic {
        sig: Signature {
            type_vars,
            params,
            return_ty: f.return_ty.clone(),
            effects: f.effects.clone(),
        },
        body: f.body.clone(),
    })
}

/// Normalize a parameter type, replacing each anonymous `any` (only valid as an
/// array element or optional inner) with a fresh synthetic type variable.
fn normalize_param_ty(
    t: &Type,
    type_vars: &mut Vec<String>,
    counter: &mut usize,
    fname: &str,
) -> Result<Type, Error> {
    match t {
        Type::Any => Err(Error::msg(format!(
            "fn \"{fname}\": bare \"any\" is not allowed; use \"any[]\", \"any?\", or a named type parameter \"<T: any>\""
        ))),
        Type::Primitive(_)
        | Type::Named(_)
        | Type::Unit
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => Ok(t.clone()),
        Type::Optional(inner) => Ok(Type::Optional(Box::new(normalize_inner(
            inner, type_vars, counter,
        )))),
        Type::Array(inner) => Ok(Type::Array(Box::new(normalize_inner(
            inner, type_vars, counter,
        )))),
        Type::Set(inner) => Ok(Type::Set(Box::new(normalize_inner(
            inner, type_vars, counter,
        )))),
        Type::Dict(k, v) => Ok(Type::Dict(
            Box::new(normalize_inner(k, type_vars, counter)),
            Box::new(normalize_inner(v, type_vars, counter)),
        )),
        Type::Result(ok, err) => Ok(Type::Result(
            Box::new(normalize_inner(ok, type_vars, counter)),
            Box::new(normalize_inner(err, type_vars, counter)),
        )),
        Type::Fn(params, ret) => {
            let ps = params
                .iter()
                .map(|p| normalize_param_ty(p, type_vars, counter, fname))
                .collect::<Result<Vec<_>, _>>()?;
            let r = normalize_param_ty(ret, type_vars, counter, fname)?;
            Ok(Type::Fn(ps, Box::new(r)))
        }
        Type::Tuple(elems) => {
            let es = elems
                .iter()
                .map(|e| normalize_param_ty(e, type_vars, counter, fname))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Type::Tuple(es))
        }
    }
}

/// Normalize the element/inner type of an array/optional parameter: anonymous
/// `any` becomes a fresh synthetic variable; anything else is kept as-is.
fn normalize_inner(t: &Type, type_vars: &mut Vec<String>, counter: &mut usize) -> Type {
    match t {
        Type::Any => {
            let name = format!("$any{counter}");
            *counter += 1;
            type_vars.push(name.clone());
            Type::Named(name)
        }
        Type::Primitive(_)
        | Type::Named(_)
        | Type::Unit
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => t.clone(),
        // The surface grammar can't nest deeper, but recurse defensively.
        Type::Optional(inner) => {
            Type::Optional(Box::new(normalize_inner(inner, type_vars, counter)))
        }
        Type::Array(inner) => Type::Array(Box::new(normalize_inner(inner, type_vars, counter))),
        Type::Set(inner) => Type::Set(Box::new(normalize_inner(inner, type_vars, counter))),
        Type::Dict(k, v) => Type::Dict(
            Box::new(normalize_inner(k, type_vars, counter)),
            Box::new(normalize_inner(v, type_vars, counter)),
        ),
        Type::Result(ok, err) => Type::Result(
            Box::new(normalize_inner(ok, type_vars, counter)),
            Box::new(normalize_inner(err, type_vars, counter)),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|p| normalize_inner(p, type_vars, counter))
                .collect(),
            Box::new(normalize_inner(ret, type_vars, counter)),
        ),
        Type::Tuple(elems) => Type::Tuple(
            elems
                .iter()
                .map(|e| normalize_inner(e, type_vars, counter))
                .collect(),
        ),
    }
}

/// Substitute each type variable in `t` with its concrete type from `map`.
/// `T[]` with `T` bound to the empty-array pseudo marker collapses to
/// `Array(__none__)`; `T?` with the none-literal marker collapses to
/// `Optional(__none__)`. Codegen treats these like the empty array literal
/// and bare `none` it already knows about.
fn subst_vars(t: &Type, map: &HashMap<String, Type>) -> Type {
    match t {
        // `t` is always a piece of an already-`normalize`d signature, which
        // never contains `Any` (converted to a synthetic `Named` type var) or
        // the mono-only pseudo-types (only ever produced as *substituted
        // values*, via `map`, never as signature structure to recurse into).
        Type::Primitive(_)
        | Type::Unit
        | Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => t.clone(),
        Type::Named(v) => map
            .get(v)
            .cloned()
            .unwrap_or_else(|| Type::Named(v.clone())),
        Type::Optional(inner) => {
            let i = subst_vars(inner, map);
            if is_none_literal_arg(&i) {
                Type::Optional(Box::new(none_inner_ty()))
            } else {
                Type::Optional(Box::new(i))
            }
        }
        Type::Array(inner) => {
            let i = subst_vars(inner, map);
            if is_empty_array_arg(&i) {
                Type::Array(Box::new(none_inner_ty()))
            } else {
                Type::Array(Box::new(i))
            }
        }
        Type::Set(inner) => Type::Set(Box::new(subst_vars(inner, map))),
        Type::Dict(k, v) => Type::Dict(Box::new(subst_vars(k, map)), Box::new(subst_vars(v, map))),
        Type::Result(ok, err) => {
            // Each side independently collapses its none-literal marker, like the
            // `Optional` arm above (so `ok(5)`'s `__none__` err stays `__none__`).
            let collapse = |t: Type| {
                if is_none_literal_arg(&t) {
                    none_inner_ty()
                } else {
                    t
                }
            };
            Type::Result(
                Box::new(collapse(subst_vars(ok, map))),
                Box::new(collapse(subst_vars(err, map))),
            )
        }
        Type::Fn(params, ret) => Type::Fn(
            params.iter().map(|p| subst_vars(p, map)).collect(),
            Box::new(subst_vars(ret, map)),
        ),
        Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| subst_vars(e, map)).collect()),
    }
}

/// Does `t` mention the named type `name` anywhere? `t` may be a raw,
/// pre-`normalize` signature (this is how `is_generic` and its callers detect
/// anonymous `any`), so `Type::Any` counts as mentioning `"any"`.
fn ty_mentions(t: &Type, name: &str) -> bool {
    match t {
        Type::Primitive(_)
        | Type::Unit
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => false,
        Type::Any => name == "any",
        Type::Named(n) => n == name,
        Type::Optional(inner) | Type::Array(inner) | Type::Set(inner) => ty_mentions(inner, name),
        Type::Dict(k, v) => ty_mentions(k, name) || ty_mentions(v, name),
        Type::Result(ok, err) => ty_mentions(ok, name) || ty_mentions(err, name),
        Type::Fn(ps, ret) => ps.iter().any(|p| ty_mentions(p, name)) || ty_mentions(ret, name),
        Type::Tuple(elems) => elems.iter().any(|e| ty_mentions(e, name)),
    }
}

/// Does `t` reference any of the given type variables? `t` is always a piece
/// of an already-`normalize`d signature (never `Any`, never a mono-only
/// pseudo-type — see `subst_vars`).
fn ty_contains_var(t: &Type, vars: &HashSet<&str>) -> bool {
    match t {
        Type::Unit
        | Type::Primitive(_)
        | Type::Any
        | Type::NoneInner
        | Type::EmptyArrayArg
        | Type::NoneLiteralArg
        | Type::ConcatStr => false,
        Type::Named(v) => vars.contains(v.as_str()),
        Type::Optional(inner) | Type::Array(inner) | Type::Set(inner) => {
            ty_contains_var(inner, vars)
        }
        Type::Dict(k, v) => ty_contains_var(k, vars) || ty_contains_var(v, vars),
        Type::Result(ok, err) => ty_contains_var(ok, vars) || ty_contains_var(err, vars),
        Type::Fn(ps, ret) => {
            ps.iter().any(|p| ty_contains_var(p, vars)) || ty_contains_var(ret, vars)
        }
        Type::Tuple(elems) => elems.iter().any(|e| ty_contains_var(e, vars)),
    }
}

/// User-facing spelling of a type variable: synthetic anonymous vars (`$anyN`)
/// are shown as `any`.
fn display_var(v: &str) -> &str {
    if v.starts_with("$any") {
        "any"
    } else {
        v
    }
}

// ---- Lambda lifting ---------------------------------------------------------

/// Free variables of a lambda `body` — identifiers referencing the enclosing
/// scope (not the lambda's `params` nor anything bound within the body), paired
/// with their types from `env`, in first-occurrence order. (Globals are calls,
/// not identifiers, so any free identifier is a capture.)
fn free_vars(body: &Expr, params: &[LambdaParam], env: &Env) -> Vec<(String, Type)> {
    let mut bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut out: Vec<(String, Type)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    collect_free(body, &mut bound, env, &mut out, &mut seen);
    out
}

fn collect_free(
    e: &Expr,
    bound: &mut HashSet<String>,
    env: &Env,
    out: &mut Vec<(String, Type)>,
    seen: &mut HashSet<String>,
) {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !bound.contains(n) && !seen.contains(n) {
                if let Some(t) = env.get(n) {
                    out.push((n.clone(), t.clone()));
                    seen.insert(n.clone());
                }
            }
        }
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => {}
        ExprKind::Neg(x)
        | ExprKind::Not(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x) => collect_free(x, bound, env, out, seen),
        ExprKind::Binop(a, _, b) | ExprKind::Seq(a, b) | ExprKind::Index(a, b) => {
            collect_free(a, bound, env, out, seen);
            collect_free(b, bound, env, out, seen);
        }
        ExprKind::If(a, b, c) => {
            collect_free(a, bound, env, out, seen);
            collect_free(b, bound, env, out, seen);
            collect_free(c, bound, env, out, seen);
        }
        ExprKind::Slice(a, b, c) => {
            collect_free(a, bound, env, out, seen);
            collect_free(b, bound, env, out, seen);
            if let Some(c) = c {
                collect_free(c, bound, env, out, seen);
            }
        }
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args)
        | ExprKind::TupleLit(args) => {
            for a in args {
                collect_free(a, bound, env, out, seen);
            }
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                collect_free(k, bound, env, out, seen);
                collect_free(v, bound, env, out, seen);
            }
        }
        ExprKind::Construct(_, inits) => {
            for fi in inits {
                collect_free(&fi.value, bound, env, out, seen);
            }
        }
        ExprKind::Match(s, arms) => {
            collect_free(s, bound, env, out, seen);
            for arm in arms {
                // The arm's pattern bindings are bound within its body.
                let added: Vec<&String> = arm
                    .pattern
                    .bindings()
                    .iter()
                    .filter(|b| bound.insert((*b).clone()))
                    .collect();
                collect_free(&arm.body, bound, env, out, seen);
                for b in added {
                    bound.remove(b);
                }
            }
        }
        ExprKind::Let(n, val, b) | ExprKind::LetMut(n, val, b) => {
            collect_free(val, bound, env, out, seen);
            let added = bound.insert(n.clone());
            collect_free(b, bound, env, out, seen);
            if added {
                bound.remove(n);
            }
        }
        ExprKind::Assign(_, val, b) => {
            collect_free(val, bound, env, out, seen);
            collect_free(b, bound, env, out, seen);
        }
        ExprKind::For(v, iter, b) => {
            collect_free(iter, bound, env, out, seen);
            let added = bound.insert(v.clone());
            collect_free(b, bound, env, out, seen);
            if added {
                bound.remove(v);
            }
        }
        ExprKind::While(cond, b) => {
            collect_free(cond, bound, env, out, seen);
            collect_free(b, bound, env, out, seen);
        }
        ExprKind::Lambda(ps, b) => {
            let added: Vec<String> = ps
                .iter()
                .filter_map(|p| bound.insert(p.name.clone()).then(|| p.name.clone()))
                .collect();
            collect_free(b, bound, env, out, seen);
            for n in added {
                bound.remove(&n);
            }
        }
    }
}

// ---- Use-count analysis (inlining groundwork) -------------------------------
//
// Count how often each top-level function is referenced across the program — a
// call site or a function-value use. The first step toward inlining: a function
// referenced exactly once is an unconditional inline candidate (the single use
// site is replaced with the body, and the definition then has no remaining
// users). The actual inlining transform is not implemented yet.

/// How many times each function is referenced across the program — direct/method
/// calls (`Call`) plus function-value uses (`Ident`) — summed over every function
/// body, **including a function's own body** so a recursive self-reference counts
/// (a recursive function used once externally still has a count >= 2, so it isn't
/// a single-use inline candidate). **Builtins are counted too** (keyed by their
/// callee name — `__builtin_*` after loading, the bare name before), since
/// they're inline candidates as well. Names shadowed by a `let`/parameter/lambda
/// parameter/`match` binding/`for` variable are *not* counted (that name is the
/// local, not a function). Every user function appears in the map (with `0` if
/// unreferenced — e.g. `main`, which only the runtime calls); a builtin appears
/// only once referenced. `.test` blocks are not counted — the count reflects the
/// production program.
pub fn use_counts(program: &Program) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some((f.name.clone(), 0)),
            _ => None,
        })
        .collect();
    for it in &program.items {
        if let Item::Fn(f) = it {
            // A function's parameters shadow same-named globals within its body
            // (e.g. a `(T) -> bool` parameter `pred` called as `pred(x)` is the
            // parameter, not a top-level `pred`).
            let mut bound: HashSet<String> = f.params.iter().map(|p| p.name.clone()).collect();
            count_uses(&f.body, &mut bound, &mut counts);
        }
    }
    counts
}

/// Walk `e`, bumping `counts[name]` for each non-shadowed function reference (a
/// `Call` callee or an `Ident` used as a value), creating the entry on first use
/// (so builtins are counted too). `bound` tracks names shadowed by enclosing
/// parameters / `let`s / lambda params / `match` bindings / `for` vars —
/// mirroring [`collect_free`]'s scope handling — so a bound name (a local) is not
/// counted. In a valid program every other unbound name is a function (user or
/// builtin), since AIPL has no global variables.
fn count_uses(e: &Expr, bound: &mut HashSet<String>, counts: &mut HashMap<String, usize>) {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !bound.contains(n) {
                *counts.entry(n.clone()).or_insert(0) += 1;
            }
        }
        ExprKind::Call(name, args, _) => {
            if !bound.contains(name) {
                *counts.entry(name.clone()).or_insert(0) += 1;
            }
            for a in args {
                count_uses(a, bound, counts);
            }
        }
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => {}
        ExprKind::Neg(x)
        | ExprKind::Not(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x) => count_uses(x, bound, counts),
        ExprKind::Binop(a, _, b) | ExprKind::Seq(a, b) | ExprKind::Index(a, b) => {
            count_uses(a, bound, counts);
            count_uses(b, bound, counts);
        }
        ExprKind::If(a, b, c) => {
            count_uses(a, bound, counts);
            count_uses(b, bound, counts);
            count_uses(c, bound, counts);
        }
        ExprKind::Slice(a, b, c) => {
            count_uses(a, bound, counts);
            count_uses(b, bound, counts);
            if let Some(c) = c {
                count_uses(c, bound, counts);
            }
        }
        ExprKind::ArrayLit(args) | ExprKind::SetLit(args) | ExprKind::TupleLit(args) => {
            for a in args {
                count_uses(a, bound, counts);
            }
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                count_uses(k, bound, counts);
                count_uses(v, bound, counts);
            }
        }
        ExprKind::Construct(_, inits) => {
            for fi in inits {
                count_uses(&fi.value, bound, counts);
            }
        }
        ExprKind::Match(s, arms) => {
            count_uses(s, bound, counts);
            for arm in arms {
                let added: Vec<String> = arm
                    .pattern
                    .bindings()
                    .iter()
                    .filter(|b| bound.insert((*b).clone()))
                    .cloned()
                    .collect();
                count_uses(&arm.body, bound, counts);
                for b in added {
                    bound.remove(&b);
                }
            }
        }
        ExprKind::Let(n, val, b) | ExprKind::LetMut(n, val, b) => {
            count_uses(val, bound, counts);
            let added = bound.insert(n.clone());
            count_uses(b, bound, counts);
            if added {
                bound.remove(n);
            }
        }
        // `Assign`'s target is an existing binding (an lvalue), not a use.
        ExprKind::Assign(_, val, b) => {
            count_uses(val, bound, counts);
            count_uses(b, bound, counts);
        }
        ExprKind::For(v, iter, b) => {
            count_uses(iter, bound, counts);
            let added = bound.insert(v.clone());
            count_uses(b, bound, counts);
            if added {
                bound.remove(v);
            }
        }
        ExprKind::While(cond, b) => {
            count_uses(cond, bound, counts);
            count_uses(b, bound, counts);
        }
        ExprKind::Lambda(ps, b) => {
            let added: Vec<String> = ps
                .iter()
                .filter_map(|p| bound.insert(p.name.clone()).then(|| p.name.clone()))
                .collect();
            count_uses(b, bound, counts);
            for n in added {
                bound.remove(&n);
            }
        }
    }
}

// ---- Inlining ---------------------------------------------------------------
//
// Inline a *private* function that is used exactly once: replace its single call
// site with the function's body (parameters bound to the arguments via `let`s)
// and drop the now-uncalled definition. Gated to programs with a `main` (and no
// `__test_main`) — i.e. the `run`/`build` path, where reachability is seeded from
// `main` and removing a function is safe. No-`main` programs (FFI engines,
// libraries, the `check` driver) are left untouched, since any function may be
// called by name there.

/// Inline every single-use private function in `program` and drop it. Returns the
/// rewritten program (a clone, unchanged when the gate below fails). Conservative
/// (see `is_inline_candidate`): only direct, exactly-once, exact-arity calls to
/// non-`pub`, non-generic, non-higher-order, non-mutating, `return`/`?`-free,
/// non-recursive functions are inlined. Repeats to a fixpoint, since inlining
/// `f` into `g` can make `g` itself single-use.
pub fn inline_single_use(program: &Program) -> Program {
    let has_fn = |n: &str| {
        program
            .items
            .iter()
            .any(|it| matches!(it, Item::Fn(f) if f.name == n))
    };
    // Only when `main` is the entry and we're not building the `check` test
    // driver (which must keep every function and its `.test` intact).
    if !has_fn("main") || has_fn("__test_main") {
        return program.clone();
    }

    let mut program = program.clone();
    let mut counter = 0usize;
    // Functions that are used once but only as a *value* (an `Ident`, never a
    // direct call) — nothing to substitute, so don't re-select them forever.
    let mut skip: HashSet<String> = HashSet::new();

    loop {
        let counts = use_counts(&program);
        let binders = collect_binders(&program);
        let candidate = program.items.iter().find_map(|it| match it {
            Item::Fn(f) if is_inline_candidate(f, &counts, &binders, &skip) => Some(f.clone()),
            _ => None,
        });
        let Some(f) = candidate else { break };

        // Replace the single `Call(f, ..)` (it lives in exactly one body — `f`'s
        // name is never shadowed, see the `binders` guard).
        let fparam_names: Vec<String> = f.params.iter().map(|p| p.name.clone()).collect();
        let mut replaced = false;
        for it in &mut program.items {
            if let Item::Fn(g) = it {
                let body = replace_call(
                    &g.body,
                    &f.name,
                    &fparam_names,
                    &f.body,
                    &mut counter,
                    &mut replaced,
                );
                g.body = body;
            }
        }
        if replaced {
            program
                .items
                .retain(|it| !matches!(it, Item::Fn(g) if g.name == f.name));
        } else {
            // The single use was an `Ident` (function value), not a call.
            skip.insert(f.name.clone());
        }
    }
    program
}

/// Inline single-use functions in the *monomorphized* program — same idea as
/// [`inline_single_use`], but over [`MonoProgram`]/[`ConcreteFn`] and with `pub`
/// no longer protecting a function from inlining. Before monomorphization a
/// `pub` function may be imported by another file, so its in-program use count
/// understates its references; after, every reference is resolved and the only
/// externally-reachable entry of a `main` binary is `main` itself, so a
/// single-use non-`main` function is genuinely single-use. This is what lets the
/// lifted lambdas (synthesized, each called from exactly one specialization) and
/// other single-use instances fold away. Still gated to `main` binaries, so FFI
/// engines and the `check` driver are untouched.
pub fn inline_single_use_post_mono(program: &MonoProgram) -> MonoProgram {
    if !program.fns.iter().any(|f| f.name == "main")
        || program.fns.iter().any(|f| f.name == "__test_main")
    {
        return program.clone();
    }

    let mut program = program.clone();
    let mut counter = 0usize;
    let mut skip: HashSet<String> = HashSet::new();

    loop {
        let counts = use_counts_mono(&program);
        let binders = collect_binders_mono(&program);
        let candidate = program
            .fns
            .iter()
            .find(|f| is_inline_candidate_mono(f, &counts, &binders, &skip))
            .cloned();
        let Some(f) = candidate else { break };

        let fparam_names: Vec<String> = f.params.iter().map(|p| p.name.clone()).collect();
        let mut replaced = false;
        for g in &mut program.fns {
            let body = replace_call(
                &g.body,
                &f.name,
                &fparam_names,
                &f.body,
                &mut counter,
                &mut replaced,
            );
            g.body = body;
        }
        if replaced {
            program.fns.retain(|g| g.name != f.name);
        } else {
            skip.insert(f.name.clone());
        }
    }
    program
}

/// Whether `f` is a single-use private function safe to inline. See the module
/// note for the conservative criteria.
fn is_inline_candidate(
    f: &Function,
    counts: &HashMap<String, usize>,
    binders: &HashSet<String>,
    skip: &HashSet<String>,
) -> bool {
    counts.get(&f.name) == Some(&1)
        // A `pub` fn may be imported by another file, so its in-program use
        // count understates its references.
        && !f.is_pub
        && f.name != "main"
        && !skip.contains(&f.name)
        // Never shadowed by a local anywhere, so the single counted use is the
        // only `Call`/`Ident` of this name in the whole program.
        && !binders.contains(&f.name)
        && f.type_params.is_empty()
        && is_inline_shape(
            f.params.iter().any(|p| matches!(p.ty, Type::Fn(_, _)) || p.variadic),
            f.params.first().is_some_and(|p| p.mutable),
            &f.body,
            &f.name,
        )
}

/// The shape checks shared by [`is_inline_candidate`] and
/// [`is_inline_candidate_mono`]: not a higher-order template (those resolve via
/// mono), not a mutating method, and the body has no early exit, no
/// context-typed literal, no in-place HOF intrinsic, and no self-reference. The
/// pre-mono caller additionally folds "or variadic" into `higher_order_or_variadic`
/// — variadic resolution is also a mono job — but the post-mono [`ConcreteParam`]
/// has no `variadic` field to check (`specialize_variadic` has already resolved
/// every parameter by the time a [`ConcreteFn`] exists), so its caller passes
/// just the higher-order check.
fn is_inline_shape(
    higher_order_or_variadic: bool,
    mutating: bool,
    body: &Expr,
    name: &str,
) -> bool {
    !higher_order_or_variadic
        && !mutating
        && !contains_early_exit(body)
        && !contains_context_literal(body)
        && !contains_inplace_hof_intrinsic(body)
        && !references_name(body, name)
}

/// Every name bound by a parameter / `let` / `for` / `match` arm / lambda
/// parameter anywhere in `program` — used to confirm a candidate's name is never
/// shadowed (so its single use is unambiguous).
fn collect_binders(program: &Program) -> HashSet<String> {
    let mut set = HashSet::new();
    for it in &program.items {
        if let Item::Fn(f) = it {
            for p in &f.params {
                set.insert(p.name.clone());
            }
            collect_body_binders(&f.body, &mut set);
        }
    }
    set
}

/// [`is_inline_candidate`] for the post-mono [`ConcreteFn`] representation:
/// every instance is already concrete (no `is_pub`/`type_params` to check —
/// nothing but `main` is externally reachable, and monomorphization leaves no
/// type parameters), so only the shared shape checks apply.
fn is_inline_candidate_mono(
    f: &ConcreteFn,
    counts: &HashMap<String, usize>,
    binders: &HashSet<String>,
    skip: &HashSet<String>,
) -> bool {
    counts.get(&f.name) == Some(&1)
        && f.name != "main"
        && !skip.contains(&f.name)
        && !binders.contains(&f.name)
        && is_inline_shape(
            f.params.iter().any(|p| matches!(p.ty, Type::Fn(_, _))),
            f.params.first().is_some_and(|p| p.mutable),
            &f.body,
            &f.name,
        )
}

/// [`use_counts`] for a [`MonoProgram`].
fn use_counts_mono(program: &MonoProgram) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> =
        program.fns.iter().map(|f| (f.name.clone(), 0)).collect();
    for f in &program.fns {
        let mut bound: HashSet<String> = f.params.iter().map(|p| p.name.clone()).collect();
        count_uses(&f.body, &mut bound, &mut counts);
    }
    counts
}

/// [`collect_binders`] for a [`MonoProgram`].
fn collect_binders_mono(program: &MonoProgram) -> HashSet<String> {
    let mut set = HashSet::new();
    for f in &program.fns {
        for p in &f.params {
            set.insert(p.name.clone());
        }
        collect_body_binders(&f.body, &mut set);
    }
    set
}

fn collect_body_binders(e: &Expr, set: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Let(n, _, _) | ExprKind::LetMut(n, _, _) | ExprKind::For(n, _, _) => {
            set.insert(n.clone());
        }
        ExprKind::Lambda(ps, _) => {
            for p in ps {
                set.insert(p.name.clone());
            }
        }
        ExprKind::Match(_, arms) => {
            for a in arms {
                for b in a.pattern.bindings() {
                    set.insert(b.clone());
                }
            }
        }
        _ => {}
    }
    for c in children(e) {
        collect_body_binders(c, set);
    }
}

/// The direct sub-expressions of `e` (read-only traversal helper).
fn children(e: &Expr) -> Vec<&Expr> {
    match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::None
        | ExprKind::Unit => vec![],
        ExprKind::Neg(x)
        | ExprKind::Not(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x) => vec![x],
        ExprKind::Binop(a, _, b)
        | ExprKind::Seq(a, b)
        | ExprKind::Index(a, b)
        | ExprKind::Let(_, a, b)
        | ExprKind::LetMut(_, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::For(_, a, b)
        | ExprKind::While(a, b) => vec![a, b],
        ExprKind::If(a, b, c) => vec![a, b, c],
        ExprKind::Slice(a, b, c) => {
            let mut v = vec![a.as_ref(), b.as_ref()];
            if let Some(c) = c {
                v.push(c);
            }
            v
        }
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args)
        | ExprKind::TupleLit(args) => args.iter().collect(),
        ExprKind::DictLit(pairs) => pairs.iter().flat_map(|(k, v)| [k, v]).collect(),
        ExprKind::Construct(_, inits) => inits.iter().map(|i| &i.value).collect(),
        ExprKind::Match(s, arms) => {
            let mut v = vec![s.as_ref()];
            v.extend(arms.iter().map(|a| &a.body));
            v
        }
        ExprKind::Lambda(_, b) => vec![b],
    }
}

/// Whether `e` (or any sub-expression) is an early exit — `return` or `?`.
fn contains_early_exit(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Return(_) | ExprKind::Try(_))
        || children(e).iter().any(|c| contains_early_exit(c))
}

/// Whether `e` (or any sub-expression) calls an in-place higher-order intrinsic.
/// These mark the synthesized `map`/`filter`/`zip_with` loop bodies the
/// monomorphizer emits; `__map_result` reinterprets the reused buffer using the
/// *enclosing* function's return type, so such a function can't be inlined out of
/// its own frame without losing that type (the buffer would stay the input
/// element type — e.g. `i64` instead of `i64[]`). Never appears pre-mono (these
/// intrinsics are synthesized during monomorphization), so this is a no-op there.
fn contains_inplace_hof_intrinsic(e: &Expr) -> bool {
    if let ExprKind::Call(name, _, _) = &e.kind {
        if matches!(
            name.as_str(),
            "__map_set" | "__map_result" | "__filter_drop" | "__filter_keep" | "__filter_truncate"
        ) {
            return true;
        }
    }
    children(e)
        .iter()
        .any(|c| contains_inplace_hof_intrinsic(c))
}

/// Whether `e` (or any sub-expression) is a *context-typed* literal — `none`, or
/// an empty `[]` / `#{}` / `#{:}` — whose type is fixed by its surroundings (the
/// enclosing function's return type, a sibling branch, a parameter type, …). The
/// enclosing return type is dropped when inlining, so a function whose body holds
/// such a literal is conservatively not inlined (it could otherwise leave the
/// literal's type unresolved as `__none__`).
fn contains_context_literal(e: &Expr) -> bool {
    let here = match &e.kind {
        ExprKind::None => true,
        ExprKind::ArrayLit(v) | ExprKind::SetLit(v) => v.is_empty(),
        ExprKind::DictLit(v) => v.is_empty(),
        _ => false,
    };
    here || children(e).iter().any(|c| contains_context_literal(c))
}

/// Whether `e` references `name` as a `Call` callee or an `Ident` value. Only
/// called for a `name` that is never a binder, so no shadowing to consider.
fn references_name(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Call(n, args, _) => n == name || args.iter().any(|a| references_name(a, name)),
        _ => children(e).iter().any(|c| references_name(c, name)),
    }
}

/// Rebuild `e`, replacing the (single) direct call to `fname` — with matching
/// arity — by the inlined body of the function it names (`fparam_names`/`fbody`).
/// `replaced` guards against touching more than one site (there is only one,
/// but the flag also short-circuits the rest of the walk). Takes just the
/// callee's parameter names (rather than a whole function/param list) so it
/// works for both the pre-mono [`Function`]/[`Param`] and the post-mono
/// [`ConcreteFn`]/[`ConcreteParam`] representations — inlining only ever
/// renames and rebinds parameters by name.
fn replace_call(
    e: &Expr,
    fname: &str,
    fparam_names: &[String],
    fbody: &Expr,
    counter: &mut usize,
    replaced: &mut bool,
) -> Expr {
    if !*replaced {
        if let ExprKind::Call(name, args, _) = &e.kind {
            // Skip if any argument is a context-typed literal (`none`, empty
            // `[]`/`#{}`/`#{:}`, or a `some(..)`/`ok(..)` wrapping one): its type
            // comes from `f`'s *parameter*, which a `let`-binding discards. Leave
            // the call (so `inline_single_use` keeps `f`).
            if name == fname
                && args.len() == fparam_names.len()
                && !args.iter().any(contains_context_literal)
            {
                *replaced = true;
                return build_inlined(fparam_names, fbody, args, e.span.clone(), counter);
            }
        }
    }
    let rc = |x: &Expr, counter: &mut usize, replaced: &mut bool| {
        replace_call(x, fname, fparam_names, fbody, counter, replaced)
    };
    let kind = match &e.kind {
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Ident(_)
        | ExprKind::None
        | ExprKind::Unit => e.kind.clone(),
        ExprKind::Call(name, args, m) => ExprKind::Call(
            name.clone(),
            args.iter().map(|a| rc(a, counter, replaced)).collect(),
            *m,
        ),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(rc(x, counter, replaced))),
        ExprKind::Not(x) => ExprKind::Not(Box::new(rc(x, counter, replaced))),
        ExprKind::Try(x) => ExprKind::Try(Box::new(rc(x, counter, replaced))),
        ExprKind::Return(x) => ExprKind::Return(Box::new(rc(x, counter, replaced))),
        ExprKind::Field(x, fld) => ExprKind::Field(Box::new(rc(x, counter, replaced)), fld.clone()),
        ExprKind::Binop(a, op, b) => ExprKind::Binop(
            Box::new(rc(a, counter, replaced)),
            *op,
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::Seq(a, b) => ExprKind::Seq(
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::Index(a, b) => ExprKind::Index(
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::While(a, b) => ExprKind::While(
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::Let(n, a, b) => ExprKind::Let(
            n.clone(),
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::LetMut(n, a, b) => ExprKind::LetMut(
            n.clone(),
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::Assign(n, a, b) => ExprKind::Assign(
            n.clone(),
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::For(v, a, b) => ExprKind::For(
            v.clone(),
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
        ),
        ExprKind::If(a, b, c) => ExprKind::If(
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
            Box::new(rc(c, counter, replaced)),
        ),
        ExprKind::Slice(a, b, c) => ExprKind::Slice(
            Box::new(rc(a, counter, replaced)),
            Box::new(rc(b, counter, replaced)),
            c.as_ref().map(|c| Box::new(rc(c, counter, replaced))),
        ),
        ExprKind::ArrayLit(xs) => {
            ExprKind::ArrayLit(xs.iter().map(|x| rc(x, counter, replaced)).collect())
        }
        ExprKind::SetLit(xs) => {
            ExprKind::SetLit(xs.iter().map(|x| rc(x, counter, replaced)).collect())
        }
        ExprKind::TupleLit(xs) => {
            ExprKind::TupleLit(xs.iter().map(|x| rc(x, counter, replaced)).collect())
        }
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (rc(k, counter, replaced), rc(v, counter, replaced)))
                .collect(),
        ),
        ExprKind::Construct(name, inits) => ExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|i| FieldInit {
                    name: i.name.clone(),
                    value: rc(&i.value, counter, replaced),
                })
                .collect(),
        ),
        ExprKind::Match(s, arms) => ExprKind::Match(
            Box::new(rc(s, counter, replaced)),
            arms.iter()
                .map(|a| MatchArm {
                    pattern: a.pattern.clone(),
                    body: rc(&a.body, counter, replaced),
                    span: a.span.clone(),
                })
                .collect(),
        ),
        ExprKind::Lambda(ps, b) => ExprKind::Lambda(ps.clone(), Box::new(rc(b, counter, replaced))),
    };
    Expr::new(kind, e.span.clone())
}

/// Build the inlined expression for `Call(f, args)`: bind each (freshly renamed)
/// parameter to its argument via a `let`, in order, wrapping `f`'s body. Renaming
/// the parameters to `$inl<N>_<name>` is required for correctness — an argument
/// is evaluated in the parameter-binding scope, so a later argument referencing a
/// caller name that collides with a parameter would otherwise be captured. (`$`
/// can't appear in user identifiers, so the fresh names can never collide.)
fn build_inlined(
    fparam_names: &[String],
    fbody: &Expr,
    args: &[Expr],
    span: Span,
    counter: &mut usize,
) -> Expr {
    let mut map: HashMap<String, String> = HashMap::new();
    let fresh: Vec<String> = fparam_names
        .iter()
        .map(|pname| {
            let nm = format!("$inl{}_{}", *counter, pname);
            *counter += 1;
            map.insert(pname.clone(), nm.clone());
            nm
        })
        .collect();
    let mut expr = rename_params(fbody, &map);
    for (fp, arg) in fresh.iter().zip(args).rev() {
        expr = Expr::new(
            ExprKind::Let(fp.clone(), Box::new(arg.clone()), Box::new(expr)),
            span.clone(),
        );
    }
    expr
}

/// Rebuild `e` (a function body) substituting parameter references per `map`
/// (original → fresh name), respecting shadowing: a `let`/`for`/`match`/lambda
/// binder of a mapped name removes it from the map within that scope (there the
/// name is the local, not the parameter). Body-internal binders are left as-is —
/// only the substituted *parameter* references are renamed.
fn rename_params(e: &Expr, map: &HashMap<String, String>) -> Expr {
    let sub = |n: &String| map.get(n).cloned().unwrap_or_else(|| n.clone());
    let without = |n: &str| -> HashMap<String, String> {
        let mut m = map.clone();
        m.remove(n);
        m
    };
    let without_all = |names: &[String]| -> HashMap<String, String> {
        let mut m = map.clone();
        for n in names {
            m.remove(n);
        }
        m
    };
    let kind = match &e.kind {
        ExprKind::Ident(n) => ExprKind::Ident(sub(n)),
        ExprKind::Call(name, args, m) => ExprKind::Call(
            sub(name),
            args.iter().map(|a| rename_params(a, map)).collect(),
            *m,
        ),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => e.kind.clone(),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(rename_params(x, map))),
        ExprKind::Not(x) => ExprKind::Not(Box::new(rename_params(x, map))),
        ExprKind::Try(x) => ExprKind::Try(Box::new(rename_params(x, map))),
        ExprKind::Return(x) => ExprKind::Return(Box::new(rename_params(x, map))),
        ExprKind::Field(x, fld) => ExprKind::Field(Box::new(rename_params(x, map)), fld.clone()),
        ExprKind::Binop(a, op, b) => ExprKind::Binop(
            Box::new(rename_params(a, map)),
            *op,
            Box::new(rename_params(b, map)),
        ),
        ExprKind::Seq(a, b) => ExprKind::Seq(
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, map)),
        ),
        ExprKind::Index(a, b) => ExprKind::Index(
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, map)),
        ),
        ExprKind::While(a, b) => ExprKind::While(
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, map)),
        ),
        ExprKind::If(a, b, c) => ExprKind::If(
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, map)),
            Box::new(rename_params(c, map)),
        ),
        ExprKind::Slice(a, b, c) => ExprKind::Slice(
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, map)),
            c.as_ref().map(|c| Box::new(rename_params(c, map))),
        ),
        ExprKind::ArrayLit(xs) => {
            ExprKind::ArrayLit(xs.iter().map(|x| rename_params(x, map)).collect())
        }
        ExprKind::SetLit(xs) => {
            ExprKind::SetLit(xs.iter().map(|x| rename_params(x, map)).collect())
        }
        ExprKind::TupleLit(xs) => {
            ExprKind::TupleLit(xs.iter().map(|x| rename_params(x, map)).collect())
        }
        ExprKind::DictLit(pairs) => ExprKind::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (rename_params(k, map), rename_params(v, map)))
                .collect(),
        ),
        ExprKind::Construct(name, inits) => ExprKind::Construct(
            name.clone(),
            inits
                .iter()
                .map(|i| FieldInit {
                    name: i.name.clone(),
                    value: rename_params(&i.value, map),
                })
                .collect(),
        ),
        // `Assign`'s target is a `mut` local (parameters are immutable), so it's
        // never a mapped parameter — keep it.
        ExprKind::Assign(n, a, b) => ExprKind::Assign(
            n.clone(),
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, map)),
        ),
        ExprKind::Let(n, a, b) => ExprKind::Let(
            n.clone(),
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, &without(n))),
        ),
        ExprKind::LetMut(n, a, b) => ExprKind::LetMut(
            n.clone(),
            Box::new(rename_params(a, map)),
            Box::new(rename_params(b, &without(n))),
        ),
        ExprKind::For(v, iter, b) => ExprKind::For(
            v.clone(),
            Box::new(rename_params(iter, map)),
            Box::new(rename_params(b, &without(v))),
        ),
        ExprKind::Match(s, arms) => ExprKind::Match(
            Box::new(rename_params(s, map)),
            arms.iter()
                .map(|a| MatchArm {
                    pattern: a.pattern.clone(),
                    body: rename_params(&a.body, &without_all(a.pattern.bindings())),
                    span: a.span.clone(),
                })
                .collect(),
        ),
        ExprKind::Lambda(ps, b) => {
            let names: Vec<String> = ps.iter().map(|p| p.name.clone()).collect();
            ExprKind::Lambda(ps.clone(), Box::new(rename_params(b, &without_all(&names))))
        }
    };
    Expr::new(kind, e.span.clone())
}

// ---- Ownership analysis -----------------------------------------------------
//
// Deciding which parameters an instance can *take ownership of* (move in and
// reuse) is a property of the source, so it lives here in monomorphization and
// becomes part of the instance key. `binding_is_exclusive` is re-exported for
// codegen, which uses the same analysis for in-place mutation of local `mut`
// bindings.

/// A heap-pointer value type (refcounted): a `str` or an array. Mirrors
/// codegen's `is_heap` — structs/optionals are not moved as whole params.
fn is_heap(t: &Type) -> bool {
    *t == Type::Primitive(Primitive::Str)
        || is_error(t)
        || matches!(t, Type::Array(_) | Type::Set(_) | Type::Dict(_, _))
}

/// Whether `arg` evaluates to a freshly-allocated, uniquely-owned heap value, so
/// it can be *moved* into an owning parameter rather than borrowed: an array
/// literal, or a call returning a heap value (a fresh rc-1 block). `arg_ty` is
/// `arg`'s inferred type. Mirrors codegen's former `is_fresh_heap_arg`.
fn is_fresh_heap(arg: &Expr, arg_ty: &Type) -> bool {
    is_heap(arg_ty) && matches!(&arg.kind, ExprKind::ArrayLit(_) | ExprKind::Call(_, _, _))
}

/// Whether the function with these (concrete) `params`/`return_ty`/`body` can
/// take ownership of a parameter, returning that parameter's index. The v0
/// "take ownership and mutate" shape: not `main`, exactly one heap parameter
/// (not a `self` receiver), a heap return, and the parameter consumed exactly
/// once as `mut y = p` with `y` exclusive in the rest of the body — so a fresh
/// argument can be moved in and reused. `name` distinguishes `main`.
fn owned_eligible(
    name: &str,
    params: &[Param],
    return_ty: &Option<Type>,
    body: &Expr,
) -> Option<usize> {
    if name == "main" || params.len() != 1 {
        return None;
    }
    let p = &params[0];
    if p.name == "self" || !is_heap(&p.ty) {
        return None;
    }
    if !return_ty.as_ref().is_some_and(is_heap) {
        return None;
    }
    if count_ident(&p.name, body) != 1 {
        return None;
    }
    let (y, body_after) = find_move_into(&p.name, body)?;
    binding_is_exclusive(y, body_after, true).then_some(0)
}

/// Static "exclusivity" analysis for a mutable heap binding (array or `str`):
/// true iff every use of `name` in `body` is provably non-aliasing, so `push` /
/// `+` may mutate it in place. `allow_tail_move` permits a final move-out (a
/// returned binding). Conservative: any unhandled use disqualifies it.
pub fn binding_is_exclusive(name: &str, body: &Expr, allow_tail_move: bool) -> bool {
    !aliases_or_unsafe(name, body, false, allow_tail_move)
}

/// True if `name` is used in `e` in a way that makes in-place mutation unsafe.
/// `iterating` is set inside a `for` over `name`; `tail` means `e` is in tail
/// position, where a bare `Ident(name)` is a move-out, not an alias.
fn aliases_or_unsafe(name: &str, e: &Expr, iterating: bool, tail: bool) -> bool {
    let is_n = |x: &Expr| matches!(&x.kind, ExprKind::Ident(n) if n == name);
    let rec = |x: &Expr| aliases_or_unsafe(name, x, iterating, false);
    let rec_tail = |x: &Expr| aliases_or_unsafe(name, x, iterating, tail);
    match &e.kind {
        ExprKind::Ident(n) => n == name && !tail,
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => false,
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Field(x, _) | ExprKind::Try(x) => {
            is_n(x) || rec(x)
        }
        ExprKind::Binop(a, _, b) => (!is_n(a) && rec(a)) || (!is_n(b) && rec(b)),
        // `return value` moves `value` out of the function, exactly like a tail
        // expression — a bare `return name` is a move-out, not an alias.
        ExprKind::Return(x) => rec_tail(x),
        ExprKind::Seq(a, b) => rec(a) || rec_tail(b),
        ExprKind::If(c, t, f) => rec(c) || rec_tail(t) || rec_tail(f),
        ExprKind::Index(base, idx) => (!is_n(base) && rec(base)) || rec(idx),
        // Slicing `name` makes a *view* that shares `name`'s buffer, so it
        // aliases — disqualify in-place mutation (don't exempt `is_n(obj)`).
        ExprKind::Slice(obj, start, end) => {
            is_n(obj) || rec(obj) || rec(start) || end.as_ref().is_some_and(|e| rec(e))
        }
        ExprKind::Call(fname, args, method_style) => {
            // `__map_result($a)` hands the (reused) array back as the function
            // result — in tail position that's a move-out, exactly like a bare
            // trailing `$a`, so it doesn't alias the binding.
            if fname == "__map_result" {
                return args.iter().any(|a| if is_n(a) { !tail } else { rec(a) });
            }
            if *method_style {
                // Method form: `args[0]` is the receiver, which a few builtins
                // consume (or mutate in place) without aliasing it; any other
                // method, or any *non-receiver* arg that is `name`, aliases it.
                let recv = &args[0];
                let recv_bad = if is_n(recv) {
                    match fname.as_str() {
                        "__builtin_push" => iterating,
                        "__builtin_len" | "__builtin_to_str" | "__builtin_trim" => false,
                        _ => true,
                    }
                } else {
                    rec(recv)
                };
                recv_bad || args[1..].iter().any(|a| is_n(a) || rec(a))
            } else {
                let consuming = matches!(
                    fname.as_str(),
                    "__builtin_len"
                        | "__builtin_to_str"
                        | "__builtin_print"
                        | "__builtin_trim"
                        // In-place-filter/map intrinsics mutate the array in place
                        // without aliasing it (see `expand_filter`/`expand_map`), so
                        // they don't disqualify the array binding from being exclusive.
                        | "__filter_keep"
                        | "__filter_truncate"
                        | "__map_set"
                );
                args.iter()
                    .any(|a| if is_n(a) { !consuming } else { rec(a) })
            }
        }
        ExprKind::ArrayLit(elems) | ExprKind::SetLit(elems) | ExprKind::TupleLit(elems) => {
            elems.iter().any(|x| is_n(x) || rec(x))
        }
        ExprKind::DictLit(pairs) => pairs
            .iter()
            .any(|(k, v)| is_n(k) || rec(k) || is_n(v) || rec(v)),
        ExprKind::Construct(_, inits) => inits.iter().any(|i| is_n(&i.value) || rec(&i.value)),
        // A capture of `name` inside a lambda body counts as a use (conservative).
        ExprKind::Lambda(_, body) => rec(body),
        ExprKind::Let(_, val, b) | ExprKind::LetMut(_, val, b) => {
            is_n(val) || rec(val) || rec_tail(b)
        }
        ExprKind::Assign(target, val, b) => {
            let lhs_bad = if target == name {
                match &val.kind {
                    ExprKind::Binop(l, '+', r) if matches!(&l.kind, ExprKind::Ident(n) if n == name) => {
                        iterating || rec(r)
                    }
                    // `set a = a.trim()` / `set a = trim(a)` both fold to the
                    // same arg list `[a]`, so one pattern covers both forms.
                    ExprKind::Call(f, cargs, _)
                        if f == "__builtin_trim"
                            && cargs.len() == 1
                            && matches!(&cargs[0].kind, ExprKind::Ident(n) if n == name) =>
                    {
                        iterating
                    }
                    // `set a = a.union(b)` / `set a = union(a, b)` reuse `a`'s
                    // allocation in place — safe unless we're iterating `a`, or
                    // the other operand aliases `a` (it's read while `a` grows).
                    // Both forms fold to `[a, b]`.
                    ExprKind::Call(f, cargs, _)
                        if f == "__builtin_union"
                            && cargs.len() == 2
                            && matches!(&cargs[0].kind, ExprKind::Ident(n) if n == name) =>
                    {
                        iterating || rec(&cargs[1])
                    }
                    _ => true,
                }
            } else {
                is_n(val) || rec(val)
            };
            lhs_bad || rec_tail(b)
        }
        ExprKind::For(_, iter, fbody) => {
            if is_n(iter) {
                aliases_or_unsafe(name, fbody, true, false)
            } else {
                rec(iter) || aliases_or_unsafe(name, fbody, iterating, false)
            }
        }
        // A loop body re-runs, so nothing in it is a tail-move; the condition
        // and body are both ordinary (non-tail) uses.
        ExprKind::While(cond, wbody) => rec(cond) || rec(wbody),
        ExprKind::Match(scrut, arms) => {
            is_n(scrut) || rec(scrut) || arms.iter().any(|arm| rec_tail(&arm.body))
        }
    }
}

/// Count `Ident(name)` reads in `e`. Used to confirm a moved parameter is
/// consumed exactly once. Overcounts never undercount, so a miscount only
/// disqualifies the move (safe).
pub(crate) fn count_ident(name: &str, e: &Expr) -> usize {
    let c = |x: &Expr| count_ident(name, x);
    match &e.kind {
        ExprKind::Ident(n) => usize::from(n == name),
        ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => 0,
        ExprKind::Neg(x)
        | ExprKind::Not(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x) => c(x),
        ExprKind::Binop(a, _, b)
        | ExprKind::Seq(a, b)
        | ExprKind::Index(a, b)
        | ExprKind::Let(_, a, b)
        | ExprKind::LetMut(_, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::For(_, a, b)
        | ExprKind::While(a, b) => c(a) + c(b),
        ExprKind::If(a, b, d) => c(a) + c(b) + c(d),
        ExprKind::Slice(a, b, d) => c(a) + c(b) + d.as_ref().map_or(0, |d| c(d)),
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args)
        | ExprKind::TupleLit(args) => args.iter().map(c).sum(),
        ExprKind::DictLit(pairs) => pairs.iter().map(|(k, v)| c(k) + c(v)).sum(),
        ExprKind::Construct(_, inits) => inits.iter().map(|i| c(&i.value)).sum(),
        ExprKind::Match(s, arms) => c(s) + arms.iter().map(|a| c(&a.body)).sum::<usize>(),
        ExprKind::Lambda(_, body) => c(body),
    }
}

/// Find a `mut y = p` binding (`p` is `Ident(param)`), returning `(y, the LetMut
/// body)` — the local a moved parameter is consumed into.
fn find_move_into<'a>(param: &str, e: &'a Expr) -> Option<(&'a str, &'a Expr)> {
    match &e.kind {
        ExprKind::LetMut(y, val, body) if matches!(&val.kind, ExprKind::Ident(n) if n == param) => {
            Some((y.as_str(), body))
        }
        ExprKind::Let(_, a, b)
        | ExprKind::LetMut(_, a, b)
        | ExprKind::Assign(_, a, b)
        | ExprKind::Seq(a, b)
        | ExprKind::Binop(a, _, b)
        | ExprKind::Index(a, b)
        | ExprKind::For(_, a, b)
        | ExprKind::While(a, b) => find_move_into(param, a).or_else(|| find_move_into(param, b)),
        ExprKind::If(a, b, d) => find_move_into(param, a)
            .or_else(|| find_move_into(param, b))
            .or_else(|| find_move_into(param, d)),
        ExprKind::Slice(a, b, d) => find_move_into(param, a)
            .or_else(|| find_move_into(param, b))
            .or_else(|| d.as_ref().and_then(|d| find_move_into(param, d))),
        ExprKind::Neg(x)
        | ExprKind::Not(x)
        | ExprKind::Field(x, _)
        | ExprKind::Try(x)
        | ExprKind::Return(x) => find_move_into(param, x),
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args)
        | ExprKind::TupleLit(args) => args.iter().find_map(|a| find_move_into(param, a)),
        ExprKind::DictLit(pairs) => pairs
            .iter()
            .find_map(|(k, v)| find_move_into(param, k).or_else(|| find_move_into(param, v))),
        ExprKind::Construct(_, inits) => inits.iter().find_map(|i| find_move_into(param, &i.value)),
        ExprKind::Match(s, arms) => find_move_into(param, s)
            .or_else(|| arms.iter().find_map(|a| find_move_into(param, &a.body))),
        ExprKind::Lambda(_, body) => find_move_into(param, body),
        ExprKind::Ident(_)
        | ExprKind::Num(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => None,
    }
}
