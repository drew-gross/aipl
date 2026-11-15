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
    sync::OnceLock,
};

mod match_check;
mod patterns;
pub mod pin;
mod typing;
pub use typing::{attribute, set_mangle};

mod fold;
pub use fold::{fold_constants_pinned, fold_matches_pinned, fold_matches_post_mono};

mod fuse_pinned;
pub use appending_variants_pinned::appending_variants_pinned;
pub use counting_variants_pinned::counting_variants_pinned;
pub use fold_extends_pinned::fold_extends_pinned;
pub use fold_lengths_pinned::fold_lengths_pinned;
pub use fuse_pinned::effectful_fns;
pub use fuse_pinned::fuse_operations_pinned;
pub use inline_pinned::{inline_inspected_pinned, inline_single_use_pinned, inline_small_pinned};
pub use pinned_pipeline::retarget_int_parses_pinned;

mod fold_lengths_pinned;

mod lower_reductions;
pub use lower_reductions::lower_reductions_pinned;

mod fuse_try_with;
mod grid_literals;
pub use fuse_try_with::{fuse_try_with_pinned, fuse_try_with_post_mono, lower_try_set_pinned};
pub use grid_literals::lower_grid_literals;

mod counting_variants_pinned;

mod fold_extends_pinned;

mod appending_variants_pinned;

mod sink;
pub use sink::{sink_bindings_pinned, sink_bindings_post_mono};

mod known_constructor_elimination;
mod reuse_matched_values;
pub use known_constructor_elimination::eliminate_known_constructors_post_mono;
pub use reuse_matched_values::reuse_matched_values;

pub use move_last_use_typed::move_last_uses_post_mono;
mod inline_pinned;
mod inline_typed;
pub mod instantiation;
mod move_last_use_typed;
mod ownership_typed;
mod pinned_pipeline;
mod typed_adapter;
mod typed_function_calls;
mod typed_higher_order;
mod typed_infer;
mod typed_match_tree;
mod typed_retype;
mod typed_shims;
mod typed_util;
/// The post-monomorphization tree and the questions codegen asks of it.
pub mod typed {
    pub use crate::ownership_typed::{
        assigns_binding, binding_is_exclusive, inspect_only_params, is_receiver, moves_binding,
        reowns_binding,
    };
    pub use crate::typed_util::{any_child, body_size, each_child, each_child_mut, Typed};
}

mod subst;
pub use subst::inline_single_use_bindings_pinned;

mod early_exit;
mod shim_closures;

/// The optimization pass manager: the pass list, and the loop that runs it
/// until the passes stop finding anything. See the module docs for the
/// worklist and what a pass may assume about being run twice.
pub mod hygiene;
pub mod passes;

use aipl_syntax::subst_type_params;
use aipl_syntax::{
    ast::{
        is_unit, Arity, Bound, Callee, CaseParam, ConcreteFieldDecl, ConcreteStructDecl,
        ConcreteType, ConcreteVariantCase, ConcreteVariantDecl, Expr, ExprKind, FieldDecl,
        FieldInit, Function, Item, LambdaParam, MatchArm, Param, Primitive, Program, SeqShape,
        Signature, StructDecl, Type, TypeParam, VariantCase, VariantDecl,
    },
    is_concat_str, is_error, is_str_repr, type_name, DebugOptions, Error, Span, BUILTIN_SIGNATURES,
};
use aipl_syntax::{Children, ChildrenMut};

/// Hard cap on the number of generic instances monomorphization will emit.
/// For well-behaved generics the reachable instance set is finite (type
/// arguments are primitives), so blowing past this means a generic is
/// instantiating itself with an ever-growing type — a non-terminating loop.
/// Turning that into an error rather than hanging is what makes the bug
/// findable; `--debug` then prints the exact chain of growing instances.
const INSTANTIATION_LIMIT: usize = 10_000;

/// Rewrite a bare reference to a *payload-carrying* variant constructor used as
/// a value (`xs.map(Circle)`) into the equivalent lambda (`xs.map(|a| Circle(a))`),
/// so a constructor can be passed wherever a named function can. Runs before
/// `check`, which therefore only ever sees the lambda; the synthesized lambda's
/// parameters are annotated with the payload types from the variant declaration,
/// so no inference is needed. A *nullary* constructor is untouched — a bare
/// `Empty` is already a value, not a function — and any binding in scope with
/// the constructor's name shadows it, like everywhere else.
/// The shared function a constructor of a non-generic variant lowers to.
struct CtorWrapper {
    /// The synthesized top-level function's name.
    fn_name: String,
    /// The payload types, which are its parameter types.
    payload: Vec<Type>,
    /// The variant it constructs, which is its return type.
    variant: String,
}

/// [`lower_ctor_refs`]'s state: what a constructor reference may become, and
/// which of those were reached.
struct Lcr<'a> {
    /// Payload types per payload-carrying constructor, keyed by the loader's
    /// `Case@Variant`.
    ctors: &'a HashMap<String, Vec<Type>>,
    /// The one shared wrapper each constructor of a *non-generic* variant
    /// lowers to, keyed the same way. A generic template is absent: its
    /// wrapper would have to be generic too, and a generic function has no
    /// single address to pass as a value — so those keep the per-reference
    /// lambda, whose types monomorphization pins per instance.
    wrappers: &'a HashMap<String, CtorWrapper>,
    /// Which wrappers a reference actually reached. Only these are emitted, so
    /// a program that never passes a constructor as a value gains nothing.
    used: std::cell::RefCell<HashSet<String>>,
}

pub fn lower_ctor_refs(program: &Program) -> Program {
    // Payload-carrying constructors: name → payload types (arity >= 1). Keyed by
    // both the bare case name and the loader's variant-qualified form
    // `Case@Variant`, so a qualified reference used as a value is wrapped too.
    let mut ctors: HashMap<String, Vec<Type>> = HashMap::new();
    for item in &program.items {
        if let Item::Variant(v) = item {
            // Every in-scope constructor is referenced by its loader-assigned
            // variant-qualified name `Case@Variant` (generic templates included —
            // the monomorphizer re-qualifies those to their instance).
            for c in &v.cases {
                if c.payload.is_empty() {
                    continue;
                }
                ctors.insert(format!("{}@{}", c.name, v.name), c.payload_tys());
            }
        }
    }
    if ctors.is_empty() {
        return program.clone();
    }
    // A constructor of a non-generic variant is a function with one address, so
    // every reference to it can share one. Building the table up front (rather
    // than on demand) keeps the walk below a pure rewrite.
    let mut wrappers: HashMap<String, CtorWrapper> = HashMap::new();
    for item in &program.items {
        if let Item::Variant(v) = item {
            if !v.type_vars.is_empty() {
                continue;
            }
            for c in v.cases.iter().filter(|c| !c.payload.is_empty()) {
                wrappers.insert(
                    format!("{}@{}", c.name, v.name),
                    CtorWrapper {
                        fn_name: aipl_syntax::ctor_wrapper_name(&v.name, &c.name),
                        payload: c.payload_tys(),
                        variant: v.name.text.clone(),
                    },
                );
            }
        }
    }
    let lcr = Lcr {
        ctors: &ctors,
        wrappers: &wrappers,
        used: std::cell::RefCell::new(HashSet::new()),
    };
    let items: Vec<Item> = program
        .items
        .iter()
        .map(|item| match item {
            Item::Fn(f) => {
                // The function's parameters are in scope everywhere in it —
                // including keyword-parameter defaults, which may name them.
                let mut scope: Vec<String> = f.sig.params.iter().map(|p| p.name.clone()).collect();
                let params: Vec<Param> = f
                    .sig
                    .params
                    .iter()
                    .map(|p| Param {
                        default: p.default.as_ref().map(|d| lcr_expr(d, &lcr, &mut scope)),
                        ..p.clone()
                    })
                    .collect();
                Item::Fn(Function {
                    sig: Signature {
                        params,
                        ..f.sig.clone()
                    },
                    body: lcr_expr(&f.body, &lcr, &mut scope),
                    test_body: f
                        .test_body
                        .as_ref()
                        .map(|tb| lcr_expr(tb, &lcr, &mut scope)),
                    ..f.clone()
                })
            }
            Item::Struct(s) => Item::Struct(StructDecl {
                name: s.name.clone(),
                doc: s.doc.clone(),
                type_vars: s.type_vars.clone(),
                fields: s
                    .fields
                    .iter()
                    .map(|fd| FieldDecl {
                        default: fd
                            .default
                            .as_ref()
                            .map(|d| lcr_expr(d, &lcr, &mut Vec::new())),
                        ..fd.clone()
                    })
                    .collect(),
            }),
            // A payload default is an expression like any other and may name a
            // constructor: it is spliced at construction sites, but the
            // declaration keeps its own copy, and the checker reads that one.
            Item::Variant(v) => Item::Variant(VariantDecl {
                cases: v
                    .cases
                    .iter()
                    .map(|c| VariantCase {
                        payload: c
                            .payload
                            .iter()
                            .map(|slot| CaseParam {
                                default: slot
                                    .default
                                    .as_ref()
                                    .map(|d| lcr_expr(d, &lcr, &mut Vec::new())),
                                ..slot.clone()
                            })
                            .collect(),
                        ..c.clone()
                    })
                    .collect(),
                ..v.clone()
            }),
            Item::Const(_) => unreachable!("constants are substituted and dropped during load"),
            Item::Refinement(_) => {
                unreachable!("refinements are expanded and dropped during load")
            }
            Item::Import(_) => item.clone(),
        })
        .collect();
    // The wrappers the walk reached, in a deterministic order: two runs of the
    // compiler over one program must emit the same items in the same order, and
    // a `HashSet`'s iteration order is neither.
    let mut reached: Vec<String> = lcr.used.into_inner().into_iter().collect();
    reached.sort();
    let items = items
        .into_iter()
        .chain(
            reached
                .iter()
                .map(|k| Item::Fn(ctor_wrapper_fn(&wrappers[k], k))),
        )
        .collect();
    Program {
        items,
        sources: program.sources.clone(),
        doc: program.doc.clone(),
    }
}

/// The function a shared constructor wrapper is: `fn __ctor$V$C(__ctor0: T0,
/// ..) -> V { C(__ctor0, ..) }`. Private, effect-free and non-generic, which is
/// what a function passed as a value has to be.
///
/// Spans are empty: the function is synthesized, not written, and a diagnostic
/// inside it would have nowhere useful to point. Every reference to the
/// constructor keeps its own span, on the `Ident` that names this.
fn ctor_wrapper_fn(w: &CtorWrapper, ctor: &str) -> Function {
    let params: Vec<Param> = w
        .payload
        .iter()
        .enumerate()
        .map(|(i, ty)| Param {
            name_span: Default::default(),
            name: format!("__ctor{i}"),
            ty: ty.clone(),
            mutable: false,
            arity: Arity::One,
            default: None,
            implicit_some: false,
        })
        .collect();
    let args: Vec<Expr> = (0..w.payload.len())
        .map(|i| Expr::new(ExprKind::Ident(format!("__ctor{i}")), 0..0))
        .collect();
    Function {
        name: aipl_syntax::SpanStr::synthetic(w.fn_name.clone()),
        is_pub: false,
        sig: Signature {
            type_vars: Vec::new(),
            params,
            effects: Vec::new(),
            return_ty: Some(Type::Named(w.variant.clone())),
        },
        body: Expr::new(
            ExprKind::Call(Callee::User(ctor.to_string()), args, false),
            0..0,
        ),
        test_body: None,
        test_fns: Vec::new(),
        doc: None,
    }
}

/// [`lower_ctor_refs`]'s expression walk. `scope` is the stack of bindings in
/// scope (a name is pushed for the subexpressions it covers and popped after),
/// so a shadowed constructor name is left alone.
fn lcr_expr(e: &Expr, lcr: &Lcr<'_>, scope: &mut Vec<String>) -> Expr {
    use ExprKind as K;
    // `rebuilt`, not `new`: this is the same expression with a lowered kind, so
    // it keeps the spans the parser recorded on it (`Expr::value_span`).
    let rw = |k: ExprKind| Expr::rebuilt(k, e);
    match &e.kind {
        // A shim's bindings already name functions directly (never a bare
        // constructor reference needing a lambda), so only the body is walked.
        K::Shim(effect, bindings, body) => rw(K::Shim(
            effect.clone(),
            bindings.clone(),
            Box::new(lcr_expr(body, lcr, scope)),
        )),
        K::Ident(name) => {
            let Some(payload) = lcr.ctors.get(name.as_str()) else {
                return e.clone();
            };
            if scope.iter().any(|s| s == name) {
                return e.clone();
            }
            // One shared wrapper where there can be one: the reference becomes
            // a plain call to it, and every other reference to the same
            // constructor becomes the same call. See `Lcr::wrappers`.
            if let Some(w) = lcr.wrappers.get(name.as_str()) {
                lcr.used.borrow_mut().insert(name.clone());
                return rw(K::Ident(w.fn_name.clone()));
            }
            // `Circle` → `|__ctor0: T0, ..| Circle(__ctor0, ..)`, every piece
            // carrying the reference's span.
            let params: Vec<LambdaParam> = payload
                .iter()
                .enumerate()
                .map(|(i, ty)| LambdaParam {
                    name: format!("__ctor{i}"),
                    ty: Some(ty.clone()),
                    span: e.span.clone(),
                })
                .collect();
            let args: Vec<Expr> = (0..payload.len())
                .map(|i| Expr::new(K::Ident(format!("__ctor{i}")), e.span.clone()))
                .collect();
            let body = Expr::new(
                K::Call(Callee::User(name.clone()), args, false),
                e.span.clone(),
            );
            rw(K::Lambda(params, Box::new(body)))
        }
        K::Num(_) | K::Float(_) | K::Bool(_) | K::Str(_) | K::Char(_) | K::None | K::Unit => {
            e.clone()
        }
        K::Call(name, args, method) => rw(K::Call(
            name.clone(),
            args.iter().map(|a| lcr_expr(a, lcr, scope)).collect(),
            *method,
        )),
        K::ArrayLit(xs) => rw(K::ArrayLit(
            xs.iter().map(|x| lcr_expr(x, lcr, scope)).collect(),
        )),
        K::SetLit(xs, o) => rw(K::SetLit(
            xs.iter().map(|x| lcr_expr(x, lcr, scope)).collect(),
            *o,
        )),
        K::TupleLit(xs) => rw(K::TupleLit(
            xs.iter().map(|x| lcr_expr(x, lcr, scope)).collect(),
        )),
        K::DictLit(pairs) => rw(K::DictLit(
            pairs
                .iter()
                .map(|(k, v)| (lcr_expr(k, lcr, scope), lcr_expr(v, lcr, scope)))
                .collect(),
        )),
        K::Construct(name, inits) => rw(K::Construct(
            name.clone(),
            inits
                .iter()
                .map(|init| FieldInit {
                    value: lcr_expr(&init.value, lcr, scope),
                    ..init.clone()
                })
                .collect(),
        )),
        K::Seq(a, b) => rw(K::Seq(
            Box::new(lcr_expr(a, lcr, scope)),
            Box::new(lcr_expr(b, lcr, scope)),
        )),
        K::Index(a, b) => rw(K::Index(
            Box::new(lcr_expr(a, lcr, scope)),
            Box::new(lcr_expr(b, lcr, scope)),
        )),
        K::While(a, b) => rw(K::While(
            Box::new(lcr_expr(a, lcr, scope)),
            Box::new(lcr_expr(b, lcr, scope)),
        )),
        K::Let(name, ty, val, body) => {
            let val = lcr_expr(val, lcr, scope);
            scope.push(name.clone());
            let body = lcr_expr(body, lcr, scope);
            scope.pop();
            rw(K::Let(
                name.clone(),
                ty.clone(),
                Box::new(val),
                Box::new(body),
            ))
        }
        K::LetMut(name, ty, val, body) => {
            let val = lcr_expr(val, lcr, scope);
            scope.push(name.clone());
            let body = lcr_expr(body, lcr, scope);
            scope.pop();
            rw(K::LetMut(
                name.clone(),
                ty.clone(),
                Box::new(val),
                Box::new(body),
            ))
        }
        K::For(var, index, iter, body) => {
            let iter = lcr_expr(iter, lcr, scope);
            scope.push(var.clone());
            // Both binders scope over the body.
            if let Some(i) = index {
                scope.push(i.clone());
            }
            let body = lcr_expr(body, lcr, scope);
            if index.is_some() {
                scope.pop();
            }
            scope.pop();
            rw(K::For(
                var.clone(),
                index.clone(),
                Box::new(iter),
                Box::new(body),
            ))
        }
        K::Assign(lhs, val, body) => rw(K::Assign(
            Box::new(lcr_expr(lhs, lcr, scope)),
            Box::new(lcr_expr(val, lcr, scope)),
            Box::new(lcr_expr(body, lcr, scope)),
        )),
        K::If(c, t, f) => rw(K::If(
            Box::new(lcr_expr(c, lcr, scope)),
            Box::new(lcr_expr(t, lcr, scope)),
            Box::new(lcr_expr(f, lcr, scope)),
        )),
        K::Neg(x) => rw(K::Neg(Box::new(lcr_expr(x, lcr, scope)))),
        K::Field(x, f) => rw(K::Field(Box::new(lcr_expr(x, lcr, scope)), f.clone())),
        K::Try(x) => rw(K::Try(Box::new(lcr_expr(x, lcr, scope)))),
        K::Return(x) => rw(K::Return(Box::new(lcr_expr(x, lcr, scope)))),
        K::KwArg(name, x, fwd) => rw(K::KwArg(
            name.clone(),
            Box::new(lcr_expr(x, lcr, scope)),
            *fwd,
        )),
        K::TypeArgs(x, tys) => rw(K::TypeArgs(Box::new(lcr_expr(x, lcr, scope)), tys.clone())),
        K::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        K::Lambda(params, body) => {
            for p in params {
                scope.push(p.name.clone());
            }
            let body = lcr_expr(body, lcr, scope);
            for _ in params {
                scope.pop();
            }
            rw(K::Lambda(params.clone(), Box::new(body)))
        }
        K::Match(scrutinee, arms) => {
            let scrutinee = lcr_expr(scrutinee, lcr, scope);
            let arms = arms
                .iter()
                .map(|arm| {
                    let bindings = arm.pattern.bindings();
                    for b in &bindings {
                        scope.push(b.clone());
                    }
                    let body = lcr_expr(&arm.body, lcr, scope);
                    for _ in &bindings {
                        scope.pop();
                    }
                    MatchArm {
                        body,
                        ..arm.clone()
                    }
                })
                .collect();
            rw(K::Match(Box::new(scrutinee), arms))
        }
        K::IfLet(arm, scrutinee, else_b) => {
            let scrutinee = lcr_expr(scrutinee, lcr, scope);
            let bindings = arm.pattern.bindings();
            for b in &bindings {
                scope.push(b.clone());
            }
            let body = lcr_expr(&arm.body, lcr, scope);
            for _ in &bindings {
                scope.pop();
            }
            let arm = MatchArm {
                body,
                ..(**arm).clone()
            };
            let else_b = lcr_expr(else_b, lcr, scope);
            rw(K::IfLet(
                Box::new(arm),
                Box::new(scrutinee),
                Box::new(else_b),
            ))
        }
        K::Slice(a, b, c) => rw(K::Slice(
            Box::new(lcr_expr(a, lcr, scope)),
            Box::new(lcr_expr(b, lcr, scope)),
            c.as_ref().map(|c| Box::new(lcr_expr(c, lcr, scope))),
        )),
    }
}

/// Lower all `Type::Tuple` annotations in `program` to synthetic named structs.
///
/// Every `(A, B, C)` type annotation is replaced by `Type::Named("__tuple$A$B$C")` and
/// a corresponding `struct __tuple$A$B$C { _0: A; _1: B; _2: C; }` declaration is
/// prepended to the program. This runs before `check` so the checker and codegen
/// only ever see named struct types, never raw tuples. Expression-level `TupleLit`
/// nodes are left for mono to lower.
pub fn lower_tuples(program: &Program) -> Program {
    let mut fields_map: HashMap<String, Vec<FieldDecl>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    let mut new_items: Vec<Item> = program
        .items
        .iter()
        .map(|item| match item {
            Item::Fn(f) => {
                let new_params: Vec<Param> = f
                    .sig
                    .params
                    .iter()
                    .map(|p| Param {
                        ty: lt_ty(&p.ty, &mut fields_map, &mut order),
                        ..p.clone()
                    })
                    .collect();
                let new_ret = f
                    .sig
                    .return_ty
                    .as_ref()
                    .map(|t| lt_ty(t, &mut fields_map, &mut order));
                let new_body = lt_expr(&f.body, &mut fields_map, &mut order);
                let new_test = f
                    .test_body
                    .as_ref()
                    .map(|tb| lt_expr(tb, &mut fields_map, &mut order));
                Item::Fn(Function {
                    sig: Signature {
                        params: new_params,
                        return_ty: new_ret,
                        ..f.sig.clone()
                    },
                    body: new_body,
                    test_body: new_test,
                    ..f.clone()
                })
            }
            Item::Struct(s) => Item::Struct(StructDecl {
                name: s.name.clone(),
                doc: s.doc.clone(),
                type_vars: s.type_vars.clone(),
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
                doc: v.doc.clone(),
                type_vars: v.type_vars.clone(),
                cases: v
                    .cases
                    .iter()
                    .map(|c| VariantCase {
                        name: c.name.clone(),
                        doc: c.doc.clone(),
                        payload: c
                            .payload
                            .iter()
                            .map(|p| CaseParam {
                                ty: lt_ty(&p.ty, &mut fields_map, &mut order),
                                ..p.clone()
                            })
                            .collect(),
                    })
                    .collect(),
            }),
            Item::Const(_) => unreachable!("constants are substituted and dropped during load"),
            Item::Refinement(_) => {
                unreachable!("refinements are expanded and dropped during load")
            }
            Item::Import(_) => item.clone(),
        })
        .collect();

    let mut synth: Vec<Item> = order
        .into_iter()
        .map(|name| {
            Item::Struct(StructDecl {
                doc: None,
                type_vars: tuple_template_vars(&name),
                fields: fields_map.remove(&name).unwrap(),
                name: aipl_syntax::SpanStr::synthetic(name),
            })
        })
        .collect();
    synth.append(&mut new_items);
    Program {
        items: synth,
        sources: program.sources.clone(),
        doc: program.doc.clone(),
    }
}

/// The arity of the tuple instance `inst`, or `None` if it is not one.
///
/// A tuple instance is named by its element types alone (`__tuple$i64$str`), so
/// its arity is the number of `$`-separated mangles after the prefix. Element
/// mangles never contain a `$` themselves — `mangle_type` flattens one to `_` —
/// so the split is exact.
pub(crate) fn tuple_instance_arity(inst: &str) -> Option<usize> {
    inst.strip_prefix(&format!("{}$", typing::TUPLE_TEMPLATE))
        .map(|rest| rest.split('$').count())
}

/// Whether the synthetic struct `inst` could be an instance of the template
/// `base`.
///
/// Ordinarily an instance wears its template's name as a `base$` prefix. A
/// tuple's does not: both routes to a tuple struct have to agree on one name
/// (see `generic_instance_name`), and that name is the concrete one, which says
/// nothing about which per-arity template built it. So a tuple instance belongs
/// to the template of its own arity.
fn tuple_instance_of(inst: &str, base: &str) -> bool {
    match tuple_instance_arity(inst) {
        Some(n) => base == format!("{}{n}", typing::TUPLE_TEMPLATE),
        None => inst.starts_with(&format!("{base}$")),
    }
}

/// The `i`th type parameter of a tuple template. Named with a leading `_` so it
/// cannot collide with a source type parameter, which is an ordinary identifier.
fn tuple_var_name(i: usize) -> String {
    format!("_T{i}")
}

/// The type parameters of the synthetic struct called `name`, which is a tuple
/// template (`__tuple2`, `__tuple3`, …) or an ordinary monomorphic synthetic
/// struct with none. Read off the name rather than carried alongside it: a
/// template's arity *is* the digits in its name, so there is nothing to keep in
/// step.
pub(crate) fn tuple_template_vars(name: &str) -> Vec<TypeParam> {
    let Some(arity) = name.strip_prefix(typing::TUPLE_TEMPLATE) else {
        return Vec::new();
    };
    // A concrete tuple's struct also starts with the prefix, but what follows is
    // `$`-led element types rather than an arity.
    let Ok(n) = arity.parse::<usize>() else {
        return Vec::new();
    };
    (0..n)
        .map(|i| TypeParam {
            name: tuple_var_name(i),
            bound: aipl_syntax::ast::Bound::Any,
            default: None,
            name_span: Default::default(),
        })
        .collect()
}

/// Whether `t` mentions a type variable anywhere — the question that decides
/// whether a tuple has a layout yet.
fn ty_has_var(t: &Type) -> bool {
    match t {
        Type::TypeVar(_) => true,
        Type::Case(i)
        | Type::Optional(i)
        | Type::Array(i)
        | Type::Set(i, _)
        | Type::Without(i, _) => ty_has_var(i),
        Type::Dict(a, b) | Type::Result(a, b) => ty_has_var(a) || ty_has_var(b),
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(ty_has_var),
        Type::Fn(ps, r) => ps.iter().any(ty_has_var) || ty_has_var(r),
        Type::Primitive(_)
        | Type::Named(_)
        | Type::Unit
        | Type::Any
        | Type::Unknown
        | Type::ConcatStr => false,
    }
}

/// Lower a type, registering any new synthetic tuple-struct in `fields_map`/`order`.
fn lt_ty(
    t: &Type,
    fields_map: &mut HashMap<String, Vec<FieldDecl>>,
    order: &mut Vec<String>,
) -> Type {
    match t {
        Type::Case(v) => Type::Case(Box::new(lt_ty(v, fields_map, order))),
        Type::Tuple(elems) => {
            let lowered: Vec<Type> = elems.iter().map(|e| lt_ty(e, fields_map, order)).collect();
            // A tuple whose elements are all concrete has a layout right here,
            // so it is lowered straight to its own struct. One that mentions a
            // type variable does not: it is a *template*, and which struct it
            // becomes is only known once monomorphization binds that variable.
            // So it takes the same road `Box<T>` does — an abstract
            // `Type::Generic` that `instantiate_generic` specializes per
            // substitution — and lands on the very struct the concrete route
            // would have built, because `generic_instance_name` sends a tuple
            // base back through `tuple_struct_name`.
            if lowered.iter().any(ty_has_var) {
                let name = format!("{}{}", typing::TUPLE_TEMPLATE, lowered.len());
                if !fields_map.contains_key(&name) {
                    order.push(name.clone());
                    let fs: Vec<FieldDecl> = (0..lowered.len())
                        .map(|i| FieldDecl {
                            name: aipl_syntax::SpanStr::synthetic(format!("_{i}")),
                            ty: Type::TypeVar(tuple_var_name(i)),
                            default: None,
                        })
                        .collect();
                    fields_map.insert(name.clone(), fs);
                }
                return Type::Generic(name, lowered);
            }
            let name = typing::tuple_struct_name(&lowered);
            if !fields_map.contains_key(&name) {
                order.push(name.clone());
                let fs: Vec<FieldDecl> = lowered
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| FieldDecl {
                        name: aipl_syntax::SpanStr::synthetic(format!("_{i}")),
                        ty: ty.clone(),
                        default: None,
                    })
                    .collect();
                fields_map.insert(name.clone(), fs);
            }
            Type::Named(name)
        }
        Type::TypeVar(v) => Type::TypeVar(v.clone()),
        Type::Unknown => Type::Unknown,
        Type::Array(inner) => Type::Array(Box::new(lt_ty(inner, fields_map, order))),
        Type::Without(base, x) => {
            Type::Without(Box::new(lt_ty(base, fields_map, order)), x.clone())
        }
        Type::Set(inner, o) => Type::Set(Box::new(lt_ty(inner, fields_map, order)), *o),
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
        Type::Named(_) | Type::Primitive(_) | Type::Unit | Type::Any | Type::ConcatStr => t.clone(),
        // `lower_generics` resolves every *concrete* generic application to a
        // `Named` type; an abstract one (`Box<T>` over a generic function's own
        // type variable) is left for monomorphization — pass it through,
        // lowering any tuple inside its arguments.
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter().map(|a| lt_ty(a, fields_map, order)).collect(),
        ),
    }
}

/// Walk an expression, lowering any `Type::Tuple` that appears in lambda-param
/// type annotations. All other expression structure is preserved unchanged.
fn lt_expr(e: &Expr, fm: &mut HashMap<String, Vec<FieldDecl>>, ord: &mut Vec<String>) -> Expr {
    let kind = match &e.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::TypeArgs(x, tys) => {
            ExprKind::TypeArgs(Box::new(lt_expr(x, fm, ord)), tys.clone())
        }
        ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        // A shim carries no type annotations of its own; only its body can.
        ExprKind::Shim(effect, bindings, body) => ExprKind::Shim(
            effect.clone(),
            bindings.clone(),
            Box::new(lt_expr(body, fm, ord)),
        ),
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
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit
        | ExprKind::Ident(_) => e.kind.clone(),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(lt_expr(x, fm, ord))),
        ExprKind::Field(x, f) => ExprKind::Field(Box::new(lt_expr(x, fm, ord)), f.clone()),
        ExprKind::Try(x) => ExprKind::Try(Box::new(lt_expr(x, fm, ord))),
        ExprKind::Return(x) => ExprKind::Return(Box::new(lt_expr(x, fm, ord))),
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
        // The annotation is lowered like any other declared type: `let p: (i64,
        // str) = ..` has to name the same struct the value's tuple literal
        // builds, since comparing the two is how the binding is checked.
        ExprKind::Let(n, ty, a, b) => ExprKind::Let(
            n.clone(),
            ty.as_ref().map(|t| lt_ty(t, fm, ord)),
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::LetMut(n, ty, a, b) => ExprKind::LetMut(
            n.clone(),
            ty.as_ref().map(|t| lt_ty(t, fm, ord)),
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::Assign(lhs, a, b) => ExprKind::Assign(
            lhs.clone(),
            Box::new(lt_expr(a, fm, ord)),
            Box::new(lt_expr(b, fm, ord)),
        ),
        ExprKind::For(v, i, iter, body) => ExprKind::For(
            v.clone(),
            i.clone(),
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
        ExprKind::SetLit(elems, o) => {
            ExprKind::SetLit(elems.iter().map(|a| lt_expr(a, fm, ord)).collect(), *o)
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
                    written: arm.written.clone(),
                    pattern: arm.pattern.clone(),
                    body: lt_expr(&arm.body, fm, ord),
                    span: arm.span.clone(),
                })
                .collect(),
        ),
        ExprKind::IfLet(arm, s, else_b) => ExprKind::IfLet(
            Box::new(MatchArm {
                written: arm.written.clone(),
                pattern: arm.pattern.clone(),
                body: lt_expr(&arm.body, fm, ord),
                span: arm.span.clone(),
            }),
            Box::new(lt_expr(s, fm, ord)),
            Box::new(lt_expr(else_b, fm, ord)),
        ),
    };
    Expr::rebuilt(kind, e)
}

/// Resolves generic struct/variant *type annotations* (`Type::Generic`) into
/// synthetic monomorphic named types, one per distinct instantiation — the
/// generic analogue of `lower_tuples`. Constructions (`Box { value: 5 }`) carry
/// no type annotation and are resolved separately by the checker's / mono's
/// inference; this pass handles every *written* type (`fn f(b: Box<i64>)`,
/// `struct S { b: Box<i64> }`, a lambda-param annotation, …).
///
/// Generic template declarations themselves are left untouched (their fields
/// still mention their own type variables and are only instantiated on use);
/// each *concrete* instantiation is appended as an ordinary named struct/variant
/// with its type variables substituted.
struct GenericLowerer {
    struct_templates: HashMap<String, StructDecl>,
    variant_templates: HashMap<String, VariantDecl>,
    /// Synthesized monomorphic instance items, in first-seen order.
    synth: Vec<Item>,
    /// Instance names already synthesized (dedup + recursion guard).
    seen: HashSet<String>,
    /// Type variables that are abstract in the type currently being lowered —
    /// the enclosing generic function's parameters. A generic application whose
    /// arguments mention one of these (`Box<T>` inside `fn f<T>(..)`) is *not*
    /// instantiated: it can't be, `T` has no concrete layout. It's left as a
    /// `Type::Generic` for the checker to handle abstractly and for
    /// monomorphization to instantiate once `T` is pinned to a concrete type.
    abstract_vars: Vec<String>,
}

impl GenericLowerer {
    fn new(program: &Program) -> Self {
        let mut struct_templates = HashMap::new();
        let mut variant_templates = HashMap::new();
        for item in &program.items {
            match item {
                Item::Struct(s) if s.is_generic() => {
                    struct_templates.insert(s.name.text.clone(), s.clone());
                }
                Item::Variant(v) if v.is_generic() => {
                    variant_templates.insert(v.name.text.clone(), v.clone());
                }
                _ => {}
            }
        }
        GenericLowerer {
            struct_templates,
            variant_templates,
            synth: Vec::new(),
            seen: HashSet::new(),
            abstract_vars: Vec::new(),
        }
    }

    /// Resolve every generic application in `t` to a synthetic `Named` type,
    /// recursing into type arguments first (so `Box<Box<i64>>` resolves the
    /// inner instance before the outer).
    fn lower_ty(&mut self, t: &Type) -> Result<Type, Error> {
        Ok(match t {
            Type::Generic(base, args) => {
                let args: Vec<Type> = args
                    .iter()
                    .map(|a| self.lower_ty(a))
                    .collect::<Result<_, _>>()?;
                // A generic application over an abstract type variable (an
                // enclosing generic function's parameter) can't be instantiated
                // yet — keep it abstract for the checker and monomorphization.
                if args.iter().any(|a| mentions_var(a, &self.abstract_vars)) {
                    Type::Generic(base.clone(), args)
                } else {
                    Type::Named(self.instantiate(base, &args)?)
                }
            }
            Type::Optional(i) => Type::Optional(Box::new(self.lower_ty(i)?)),
            Type::Array(i) => Type::Array(Box::new(self.lower_ty(i)?)),
            Type::Without(b, x) => Type::Without(Box::new(self.lower_ty(b)?), x.clone()),
            Type::Set(i, o) => Type::Set(Box::new(self.lower_ty(i)?), *o),
            Type::Dict(k, v) => {
                Type::Dict(Box::new(self.lower_ty(k)?), Box::new(self.lower_ty(v)?))
            }
            Type::Result(ok, err) => {
                Type::Result(Box::new(self.lower_ty(ok)?), Box::new(self.lower_ty(err)?))
            }
            Type::Fn(ps, r) => Type::Fn(
                ps.iter()
                    .map(|p| self.lower_ty(p))
                    .collect::<Result<_, _>>()?,
                Box::new(self.lower_ty(r)?),
            ),
            Type::Tuple(es) => Type::Tuple(
                es.iter()
                    .map(|e| self.lower_ty(e))
                    .collect::<Result<_, _>>()?,
            ),
            _ => t.clone(),
        })
    }

    /// Register (if new) the monomorphic instance of generic `base` applied to
    /// concrete `args`, returning its synthetic name. Substitutes the template's
    /// type variables and recursively lowers the resulting field/payload types
    /// (so a field of a nested generic type instantiates that one too).
    fn instantiate(&mut self, base: &str, args: &[Type]) -> Result<String, Error> {
        let name = typing::generic_instance_name(base, args);
        if self.seen.contains(&name) {
            return Ok(name);
        }
        // Mark seen before recursing so a (mutually) recursive generic doesn't
        // loop forever — it'll reference the in-progress name instead.
        self.seen.insert(name.clone());

        if let Some(tmpl) = self.struct_templates.get(base).cloned() {
            let map = bind_type_args(base, &tmpl.type_vars, args)?;
            let mut fields = Vec::with_capacity(tmpl.fields.len());
            for fd in &tmpl.fields {
                let ty = self.lower_ty(&subst_type_params(&fd.ty, &map))?;
                fields.push(FieldDecl {
                    name: fd.name.clone(),
                    ty,
                    default: fd.default.clone(),
                });
            }
            self.synth.push(Item::Struct(StructDecl {
                doc: None,
                name: aipl_syntax::SpanStr::synthetic(name.clone()),
                type_vars: Vec::new(),
                fields,
            }));
        } else if let Some(tmpl) = self.variant_templates.get(base).cloned() {
            let map = bind_type_args(base, &tmpl.type_vars, args)?;
            let mut cases = Vec::with_capacity(tmpl.cases.len());
            for c in &tmpl.cases {
                let payload = c
                    .payload
                    .iter()
                    .map(|p| {
                        Ok(CaseParam {
                            ty: self.lower_ty(&subst_type_params(&p.ty, &map))?,
                            ..p.clone()
                        })
                    })
                    .collect::<Result<_, Error>>()?;
                cases.push(VariantCase {
                    doc: c.doc.clone(),
                    name: c.name.clone(),
                    payload,
                });
            }
            self.synth.push(Item::Variant(VariantDecl {
                doc: None,
                name: aipl_syntax::SpanStr::synthetic(name.clone()),
                type_vars: Vec::new(),
                cases,
            }));
        } else {
            return Err(Error::msg(format!(
                "unknown generic type {base:?} (no such generic struct or variant)"
            )));
        }
        Ok(name)
    }

    /// Lower every type annotation reachable from an expression — currently only
    /// lambda-parameter type annotations carry a `Type`.
    fn lower_expr(&mut self, e: &Expr) -> Result<Expr, Error> {
        use ExprKind as K;
        let b = |lc: &mut Self, x: &Expr| lc.lower_expr(x).map(Box::new);
        let kind = match &e.kind {
            // A shim carries no type annotations of its own.
            K::Shim(effect, bindings, body) => {
                K::Shim(effect.clone(), bindings.clone(), b(self, body)?)
            }
            K::Lambda(params, body) => {
                let params = params
                    .iter()
                    .map(|p| {
                        Ok(LambdaParam {
                            ty: p.ty.as_ref().map(|t| self.lower_ty(t)).transpose()?,
                            ..p.clone()
                        })
                    })
                    .collect::<Result<_, Error>>()?;
                K::Lambda(params, b(self, body)?)
            }
            K::Num(_)
            | K::Float(_)
            | K::Bool(_)
            | K::Str(_)
            | K::Char(_)
            | K::None
            | K::Unit
            | K::Ident(_) => e.kind.clone(),
            K::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
            K::TypeArgs(x, tys) => K::TypeArgs(b(self, x)?, tys.clone()),
            K::Spread(..) => unreachable!("array spreads are desugared by the loader"),
            K::Neg(x) => K::Neg(b(self, x)?),
            K::Field(x, f) => K::Field(b(self, x)?, f.clone()),
            K::Try(x) => K::Try(b(self, x)?),
            K::Return(x) => K::Return(b(self, x)?),
            K::Seq(a, c) => K::Seq(b(self, a)?, b(self, c)?),
            K::Index(a, c) => K::Index(b(self, a)?, b(self, c)?),
            K::While(a, c) => K::While(b(self, a)?, b(self, c)?),
            K::If(a, c, d) => K::If(b(self, a)?, b(self, c)?, b(self, d)?),
            K::Slice(a, c, d) => K::Slice(
                b(self, a)?,
                b(self, c)?,
                d.as_ref().map(|x| b(self, x)).transpose()?,
            ),
            K::Let(n, ty, a, c) => K::Let(n.clone(), ty.clone(), b(self, a)?, b(self, c)?),
            K::LetMut(n, ty, a, c) => K::LetMut(n.clone(), ty.clone(), b(self, a)?, b(self, c)?),
            K::Assign(lhs, a, c) => K::Assign(lhs.clone(), b(self, a)?, b(self, c)?),
            K::For(v, i, iter, body) => {
                K::For(v.clone(), i.clone(), b(self, iter)?, b(self, body)?)
            }
            K::Call(name, args, ms) => K::Call(
                name.clone(),
                args.iter()
                    .map(|a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?,
                *ms,
            ),
            K::ArrayLit(es) => K::ArrayLit(
                es.iter()
                    .map(|a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?,
            ),
            K::SetLit(es, o) => K::SetLit(
                es.iter()
                    .map(|a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?,
                *o,
            ),
            K::TupleLit(es) => K::TupleLit(
                es.iter()
                    .map(|a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?,
            ),
            K::DictLit(pairs) => K::DictLit(
                pairs
                    .iter()
                    .map(|(k, v)| Ok((self.lower_expr(k)?, self.lower_expr(v)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            K::Construct(name, inits) => K::Construct(
                name.clone(),
                inits
                    .iter()
                    .map(|fi| {
                        Ok(FieldInit {
                            name: fi.name.clone(),
                            value: self.lower_expr(&fi.value)?,
                        })
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            K::Match(s, arms) => K::Match(
                b(self, s)?,
                arms.iter()
                    .map(|arm| {
                        Ok(MatchArm {
                            written: arm.written.clone(),
                            pattern: arm.pattern.clone(),
                            body: self.lower_expr(&arm.body)?,
                            span: arm.span.clone(),
                        })
                    })
                    .collect::<Result<_, Error>>()?,
            ),
            K::IfLet(arm, s, else_b) => K::IfLet(
                Box::new(MatchArm {
                    written: arm.written.clone(),
                    pattern: arm.pattern.clone(),
                    body: self.lower_expr(&arm.body)?,
                    span: arm.span.clone(),
                }),
                b(self, s)?,
                b(self, else_b)?,
            ),
        };
        Ok(Expr::rebuilt(kind, e))
    }
}

/// Bind a generic template's declared type variables to the concrete `args` of
/// one instantiation, erroring on an arity mismatch.
/// Whether `t` mentions any of the type-variable names in `vars`.
fn mentions_var(t: &Type, vars: &[String]) -> bool {
    vars.iter().any(|v| ty_mentions(t, v))
}

fn bind_type_args(
    base: &str,
    type_vars: &[TypeParam],
    args: &[Type],
) -> Result<HashMap<String, Type>, Error> {
    if type_vars.len() != args.len() {
        return Err(Error::msg(format!(
            "generic type {base:?} expects {} type argument(s), got {}",
            type_vars.len(),
            args.len()
        )));
    }
    Ok(type_vars
        .iter()
        .map(|tv| tv.name.clone())
        .zip(args.iter().cloned())
        .collect())
}

/// Resolve generic struct/variant type annotations to synthetic monomorphic
/// named types (see [`GenericLowerer`]). Runs before `lower_tuples` so the rest
/// of the pipeline only ever sees ordinary named types for generics.
pub fn lower_generics(program: &Program) -> Result<Program, Error> {
    let mut lc = GenericLowerer::new(program);
    let mut items: Vec<Item> = Vec::with_capacity(program.items.len());
    for item in &program.items {
        lc.abstract_vars.clear();
        // A type in a signature or a field carries no span, so an error from
        // lowering one is located at the declaration that wrote it.
        let (declared_name, declared_at) = match item {
            Item::Fn(f) => (Some(&f.name), Some(f.name.span.clone())),
            Item::Struct(s) => (Some(&s.name), Some(s.name.span.clone())),
            Item::Variant(v) => (Some(&v.name), Some(v.name.span.clone())),
            Item::Const(_) | Item::Refinement(_) | Item::Import(_) => (None, None),
        };
        let lowered = (|| -> Result<Item, Error> {
            Ok(match item {
                Item::Fn(f) => {
                    // The function's own type parameters are abstract while lowering
                    // its signature/body: a `Box<T>` over one of them stays generic.
                    lc.abstract_vars = f.sig.type_var_names();
                    let params = f
                        .sig
                        .params
                        .iter()
                        .map(|p| {
                            Ok(Param {
                                ty: lc.lower_ty(&p.ty)?,
                                default: p
                                    .default
                                    .as_ref()
                                    .map(|d| lc.lower_expr(d))
                                    .transpose()?,
                                ..p.clone()
                            })
                        })
                        .collect::<Result<Vec<_>, Error>>()?;
                    let return_ty = f
                        .sig
                        .return_ty
                        .as_ref()
                        .map(|t| lc.lower_ty(t))
                        .transpose()?;
                    let body = lc.lower_expr(&f.body)?;
                    let test_body = f.test_body.as_ref().map(|t| lc.lower_expr(t)).transpose()?;
                    Item::Fn(Function {
                        sig: Signature {
                            params,
                            return_ty,
                            ..f.sig.clone()
                        },
                        body,
                        test_body,
                        ..f.clone()
                    })
                }
                // A generic template is left untouched — its fields still reference
                // its own type variables; each concrete use is instantiated on demand.
                Item::Struct(s) if s.is_generic() => item.clone(),
                Item::Variant(v) if v.is_generic() => item.clone(),
                Item::Struct(s) => Item::Struct(StructDecl {
                    doc: s.doc.clone(),
                    name: s.name.clone(),
                    type_vars: Vec::new(),
                    fields: s
                        .fields
                        .iter()
                        .map(|fd| {
                            Ok(FieldDecl {
                                ty: lc.lower_ty(&fd.ty)?,
                                default: fd
                                    .default
                                    .as_ref()
                                    .map(|d| lc.lower_expr(d))
                                    .transpose()?,
                                name: fd.name.clone(),
                            })
                        })
                        .collect::<Result<_, Error>>()?,
                }),
                Item::Variant(v) => Item::Variant(VariantDecl {
                    doc: v.doc.clone(),
                    name: v.name.clone(),
                    type_vars: Vec::new(),
                    cases: v
                        .cases
                        .iter()
                        .map(|c| {
                            Ok(VariantCase {
                                doc: c.doc.clone(),
                                name: c.name.clone(),
                                payload: c
                                    .payload
                                    .iter()
                                    .map(|p| {
                                        Ok(CaseParam {
                                            ty: lc.lower_ty(&p.ty)?,
                                            ..p.clone()
                                        })
                                    })
                                    .collect::<Result<_, Error>>()?,
                            })
                        })
                        .collect::<Result<_, Error>>()?,
                }),
                Item::Const(_) => unreachable!("constants are substituted and dropped during load"),
                Item::Refinement(_) => {
                    unreachable!("refinements are expanded and dropped during load")
                }
                Item::Import(_) => item.clone(),
            })
        })()
        .map_err(|e| match (declared_name, declared_at) {
            (Some(name), Some(span)) => typing::attribute(e.or_at(span), name, &program.sources),
            _ => e,
        })?;
        items.push(lowered);
    }
    // Prepend synthesized instances (like `lower_tuples`) so their declarations
    // precede any use.
    let mut out = lc.synth;
    out.extend(items);
    Ok(Program {
        items: out,
        sources: program.sources.clone(),
        doc: program.doc.clone(),
    })
}

/// Rewrite `program` so it contains no type variables: every generic function
/// is replaced by zero or more concrete instances, one per distinct tuple of
/// type arguments it's called with.
pub fn monomorphize(
    typed: &aipl_syntax::typed::TProgram,
    instances: &pin::Instances,
    dbg: DebugOptions,
) -> Result<MonoProgram, Error> {
    // Signatures, structs and variants are read from the written declarations.
    let program = &pinned_pipeline::to_program(typed);
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
    // Generic templates: kept aside (never emitted — they have no concrete
    // layout) so a construction `Box { value: 5 }` can be instantiated on demand.
    let mut generic_structs: HashMap<String, StructDecl> = HashMap::new();
    let mut generic_variants: HashMap<String, VariantDecl> = HashMap::new();

    for item in &program.items {
        match item {
            Item::Struct(s) if s.is_generic() => {
                generic_structs.insert(s.name.text.clone(), s.clone());
            }
            Item::Struct(s) => {
                structs.insert(
                    s.name.text.clone(),
                    s.fields
                        .iter()
                        .map(|f| (f.name.text.clone(), f.ty.clone(), f.default.clone()))
                        .collect(),
                );
                passthrough.push(item.clone());
            }
            Item::Variant(v) if v.is_generic() => {
                generic_variants.insert(v.name.text.clone(), v.clone());
            }
            Item::Variant(v) => {
                // Constructor registration is deferred below: a generic-variant
                // instance shares its constructor names across every instance, so
                // those are resolved by type rather than the unique `ctors` map.
                variants.insert(
                    v.name.text.clone(),
                    v.cases
                        .iter()
                        .map(|c| (c.name.text.clone(), c.payload_tys()))
                        .collect(),
                );
                passthrough.push(item.clone());
            }
            Item::Const(_) => unreachable!("constants are substituted and dropped during load"),
            Item::Refinement(_) => {
                unreachable!("refinements are expanded and dropped during load")
            }
            Item::Import(_) => passthrough.push(item.clone()),
            Item::Fn(f) => {
                if f.sig.is_mutating() {
                    mutating.insert(f.name.text.clone());
                }
                // Both rejections below are about the signature as written, which
                // carries no span of its own: they point at the name.
                let at_name = |e: Error| {
                    typing::attribute(e.or_at(f.name.span.clone()), &f.name, &program.sources)
                };
                if is_generic(f) {
                    generics.insert(f.name.text.clone(), normalize(f).map_err(at_name)?);
                } else {
                    // A non-generic fn must not mention `any` in its return.
                    if f.sig
                        .return_ty
                        .as_ref()
                        .is_some_and(|t| ty_mentions(t, "any"))
                    {
                        return Err(at_name(Error::msg(format!(
                            "fn \"{}\": \"any\" is only allowed in a parameter (\"any[]\", \"any?\") \
                             or via a declared type parameter \"<T: any>\"",
                            f.name
                        ))));
                    }
                    fn_returns.insert(
                        f.name.text.clone(),
                        f.sig
                            .return_ty
                            .clone()
                            .unwrap_or(Type::Primitive(Primitive::I64)),
                    );
                    concrete_fns.push(f);
                }
            }
        }
    }

    // A generic-variant template's constructors, keyed by the loader's
    // variant-qualified name `Case@Template` → the template base. (Like
    // non-generic constructors, generic ones are addressed only when in scope;
    // the loader rewrites every reference to the qualified form.)
    let mut generic_ctors: HashMap<String, String> = HashMap::new();
    for (base, tmpl) in &generic_variants {
        for c in &tmpl.cases {
            generic_ctors.insert(format!("{}@{}", c.name, base), base.clone());
        }
    }
    // Register the constructors of concrete, non-instance variants. A generic-
    // variant instance's constructors are shared by name across instances and
    // resolved by type (see `typing::is_variant_instance`), so they're skipped.
    for (vn, cases) in &variants {
        if typing::is_variant_instance(vn, &generic_variants) {
            continue;
        }
        for (c, _) in cases {
            // Only the variant-qualified name resolves — the loader rewrites every
            // in-scope reference to `Case@Variant`; a bare case reaching here means
            // the constructor wasn't imported.
            ctors.insert(format!("{c}@{vn}"), vn.clone());
        }
    }

    // Builtins declared with a `mut self` receiver join the mutating set too —
    // they aren't `Item::Fn`s in `program`, so the loop above never sees them.
    // Membership buys the same treatment as a user mutating method: an
    // expression-position `v.f(x)` (method or free form) copy-and-modifies,
    // while `set v.f(x)` writes back in place. Derived from the signature table
    // rather than naming `push`, so a future mutating builtin is picked up here
    // automatically.
    mutating.extend(mutating_builtins().map(str::to_string));

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
                f.name.text.clone(),
                ConcreteTemplate {
                    typed: None,
                    params: f.sig.params.clone(),
                    effects: f.sig.effects.clone(),
                    return_ty: f.sig.return_ty.clone(),
                },
            )
        })
        .collect();

    let mut mono = Mono {
        generics: &generics,
        concrete,
        mutating: &mutating,
        structs: &structs,
        syn_structs: HashMap::new(),
        variants: &variants,
        syn_variants: HashMap::new(),
        ctors: &ctors,
        generic_structs: &generic_structs,
        generic_variants: &generic_variants,
        generic_ctors: &generic_ctors,
        emitted: HashSet::new(),
        queue: VecDeque::new(),
        cur_fn: String::new(),
        cur_receiver: None,
        synth: 0,
        cur_effects: Vec::new(),
        shim_cells: Vec::new(),
        cur_lenv: HashMap::new(),
        lambda_envs: HashMap::new(),
        spec_memo: HashMap::new(),
        resolving: std::cell::RefCell::new(HashSet::new()),
        cur_ret: Type::Unit,
        scope_names: Vec::new(),
        in_place_call: false,
        current_parameters: HashMap::new(),
        specialized_from: HashMap::new(),
        pinned_instances: instances
            .iter()
            .map(|(name, (base, args))| {
                (
                    name.clone(),
                    (
                        base.clone(),
                        args.iter()
                            .map(aipl_syntax::typed::TyRepr::to_type)
                            .collect(),
                    ),
                )
            })
            .collect(),
        typed_fns: typed
            .items
            .iter()
            .filter_map(|item| match item {
                aipl_syntax::typed::TItem::Fn(f) => Some((f.name.text.as_str(), f)),
                _ => None,
            })
            .collect(),
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
            .filter(|f| !f.sig.params.iter().any(|p| matches!(p.ty, Type::Fn(_, _))))
            .map(|f| f.name.text.clone())
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
        let mut type_args: HashMap<String, Type> = HashMap::new();
        let mut typed_template: Option<typed_adapter::Typed> = None;
        let (params, effects, return_ty) =
            if let Some(Generic { sig, .. }) = generics.get(&inst.template) {
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
                    .map(|tp| tp.name.clone())
                    .zip(inst.specs.type_args.iter().cloned())
                    .collect();
                type_args = map.clone();
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
                        let set_kept = inst.specs.params.get(i).and_then(|p| p.set_kept);
                        let ty = match (str_kept, set_kept, ty) {
                            (true, _, t) if t == chars => Type::Primitive(Primitive::Str),
                            // A set passed as a `T[]` stays the set it is.
                            (_, Some(order), Type::Array(e)) => Type::Set(e, order),
                            (_, _, t) => t,
                        };
                        Param {
                            name_span: p.name_span.clone(),
                            name: p.name.clone(),
                            ty,
                            mutable: p.mutable,
                            arity: p.arity,
                            default: p.default.clone(),
                            implicit_some: p.implicit_some,
                        }
                    })
                    .collect();
                // See [`returns_its_receiver`]: an instance that took a `str` as its
                // sequence parameter hands one back.
                let declared: Vec<Type> = sig.params.iter().map(|p| p.ty.clone()).collect();
                let return_ty = match params.first() {
                    Some(first)
                        if first.ty == Type::Primitive(Primitive::Str)
                            && returns_its_receiver(&declared, sig.return_ty.as_ref()) =>
                    {
                        Some(Type::Primitive(Primitive::Str))
                    }
                    _ => sig.return_ty.as_ref().map(|t| subst_vars(t, &map)),
                };
                (params, sig.effects.clone(), return_ty)
            } else {
                // Concrete function: its signature is already concrete.
                let f = mono.concrete[&inst.template].clone();
                typed_template = f.typed;
                (f.params, f.effects, f.return_ty)
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
        // Substituting a generic function's type variables can turn a generic-
        // application parameter/return (`Emit<K>` → `Emit<Tok>`) concrete;
        // resolve those to their synthesized named instance (`Emit$Tok`) so
        // codegen — and `process` below — see only ordinary named types.
        let mut params = params;
        for p in &mut params {
            p.ty = mono.resolve_generic_ty(&p.ty)?;
        }
        let return_ty = return_ty.map(|t| mono.resolve_generic_ty(&t)).transpose()?;
        // Resolve any variadic parameters into their concrete shape: retype each
        // and prepend a prologue rebuilding the sequence the body iterates, so
        // `process` (and codegen) see only concrete, non-variadic parameters.
        let params = specialize_variadic(
            params,
            &inst.specs.indices(|p| p.variadic == VShape::Elem),
            &inst.specs.indices(|p| p.variadic == VShape::Opt),
        );
        mono.dbg.trace(
            "mono",
            format_args!(
                "process `{}` (owns {:?}, {} still queued)",
                inst.mangled,
                inst.specs.indices(|p| p.owned),
                mono.queue.len()
            ),
        );
        let out = mono.process(
            &inst.mangled,
            &inst.template,
            &params,
            &effects,
            &return_ty,
            &inst.specs.params,
            &type_args,
            typed_template.as_ref(),
        )?;
        out_fns.push(out);
    }

    let syn_structs: Vec<StructDecl> = mono
        .syn_structs
        .drain()
        .map(|(name, fields)| StructDecl {
            doc: None,
            name: aipl_syntax::SpanStr::synthetic(name),
            type_vars: Vec::new(),
            fields: fields
                .into_iter()
                .map(|(fname, ty, _)| aipl_syntax::ast::FieldDecl {
                    name: aipl_syntax::SpanStr::synthetic(fname),
                    ty,
                    default: None,
                })
                .collect(),
        })
        .collect();
    let syn_variants: Vec<VariantDecl> = mono
        .syn_variants
        .drain()
        .map(|(name, cases)| VariantDecl {
            doc: None,
            name: aipl_syntax::SpanStr::synthetic(name),
            type_vars: Vec::new(),
            cases: cases
                .into_iter()
                .map(|(name, payload)| VariantCase {
                    doc: None,
                    name: aipl_syntax::SpanStr::synthetic(name),
                    // A synthesized instance is downstream of every default
                    // (the loader filled them at each construction site), so
                    // its slots are plain types again.
                    payload: payload
                        .into_iter()
                        .map(|ty| CaseParam {
                            name: None,
                            ty,
                            default: None,
                            implicit_some: false,
                            doc: None,
                        })
                        .collect(),
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
            Item::Const(_) => unreachable!("constants are substituted and dropped during load"),
            Item::Refinement(_) => {
                unreachable!("refinements are expanded and dropped during load")
            }
            Item::Import(_) => {}
            Item::Fn(_) => unreachable!("passthrough never carries a fn item"),
        }
    }
    structs.extend(syn_structs);
    variants_out.extend(syn_variants);
    // The boundary. Everything past this point is concrete by construction, so
    // a type variable that escaped instantiation is caught here — with a name to
    // report — rather than reaching codegen and being mangled into a nonsense
    // instance name.
    let concrete_structs: Vec<ConcreteStructDecl> = structs
        .into_iter()
        .map(|s| ConcreteStructDecl {
            fields: s
                .fields
                .iter()
                .map(|f| ConcreteFieldDecl {
                    name: f.name.text.clone(),
                    ty: settled(&f.ty, &format!("struct {:?} field {:?}", s.name, f.name)),
                })
                .collect(),
            // Settled here, where the tuple naming scheme lives: past this
            // boundary a tuple is told apart by this flag, not by its name.
            is_tuple: tuple_instance_arity(&s.name).is_some(),
            name: s.name.text,
        })
        .collect();
    let concrete_variants: Vec<ConcreteVariantDecl> = variants_out
        .into_iter()
        .map(|v| ConcreteVariantDecl {
            cases: v
                .cases
                .iter()
                .map(|c| ConcreteVariantCase {
                    name: c.name.text.clone(),
                    payload: c
                        .payload
                        .iter()
                        .map(|p| settled(&p.ty, &format!("variant {:?} case {:?}", v.name, c.name)))
                        .collect(),
                })
                .collect(),
            name: v.name.text,
        })
        .collect();
    let shim_cells: Vec<ConcreteType> = mono
        .shim_cells
        .iter()
        .map(|ty| settled(ty, "a variable a shim closure captures"))
        .collect();
    Ok(MonoProgram {
        structs: concrete_structs,
        variants: concrete_variants,
        shim_cells,
        fns: out_fns,
    })
}

/// A type monomorphization has finished with, as a [`ConcreteType`].
///
/// Reaching the `None` case means an abstraction survived instantiation, which
/// is a compiler bug: `what` names where it was found so the report points at
/// the declaration rather than at whatever downstream pass first choked.
pub(crate) fn settled(ty: &Type, what: &str) -> ConcreteType {
    ty.to_concrete().unwrap_or_else(|| {
        panic!(
            "{what} is still abstract after monomorphization: {}",
            aipl_syntax::type_name(ty)
        )
    })
}

/// The shape a variadic (`T*`) argument takes at a call site: the sequence
/// itself, a single element, or an optional element. Each maps to a distinct
/// specialization. `Seq` (the default) is the plain instance; a non-variadic
/// parameter is always `Seq`. The same enum a shape-variadic builtin callee
/// carries once mono has classified its pattern.
type VShape = SeqShape;

/// Classify a variadic argument of type `arg_ty` against the parameter's
/// sequence type `seq_ty`. An optional is always the optional shape; otherwise
/// the argument is the sequence if it matches the sequence representation
/// (`str`/`Error` for a `str` sequence, any array for a `T[]` one), else a
/// single element.
fn variadic_shape(arg_ty: &Type, seq_ty: &Type) -> VShape {
    if matches!(arg_ty, Type::Optional(_)) {
        VShape::Opt
    } else if is_str_repr(seq_ty) || typing::is_char_array(seq_ty) {
        // A `char` sequence is str-shaped however it is spelled. A *declared*
        // `char*` parameter has sequence type `str`, but a generic `T*` keeps
        // `T[]` and only becomes `char[]` once `T` is substituted — the same
        // representation, so both have to take this branch or a `str` argument
        // to a `T = char` instance is misread as a single element.
        if is_str_repr(arg_ty) || typing::is_char_array(arg_ty) {
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

/// `params` with each variadic parameter given the shape this instance receives it in: one
/// element or an optional, or the sequence itself.
fn specialize_variadic(
    mut params: Vec<Param>,
    variadic_elem: &[usize],
    variadic_opt: &[usize],
) -> Vec<Param> {
    for (i, p) in params.iter_mut().enumerate() {
        if !p.arity.is_variadic() {
            continue;
        }
        p.arity = Arity::One;
        let elem = variadic_elem_ty(&p.ty);
        if variadic_elem.contains(&i) {
            p.name = format!("{}$v", p.name);
            p.ty = elem;
        } else if variadic_opt.contains(&i) {
            p.name = format!("{}$v", p.name);
            p.ty = Type::Optional(Box::new(elem));
        }
        // else: the sequence form — keep the name and (sequence) type.
    }
    params
}

/// Substitute `type_args` (positional, matching `sig.type_vars` order) into
/// `sig`'s params and return type, yielding the concrete (monomorphic)
/// params/return-type pair a specialized instance has. Used by
/// [`Mono::concrete_signature`] for ownership eligibility — the substituted
/// params/return must be concrete heap types for `owned_eligible` to
/// recognize them, so a raw type variable left unsubstituted here would
/// silently disable the owned optimization. A free function (not an inherent
/// method) because `Signature` is defined in `aipl_syntax`, which orphan rules
/// forbid inherent-impl'ing from here.
fn make_concrete(sig: &Signature, type_args: &[Type]) -> (Vec<Param>, Option<Type>) {
    let map: HashMap<String, Type> = sig
        .type_vars
        .iter()
        .map(|tp| tp.name.clone())
        .zip(type_args.iter().cloned())
        .collect();
    let params = sig
        .params
        .iter()
        .map(|p| Param {
            name_span: p.name_span.clone(),
            name: p.name.clone(),
            ty: subst_vars(&p.ty, &map),
            mutable: p.mutable,
            arity: p.arity,
            default: p.default.clone(),
            implicit_some: p.implicit_some,
        })
        .collect();
    let return_ty = sig.return_ty.as_ref().map(|t| subst_vars(t, &map));
    (params, return_ty)
}

/// The body of a callee as the analyses of which parameters it can own read it: the pinned tree of
/// a source function, the typed tree of one synthesized from typed code, or nothing for a builtin.
enum OwnershipBody<'b> {
    Pinned(&'b aipl_syntax::typed::TExpr<aipl_syntax::typed::Ty>),
    Concrete(&'b typed_adapter::Typed),
    None,
}

/// A generic function template ready to monomorphize: a [`Signature`] plus the
/// body to substitute into when specializing a call.
#[derive(Clone)]
struct Generic {
    sig: Signature,
}

/// A fully-resolved function: every type variable has been substituted (there
/// are none left) and ownership/representation specialization decisions are
/// final, so it's ready for codegen. Distinct from [`Function`] (the AST type),
/// which additionally carries source-only concerns — visibility, declared type
/// parameters, an attached `.test`, documentation — that have no meaning once
/// monomorphization is done; conversely each param's `owned` has no meaning
/// *before* monomorphization, since specialization is the only thing that ever
/// sets it. Reusing one struct for both ends of the pass meant every source
/// function and every synthesized instance carried dead fields; splitting them
/// apart makes each representation self-explanatory and lets the compiler
/// catch a stray reference to the wrong one.
#[derive(Clone)]
pub struct ConcreteFn<Body = typed_util::Typed> {
    pub name: String,
    pub params: Vec<ConcreteParam>,
    pub effects: Vec<String>,
    pub return_ty: Option<ConcreteType>,
    /// The typed tree, once the instance has left monomorphization; the stamped `Expr` it is
    /// inferred as until then (`typed_adapter::to_typed` is the boundary).
    pub body: Body,
}

/// Whether a body names a function, for [`ConcreteFn::is_directly_recursive`] on either side
/// of the typed boundary.
pub trait Recursion {
    fn references(&self, name: &str) -> bool;
}

impl Recursion for Expr {
    fn references(&self, name: &str) -> bool {
        references_name(self, name)
    }
}

impl Recursion for typed_util::Typed {
    fn references(&self, name: &str) -> bool {
        typed_util::references_name(self, name)
    }
}

/// Classification every consumer of a [`ConcreteFn`] needs, answered from the
/// declaration alone.
///
/// These mirror the same questions [`Signature`] answers about a source
/// function — [`Signature::return_type`] and [`Signature::is_mutating`] have the
/// same names and the same meanings here — so the two ends of monomorphization
/// read alike. They live on the type rather than in codegen because every one of
/// them is a property of the function itself: no target, no layout, no ABI
/// lowering is consulted, only what was declared.
impl<Body> ConcreteFn<Body> {
    /// The declared return type, with an absent one read as `Unit`.
    ///
    /// This is what the *body* is checked against. What a caller receives can
    /// differ — see [`ConcreteFn::abi_return_type`].
    pub fn return_type(&self) -> ConcreteType {
        self.return_ty.clone().unwrap_or(ConcreteType::Unit)
    }

    /// A mutating method: `fn f(mut self: T, ...)`. It returns nothing to the
    /// user and mutates its receiver.
    pub fn is_mutating(&self) -> bool {
        self.params.first().is_some_and(|p| p.mutable)
    }

    /// Whether the body names the function itself, as a call or as a value.
    /// Direct only: two functions that name each other are not recursive by this
    /// test. Matches by name, so a local that rebinds the name would count too.
    /// Suitable as a guard before copying the body somewhere else.
    pub fn is_directly_recursive(&self) -> bool
    where
        Body: Recursion,
    {
        self.body.references(&self.name)
    }

    /// `main` declared with no return type: it returns nothing, and its exit
    /// code is implicitly 0. As the program entry it still needs an `i64`
    /// result, which codegen gives it (emitting 0) — but its body is
    /// type-checked as unit, so a trailing expression (an attempt to return a
    /// value) is an error.
    pub fn is_unit_main(&self) -> bool {
        self.name == "main" && self.return_ty.is_none()
    }

    /// `fn main() -> !Error`: an allowed entry variation. Its body yields a
    /// void-Ok result; at the ABI level `main` still returns an `i64` exit code,
    /// which codegen derives from the result — `ok()` → 0, `err(msg)` → print
    /// `error: <msg>` and exit 1.
    pub fn is_error_main(&self) -> bool {
        self.name == "main"
            && matches!(&self.return_ty, Some(ConcreteType::Result(ok, err))
                if aipl_syntax::concrete::is_unit(ok) && aipl_syntax::concrete::is_error(err))
    }

    /// The type this function actually yields to a caller — what its signature
    /// returns and what a call site sees.
    ///
    /// This is [`ConcreteFn::return_type`] except for the entry-style cases that
    /// produce a value while their body is checked as unit: a mutating method
    /// yields its (mutated) `self`, and either flavour of entry `main` yields an
    /// `i64` exit code.
    pub fn abi_return_type(&self) -> ConcreteType {
        if self.is_mutating() {
            self.params[0].ty.clone()
        } else if self.is_unit_main() || self.is_error_main() {
            // Both produce an `i64` exit code: a unit `main` always 0, an
            // `!Error` `main` 0/1 derived from its result.
            ConcreteType::Primitive(Primitive::I64)
        } else {
            self.return_type()
        }
    }
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
    pub ty: ConcreteType,
    pub mutable: bool,
    /// Whether this instance *takes ownership* of the parameter: the caller
    /// transfers its sole reference instead of retaining, and the callee is
    /// responsible for consuming it (so it isn't dropped on entry-scope exit).
    /// Set when a call passes a fresh, uniquely-owned heap argument; `false`
    /// for a plain borrow instance. Per-parameter (rather than an index list
    /// on `ConcreteFn`) because ownership is exactly a property of the
    /// parameter, decided once a call site is known — same reasoning as
    /// dropping `variadic`'s counterpart onto this type.
    pub owned: bool,
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
    /// The same body as the typed tree it is processed from, for a function synthesized from
    /// typed code; `body` is then this erased.
    typed: Option<typed_adapter::Typed>,
}

/// The output of [`monomorphize`]: struct/variant declarations pass through
/// unchanged, and every function is a [`ConcreteFn`] instance ready for
/// codegen. Replaces a whole [`Program`] (whose `Item::Fn` would otherwise
/// force codegen to share `Function` with the pre-mono source representation).
#[derive(Clone)]
pub struct MonoProgram {
    pub structs: Vec<ConcreteStructDecl>,
    pub variants: Vec<ConcreteVariantDecl>,
    pub fns: Vec<ConcreteFn>,
    /// The type of each cell a shim closure keeps a captured variable in, by cell
    /// number: `__shim_cell_get(n)` answers a value of the type at index `n`.
    pub shim_cells: Vec<ConcreteType>,
}

/// How a *single parameter* is specialized in an instance — the per-parameter
/// markers the mangled name encodes. A parameter can carry more than one at once
/// (e.g. a single owned `char[]`-kept-as-`str` parameter is both `owned` and
/// `str_kept`), so these are independent flags rather than one tag.
///
/// - `owned`: the caller moves its sole ref in (`$own{i}`).
/// - `str_kept`: a `char[]` parameter passed a `str`, kept as `str` — not
///   materialized; the body operates on the str directly (`$s{i}`).
/// - `set_kept`: a `T[]` parameter passed a set, kept as that set type (its
///   order recorded here) — the body walks and measures the set directly, which
///   is all a read-only generic does with a sequence (`$k{i}`).
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
    set_kept: Option<aipl_syntax::ast::SetOrder>,
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
    /// Names of mutating functions (`mut self` receiver), generic or not.
    mutating: &'a HashSet<String>,
    structs: &'a HashMap<String, Vec<(String, Type, Option<Expr>)>>,
    /// Synthetic struct definitions created on the fly for tuple literals and
    /// for generic-struct constructions (`Box { value: 5 }` → `Box$i64`).
    syn_structs: HashMap<String, Vec<(String, Type, Option<Expr>)>>,
    /// Variant types (name → cases) and the ctor → variant reverse map.
    variants: &'a HashMap<String, Vec<(String, Vec<Type>)>>,
    /// Synthetic variant definitions for generic-variant instances (`Opt$i64`).
    syn_variants: HashMap<String, Vec<(String, Vec<Type>)>>,
    ctors: &'a HashMap<String, String>,
    /// Generic struct/variant templates, for inferring and instantiating a
    /// construction of a generic type on demand.
    generic_structs: &'a HashMap<String, StructDecl>,
    generic_variants: &'a HashMap<String, VariantDecl>,
    /// Constructor name → the generic variant template it belongs to.
    generic_ctors: &'a HashMap<String, String>,
    /// Mangled names already queued, so each instance is processed exactly once.
    emitted: HashSet<String>,
    /// Reachable instances (concrete fns and generic specializations, in their
    /// borrow and owned forms) awaiting processing, in discovery order. Lambda
    /// specializations and synthesized lambda functions are inserted into
    /// `concrete` and queued here too.
    queue: VecDeque<Instance>,
    /// Mangled name of the instance currently being processed — the prefix of
    /// every function synthesized while inferring its body (see [`Mono::synth`]).
    cur_fn: String,
    /// The `mut self` receiver of the instance being processed: its name, and
    /// whether *this* instance owns it.
    ///
    /// It decides whether a call may hand the receiver on. An instance that owns
    /// its receiver holds the caller's reference outright and can transfer it; a
    /// *lent* one holds a reference of its own beside the borrow protocol's
    /// entry track — two owners for one block, deliberately — and handing that
    /// away loses one of them. Without this, a borrowed instance called the
    /// owned instance of the same method and the refcount went one release too
    /// far (a use-after-free, in mutual recursion, where both instances exist).
    cur_receiver: Option<(String, bool)>,
    /// Ordinal of the next name synthesized for `cur_fn` — a lifted lambda, a
    /// `map`/`filter`/`filter_map`/`zip` loop wrapper, a desugaring temporary.
    /// Reset per instance, so a synthesized function is `<instance>$<kind><n>`:
    /// local to the function that produced it, and stable under edits anywhere
    /// else. (A temporary's name is only ever bound inside the instance, so the
    /// per-instance ordinal is all the uniqueness it needs.) A single program-wide counter was
    /// what it replaced, and under that scheme every edit that added or removed
    /// a lambda renumbered all the ones discovered after it — hundreds of
    /// baseline-row moves and a `.clif` diff for a one-line change.
    synth: usize,
    /// Declared effects of the function currently being processed. A lambda's
    /// synthesized function and the lambda-specialized callee inherit these:
    /// the checker guarantees the enclosing function's effects cover the
    /// lambda's, so this is a sound (and per-call-site precise) over-approximation.
    cur_effects: Vec<String>,
    /// The type of each cell a shim closure keeps a captured variable in, by cell number
    /// (`shim_closures`).
    shim_cells: Vec<Type>,
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
    /// Instance names [`Mono::instance_args`] is part-way through recovering the
    /// type arguments of — the twin of the checker's `resolving`, and needed for
    /// the same reason: a recursive generic's payload names its own instance, so
    /// unifying it re-asks for the arguments being recovered.
    resolving: std::cell::RefCell<HashSet<String>>,
    /// Return type of the function currently being processed.
    cur_ret: Type,
    /// The pinned function each source function was written as, for the instances typed from it.
    typed_fns: HashMap<&'a str, &'a aipl_syntax::typed::TFunction>,
    /// The names bound around the expression being lowered from the typed tree, innermost last.
    scope_names: Vec<String>,
    /// Set while the call that a `set recv = recv.f(..)` updates in place is lowered.
    in_place_call: bool,
    /// The parameters of the instance being lowered from the typed tree, with their types.
    current_parameters: HashMap<String, ConcreteType>,
    /// The function each copy specialized for the functions it is handed has its body from.
    specialized_from: HashMap<String, typed_function_calls::SpecializedFrom>,
    /// The generic applications the pinner named as instances (`Box$i64`), by instance name.
    pinned_instances: HashMap<String, (String, Vec<Type>)>,
    dbg: DebugOptions,
}

type Env = HashMap<String, Type>;

impl Mono<'_> {
    /// Process one (already concrete) function: type the body and rewrite every
    /// generic call within it to its mangled instance name.
    fn process(
        &mut self,
        name: &str,
        template: &str,
        params: &[Param],
        effects: &[String],
        return_ty: &Option<Type>,
        specs: &[ParamSpec],
        type_args: &HashMap<String, Type>,
        typed_template: Option<&typed_adapter::Typed>,
    ) -> Result<ConcreteFn, Error> {
        let mut env: Env = HashMap::new();
        for p in params {
            env.insert(p.name.clone(), p.ty.clone());
        }
        // Lambdas synthesized while processing this body inherit its effects,
        // and are named under it.
        self.cur_fn = name.to_string();
        self.cur_receiver = params
            .first()
            .filter(|p| p.mutable)
            .map(|p| (p.name.clone(), specs.first().is_some_and(|s| s.owned)));
        self.synth = 0;
        self.cur_effects = effects.to_vec();
        self.cur_ret = return_ty.clone().unwrap_or(Type::Unit);
        // A lambda-specialized callee carries its function-typed parameters'
        // bindings; an ordinary function has none. Keyed by the template: its
        // per-shape instances (`$ve2`, a variadic passed one element) share it.
        self.cur_lenv = self.lambda_envs.get(template).cloned().unwrap_or_default();
        let body = match self.typed_body(template, params, specs, type_args, typed_template) {
            Some(typed) => typed,
            None => {
                return Err(Error::msg(format!(
                    "internal error: the typed route declined fn {name:?}"
                )))
            }
        };
        Ok(ConcreteFn {
            name: name.to_string(),
            // By now `specialize_variadic` has already resolved every
            // parameter's shape, so `variadic` is always false — dropped here.
            // `specs` is sized to the highest special-cased index (see
            // `enqueue`), not necessarily `params.len()`, so a missing entry
            // means "not owned" — same defensive `.get` used for `str_kept`/
            // `concat` elsewhere in the driver loop.
            params: params
                .iter()
                .enumerate()
                .map(|(i, p)| ConcreteParam {
                    ty: settled(&p.ty, &format!("fn {name:?} parameter {:?}", p.name)),
                    name: p.name.clone(),
                    mutable: p.mutable,
                    owned: specs.get(i).is_some_and(|s| s.owned),
                })
                .collect(),
            effects: effects.to_vec(),
            return_ty: return_ty
                .as_ref()
                .map(|t| settled(t, &format!("fn {name:?} return type"))),
            body,
        })
    }

    /// A fresh name for a function synthesized while processing the current
    /// instance: `<instance>$<kind><n>`, `n` counting per instance (see
    /// [`Mono::synth`]).
    /// The effects of a function with effects `own` whose function-typed
    /// parameters are bound to the functions `bound`: its own, and each of
    /// theirs. A bound function whose effects are not known here contributes
    /// the current function's, which is what it can have at most.
    fn specialization_effects(&self, own: &[String], bound: &[String]) -> Vec<String> {
        let mut out = own.to_vec();
        for f in bound {
            let theirs = self
                .concrete
                .get(f)
                .map(|t| t.effects.clone())
                .or_else(|| self.generics.get(f).map(|g| g.sig.effects.clone()))
                .or_else(|| builtin_sig(f).map(|sig| sig.effects.clone()))
                .unwrap_or_else(|| self.cur_effects.clone());
            for e in theirs {
                if !out.contains(&e) {
                    out.push(e);
                }
            }
        }
        out
    }

    fn synth_name(&mut self, kind: &str) -> String {
        let name = format!("{}${kind}{}", self.cur_fn, self.synth);
        self.synth += 1;
        name
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
    /// `str`), `$c{i}` (concat-str), `$ve{i}`/`$vo{i}` (variadic element/optional),
    /// `$dn{i}` (dropped bare-`none` parameter), `$lit{i}_{v}` (dropped integer
    /// literal parameter, `v` the inlined value, `n`-prefixed when negative) —
    /// grouped by marker, ascending parameter index within each group.
    fn enqueue_full(&mut self, template: &str, specs: ParamSpecs) -> String {
        let mut mangled = template.to_string();
        for t in &specs.type_args {
            mangled.push('$');
            mangled.push_str(&aipl_syntax::raw_type_name(t));
        }
        for i in specs.indices(|p| p.owned) {
            mangled.push_str(&format!("$own{i}"));
        }
        for i in specs.indices(|p| p.str_kept) {
            mangled.push_str(&format!("$s{i}"));
        }
        for i in specs.indices(|p| p.set_kept.is_some()) {
            mangled.push_str(&format!("$k{i}"));
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
    /// the type arguments, the concrete return type, and the set of type
    /// variables that *nothing but a bare integer literal* pinned. Does not
    /// queue — the caller decides ownership and enqueues the right instance.
    fn instantiate_types(
        &mut self,
        gname: &str,
        arg_tys: &[Type],
        // The type the checker recorded for the call's result, when the position
        // it sits in gave it one (see `Expr::ty`).
        locked: Option<&Type>,
        span: Span,
    ) -> Result<(Vec<Type>, Type), Error> {
        // Copy the bits we need so we don't borrow `self.generics` across the
        // `&mut self` enqueue.
        let (type_vars, param_tys, variadic, return_ty, defaults) = {
            let Generic { sig, .. } = &self.generics[gname];
            (
                sig.type_var_names(),
                sig.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>(),
                sig.params
                    .iter()
                    .map(|p| p.arity.is_variadic())
                    .collect::<Vec<_>>(),
                sig.return_ty.clone(),
                sig.type_var_defaults(),
            )
        };
        let var_set: HashSet<&str> = type_vars.iter().map(String::as_str).collect();
        let mut map: HashMap<String, Type> = HashMap::new();
        // Non-variadic parameters first: their arguments pin a type variable
        // unambiguously, and a variadic's does not. `sep: T*` has sequence type
        // `T[]`, and a `str` argument to it is either the sequence (`T = char`,
        // since an AIPL string *is* the char sequence) or a single element
        // (`T = str`) — indistinguishable in isolation. Binding the rest first
        // usually settles `T`, and then the variadic only has to agree with it.
        for (i, (pty, aty)) in param_tys.iter().zip(arg_tys).enumerate() {
            if variadic[i] {
                continue;
            }
            self.bind_generic_or(pty, aty, &var_set, &mut map, gname, span.clone())?;
        }
        for (i, (pty, aty)) in param_tys.iter().zip(arg_tys).enumerate() {
            if !variadic[i] {
                continue;
            }
            // Bind against whichever half the argument actually is. With `T`
            // already known, `variadic_shape` decides against the *substituted*
            // sequence type; without it, the structural fallback (an array is
            // the sequence, anything else an element) is the same rule the
            // concrete path uses.
            let seq = subst_vars(pty, &map);
            let target = match variadic_shape(aty, &seq) {
                VShape::Seq => pty.clone(),
                VShape::Elem | VShape::Opt => variadic_elem_ty(pty),
            };
            let aty = match (variadic_shape(aty, &seq), aty) {
                (VShape::Opt, Type::Optional(inner)) => (**inner).clone(),
                _ => aty.clone(),
            };
            self.bind_generic_or(&target, &aty, &var_set, &mut map, gname, span.clone())?;
        }
        // A variable only the return type mentions is the recorded result type's
        // to give. It binds only what no argument did, so a variable an argument
        // pinned keeps that answer.
        if let (Some(locked), Some(declared)) = (locked, &return_ty) {
            let mut from_result: HashMap<String, Type> = HashMap::new();
            self.bind_generic_or(
                declared,
                locked,
                &var_set,
                &mut from_result,
                gname,
                span.clone(),
            )?;
            for (v, t) in from_result {
                map.entry(v).or_insert(t);
            }
        }
        // What nothing pinned — no argument, and not the position the result sits in —
        // is the variable's default, if it was declared one.
        for (v, default) in defaults {
            map.entry(v).or_insert(default);
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
            // See [`returns_its_receiver`]: a `str` receiver is returned as one.
            Some(_)
                if returns_its_receiver(&param_tys, return_ty.as_ref())
                    && arg_tys.first().is_some_and(is_str_repr) =>
            {
                Type::Primitive(Primitive::Str)
            }
            Some(t) => subst_vars(t, &map),
            None => Type::Primitive(Primitive::I64),
        };
        Ok((targs, ret))
    }

    /// Which parameters the call to `template` (with the given concrete
    /// `type_args`) takes ownership of: every parameter the template is
    /// owned-eligible for whose argument is a fresh, uniquely-owned heap value.
    /// Empty unless an owned instance is warranted.
    /// Whether `name` is this instance's `mut self` receiver held on *loan* —
    /// the one binding whose reference it may not hand on (see
    /// [`Mono::cur_receiver`]).
    fn lends(&self, name: &str) -> bool {
        self.cur_receiver
            .as_ref()
            .is_some_and(|(recv, owned)| recv == name && !*owned)
    }

    fn owned_for_call(
        &self,
        template: &str,
        type_args: &[Type],
        args: &[ArgShape],
        arg_tys: &[Type],
    ) -> Vec<usize> {
        let (params, return_ty, body) = self.concrete_signature(template, type_args);
        let mutating_receiver = params.first().is_some_and(|p| p.mutable);
        let owns = |t: &Type| self.owns_heap(t);
        let heap_fields = |t: &Type| self.heap_fields_of(t);
        let mutating = |f: &str| self.mutating.contains(f);
        match body {
            OwnershipBody::Pinned(body) => owned_eligible(
                template,
                &params,
                &return_ty,
                body,
                &owns,
                &heap_fields,
                &mutating,
            ),
            OwnershipBody::Concrete(body) => owned_eligible(
                template,
                &params,
                &return_ty,
                body,
                &owns,
                &heap_fields,
                &mutating,
            ),
            OwnershipBody::None => Vec::new(),
        }
        .into_iter()
        .filter(|&i| i < args.len() && self.owns_heap(&arg_tys[i]))
        .filter(|&i| {
            // An ordinary owned parameter needs a *fresh* argument: the caller
            // must have a reference of its own to give away, and only a value
            // nobody else named does.
            if args[i].fresh {
                return true;
            }
            // A `mut self` receiver is the exception, because the writeback is
            // what gives it one: `set v.f(x)` names the binding on both sides,
            // so the reference the slot held goes in and whatever comes back
            // goes into the same slot. It arrives here as the bare name — the
            // `__move` that performs the transfer is added after
            // monomorphization, which is also why it cannot be the test.
            //
            // Unless the binding is *this* instance's own lent receiver. Then
            // there is no reference to give: a lent `mut self` slot holds one
            // beside the borrow protocol's entry track, and handing it on loses
            // one of the two owners. Declining keeps a borrowed instance calling
            // borrowed instances, which is what makes the pairing consistent
            // instead of mixed.
            mutating_receiver && i == 0 && args[0].name.as_deref().is_some_and(|n| !self.lends(n))
        })
        .collect()
    }

    /// The concrete signature (params, return type) of `template` under `type_args`, and the body
    /// the ownership analyses read: substituting a generic's type variables, or returning a
    /// concrete fn's signature directly. Used for ownership eligibility.
    fn concrete_signature(
        &self,
        template: &str,
        type_args: &[Type],
    ) -> (Vec<Param>, Option<Type>, OwnershipBody<'_>) {
        let pinned = |name: &str| {
            let source = self
                .specialized_from
                .get(name)
                .map_or(name, |from| from.source.as_str());
            self.typed_fns
                .get(source)
                .map_or(OwnershipBody::None, |f| OwnershipBody::Pinned(&f.body))
        };
        if let Some(Generic { sig }) = self.generics.get(template) {
            let (params, return_ty) = make_concrete(sig, type_args);
            (params, return_ty, pinned(template))
        } else if let Some(f) = self.concrete.get(template) {
            let body = match &f.typed {
                Some(typed) => OwnershipBody::Concrete(typed),
                None => pinned(template),
            };
            (f.params.clone(), f.return_ty.clone(), body)
        } else {
            // Not a user function (e.g. a builtin) — never owned-eligible.
            (Vec::new(), None, OwnershipBody::None)
        }
    }

    /// Payload types a `match` arm binds: the optional's element for `some`, a
    /// variant case's payload positionally, else a permissive `i64` fill (the
    /// checker has already validated the pattern).
    /// Every case of `st` other than `given` (a bare case name — no `@Variant`
    /// qualifier), as `(case name, payload types)`. The [`ExprKind::IfLet`]
    /// desugar's "everything else" arm set: mirrors [`Mono::match_payload_tys`]'s
    /// type-driven case lookup, but for the whole case list rather than one
    /// case's payload.
    fn other_cases(&self, st: &Type, given: &str) -> Vec<(String, Vec<Type>)> {
        let all: Vec<(String, Vec<Type>)> = match st {
            Type::Optional(inner) => vec![
                ("some".to_string(), vec![(**inner).clone()]),
                ("none".to_string(), Vec::new()),
            ],
            Type::Result(ok, err) => vec![
                (
                    "ok".to_string(),
                    if is_unit(ok) {
                        Vec::new()
                    } else {
                        vec![(**ok).clone()]
                    },
                ),
                ("err".to_string(), vec![(**err).clone()]),
            ],
            Type::Named(name) => self
                .variants
                .get(name)
                .or_else(|| self.syn_variants.get(name))
                .cloned()
                .unwrap_or_default(),
            Type::Generic(base, args) => self
                .generic_variants
                .get(base)
                .map(|tmpl| {
                    let map: HashMap<String, Type> = tmpl
                        .type_vars
                        .iter()
                        .map(|tv| tv.name.clone())
                        .zip(args.iter().cloned())
                        .collect();
                    tmpl.cases
                        .iter()
                        .map(|c| {
                            (
                                c.name.text.clone(),
                                c.payload
                                    .iter()
                                    .map(|p| subst_type_params(&p.ty, &map))
                                    .collect(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        all.into_iter().filter(|(c, _)| c != given).collect()
    }

    /// The element types of a tuple type — a `Type::Tuple` (an element-wise
    /// scrutinee's), or a synthetic tuple struct's fields in order — or `None`
    /// for anything that is not a tuple.
    /// The fields of the struct `ty` names, from either table — `None` for a
    /// type that is not a struct.
    fn struct_fields_of(&self, ty: &Type) -> Option<&Vec<(String, Type, Option<Expr>)>> {
        let Type::Named(name) = ty else {
            return None;
        };
        self.structs
            .get(name)
            .or_else(|| self.syn_structs.get(name))
    }

    /// Whether a value of `ty` owns heap references — a heap type itself, or a
    /// struct with a field that does. The ownership analyses ask this where
    /// codegen asks `needs_drop`: only such a value has anything to move.
    fn owns_heap(&self, ty: &Type) -> bool {
        is_heap(ty)
            || self
                .struct_fields_of(ty)
                .is_some_and(|fields| fields.iter().any(|(_, t, _)| self.owns_heap(t)))
    }

    /// The names of `ty`'s fields that own heap references — the fields a move
    /// of the whole value is *about*, and the ones a borrower must not read
    /// once it has been moved. Empty for anything but a struct.
    fn heap_fields_of(&self, ty: &Type) -> HashSet<String> {
        self.struct_fields_of(ty)
            .map(|fields| {
                fields
                    .iter()
                    .filter(|(_, t, _)| self.owns_heap(t))
                    .map(|(n, _, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn tuple_field_tys(&self, ty: &Type) -> Option<Vec<Type>> {
        if let Type::Tuple(es) = ty {
            return Some(es.clone());
        }
        let Type::Named(name) = ty else {
            return None;
        };
        tuple_instance_arity(name)?;
        let fields = self
            .structs
            .get(name)
            .or_else(|| self.syn_structs.get(name))?;
        Some(fields.iter().map(|(_, t, _)| t.clone()).collect())
    }

    fn match_payload_tys(&self, scrut: &Type, ctor: &str, n: usize) -> Vec<Type> {
        // A pattern constructor may be variant-qualified (`Case@Variant`); the
        // scrutinee's type already fixes the variant, so match on the bare case.
        let ctor = ctor.split('@').next().unwrap_or(ctor);
        match scrut {
            Type::Optional(inner) if ctor == "some" => vec![(**inner).clone()],
            // A void-Ok (`!E`) binds nothing in its `ok` arm.
            Type::Result(ok, _) if ctor == "ok" && is_unit(ok) => vec![],
            Type::Result(ok, _) if ctor == "ok" => vec![(**ok).clone()],
            Type::Result(_, err) if ctor == "err" => vec![(**err).clone()],
            // `syn_variants` as well as `variants`: a synthesized generic
            // instance (`Rule$Tok`) is where a generic variant's payload types
            // live, and without it every binding fell back to `i64` — which then
            // failed to pin the type argument of any generic call it was passed to.
            Type::Named(name) => self
                .variants
                .get(name)
                .or_else(|| self.syn_variants.get(name))
                .and_then(|cases| cases.iter().find(|(c, _)| c == ctor))
                .map(|(_, p)| p.clone())
                .unwrap_or_else(|| vec![Type::Primitive(Primitive::I64); n]),
            // An application still abstract (inside a generic body): take the
            // template's payload with the application's arguments substituted in.
            Type::Generic(base, args) => self
                .generic_variants
                .get(base)
                .and_then(|tmpl| {
                    let case = tmpl.cases.iter().find(|c| c.name == ctor)?;
                    let map: HashMap<String, Type> = tmpl
                        .type_vars
                        .iter()
                        .map(|tv| tv.name.clone())
                        .zip(args.iter().cloned())
                        .collect();
                    Some(
                        case.payload
                            .iter()
                            .map(|p| subst_type_params(&p.ty, &map))
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_else(|| vec![Type::Primitive(Primitive::I64); n]),
            _ => vec![Type::Primitive(Primitive::I64); n],
        }
    }

    /// Resolve every `Type::Generic` in `t` to a synthetic monomorphic `Named`
    /// type, registering the instance layout (recursively) into the syn maps.
    /// The mono-side twin of the checker's `resolve_generic_ty`.
    fn resolve_generic_ty(&mut self, t: &Type) -> Result<Type, Error> {
        Ok(match t {
            Type::Generic(base, args) => {
                let args: Vec<Type> = args
                    .iter()
                    .map(|a| self.resolve_generic_ty(a))
                    .collect::<Result<_, _>>()?;
                Type::Named(self.instantiate_generic(base, &args)?)
            }
            Type::Optional(i) => Type::Optional(Box::new(self.resolve_generic_ty(i)?)),
            Type::Array(i) => Type::Array(Box::new(self.resolve_generic_ty(i)?)),
            Type::Set(i, o) => Type::Set(Box::new(self.resolve_generic_ty(i)?), *o),
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

    /// Register (if new) the monomorphic instance of generic `base` applied to
    /// concrete `args`, returning its synthetic name.
    fn instantiate_generic(&mut self, base: &str, args: &[Type]) -> Result<String, Error> {
        let name = typing::generic_instance_name(base, args);
        if self.structs.contains_key(&name)
            || self.syn_structs.contains_key(&name)
            || self.variants.contains_key(&name)
            || self.syn_variants.contains_key(&name)
        {
            return Ok(name);
        }
        if let Some(tmpl) = self.generic_structs.get(base).cloned() {
            let map = bind_type_args(base, &tmpl.type_vars, args)?;
            // Placeholder before recursing so a recursive generic terminates.
            self.syn_structs.insert(name.clone(), Vec::new());
            let mut fields = Vec::with_capacity(tmpl.fields.len());
            for fd in &tmpl.fields {
                let ty = self.resolve_generic_ty(&subst_type_params(&fd.ty, &map))?;
                fields.push((fd.name.text.clone(), ty, fd.default.clone()));
            }
            self.syn_structs.insert(name.clone(), fields);
        } else if let Some(tmpl) = self.generic_variants.get(base).cloned() {
            let map = bind_type_args(base, &tmpl.type_vars, args)?;
            self.syn_variants.insert(name.clone(), Vec::new());
            let mut cases = Vec::with_capacity(tmpl.cases.len());
            for c in &tmpl.cases {
                let payload = c
                    .payload
                    .iter()
                    .map(|p| self.resolve_generic_ty(&subst_type_params(&p.ty, &map)))
                    .collect::<Result<_, _>>()?;
                cases.push((c.name.text.clone(), payload));
            }
            self.syn_variants.insert(name.clone(), cases);
        } else {
            return Err(Error::msg(format!(
                "unknown generic type {base:?} (no such generic struct or variant)"
            )));
        }
        Ok(name)
    }

    /// Infer the concrete type arguments of a generic-struct construction
    /// (`Box { value: 5 }`) from its provided field values, then register the
    /// monomorphic instance and return its name.
    /// Mono twin of the checker's `bind_field`: unify a construction's declared
    /// field type against the provided value's type, matching a generic-
    /// application field type (`Emit<K>`) against an already-synthesized instance
    /// value (`Emit$Tok`) by recovering the instance's type arguments.
    /// Like [`collect_bindings`] (conflict-checking type-variable inference for a
    /// generic *call*), but also unifies a generic-application parameter (`Box<T>`,
    /// or `TokenRule<K>[]`) against a synthesized instance argument (`Box$i64`) —
    /// the higher-order case where a generic function is called with a concrete
    /// generic value. Leaves fall through to `collect_bindings`, preserving its
    /// conflict detection and valid-type-argument checks.
    fn bind_generic_or(
        &self,
        pty: &Type,
        aty: &Type,
        vars: &HashSet<&str>,
        map: &mut HashMap<String, Type>,
        gname: &str,
        span: Span,
    ) -> Result<(), Error> {
        match (pty, aty) {
            (Type::Generic(base, params), Type::Named(inst)) => {
                if let Some((b, args)) = self.instance_args(inst) {
                    if b == *base && args.len() == params.len() {
                        for (p, a) in params.iter().zip(&args) {
                            self.bind_generic_or(p, a, vars, map, gname, span.clone())?;
                        }
                    }
                }
                Ok(())
            }
            (Type::Generic(b1, ps), Type::Generic(b2, as_))
                if b1 == b2 && ps.len() == as_.len() =>
            {
                for (p, a) in ps.iter().zip(as_) {
                    self.bind_generic_or(p, a, vars, map, gname, span.clone())?;
                }
                Ok(())
            }
            // Recurse into containers to reach a nested generic application (a
            // `TokenRule<K>[]` argument). A `none`/empty-`[]` element carries no
            // type (`__none__`) and must not pin anything — fall through to
            // `collect_bindings`, which skips it (as it did before).
            (Type::Array(p), Type::Array(a)) => self.bind_generic_or(p, a, vars, map, gname, span),
            (Type::Optional(p), Type::Optional(a)) => {
                self.bind_generic_or(p, a, vars, map, gname, span)
            }
            (Type::Set(p, _), Type::Set(a, _)) => {
                self.bind_generic_or(p, a, vars, map, gname, span)
            }
            _ => collect_bindings(pty, aty, vars, map, gname, span),
        }
    }

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
            (Type::Array(p), Type::Array(a)) => self.bind_field(p, a, vars, map),
            (Type::Optional(p), Type::Optional(a)) => self.bind_field(p, a, vars, map),
            (Type::Set(p, _), Type::Set(a, _)) => self.bind_field(p, a, vars, map),
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
            _ => typing::collect_var_bindings(field_ty, value_ty, vars, map),
        }
    }

    /// Recover the concrete type arguments of a synthesized generic instance
    /// (`Emit$Tok` → `("Emit", [Tok])`) by unifying its generic template's
    /// structure against the instance's concrete decl.
    fn instance_args(&self, inst: &str) -> Option<(String, Vec<Type>)> {
        // See the checker's `instance_args`: re-entry means a recursive
        // generic's payload named its own instance, which pins nothing new.
        if !self.resolving.borrow_mut().insert(inst.to_string()) {
            return None;
        }
        let args = self.instance_args_inner(inst);
        self.resolving.borrow_mut().remove(inst);
        args
    }

    /// [`Mono::instance_args`] proper. Call that, never this: the guard it holds
    /// is what keeps the mutual recursion below finite.
    fn instance_args_inner(&self, inst: &str) -> Option<(String, Vec<Type>)> {
        for (base, tmpl) in self.generic_structs {
            if tuple_instance_of(inst, base) {
                if let Some(fields) = self
                    .structs
                    .get(inst)
                    .or_else(|| self.syn_structs.get(inst))
                {
                    let vars: HashSet<&str> =
                        tmpl.type_vars.iter().map(|t| t.name.as_str()).collect();
                    let mut map = HashMap::new();
                    for fd in &tmpl.fields {
                        if let Some((_, ity, _)) = fields.iter().find(|(n, _, _)| *n == fd.name) {
                            self.bind_field(&fd.ty, ity, &vars, &mut map);
                        }
                    }
                    if let Some(args) = typing::collect_args(&tmpl.type_vars, &map) {
                        return Some((base.clone(), args));
                    }
                }
            }
        }
        for (base, tmpl) in self.generic_variants {
            if inst.starts_with(&format!("{base}$")) {
                if let Some(cases) = self
                    .variants
                    .get(inst)
                    .or_else(|| self.syn_variants.get(inst))
                {
                    let vars: HashSet<&str> =
                        tmpl.type_vars.iter().map(|t| t.name.as_str()).collect();
                    let mut map = HashMap::new();
                    for c in &tmpl.cases {
                        if let Some((_, ipayload)) = cases.iter().find(|(n, _)| *n == c.name.text) {
                            for (pt, it) in c.payload.iter().zip(ipayload) {
                                self.bind_field(&pt.ty, it, &vars, &mut map);
                            }
                        }
                    }
                    if let Some(args) = typing::collect_args(&tmpl.type_vars, &map) {
                        return Some((base.clone(), args));
                    }
                }
            }
        }
        None
    }

    /// If `t` is a struct with a field named `field` holding a function value,
    /// return that function's return type. Used to resolve a call through a
    /// function-valued struct field (`recv.f(args)`).
    fn fn_field_ret(&self, t: &Type, field: &str) -> Option<Type> {
        let Type::Named(sn) = t else {
            return None;
        };
        self.structs
            .get(sn)
            .or_else(|| self.syn_structs.get(sn))?
            .iter()
            .find(|(n, _, _)| n == field)
            .and_then(|(_, ft, _)| match ft {
                Type::Fn(_, ret) => Some((**ret).clone()),
                _ => None,
            })
    }

    /// Is `name` a reference to a top-level function usable as a *value* (passed
    /// to a higher-order function)? True for a concrete user function or a
    /// builtin, but not for a name bound as a local in `env` (which shadows any
    /// global) nor a generic (the checker rejects generic functions as values).
    /// Whether `index` is the `(row, column)` pair that addresses one cell of the
    /// grid `receiver`.
    fn is_grid_cell_index(&self, receiver: &Type, index: &Type) -> bool {
        matches!(receiver, Type::Named(n) if aipl_syntax::is_grid_instance(n))
            && self
                .tuple_field_tys(index)
                .is_some_and(|slots| slots.len() == 2)
    }

    /// `name` used as a function *value* — `xs.map(f)`, or `f` handed to a
    /// function-typed parameter — resolved to the function that position will
    /// call, given the parameter types it expects: the callee's name (queued
    /// for emission) and its return type for those parameters.
    ///
    /// A concrete function or builtin is itself. A *generic* function is
    /// instantiated here, which is what lets `rs.map(desugar_rule)` name one:
    /// its type variables are pinned by its parameter types against `expected`
    /// — exactly what the call `desugar_rule(r)` pins from `r`'s type — and the
    /// instance queued is the one that call would reach, less the per-argument
    /// specializations a value has no arguments to earn (nothing is moved in,
    /// no literal flexes). Callers guard with [`Mono::is_fn_ref`], so a name
    /// reaching here is one of the three.
    fn fn_value(
        &mut self,
        name: &str,
        expected: &[Type],
        span: &Span,
    ) -> Result<(String, Type), Error> {
        if !self.generics.contains_key(name) {
            self.enqueue_concrete(name);
            return Ok((name.to_string(), self.ref_return(name, expected)));
        }
        // The checker has already matched the function's arity to the
        // position's, so `expected` lines up with the parameters one to one.
        let (type_args, ret) = self.instantiate_types(name, expected, None, span.clone())?;
        // Substitution can leave a generic application (`(A, B)` is
        // `__tuple2<A, B>`); the consumers of a function value's result — a
        // wrapper deciding whether the result fits an array slot — read a
        // resolved instance type, as `specialize_generic_call` hands them.
        let ret = self.resolve_generic_ty(&ret)?;
        let params = self.param_specs(name, &type_args, expected, &[]);
        let mangled = self.enqueue_full(name, ParamSpecs { type_args, params });
        Ok((mangled, ret))
    }

    /// The per-parameter specialization of an instance of generic `gname` at
    /// `type_args`, reached with arguments of types `atys`: which are moved in
    /// (`owned`), which `str` arguments meet a `char[]`/`T[]` parameter and keep
    /// their representation (`str_kept`), each variadic's shape, and the
    /// parameters a bare `none` at the call site drops.
    fn param_specs(
        &self,
        gname: &str,
        type_args: &[Type],
        atys: &[Type],
        owned: &[usize],
    ) -> Vec<ParamSpec> {
        let chars = Type::Array(Box::new(Type::Primitive(Primitive::Char)));
        let Generic { sig, .. } = &self.generics[gname];
        let tmap: HashMap<String, Type> = sig
            .type_vars
            .iter()
            .map(|tp| tp.name.clone())
            .zip(type_args.iter().cloned())
            .collect();
        sig.params
            .iter()
            .enumerate()
            .map(|(i, p)| ParamSpec {
                owned: owned.contains(&i),
                str_kept: atys.get(i) == Some(&Type::Primitive(Primitive::Str))
                    && subst_vars(&p.ty, &tmap) == chars,
                set_kept: match (atys.get(i), &p.ty) {
                    (Some(Type::Set(_, order)), Type::Array(_)) => Some(*order),
                    _ => None,
                },
                // A generic function's variadic parameter needs the same
                // per-shape specialization a concrete one gets; without it
                // every instance was the sequence form and a bare element was
                // rejected by codegen's argument check.
                variadic: if p.arity.is_variadic() {
                    atys.get(i).map_or(VShape::Seq, |a| {
                        variadic_shape(a, &subst_vars(&p.ty, &tmap))
                    })
                } else {
                    VShape::Seq
                },
                ..ParamSpec::default()
            })
            .collect()
    }

    /// The declared return type of a resolved function value `name` called with
    /// `arg_tys`: a concrete/synthesized function's declared return, else a
    /// builtin's computed return, else unit.
    fn ref_return(&self, name: &str, arg_tys: &[Type]) -> Type {
        self.concrete
            .get(name)
            .and_then(|f| f.return_ty.clone())
            .or_else(|| {
                let callee = Callee::resolve(name.to_string());
                self.tuple_to_set_return(&callee, arg_tys)
                    .or_else(|| builtin_return(&callee, arg_tys))
            })
            .unwrap_or(Type::Unit)
    }

    /// `t.to_set()` on a tuple: the set of its elements, whose order the use site
    /// decides like any `to_set`'s. The checker has refused a tuple whose elements
    /// differ, so the first element's type is all of them.
    fn tuple_to_set_return(&self, callee: &Callee, arg_tys: &[Type]) -> Option<Type> {
        if *callee != Callee::ToSet {
            return None;
        }
        let elems = self.tuple_field_tys(arg_tys.first()?)?;
        Some(Type::Set(
            Box::new(elems.first()?.clone()),
            aipl_syntax::ast::SetOrder::Context,
        ))
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
        Type::TypeVar(v) if vars.contains(v.as_str()) => bind(v, arg_ty, map, gname, span.clone()),
        Type::Optional(inner) if ty_contains_var(inner, vars) => match arg_ty {
            Type::Optional(a) => collect_bindings(inner, a, vars, map, gname, span.clone()),
            _ => Ok(()),
        },
        Type::Array(inner) if ty_contains_var(inner, vars) => match arg_ty {
            Type::Array(a) => collect_bindings(inner, a, vars, map, gname, span.clone()),
            // `str` is usable as `char[]` — pin the element variable to `char`.
            Type::Primitive(Primitive::Str) => collect_bindings(
                inner,
                &Type::Primitive(Primitive::Char),
                vars,
                map,
                gname,
                span.clone(),
            ),
            // A set is usable as a `T[]` (see `ParamSpec::set_kept`): its
            // element pins the variable as an array's would.
            Type::Set(a, _) => collect_bindings(inner, a, vars, map, gname, span.clone()),
            _ => Ok(()),
        },
        Type::Set(inner, _) if ty_contains_var(inner, vars) => match arg_ty {
            Type::Set(a, _) => collect_bindings(inner, a, vars, map, gname, span.clone()),
            _ => Ok(()),
        },
        Type::Dict(pk, pv) if ty_contains_var(pk, vars) || ty_contains_var(pv, vars) => {
            match arg_ty {
                Type::Dict(ak, av) => {
                    collect_bindings(pk, ak, vars, map, gname, span.clone())?;
                    collect_bindings(pv, av, vars, map, gname, span.clone())?;
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        Type::Result(po, pe) if ty_contains_var(po, vars) || ty_contains_var(pe, vars) => {
            match arg_ty {
                Type::Result(ao, ae) => {
                    collect_bindings(po, ao, vars, map, gname, span.clone())?;
                    collect_bindings(pe, ae, vars, map, gname, span.clone())?;
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
    // A type argument may be any value type — a scalar, `str`, an array, an
    // optional, a struct, or a variant (a generic function can abstract over a
    // generic struct/variant `K`, higher-order-generics style). Only `()` and a
    // function type are rejected.
    if matches!(ty, Type::Unit | Type::Fn(_, _)) {
        return Err(Error::at(
            format!(
                "\"{}\" in \"{gname}\" resolved to {}, which is not a valid type argument",
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
        // A binding still carrying a placeholder — a bare `none` argument's
        // `__none__?`, an empty `[]`'s `__none__[]` — says less than one
        // without: it agrees with any binding of the same shape, and defers to
        // it. `x.value_or(none)` with `x: T?` binds `T` from `x`.
        Some(prev) if has_placeholder(ty) && typing::coerce(ty, prev).is_ok() => Ok(()),
        Some(prev) if has_placeholder(prev) && typing::coerce(prev, ty).is_ok() => {
            map.insert(v.to_string(), ty.clone());
            Ok(())
        }
        Some(prev) => Err(Error::at(
            format!(
                "conflicting types for \"{}\" in \"{}\": {} vs {}",
                display_var(v),
                display_fn(gname),
                type_name(prev),
                type_name(ty)
            ),
            span.clone(),
        )),
    }
}

/// [`aipl_syntax::BUILTIN_SIGNATURES`] parsed once and indexed by name, each
/// normalized (via [`normalize`]) into a [`Signature`] — the same declarations
/// the checker resolves builtin calls against, reused here so mono's own
/// return-type inference doesn't hand-duplicate them. A builtin is never
/// enqueued/specialized like a real [`Generic`], so only its `Signature` is
/// kept (the body `normalize` produces alongside it is discarded). The
/// AIPL-implemented builtins aren't in `BUILTIN_SIGNATURES` at all: their files
/// are files of the program, so their functions are the program's own.
fn builtin_sigs() -> &'static HashMap<String, Signature> {
    static SIGS: OnceLock<HashMap<String, Signature>> = OnceLock::new();
    SIGS.get_or_init(|| {
        let program =
            aipl_parser::parse(BUILTIN_SIGNATURES).expect("builtin signatures are valid AIPL");
        aipl_syntax::with_reduction_signatures(program.items)
            .into_iter()
            .filter_map(|item| match item {
                Item::Fn(f) => {
                    let name = f.name.text.clone();
                    let g = normalize(&f).expect("builtin signatures are valid AIPL");
                    Some((name, g.sig))
                }
                _ => None,
            })
            .collect()
    })
}

/// Names of the builtins declared with a `mut self` receiver. They follow the
/// mutating-method convention: `set v.f(x)` writes back in place, and every
/// other position copy-and-modifies. Derived from [`BUILTIN_SIGNATURES`] so no
/// call site has to name them — `push` is the only one today, but adding
/// another needs no change here or in codegen.
///
/// Only native builtins are considered: an AIPL-implemented one is a function of
/// the program, and joins the mutating set the way any of the program's
/// functions does.
pub fn mutating_builtins() -> impl Iterator<Item = &'static str> {
    builtin_sigs()
        .iter()
        .filter(|(_, sig)| sig.is_mutating())
        .map(|(name, _)| name.as_str())
}

/// `true` if `name` is one of [`mutating_builtins`].
pub fn builtin_is_mutating(name: &str) -> bool {
    builtin_sigs()
        .get(name)
        .is_some_and(|sig| sig.is_mutating())
}

/// Whether the builtin `f` only *reads* its argument `i` for the duration of
/// the call — copying what it needs out and keeping no reference to the
/// argument's buffer afterwards — so a `mut` binding passed there is still
/// exclusive once the call returns, and its `push`/`extend` may keep mutating
/// in place.
///
/// This is a runtime contract, stated per argument: `extend`'s source and
/// `union`'s second operand are copied element by element (each element
/// retained on its own; the buffer itself is not), whereas a slice, a
/// `reverse`, or a `str` concat hands back a view or a rope that keeps
/// pointing into its operand, and a user function may store its argument
/// anywhere. The in-place runtime (`array_ops::push_mut`) never checks for a
/// second reference — this analysis is the only guard — so an argument is
/// added here only once its runtime has been read.
pub(crate) fn builtin_reads_arg(f: &Callee, i: usize) -> bool {
    matches!((f, i), (Callee::Extend, 1) | (Callee::Union, 1))
}

/// The declared signature of the native builtin `name`, from
/// [`BUILTIN_SIGNATURES`]. `None` for any other name — an AIPL-implemented
/// builtin included, which is a function of the program.
fn builtin_sig(name: &str) -> Option<&'static Signature> {
    builtin_sigs().get(name)
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
        Type::TypeVar(p) if p == v => Some(arg_ty.clone()),
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
        Type::Set(inner, _) if ty_mentions(inner, v) => match arg_ty {
            Type::Set(a, _) => bind_builtin_var(inner, a, v),
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

/// Whether a function declared `(self: T[], ..) -> T[]` hands back the sequence
/// it was given — its return type is exactly its first parameter's.
///
/// For such a function a `str` receiver gets a `str` back. Unification alone binds
/// `T` to `char` and returns a `char[]`, which holds the same bytes but is not the
/// same type to the things that care: a template renders it as an array of chars,
/// and a `str` parameter refuses it. So the rule is stated once, on the
/// shape of the signature, rather than per builtin — `reverse`, `sort`, `repeat`,
/// `filter`, `drop_n` and the rest all keep a string a string. It is applied
/// wherever a result type is computed from such a signature: inference over the
/// declared builtins ([`declared_builtin_return`]), a generic call's return type
/// ([`Mono::instantiate_types`]), and the instance a `str`-receiving call
/// specializes ([`ParamSpec::str_kept`]).
fn returns_its_receiver(param_tys: &[Type], return_ty: Option<&Type>) -> bool {
    matches!((param_tys.first(), return_ty), (Some(p @ Type::Array(_)), Some(r)) if p == r)
}

/// Return type of a builtin declared in [`aipl_syntax::BUILTIN_SIGNATURES`],
/// substituting its type variables from `arg_tys`. `None` if `name` isn't
/// declared there (an internal/synthetic name, or one of `builtin_return`'s
/// own special cases).
fn declared_builtin_return(name: &str, arg_tys: &[Type]) -> Option<Type> {
    let sig = builtin_sig(name)?;
    // A mutating builtin (`mut self`) is *declared* void — it mutates in place
    // — but a call yields the mutated receiver, exactly like a user mutating
    // method (the same rule `typing::return_ty_of` applies). The in-place `set`
    // writeback discards it; expression position gets the copy-and-modify
    // result. Deriving this from `is_mutating` rather than naming the builtin
    // means a future `mut self` builtin needs no arm here.
    if sig.is_mutating() {
        return Some(
            arg_tys
                .first()
                .cloned()
                .unwrap_or(Type::Primitive(Primitive::I64)),
        );
    }
    let mut map: HashMap<String, Type> = HashMap::new();
    for tp in &sig.type_vars {
        // The first parameter whose argument pins the variable decides it.
        for (p, aty) in sig.params.iter().zip(arg_tys) {
            if let Some(t) = bind_builtin_var(&p.ty, aty, &tp.name) {
                map.insert(tp.name.clone(), t);
                break;
            }
        }
        map.entry(tp.name.clone())
            .or_insert(Type::Primitive(Primitive::I64));
    }
    let ptys: Vec<Type> = sig.params.iter().map(|p| p.ty.clone()).collect();
    // See [`returns_its_receiver`].
    if returns_its_receiver(&ptys, sig.return_ty.as_ref())
        && arg_tys.first().is_some_and(is_str_repr)
    {
        return Some(Type::Primitive(Primitive::Str));
    }
    Some(match &sig.return_ty {
        Some(t) => aipl_syntax::inherit_set_order(subst_vars(t, &map), &ptys, arg_tys),
        None => Type::Unit,
    })
}

/// Return type of a builtin call, or `None` if `callee` isn't a builtin. Most
/// builtins substitute directly from their [`aipl_syntax::BUILTIN_SIGNATURES`]
/// declaration via [`declared_builtin_return`]; the cases here are the ones
/// where mono's own inference genuinely diverges from that signature (a
/// different result than the declared one, or a synthetic/internal callee not
/// declared there at all).
fn builtin_return(callee: &Callee, arg_tys: &[Type]) -> Option<Type> {
    match callee {
        // Internal: an empty array reserved to a given capacity (`map`'s output).
        // Untyped element (`__none__`) like `[]`; refined by the first `push`.
        Callee::WithCapacity => {
            return Some(Type::Array(Box::new(Type::Primitive(Primitive::I64))))
        }
        // Declared void (it's a statement); mono needs an `i64` value for the
        // expression it emits (see the `expr`-position uses of `print`).
        Callee::Print | Callee::Put => return Some(Type::Primitive(Primitive::I64)),
        // Internal: a single `char` to a one-char `str`, emitted by variadic
        // `char*` specialization (see `specialize_variadic`).
        Callee::CharToStr => return Some(Type::Primitive(Primitive::Str)),
        // Internal: the key / value of a dict pair by position, emitted by the
        // dict loop (see the `For` arm). The dict is the first argument, so each
        // reads its half straight off it.
        Callee::DictKeyAt | Callee::DictValueAt => {
            return match arg_tys.first().map(aipl_syntax::unrefined) {
                Some(Type::Dict(k, v)) => Some(if *callee == Callee::DictKeyAt {
                    (**k).clone()
                } else {
                    (**v).clone()
                }),
                // Not a dict: the error is codegen's to report against the call.
                _ => Some(Type::Primitive(Primitive::I64)),
            };
        }
        // some(x) wraps the value's type. Not expressible via plain signature
        // substitution: a concat-str argument must decay to plain `str` before
        // it's wrapped (see `decay_concat`).
        Callee::Some => {
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
        Callee::Ok => {
            return Some(Type::Result(
                Box::new(decay_concat(arg_tys.first().cloned().unwrap_or(Type::Unit))),
                Box::new(Type::Primitive(Primitive::I64)),
            ))
        }
        Callee::Err => {
            return Some(Type::Result(
                Box::new(Type::Primitive(Primitive::I64)),
                Box::new(decay_concat(
                    arg_tys
                        .first()
                        .cloned()
                        .unwrap_or(Type::Primitive(Primitive::I64)),
                )),
            ))
        }
        // `n.narrow()`: an optional of whichever integer the use site asked for,
        // which the pinner recorded on the call. A placeholder here.
        Callee::Narrow => return Some(Type::Optional(Box::new(Type::Primitive(Primitive::I64)))),
        // `n.to_float()`: `IEEE754Float64`, unless the pinner recorded another float on
        // the call.
        Callee::ToFloat => return Some(Type::Primitive(Primitive::F64)),
        // `x.to_fixed(digits)` is the text of a float.
        Callee::ToFixed => return Some(Type::Primitive(Primitive::Str)),
        // `xs.to_set()`: the set whose order the use site decides (see
        // `SetOrder::Context`), of the array's element type.
        Callee::ToSet => {
            return Some(match arg_tys.first() {
                Some(Type::Array(inner)) => {
                    Type::Set(inner.clone(), aipl_syntax::ast::SetOrder::Context)
                }
                _ => Type::Set(
                    Box::new(Type::Primitive(Primitive::I64)),
                    aipl_syntax::ast::SetOrder::Context,
                ),
            })
        }
        // `for (let v : xs.reverse())`'s iterable after fusion: `xs` itself,
        // walked backwards by codegen — so its type is `xs`'s.
        Callee::ReverseIter => {
            return Some(
                arg_tys
                    .first()
                    .cloned()
                    .unwrap_or(Type::Array(Box::new(Type::Primitive(Primitive::I64)))),
            )
        }
        // `for (let part : s.split(sep))`'s iterable after fusion: the source
        // and the separator, walked by a split cursor. It stands where the
        // array of parts stood, so it types as that array — which is what gives
        // the loop variable its `str`.
        Callee::SplitIter => return Some(Type::Array(Box::new(Type::Primitive(Primitive::Str)))),
        // Internal: a binding at its last use (see `move_last_use`) — the
        // binding's own value, typed as itself.
        Callee::Move => return Some(arg_tys.first().cloned().unwrap_or(Type::Unit)),
        // Internal: the moved-in array as a writable block (see `writable`) —
        // the same array, typed as itself.
        Callee::ArrWritable => {
            return Some(
                arg_tys
                    .first()
                    .cloned()
                    .unwrap_or(Type::Array(Box::new(Type::Primitive(Primitive::I64)))),
            )
        }
        // Internal in-place-filter intrinsics (statements; see `expand_filter`).
        Callee::FilterKeep | Callee::FilterDrop | Callee::FilterTruncate => {
            return Some(Type::Unit)
        }
        // Internal in-place-map intrinsic (a statement; see `expand_map`).
        Callee::MapSet => return Some(Type::Unit),
        // Internal: reinterpret the reused buffer as the result element type.
        // Its real result type is fixed by codegen (the enclosing fn's return);
        // here it just borrows the input array type so mono inference proceeds.
        Callee::MapResult => {
            return Some(
                arg_tys
                    .first()
                    .cloned()
                    .unwrap_or(Type::Primitive(Primitive::I64)),
            )
        }
        _ => {}
    }
    declared_builtin_return(callee.name(), arg_tys)
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

/// Names that can't be used as a type-parameter name: every primitive spelling
/// (all integer widths, `bool`, `char`, `str`) plus `any`. In type position the
/// parser resolves these to the primitive, so a type parameter named after one
/// could never be referenced.
const RESERVED_TYPE_NAMES: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "bool", "char", "str", "any",
];

/// A function is generic if it declares type parameters or uses anonymous
/// `any` in a parameter.
fn is_generic(f: &Function) -> bool {
    !f.sig.type_vars.is_empty() || f.sig.params.iter().any(|p| ty_mentions(&p.ty, "any"))
}

/// Validate a generic function's signature and normalize it: collect its type
/// variables (declared names, then one synthetic per anonymous `any[]`/`any?`
/// parameter) and rewrite anonymous `any` to those synthetic names.
fn normalize(f: &Function) -> Result<Generic, Error> {
    // Names only, in declaration order plus one appended per anonymous
    // `any[]`/`any?`; bounds are reattached from `f.sig.type_vars` at the end
    // (a synthetic name has none of its own — it's always `any`).
    let mut var_names: Vec<String> = Vec::new();
    let mut declared: HashSet<String> = HashSet::new();
    for tp in &f.sig.type_vars {
        if RESERVED_TYPE_NAMES.contains(&tp.name.as_str()) {
            return Err(Error::msg(format!(
                "fn \"{}\": \"{}\" is not a valid type parameter name (reserved)",
                f.name, tp.name
            )));
        }
        if !declared.insert(tp.name.clone()) {
            return Err(Error::msg(format!(
                "fn \"{}\": duplicate type parameter \"{}\"",
                f.name, tp.name
            )));
        }
        var_names.push(tp.name.clone());
    }

    // Normalize parameter types: anonymous `any` → a fresh synthetic variable.
    let mut counter = 0usize;
    let mut params = Vec::with_capacity(f.sig.params.len());
    for p in &f.sig.params {
        let ty = normalize_param_ty(&p.ty, &mut var_names, &mut counter, &f.name)?;
        params.push(Param {
            name_span: p.name_span.clone(),
            name: p.name.clone(),
            ty,
            mutable: p.mutable,
            arity: p.arity,
            default: p.default.clone(),
            implicit_some: p.implicit_some,
        });
    }

    // The return type may reference declared type parameters, but never the
    // anonymous `any` keyword (it has no name to bind).
    if f.sig
        .return_ty
        .as_ref()
        .is_some_and(|t| ty_mentions(t, "any"))
    {
        return Err(Error::msg(format!(
            "fn \"{}\": bare \"any\" is not allowed in a return type; declare \"<T: any>\" and return \"T\"",
            f.name
        )));
    }

    // Every declared type parameter must appear in a parameter or in the return
    // type, or nothing at a call could ever say what it is. One that only the
    // return type mentions is taken from where the call's result goes.
    for tp in &f.sig.type_vars {
        let in_return = f
            .sig
            .return_ty
            .as_ref()
            .is_some_and(|t| ty_mentions(t, &tp.name));
        if !in_return && !params.iter().any(|p| ty_mentions(&p.ty, &tp.name)) {
            return Err(Error::msg(format!(
                "fn \"{}\": type parameter \"{}\" is not used by any parameter or the return type, so it can't be inferred",
                f.name, tp.name
            )));
        }
    }

    let type_vars = var_names
        .into_iter()
        .map(|name| {
            let declared = f.sig.type_vars.iter().find(|tp| tp.name == name);
            let bound = declared.map(|tp| tp.bound).unwrap_or(Bound::Any);
            let default = declared.and_then(|tp| tp.default.clone());
            TypeParam {
                name,
                bound,
                default,
                name_span: Default::default(),
            }
        })
        .collect();

    Ok(Generic {
        sig: Signature {
            type_vars,
            params,
            return_ty: f.sig.return_ty.clone(),
            effects: f.sig.effects.clone(),
        },
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
        Type::Case(v) => Ok(Type::Case(Box::new(normalize_param_ty(
            v, type_vars, counter, fname,
        )?))),
        Type::Any => Err(Error::msg(format!(
            "fn \"{fname}\": bare \"any\" is not allowed; use \"any[]\", \"any?\", or a named type parameter \"<T: any>\""
        ))),
        Type::Primitive(_)
        | Type::Named(_)
        | Type::TypeVar(_)
        | Type::Unit
        | Type::Unknown
        | Type::ConcatStr => Ok(t.clone()),
        Type::Optional(inner) => Ok(Type::Optional(Box::new(normalize_inner(
            inner, type_vars, counter,
        )))),
        Type::Array(inner) => Ok(Type::Array(Box::new(normalize_inner(
            inner, type_vars, counter,
        )))),
        Type::Without(base, x) => Ok(Type::Without(
            Box::new(normalize_param_ty(base, type_vars, counter, fname)?),
            x.clone(),
        )),
        Type::Set(inner, o) => Ok(Type::Set(
            Box::new(normalize_inner(inner, type_vars, counter)),
            *o,
        )),
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
        // Concrete generic applications are resolved to `Named` by
        // lower_generics before mono; recurse defensively into type arguments.
        Type::Generic(name, args) => {
            let a = args
                .iter()
                .map(|x| normalize_param_ty(x, type_vars, counter, fname))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Type::Generic(name.clone(), a))
        }
    }
}

/// Normalize the element/inner type of an array/optional parameter: anonymous
/// `any` becomes a fresh synthetic variable; anything else is kept as-is.
fn normalize_inner(t: &Type, type_vars: &mut Vec<String>, counter: &mut usize) -> Type {
    match t {
        Type::Case(v) => Type::Case(Box::new(normalize_inner(v, type_vars, counter))),
        Type::Any => {
            let name = format!("$any{counter}");
            *counter += 1;
            type_vars.push(name.clone());
            Type::TypeVar(name)
        }
        Type::Primitive(_)
        | Type::Named(_)
        | Type::TypeVar(_)
        | Type::Unit
        | Type::Unknown
        | Type::ConcatStr => t.clone(),
        // The surface grammar can't nest deeper, but recurse defensively.
        Type::Optional(inner) => {
            Type::Optional(Box::new(normalize_inner(inner, type_vars, counter)))
        }
        Type::Array(inner) => Type::Array(Box::new(normalize_inner(inner, type_vars, counter))),
        Type::Without(base, x) => Type::Without(
            Box::new(normalize_inner(base, type_vars, counter)),
            x.clone(),
        ),
        Type::Set(inner, o) => Type::Set(Box::new(normalize_inner(inner, type_vars, counter)), *o),
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
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter()
                .map(|a| normalize_inner(a, type_vars, counter))
                .collect(),
        ),
    }
}

/// Substitute each type variable in `t` with its concrete type from `map`.
fn subst_vars(t: &Type, map: &HashMap<String, Type>) -> Type {
    match t {
        Type::Case(v) => Type::Case(Box::new(subst_vars(v, map))),
        // `t` is always a piece of an already-`normalize`d signature, which
        // never contains `Any` (converted to a synthetic `Named` type var) or
        // the mono-only pseudo-types (only ever produced as *substituted
        // values*, via `map`, never as signature structure to recurse into).
        Type::Primitive(_) | Type::Unit | Type::Any | Type::Unknown | Type::ConcatStr => t.clone(),
        // A variable is what a substitution map is keyed by; an ordinary name
        // stands for itself however the map is populated.
        Type::TypeVar(v) => map
            .get(v)
            .cloned()
            .unwrap_or_else(|| Type::TypeVar(v.clone())),
        Type::Named(n) => Type::Named(n.clone()),
        Type::Optional(inner) => Type::Optional(Box::new(subst_vars(inner, map))),
        Type::Array(inner) => Type::Array(Box::new(subst_vars(inner, map))),
        Type::Without(base, x) => Type::Without(Box::new(subst_vars(base, map)), x.clone()),
        Type::Set(inner, o) => Type::Set(Box::new(subst_vars(inner, map)), *o),
        Type::Dict(k, v) => Type::Dict(Box::new(subst_vars(k, map)), Box::new(subst_vars(v, map))),
        Type::Result(ok, err) => Type::Result(
            Box::new(subst_vars(ok, map)),
            Box::new(subst_vars(err, map)),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params.iter().map(|p| subst_vars(p, map)).collect(),
            Box::new(subst_vars(ret, map)),
        ),
        Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| subst_vars(e, map)).collect()),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter().map(|a| subst_vars(a, map)).collect(),
        ),
    }
}

/// Does `t` mention the named type `name` anywhere? `t` may be a raw,
/// pre-`normalize` signature (this is how `is_generic` and its callers detect
/// anonymous `any`), so `Type::Any` counts as mentioning `"any"`.
fn ty_mentions(t: &Type, name: &str) -> bool {
    match t {
        Type::Case(v) => ty_mentions(v, name),
        Type::Primitive(_) | Type::Unit | Type::ConcatStr => false,
        Type::Any => name == "any",
        // Only a variable can *be* the name asked about; a struct called `T` is
        // not the type parameter `T`.
        Type::TypeVar(n) => n == name,
        Type::Named(_) | Type::Unknown => false,
        Type::Optional(inner)
        | Type::Array(inner)
        | Type::Set(inner, _)
        | Type::Without(inner, _) => ty_mentions(inner, name),
        Type::Dict(k, v) => ty_mentions(k, name) || ty_mentions(v, name),
        Type::Result(ok, err) => ty_mentions(ok, name) || ty_mentions(err, name),
        Type::Fn(ps, ret) => ps.iter().any(|p| ty_mentions(p, name)) || ty_mentions(ret, name),
        Type::Tuple(elems) => elems.iter().any(|e| ty_mentions(e, name)),
        Type::Generic(n, args) => n == name || args.iter().any(|a| ty_mentions(a, name)),
    }
}

/// Does `t` reference any of the given type variables? `t` is always a piece
/// of an already-`normalize`d signature (never `Any`, never a mono-only
/// pseudo-type — see `subst_vars`).
fn ty_contains_var(t: &Type, vars: &HashSet<&str>) -> bool {
    match t {
        Type::Case(v) => ty_contains_var(v, vars),
        Type::Unit | Type::Primitive(_) | Type::Any | Type::ConcatStr => false,
        Type::TypeVar(v) => vars.contains(v.as_str()),
        Type::Named(_) | Type::Unknown => false,
        Type::Optional(inner)
        | Type::Array(inner)
        | Type::Set(inner, _)
        | Type::Without(inner, _) => ty_contains_var(inner, vars),
        Type::Dict(k, v) => ty_contains_var(k, vars) || ty_contains_var(v, vars),
        Type::Result(ok, err) => ty_contains_var(ok, vars) || ty_contains_var(err, vars),
        Type::Fn(ps, ret) => {
            ps.iter().any(|p| ty_contains_var(p, vars)) || ty_contains_var(ret, vars)
        }
        Type::Tuple(elems) => elems.iter().any(|e| ty_contains_var(e, vars)),
        Type::Generic(_, args) => args.iter().any(|a| ty_contains_var(a, vars)),
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

/// A function name as a *user* would write it: an AIPL-implemented builtin is
/// registered under its canonical `__builtin_` name, which no user identifier
/// can spell, so a diagnostic that quoted it back would name something the
/// reader cannot find in their own source.
fn display_fn(name: &str) -> &str {
    name.strip_prefix("__builtin_").unwrap_or(name)
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
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::TypeArgs(x, _) => collect_free(x, bound, env, out, seen),
        ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        // A shim binds global functions, which are never free *locals*.
        ExprKind::Shim(_, _, body) => collect_free(body, bound, env, out, seen),
        ExprKind::Ident(n) => {
            if !bound.contains(n) && !seen.contains(n) {
                if let Some(t) = env.get(n) {
                    out.push((n.clone(), t.clone()));
                    seen.insert(n.clone());
                }
            }
        }
        ExprKind::Num(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => {}
        ExprKind::Neg(x) | ExprKind::Field(x, _) | ExprKind::Try(x) | ExprKind::Return(x) => {
            collect_free(x, bound, env, out, seen)
        }
        ExprKind::Seq(a, b) | ExprKind::Index(a, b) => {
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
        | ExprKind::SetLit(args, _)
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
                let names = arm.pattern.bindings();
                let added: Vec<&String> = names
                    .iter()
                    .filter(|b| bound.insert((*b).clone()))
                    .collect();
                collect_free(&arm.body, bound, env, out, seen);
                for b in added {
                    bound.remove(b);
                }
            }
        }
        ExprKind::IfLet(arm, s, else_b) => {
            collect_free(s, bound, env, out, seen);
            let names = arm.pattern.bindings();
            let added: Vec<&String> = names
                .iter()
                .filter(|b| bound.insert((*b).clone()))
                .collect();
            collect_free(&arm.body, bound, env, out, seen);
            for b in added {
                bound.remove(b);
            }
            collect_free(else_b, bound, env, out, seen);
        }
        ExprKind::Let(n, _, val, b) | ExprKind::LetMut(n, _, val, b) => {
            collect_free(val, bound, env, out, seen);
            let added = bound.insert(n.clone());
            collect_free(b, bound, env, out, seen);
            if added {
                bound.remove(n);
            }
        }
        // A bare-ident LHS is an lvalue, not a use; a field-path LHS is also a
        // *read* (the functional-update desugar copies the struct's other
        // fields out of it), so walk it like any expression — it bottoms out
        // at the base `Ident`.
        ExprKind::Assign(lhs, val, b) => {
            if !matches!(&lhs.kind, ExprKind::Ident(_)) {
                collect_free(lhs, bound, env, out, seen);
            }
            collect_free(val, bound, env, out, seen);
            collect_free(b, bound, env, out, seen);
        }
        ExprKind::For(v, index, iter, b) => {
            collect_free(iter, bound, env, out, seen);
            let added = bound.insert(v.clone());
            // The index binder is bound over the body too.
            let added_index = index.as_ref().is_some_and(|i| bound.insert(i.clone()));
            collect_free(b, bound, env, out, seen);
            if added_index {
                bound.remove(index.as_ref().expect("inserted above"));
            }
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
pub fn use_counts(program: &Program) -> HashMap<&str, usize> {
    let mut counts: HashMap<&str, usize> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some((f.name.text.as_str(), 0)),
            _ => None,
        })
        .collect();
    for it in &program.items {
        if let Item::Fn(f) = it {
            // A function's parameters shadow same-named globals within its body
            // (e.g. a `(T) -> bool` parameter `pred` called as `pred(x)` is the
            // parameter, not a top-level `pred`).
            let mut bound: HashSet<&str> = f.sig.params.iter().map(|p| p.name.as_str()).collect();
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
pub(crate) fn count_uses<'a>(
    e: &'a Expr,
    bound: &mut HashSet<&'a str>,
    counts: &mut HashMap<&'a str, usize>,
) {
    match &e.kind {
        // A shim installs its bound functions by address, which is a use like
        // any other — without counting them here they'd look dead and be pruned.
        ExprKind::Shim(_, bindings, body) => {
            for (_, f) in bindings {
                if !bound.contains(f.as_str()) {
                    *counts.entry(f).or_insert(0) += 1;
                }
            }
            count_uses(body, bound, counts);
        }
        ExprKind::Ident(n) => {
            if !bound.contains(n.as_str()) {
                *counts.entry(n).or_insert(0) += 1;
            }
        }
        ExprKind::Call(name, args, _) => {
            let name = name.name();
            if !bound.contains(name) {
                *counts.entry(name).or_insert(0) += 1;
            }
            for a in args {
                count_uses(a, bound, counts);
            }
        }
        ExprKind::Match(s, arms) => {
            count_uses(s, bound, counts);
            for arm in arms {
                let mut added: Vec<&str> = Vec::new();
                arm.pattern.each_binding(&mut |b| {
                    if bound.insert(b) {
                        added.push(b);
                    }
                });
                count_uses(&arm.body, bound, counts);
                for b in added {
                    bound.remove(b);
                }
            }
        }
        ExprKind::IfLet(arm, s, else_b) => {
            count_uses(s, bound, counts);
            let mut added: Vec<&str> = Vec::new();
            arm.pattern.each_binding(&mut |b| {
                if bound.insert(b) {
                    added.push(b);
                }
            });
            count_uses(&arm.body, bound, counts);
            for b in added {
                bound.remove(b);
            }
            count_uses(else_b, bound, counts);
        }
        ExprKind::Let(n, _, val, b) | ExprKind::LetMut(n, _, val, b) => {
            count_uses(val, bound, counts);
            let added = bound.insert(n);
            count_uses(b, bound, counts);
            if added {
                bound.remove(n.as_str());
            }
        }
        ExprKind::For(v, index, iter, b) => {
            count_uses(iter, bound, counts);
            let added = bound.insert(v);
            let added_index = index.as_ref().is_some_and(|i| bound.insert(i));
            count_uses(b, bound, counts);
            if added_index {
                bound.remove(index.as_ref().expect("inserted above").as_str());
            }
            if added {
                bound.remove(v.as_str());
            }
        }
        ExprKind::Lambda(ps, b) => {
            let added: Vec<&str> = ps
                .iter()
                .filter(|p| bound.insert(&p.name))
                .map(|p| p.name.as_str())
                .collect();
            count_uses(b, bound, counts);
            for n in added {
                bound.remove(n);
            }
        }
        // Everything else only nests expressions. A `set`'s target is rooted at an
        // existing binding — a local either way, never a countable function
        // reference — and `children` leaves it out.
        _ => {
            for child in children(e) {
                count_uses(child, bound, counts);
            }
        }
    }
}

// ---- Inlining ---------------------------------------------------------------
//
// Two selection rules share one substitution engine (`replace_call` /
// `build_inlined`) and one set of safety rules (`is_inline_shape`):
//
//   - `inline_single_use`: a private function called exactly once. Inlining it
//     is free — the body moves, nothing is duplicated — and the definition goes.
//   - `inline_small`: any function whose body is under a size threshold,
//     however many times it is called. This one *does* duplicate, which is the
//     point: a two-line callee inlined into its callers puts the retain of an
//     argument and the release at the callee's exit in the same basic block,
//     where `ElideRcPairs` can cancel them.
//
// Both are conservative in the same way, and for the same reason: an inlined
// body has to mean exactly what it meant where it was written.
//
// Inline a *private* function that is used exactly once: replace its single call
// site with the function's body (parameters bound to the arguments via `let`s)
// and drop the now-uncalled definition. Gated to programs with a `main` (and no
// `__test_main`) — i.e. the `run`/`build` path, where reachability is seeded from
// `main` and removing a function is safe. No-`main` programs (FFI engines,
// libraries, the `check` driver) are left untouched, since any function may be
// called by name there.

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
pub fn inline_single_use_post_mono(
    program: &MonoProgram,
    externally_called: &HashSet<String>,
) -> MonoProgram {
    // A program with no single entry point is a *library*: an FFI engine loaded
    // for its `pub` entries, or a fragment a Rust caller compiled to invoke by
    // name through `Compilation::run_*`. Any function in one may be called from
    // outside, so none may be elided — `externally_called` cannot enumerate the
    // callers of an open API. Inlining into AIPL callers is still safe there,
    // and still what lets the sinking pass see through a call.
    let library = !program
        .fns
        .iter()
        .any(|f| f.name == "main" || f.name == "__test_main");
    let mut program = program.clone();
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

        let fparam_names: Vec<InlineParam> = f
            .params
            .iter()
            .map(|p| InlineParam {
                name: p.name.clone(),
                // Widened for the `let` annotation the expansion produces;
                // see `InlineParam::ty`.
                ty: p.ty.widen(),
                mutable: p.mutable,
            })
            .collect();
        let replaced = inline_typed::expand_calls(
            &mut program,
            &f,
            &fparam_names,
            inline_typed::Sites::First,
            false,
        );
        if replaced {
            // Inlining and *eliding* are separate decisions. A function called
            // once from AIPL can be folded into that caller and still be
            // emitted, which is all an external caller needs — so an entry stays
            // in the program whether or not any AIPL code still calls it.
            if library || externally_called.contains(&f.name) {
                skip.insert(f.name.clone());
            } else {
                program.fns.retain(|g| g.name != f.name);
            }
        } else {
            skip.insert(f.name.clone());
        }
    }
    program
}

/// The default body-size threshold for [`inline_small`], in non-leaf
/// expressions.
///
/// Four, because that is where inlining starts paying for itself. The point of
/// inlining a small callee is not the removed call but what it exposes: an
/// argument's retain and the callee's release of it land in one basic block,
/// which is the shape `ElideRcPairs` cancels. Measured over the dogfooded
/// sources, the pairs that pass finds go 67 (inlining off) → 67 (threshold 2) →
/// 80 (4) → 83 (8): nothing at 2, because a body that small is scalar
/// arithmetic or a field access and has no refcount traffic to expose, and
/// little more after 4, which buys 3 further pairs for roughly 20% more IR.
///
/// Override at run time with `AIPL_INLINE_MAX_EXPRS` (see
/// `aipl_codegen::INLINE_MAX_EXPRS_ENV`) to re-run that experiment without a
/// rebuild.
pub const DEFAULT_INLINE_MAX_EXPRS: usize = 4;

/// Whether `ty` mentions a type that only monomorphization resolves: bare `any`,
/// or one of the literal placeholders the checker threads through inference.
///
/// [`build_inlined`] binds each parameter with its *declared* type — load-bearing
/// for narrow ints, where dropping the annotation would widen the arithmetic — so
/// a parameter typed `any[]` would pin an inlined `i64[]` argument to `any[]` and
/// fail to check. Such a signature is fine to *call*; it is only inlining it
/// early, before those types are resolved, that breaks. Declared type variables
/// are excluded separately by the candidate filter; this catches the bare-`any`
/// signatures that carry none.
fn mentions_abstract_type(ty: &Type) -> bool {
    match ty {
        Type::Case(v) => mentions_abstract_type(v),
        Type::Any | Type::ConcatStr => true,
        Type::Optional(t) | Type::Array(t) | Type::Set(t, _) | Type::Without(t, _) => {
            mentions_abstract_type(t)
        }
        Type::Dict(k, v) | Type::Result(k, v) => {
            mentions_abstract_type(k) || mentions_abstract_type(v)
        }
        Type::Fn(ps, r) => ps.iter().any(mentions_abstract_type) || mentions_abstract_type(r),
        Type::Tuple(ts) | Type::Generic(_, ts) => ts.iter().any(mentions_abstract_type),
        // A type the checker could not pin down is abstract in exactly the sense
        // this asks about: only monomorphization settles it, so a body carrying
        // one is not safe to inline early.
        Type::Unknown => true,
        // A type *variable* is abstract in the ordinary sense, but a generic
        // signature is already excluded by the candidate filter before this runs
        // (see the doc comment), so counting one here would change nothing but
        // could only misfire. Left as it behaved when a variable was a `Named`.
        Type::Unit | Type::Primitive(_) | Type::Named(_) | Type::TypeVar(_) => false,
    }
}

/// The body-size bound for [`inline_inspected`], in non-leaf expressions.
///
/// Sixteen times [`DEFAULT_INLINE_MAX_EXPRS`], because the bodies it exists for
/// are tables: a `match` with one array per arm is as many expressions as it has
/// arms, and the question asked of it afterwards (`is_nonempty`) is the case
/// where none of them is needed. Duplicating one costs its size once per
/// *inspecting* call site, not per call site, so the bound can be this much looser.
pub const DEFAULT_INLINE_INSPECTED_MAX_EXPRS: usize = 64;

/// [`inline_small`] over the *monomorphized* program: fold a small body into
/// **every** caller, not just a single one.
///
/// The single-use pass cannot reach a body with two callers, however tiny — and
/// tiny-with-several-callers is exactly the shape the AIPL-implemented builtins
/// have. `value_or`/`value_or_err` are three lines and called from everywhere;
/// until their `match` is in the caller's body, the sinking pass cannot see that
/// the default is read on one arm only, and every caller pays for it eagerly.
///
/// Runs after monomorphization because that is when those builtins exist at all:
/// they are loaded on demand while instantiating, so the pre-mono `inline_small`
/// never sees them.
///
/// Candidates are chosen once, from the program as it arrives — inlining only
/// grows callers, so re-selecting could never add to the set, while re-walking
/// it risks an unbounded ping-pong between two small mutually-calling functions.
pub fn inline_small_post_mono(
    program: &MonoProgram,
    max_exprs: usize,
    externally_called: &HashSet<String>,
) -> MonoProgram {
    let mut program = program.clone();
    let binders = collect_binders_mono(&program);
    // See `inline_single_use_post_mono`: with no single entry, any function may
    // be called by name from Rust, so none may be elided.
    let library = !program
        .fns
        .iter()
        .any(|f| f.name == "main" || f.name == "__test_main");
    let candidates: Vec<ConcreteFn> = program
        .fns
        .iter()
        .filter(|f| {
            typed_util::body_size(&f.body) <= max_exprs
                && f.name != "main"
                && f.name != "__test_main"
                // A `.test` body's name changes what `?` means inside it — see
                // `is_inline_candidate_mono`.
                && !aipl_syntax::is_test_body(&f.name)
                // See `inline_candidates`.
                && !aipl_syntax::is_constant_initializer(&f.name)
                // Never rebound by a local, so a call site means the function it
                // appears to.
                && !binders.contains(f.name.as_str())
                // A higher-order function (one with a function-typed parameter)
                // is not inlined. Variadic parameters need no check:
                // `specialize_variadic` has already resolved them.
                && is_inline_shape_typed(
                    f.params
                        .iter()
                        .any(|p| matches!(p.ty, ConcreteType::Fn(_, _))),
                    &f.body,
                )
                // Not recursive: every copy would carry the call back to `f`, so
                // inlining would never get rid of it.
                && !f.is_directly_recursive()
        })
        .cloned()
        .collect();

    for f in candidates {
        // Re-read the candidate's *current* body. The list is fixed up front,
        // but every body in it changes as earlier candidates are inlined — and
        // splicing a stale snapshot back in would revive calls to a function an
        // earlier round already expanded away and dropped, leaving codegen with
        // a call to a definition that no longer exists.
        let Some(f) = program.fns.iter().find(|g| g.name == f.name).cloned() else {
            continue;
        };
        let fparams: Vec<InlineParam> = f
            .params
            .iter()
            .map(|p| InlineParam {
                name: p.name.clone(),
                ty: p.ty.widen(),
                mutable: p.mutable,
            })
            .collect();
        // Not into itself: `is_directly_recursive` rejected a self-referencing body, but a
        // candidate is still its own function.
        let replaced =
            inline_typed::expand_calls(&mut program, &f, &fparams, inline_typed::Sites::All, true);
        // Unlike the pre-mono pass, nothing downstream will drop the orphan for
        // us — an unreferenced function still reaches codegen — so drop it here
        // once nothing names it and nothing outside can ask for it.
        if replaced && !library && !externally_called.contains(&f.name) {
            let counts = use_counts_mono(&program);
            if counts.get(f.name.as_str()) == Some(&0) {
                program.fns.retain(|g| g.name != f.name);
            }
        }
    }
    program
}

/// Whether a function's body can be moved into a caller as it stands: the
/// function is not a higher-order or variadic template (`higher_order_or_variadic`),
/// every early exit in the body can be rewritten into an expression, and the body
/// holds no in-place higher-order intrinsic. It says nothing about the function's
/// name, signature or call sites; those are for the caller to check. Suitable as
/// the shared part of an inlining candidate test.
///
/// A mutating method passes: [`build_inlined`] binds its `mut self` receiver
/// `mut` and yields it, which is what a call to such a method means in either
/// position it can appear.
fn is_inline_shape_typed(higher_order_or_variadic: bool, body: &typed_util::Typed) -> bool {
    !higher_order_or_variadic
        && early_exit::without_early_exits_typed(body).is_some()
        && !typed_util::contains_inplace_hof_intrinsic(body)
}

/// [`is_inline_candidate`] for a monomorphized [`ConcreteFn`]. It has no `is_pub`
/// or type parameters to rule out: nothing but `main` is reachable from outside,
/// and monomorphization leaves no type parameters.
fn is_inline_candidate_mono(
    f: &ConcreteFn,
    counts: &HashMap<&str, usize>,
    binders: &HashSet<&str>,
    skip: &HashSet<String>,
) -> bool {
    counts.get(f.name.as_str()) == Some(&1)
        && f.name != "main"
        // A `.test` body is not an ordinary function: codegen dispatches on the
        // *name* `__test$<fn>` to make `?` fail the current test rather than
        // propagate (see `in_test` in codegen). Folding one into `__test_main`
        // would silently change what `?` means inside it.
        && !aipl_syntax::is_test_body(&f.name)
        // See `inline_candidates`.
        && !aipl_syntax::is_constant_initializer(&f.name)
        && f.name != "__test_main"
        && !skip.contains(f.name.as_str())
        && !binders.contains(f.name.as_str())
        // A mutating method that *owns* its receiver is not inlinable: its body
        // is compiled against a transfer only a real call boundary performs —
        // the caller hands the slot's reference over (`__move`), the entry makes
        // the block unique, and the return hands it back. Splicing the body into
        // the caller reproduces none of that, and the receiver would come back
        // short of what the writeback expects. An ordinary (lent) mutating method
        // is inlined (`optimizations/inline_mutating_method`).
        && !(f.is_mutating() && f.params.first().is_some_and(|p| p.owned))
        // A higher-order function (one with a function-typed parameter) is not
        // inlined. Variadic parameters need no check: `specialize_variadic` has
        // already resolved them.
        && is_inline_shape_typed(
            f.params
                .iter()
                .any(|p| matches!(p.ty, ConcreteType::Fn(_, _))),
            &f.body,
        )
        // Not recursive: the definition is dropped once its one call is inlined,
        // so a body that calls itself would be left calling a function that is
        // gone. (A candidate's name is never rebound — `binders` above — so a
        // match by name is exact.)
        && !f.is_directly_recursive()
}

/// [`use_counts`] for a [`MonoProgram`].
fn use_counts_mono(program: &MonoProgram) -> HashMap<&str, usize> {
    let mut counts: HashMap<&str, usize> =
        program.fns.iter().map(|f| (f.name.as_str(), 0)).collect();
    for f in &program.fns {
        let mut bound: HashSet<&str> = f.params.iter().map(|p| p.name.as_str()).collect();
        typed_util::count_uses(&f.body, &mut bound, &mut counts);
    }
    counts
}

/// [`collect_binders`] for a [`MonoProgram`].
fn collect_binders_mono(program: &MonoProgram) -> HashSet<&str> {
    let mut set = HashSet::new();
    for f in &program.fns {
        for p in &f.params {
            set.insert(p.name.as_str());
        }
        typed_util::collect_body_binders(&f.body, &mut set);
    }
    set
}

/// The direct sub-expressions of `e` (read-only traversal helper), as an iterator
/// that allocates nothing. Public so codegen's tail-position walk (`tail_callees`)
/// can reuse the one exhaustive list of child expressions rather than keep a second
/// copy in step with it.
///
/// This is [`aipl_syntax::Expr::children`] for a program that has been through the
/// loader: `set`'s target is a name by now, so it is not yielded, and a keyword
/// argument or a spread, which the loader expands, is a bug to meet here.
pub fn children(e: &Expr) -> Children<'_> {
    match &e.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        // The target of a `set` is a name by now, not an expression to walk.
        ExprKind::Assign(_, value, body) => Children::of([value, body]),
        _ => e.children(),
    }
}

/// Whether `t` still contains a "decided by context" placeholder — the types a
/// bare `none` / empty literal carries until something pins them.
fn has_placeholder(t: &Type) -> bool {
    match t {
        Type::Any => true,
        Type::Optional(i) | Type::Array(i) | Type::Set(i, _) => has_placeholder(i),
        Type::Dict(k, v) => has_placeholder(k) || has_placeholder(v),
        Type::Result(a, b) => has_placeholder(a) || has_placeholder(b),
        Type::Fn(ps, r) => ps.iter().any(has_placeholder) || has_placeholder(r),
        Type::Tuple(es) | Type::Generic(_, es) => es.iter().any(has_placeholder),
        _ => false,
    }
}

/// [`children`], mutably.
pub fn children_mut(e: &mut Expr) -> ChildrenMut<'_> {
    if matches!(e.kind, ExprKind::KwArg(..)) {
        unreachable!("keyword arguments are expanded by the loader");
    }
    if matches!(e.kind, ExprKind::Spread(..)) {
        unreachable!("array spreads are desugared by the loader");
    }
    if matches!(e.kind, ExprKind::Assign(..)) {
        let ExprKind::Assign(_, value, body) = &mut e.kind else {
            unreachable!()
        };
        return ChildrenMut::of([value, body]);
    }
    e.children_mut()
}

/// Whether `e` references `name` as a `Call` callee or an `Ident` value. Only
/// called for a `name` that is never a binder, so no shadowing to consider.
fn references_name(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Call(n, args, _) => n == name || args.iter().any(|a| references_name(a, name)),
        _ => children(e).any(|c| references_name(c, name)),
    }
}

/// One parameter of the function being inlined — what [`build_inlined`] needs in
/// order to bind it at the call site.
struct InlineParam {
    name: String,
    /// Abstract, not [`ConcreteType`]: this becomes the annotation on the
    /// `let` the inlined call expands to, and annotations live on `Expr`,
    /// which is shared by both sides of monomorphization.
    ty: Type,
    /// The `mut self` receiver of a mutating method. Only the *first* parameter
    /// can ever carry this: the checker rejects `mut` on a parameter that isn't
    /// the first, and on one not named `self`. It binds with `mut` rather than
    /// `let`, and it is what the call produces — see [`build_inlined`].
    mutable: bool,
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
        || matches!(t, Type::Array(_) | Type::Set(..) | Type::Dict(_, _))
}

/// [`is_heap`] over the post-monomorphization representation, for the analyses
/// that run on [`ConcreteFn`]s.
///
/// Spelled out rather than widening at the call site: which representation an
/// analysis speaks is a property of *when* it runs, and saying so once here
/// beats converting at each use as though a boundary were being crossed.
pub(crate) fn is_heap_concrete(t: &ConcreteType) -> bool {
    *t == ConcreteType::Primitive(Primitive::Str)
        || aipl_syntax::concrete::is_error(t)
        || matches!(
            t,
            ConcreteType::Array(_) | ConcreteType::Set(..) | ConcreteType::Dict(_, _)
        )
}

/// Whether `t`'s values are `str`-shaped — a `str`/`Error`/concat-str, or a
/// `char[]`, which shares the representation.
fn str_shaped(t: &Type) -> bool {
    aipl_syntax::is_str_repr(t)
        || matches!(t, Type::Array(e) if matches!(**e, Type::Primitive(Primitive::Char)))
}

/// How many bytes a `reusable` element of type `t` occupies in an array slot.
///
/// Monomorphization does not know codegen's layout table, but this one fact it
/// has to: a `str` is a 24-byte value, and so — under the wide layouts — is an
/// array, a set and a dict, each its block plus, for an array window, where its
/// elements start and how many (`aipl-codegen`'s `is_wide_seq`). A `#{char}` is
/// its 32-byte bitfield. A narrow integer is packed at its own width in an array
/// (`aipl-codegen`'s `array_elem_size`), so an `i64[]` buffer has no room to be
/// reused for a `u8[]` or the other way round, and a 64-bit integer is a word.
///
/// A `char[]` is `str`-shaped, so it is 24 bytes either way.
fn slot_bytes(t: &Type) -> usize {
    match t {
        Type::Set(e, _) if matches!(**e, Type::Primitive(Primitive::Char)) => 32,
        _ if str_shaped(t) || matches!(t, Type::Array(_) | Type::Set(..) | Type::Dict(..)) => 24,
        Type::Primitive(p) => p.int_bits().map_or(8, |bits| (bits / 8) as usize),
        _ => 8,
    }
}

/// Whether an array of `from` can have its buffer reused to hold `to`.
///
/// This is where monomorphization has to know a representation: the buffer is
/// sized for `from`'s slots, so `to` has to fit in them exactly. It is a question
/// of *width*, and twice the answer has been wrong because it was asked as
/// something narrower. `[1, 2].map(to_str)` reused an `i64[]` buffer for `str`
/// elements and wrote three words into each one-word slot — which is what the
/// old `str`-shape test fixed — and `[1, 2].map(|x| [x])` then did the same with
/// array elements, once arrays became 24 bytes and that test had no way to know.
///
/// And it is a question of *what kind* of thing the slots hold where an integer
/// is involved: a short integer array is held inline (`aipl-codegen`'s
/// `may_be_inline`), its elements in its own bytes, and the in-place body then
/// writes into those bytes. That is sound only while the result is integers too
/// — a set or a dict is a word wide, but no inline value may hold one.
fn slot_fits(from: &Type, to: &Type) -> bool {
    let int = |t: &Type| matches!(t, Type::Primitive(p) if p.is_int());
    slot_bytes(from) == slot_bytes(to) && int(from) == int(to)
}

/// Whether `arg` evaluates to a freshly-allocated, uniquely-owned heap value, so
/// it can be *moved* into an owning parameter rather than borrowed: a
/// collection literal, or a call returning a heap value (a fresh rc-1 block). `arg_ty` is
/// `arg`'s inferred type. Mirrors codegen's former `is_fresh_heap_arg`.
///
/// "Fresh" is about ownership, not about where the bytes live: a constant
/// literal is a static block, and a call may return one or a view. That is
/// fine for a move — the owned instance never writes into a moved-in block
/// without going through a guard (`array_ops::push_mut`, `array_ops::reserve`,
/// [`writable`]) that copies a block it must not write.
/// What choosing a callee's owned instance asks of an argument: whether it is a fresh heap value
/// ([`fresh_tail`]) and, if it is a bare name, which.
pub(crate) struct ArgShape {
    pub(crate) fresh: bool,
    pub(crate) name: Option<String>,
}

/// Which parameters of the function with these (concrete)
/// `params`/`return_ty`/`body` it can take ownership of, by index. The "take
/// ownership and mutate" shape, decided per parameter: a heap parameter
/// consumed exactly once as `mut y = p` with `y` exclusive in the rest of the
/// body — so a fresh argument can be moved in and reused. The function must
/// not be `main` and must return a heap value. `name` distinguishes `main`.
///
/// Every parameter is judged on its own, so a function may own several
/// Whether a `mut self` receiver may be *transferred* rather than lent, and so
/// grown in place by the callee — the parameter's index when it may.
///
/// A `mut self` parameter is not an owned parameter on the ordinary shape: the
/// body never moves it into another binding, it mutates it where it sits. The
/// question for it is instead the one [`binding_is_exclusive`] asks of a local
/// `mut` — does anything in the body alias the receiver, so that an in-place
/// write could be seen through something else?
///
/// Marking it owned is what makes the transfer *happen*, through machinery that
/// already exists: `move_last_use` wraps such a call's receiver in
/// [`Callee::Move`], whose lowering takes the block out of the binding's slot
/// and makes it a temporary; codegen's argument hand-off then moves that
/// temporary in, as it does any fresh value. The callee makes the block unique
/// on entry, which copies only above a refcount of one — so a caller whose
/// receiver really is aliased pays a copy and its alias is untouched, and one
/// whose receiver is its own pays nothing.
fn mut_receiver_owned<T: aipl_syntax::typed::TyRepr>(
    params: &[Param],
    body: &aipl_syntax::typed::TExpr<T>,
    owns_heap: &dyn Fn(&Type) -> bool,
    mutating: &dyn Fn(&str) -> bool,
) -> Option<usize> {
    let p = params.first()?;
    // Only the receiver, and only a block-owning one: a `str` grows through its
    // own in-place append, whose ownership is the slot-value model's rather than
    // this one's.
    (p.mutable
        && matches!(aipl_syntax::unrefined(&p.ty), Type::Array(_))
        && owns_heap(&p.ty)
        && ownership_typed::binding_is_exclusive(&p.name, body, true, mutating))
    .then_some(0)
}

/// (`merge(a, b)` with `mut xs = a; mut ys = b;`), and a call moves in
/// whichever of them it has a fresh argument for — see `owned_for_call`, and
/// `enqueue`, which mangles an instance per owned set.
///
/// A `self` receiver qualifies like any other parameter — `xs.grow()` is
/// `grow(xs)` by the time this runs. A `mut self` one does not: it is a
/// slot-backed binding with the writeback protocol (`set x = x.f(..)`), not a
/// value the body rebinds.
///
/// A *struct* parameter is owned on a different shape: the body reads it only
/// through its fields, and reads each heap-owning field at most once, never
/// inside a loop or a lambda. Then every such read is a *move-out* — the
/// callee takes the field's reference instead of retaining a copy — and what
/// it never read is released at exit. `owns_heap` is the struct-aware test for
/// "has something to move" (the caller's tables know the fields); a struct
/// with no heap field has nothing to own.
fn owned_eligible<T: aipl_syntax::typed::TyRepr>(
    name: &str,
    params: &[Param],
    return_ty: &Option<Type>,
    body: &aipl_syntax::typed::TExpr<T>,
    owns_heap: &dyn Fn(&Type) -> bool,
    heap_fields_of: &dyn Fn(&Type) -> HashSet<String>,
    mutating: &dyn Fn(&str) -> bool,
) -> Vec<usize> {
    if name == "main" {
        return Vec::new();
    }
    // A `mut self` receiver is the one owned parameter a *void* function can
    // have: what it hands back is the receiver, through the writeback, so the
    // declared return type says nothing about whether anything is owned.
    let receiver = mut_receiver_owned(params, body, owns_heap, mutating);
    if receiver.is_none() && !return_ty.as_ref().is_some_and(owns_heap) {
        return Vec::new();
    }
    params
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.mutable && owns_heap(&p.ty))
        .filter(|(_, p)| {
            if is_heap(&p.ty) {
                move_last_use_typed::count_ident(&p.name, body) == 1
                    && move_last_use_typed::find_move_into(&p.name, body).is_some_and(
                        |(y, body_after)| {
                            ownership_typed::binding_is_exclusive(y, body_after, true, mutating)
                        },
                    )
            } else {
                // Either each heap field is read once and moved out, or the
                // parameter is only ever handed on whole (see `param_forwarded`):
                // a function that reads `self` several times and passes it to
                // the next at the end still owns what it was given.
                move_last_use_typed::fields_read_once(&p.name, body)
                    || move_last_use_typed::forwarded(&p.name, body, true, &heap_fields_of(&p.ty))
            }
        })
        .map(|(i, _)| i)
        .chain(receiver)
        .collect()
}

// ---- Borrow-only parameter analysis (retain elision) ------------------------
//
// A heap argument is normally *borrowed* across a call by the uniform borrow
// protocol: the caller retains it before the call and the callee releases it
// once on return (its entry-scope track). Those two operations are a matched
// pair — they exist only to keep the value alive for a callee that might
// outlive the caller's own reference to it, and every callee use that really
// *keeps* the value (storing it in a struct/array, returning it, handing it to
// a consuming builtin) emits its own retain on top. So whenever the caller
// provably holds a reference for the whole call, the pair cancels and both
// halves can be dropped: the callee just *inspects* a value someone else owns.
//
// `inspect_only_params` decides, per instance and per parameter, whether that
// holds. It must agree exactly with codegen on both sides of a call — the
// caller's skipped retain and the callee's skipped release come from this one
// answer, keyed by instance name, so they cannot drift apart.
//
// The pair is *not* cancellable when:
//
//   - the parameter is `owned` — the caller already moves its sole reference in
//     and the callee consumes it (there is no retain to elide, and no release);
//   - the instance has a `mut self` receiver — a mutating method is the one way
//     a callee can reach back and drop a value its *caller* still refers to
//     (`set self.f = v` / an element overwrite releases the old field, which a
//     sibling argument may be a borrow of, e.g. `w.set_s(w.s)`). The caller's
//     retain is exactly what keeps that sibling alive, so no parameter of a
//     mutating instance is elidable;
//   - the instance's address is taken as a function value — an indirect call
//     goes through `compile_indirect_call`, which knows only the function
//     *type* and so always retains; a callee that skipped its release would
//     leak one reference per such call;
//   - the body re-owns the parameter through a `mut` binding's slot (`mut y =
//     p` / `set y = p`), the one shape where codegen may transfer an incoming
//     reference rather than retaining a fresh one.
//
// Everything else is an inspecting use — reading, comparing, iterating,
// indexing, slicing, passing it on (a nested call retains it itself, or is
// itself elided and touches no refcount at all).

/// Count `Ident(name)` reads in `e`. Used to confirm a moved parameter is
/// consumed exactly once. Overcounts never undercount, so a miscount only
/// disqualifies the move (safe).
pub(crate) fn count_ident(name: &str, e: &Expr) -> usize {
    let c = |x: &Expr| count_ident(name, x);
    match &e.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::TypeArgs(x, _) => c(x),
        ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        ExprKind::Shim(_, _, body) => c(body),
        ExprKind::Ident(n) => usize::from(n == name),
        ExprKind::Num(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::None
        | ExprKind::Unit => 0,
        ExprKind::Neg(x) | ExprKind::Field(x, _) | ExprKind::Try(x) | ExprKind::Return(x) => c(x),
        ExprKind::Seq(a, b)
        | ExprKind::Index(a, b)
        | ExprKind::Let(_, _, a, b)
        | ExprKind::LetMut(_, _, a, b)
        | ExprKind::For(_, _, a, b)
        | ExprKind::While(a, b) => c(a) + c(b),
        // A bare-ident LHS is an lvalue, not a read; a field-path LHS also
        // *reads* its base (the functional-update desugar copies the struct's
        // other fields out of it) — counting it once overcounts at worst,
        // which is the safe direction.
        ExprKind::Assign(lhs, a, b) => {
            let lhs_reads = if matches!(&lhs.kind, ExprKind::Ident(_)) {
                0
            } else {
                c(lhs)
            };
            lhs_reads + c(a) + c(b)
        }
        ExprKind::If(a, b, d) => c(a) + c(b) + c(d),
        ExprKind::Slice(a, b, d) => c(a) + c(b) + d.as_ref().map_or(0, |d| c(d)),
        ExprKind::Call(_, args, _)
        | ExprKind::ArrayLit(args)
        | ExprKind::SetLit(args, _)
        | ExprKind::TupleLit(args) => args.iter().map(c).sum(),
        ExprKind::DictLit(pairs) => pairs.iter().map(|(k, v)| c(k) + c(v)).sum(),
        ExprKind::Construct(_, inits) => inits.iter().map(|i| c(&i.value)).sum(),
        ExprKind::Match(s, arms) => c(s) + arms.iter().map(|a| c(&a.body)).sum::<usize>(),
        ExprKind::IfLet(arm, s, else_b) => c(s) + c(&arm.body) + c(else_b),
        ExprKind::Lambda(_, body) => c(body),
    }
}
