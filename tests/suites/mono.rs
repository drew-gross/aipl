//! Unit tests for the monomorphization pass. These run on ASTs (parse →
//! monomorphize) without the codegen/cases harness, so they're fast. Generic
//! bodies here avoid builtins (whose names are only canonicalized by the
//! loader) — the instantiation logic is driven purely by call-site argument
//! types.

use aipl::ast::{Item, Program};
use aipl::mono::{inline_single_use_pinned, monomorphize, use_counts};
use aipl::DebugOptions;
use std::collections::HashMap;

/// Parse, first installing the (idempotent) parser hooks the dogfooded
/// section-header / raw-string helpers require — there's no native fallback.
fn parse(src: &str) -> Result<Program, aipl::Error> {
    aipl::install_parser_hooks();
    aipl::parse(src)
}

/// Sorted function names in a monomorphized program.
fn mono_fn_names(p: &aipl::mono::MonoProgram) -> Vec<String> {
    let mut names: Vec<String> = p.fns.iter().map(|f| f.name.clone()).collect();
    names.sort();
    names
}

/// Monomorphize one source, **through the loader and the frontend** — mono's input is a loaded
/// program whose expressions say what they are (`Expr::ty`), not a parsed one. An operator is a
/// call to its spelling until the loader resolves it against the file's imports, so a source
/// using an operator has to be loaded for mono to see the call it names.
fn mono_names(src: &str) -> Vec<String> {
    aipl::install_parser_hooks();
    let prog = aipl::loader::load_program_sources(&[("./main.aipl", src)], DebugOptions::OFF)
        .expect("load");
    let frontend = aipl::codegen::frontend(&prog).expect("frontend");
    let mono = monomorphize(&frontend.program, DebugOptions::OFF).expect("monomorphize");
    mono_fn_names(&mono)
}

/// Per-function reference counts for `src` (groundwork for inlining). Operates on
/// the parsed AST — no type-checking — so cases need only parse.
fn counts(src: &str) -> HashMap<String, usize> {
    let program = parse(src).expect("parse");
    use_counts(&program)
        .into_iter()
        .map(|(name, count)| (name.to_string(), count))
        .collect()
}

#[test]
fn use_counts_counts_call_sites() {
    let c = counts(
        "fn a() -> i64 { 1 }
         fn b() -> i64 { a() }
         fn c() -> i64 { a() }
         fn main() -> i64 { b() }",
    );
    assert_eq!(c["a"], 2); // called in `b` and `c`
    assert_eq!(c["b"], 1); // called in `main`
    assert_eq!(c["c"], 0); // never called
    assert_eq!(c["main"], 0); // only the runtime calls `main`
}

#[test]
fn use_counts_self_reference_counts() {
    // A recursive call is a use, so a single-external-use recursive fn isn't a
    // count==1 inline candidate.
    let c = counts("fn loop_forever(n: i64) -> i64 { loop_forever(n) }");
    assert_eq!(c["loop_forever"], 1);
}

#[test]
fn use_counts_ignores_shadowing_locals() {
    // A parameter named like a function refers to the parameter, not the fn.
    let c = counts(
        "fn helper() -> i64 { 1 }
         fn shadow(helper: i64) -> i64 { helper }
         fn main() -> i64 { helper() }",
    );
    assert_eq!(c["helper"], 1); // only the call in `main`
    assert_eq!(c["shadow"], 0);
}

#[test]
fn use_counts_counts_function_value_references() {
    // A bare function name passed as a value (`xs.map(dbl)`) is a use.
    let c = counts(
        "import { map } from builtins;
         fn dbl(x: i64) -> i64 { x }
         fn doubler(xs: i64[]) -> i64[] { xs.map(dbl) }",
    );
    assert_eq!(c["dbl"], 1);
    assert_eq!(c["doubler"], 0);
    // Builtins are counted too (inline candidates). `map` is the call here.
    assert_eq!(c["map"], 1);
}

/// Function names remaining after inlining single-use private functions.
fn inlined_names(src: &str) -> Vec<String> {
    aipl::install_parser_hooks();
    let prog = aipl::loader::load_program_sources(&[("./main.aipl", src)], DebugOptions::OFF)
        .expect("load");
    let frontend = aipl::codegen::frontend(&prog).expect("frontend");
    let inlined = inline_single_use_pinned(&frontend.pinned.program);
    let mut names: Vec<String> = inlined
        .items
        .iter()
        .filter_map(|it| match it {
            aipl::typed::TItem::Fn(f) => Some(f.name.text.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

#[test]
fn inline_removes_single_use_private_helper() {
    let names = inlined_names(
        "import { wrapping_add as + } from builtins;
         fn helper(x: i64) -> i64 { x + 1 }
         fn main() -> i64 { helper(5) }",
    );
    assert!(!names.contains(&"helper".to_string())); // inlined into main, then dropped
    assert!(names.contains(&"main".to_string()));
}

#[test]
fn inline_keeps_pub_helper() {
    // `pub` is the cross-file/FFI surface — preserved even when used once.
    let names = inlined_names(
        "import { wrapping_add as + } from builtins;
         pub fn helper(x: i64) -> i64 { x + 1 }
         fn main() -> i64 { helper(5) }",
    );
    assert!(names.contains(&"helper".to_string()));
}

#[test]
fn inline_keeps_multiply_used_helper() {
    let names = inlined_names(
        "import { wrapping_add as + } from builtins;
         fn helper(x: i64) -> i64 { x + 1 }
         fn main() -> i64 { helper(5) + helper(6) }",
    );
    assert!(names.contains(&"helper".to_string())); // used twice
}

#[test]
fn inline_skips_no_main_program() {
    // No `main` → FFI/library: every function stays callable by name.
    let names = inlined_names(
        "import { wrapping_add as + } from builtins;
         fn helper(x: i64) -> i64 { x + 1 }
         pub fn entry(x: i64) -> i64 { helper(x) }",
    );
    assert!(names.contains(&"helper".to_string()));
}

#[test]
fn inline_keeps_recursive() {
    // Self-call counts, and the body references its own name → not inlined.
    let names = inlined_names(
        "fn rec(n: i64) -> i64 { rec(n) }
         fn main() -> i64 { 0 }",
    );
    assert!(names.contains(&"rec".to_string()));
}

#[test]
fn inline_keeps_function_value_use() {
    // Used once but only as a value (passed to `map`) — no call site to inline.
    let names = inlined_names(
        "import { map } from builtins;
         fn dbl(x: i64) -> i64 { x }
         fn main() -> i64[] { [1].map(dbl) }",
    );
    assert!(names.contains(&"dbl".to_string()));
}

#[test]
fn inlines_a_tail_return() {
    // A `return` in tail position is nothing but its value, so the body goes
    // in as `x`. This used to be refused outright — any `return` anywhere
    // disqualified a body. See `early_exit::without_early_exits`.
    let names = inlined_names(
        "fn helper(x: i64) -> i64 {
             match (x) {
                 0 => { return 1; },
                 _ => { return x; },
             }
         }
         fn main() -> i64 { helper(5) }",
    );
    assert!(!names.contains(&"helper".to_string()), "got {names:?}");
}

#[test]
fn inlines_a_guard_clause() {
    // The shape the rewrite exists for: `if (c) { return v; }; rest` becomes
    // `if (c) { v } else { rest }`, with the continuation moving into the arm
    // that falls through.
    let names = inlined_names(
        "import { greater_than as >, wrapping_mul as * } from builtins;
         fn clamped(x: i64) -> i64 { if (x > 10) { return 10; }; x * 2 }
         fn main() -> i64 { clamped(3) }",
    );
    assert!(!names.contains(&"clamped".to_string()), "got {names:?}");
}

#[test]
fn inline_keeps_a_return_inside_a_loop() {
    // The boundary. A `return` in a loop leaves the loop *as well as* the
    // function, and an expression has no way to say "stop iterating and be
    // this value" — that wants a `break` carrying a value, which the language
    // does not have. So this one is still refused.
    let names = inlined_names(
        "import { greater_than as > } from builtins;
         fn first_over(xs: i64[], limit: i64) -> i64 {
             for (let x : xs) { if (x > limit) { return x; }; }
             0
         }
         fn main() -> i64 { first_over([1], 0) }",
    );
    assert!(names.contains(&"first_over".to_string()), "got {names:?}");
}

#[test]
fn use_counts_counts_builtins() {
    // Builtin calls are counted by their callee name.
    let c = counts(
        "import { print } from builtins;
         fn greet() !prints { print(\"hi\"); print(\"bye\") }",
    );
    assert_eq!(c["print"], 2);
    assert_eq!(c["greet"], 0);
}

#[test]
fn one_instance_per_element_type() {
    let names = mono_names(
        "import { wrapping_add as + } from builtins;
         fn g(v: any[]) -> i64 { 0 }
         fn main() -> i64 { g([1, 2]) + g(['a']) }",
    );
    // The generic template is dropped; one concrete instance per element type.
    assert_eq!(names, vec!["g$char", "g$i64", "main"]);
}

#[test]
fn same_element_type_is_memoized() {
    let names = mono_names(
        "import { wrapping_add as + } from builtins;
         fn g(v: any[]) -> i64 { 0 }
         fn main() -> i64 { g([1, 2]) + g([3, 4]) }",
    );
    assert_eq!(names, vec!["g$i64", "main"]);
}

#[test]
fn generic_owned_param_requires_substituted_heap_type() {
    // `concrete_signature`'s generic branch (`Signature::make_concrete`)
    // substitutes `T` in the param/return type before `owned_eligible` checks
    // either is heap-typed. A *bare* `T` (unwrapped in `T[]`/`T?`, where
    // `is_heap` only cares about the outer container and would pass either
    // way) is the case that actually depends on the substitution: unsubstituted
    // it's `Type::Named("T")`, which `is_heap` rejects regardless of what `T`
    // resolves to, so a substitution bug here would silently disable the
    // owned optimization for every by-value generic.
    let names = mono_names(
        "fn f<T: any>(x: T) -> T { mut y = x; y }
         fn make_str() -> str { \"abc\" }
         fn main() -> i64 { f(make_str()); 0 }",
    );
    assert!(names.contains(&"f$str$own0".to_string()), "{names:?}");
}

#[test]
fn rope_arg_makes_distinct_monomorphization() {
    // A `rope_str` value carries the concat-str representation; passing it to a
    // `str` parameter emits a distinct, concat-specialized instance (`$c0`)
    // alongside the plain instance used for a plain-str argument.
    let names = mono_names(
        "import { rope_str } from builtins;
         fn label(s: str) -> str { s }
         fn main() {
             let g = rope_str(\"a\", \"b\");
             label(g);
             label(\"x\");
         }",
    );
    assert!(names.contains(&"label".to_string()), "{names:?}");
    assert!(names.contains(&"label$c0".to_string()), "{names:?}");
}

#[test]
fn generic_calling_generic_instantiates_transitively() {
    let names = mono_names(
        "fn g(v: any[]) -> i64 { 0 }
         fn f(v: any[]) -> i64 { g(v) }
         fn main() -> i64 { f(['x']) }",
    );
    assert_eq!(names, vec!["f$char", "g$char", "main"]);
}

#[test]
fn recursive_generic_terminates() {
    // `r` recurses on the same element type; memoization must stop the pass.
    let names = mono_names(
        "fn r(v: any[]) -> i64 { r(v) }
         fn main() -> i64 { r([1]) }",
    );
    assert_eq!(names, vec!["main", "r$i64"]);
}

#[test]
fn any_optional_instances_per_inner_type() {
    let names = mono_names(
        "import { wrapping_add as + } from builtins;
         fn g(x: any?) -> i64 { 0 }
         fn main() -> i64 { g(some(1)) + g(some('a')) }",
    );
    assert_eq!(names, vec!["g$char", "g$i64", "main"]);
}

#[test]
fn each_any_param_is_an_independent_type_variable() {
    // The array's element type and the optional's inner type are inferred
    // independently and need not agree; the instance is named with both.
    let names = mono_names(
        "fn h(xs: any[], x: any?) -> i64 { 0 }
         fn main() -> i64 { h([1, 2], some('a')) }",
    );
    assert_eq!(names, vec!["h$i64$char", "main"]);
}

#[test]
fn two_array_params_specialize_on_the_type_tuple() {
    // `(i64, char)` and `(i64, i64)` are distinct instances.
    let names = mono_names(
        "import { wrapping_add as + } from builtins;
         fn f(a: any[], b: any[]) -> i64 { 0 }
         fn main() -> i64 { f([1], ['c']) + f([1], [2]) }",
    );
    assert_eq!(names, vec!["f$i64$char", "f$i64$i64", "main"]);
}

#[test]
fn named_type_param_unifies_across_uses() {
    // `value_or<T>(s: T?, d: T)` — both uses of T resolve to one instance.
    let names = mono_names(
        "fn value_or<T: any>(s: T?, d: T) -> T { d }
         fn main() -> i64 { value_or(some(1), 2) }",
    );
    assert_eq!(names, vec!["main", "value_or$i64"]);
}

#[test]
fn named_type_param_distinct_instances_per_type() {
    let names = mono_names(
        "fn value_or<T: any>(s: T?, d: T) -> T { d }
         fn main() -> i64 { value_or(some('a'), 'b'); value_or(some(1), 2) }",
    );
    assert_eq!(names, vec!["main", "value_or$char", "value_or$i64"]);
}

#[test]
fn named_type_param_inferred_from_other_argument() {
    // A bare `none` for `s: T?` carries no type, and the literal for `d: T` is an `i64`
    // like any other: `T` is `i64`, and the instance is the ordinary one.
    let names = mono_names(
        "fn value_or<T: any>(s: T?, d: T) -> T { d }
         fn main() -> i64 { value_or(none, 5) }",
    );
    assert_eq!(names, vec!["main", "value_or$i64"]);
}

#[test]
fn int_literal_args_of_one_type_share_an_instance() {
    // A literal is an argument like any other: `wrap(5)` and `wrap(7)` are one instance, and
    // so are `pair(0, 5)` and `pair(0, 7)`.
    let names = mono_names(
        "fn wrap<T: any>(x: T) -> T[] { [x] }
         fn pair<T: any>(a: T, b: T) -> T[] { [a, b] }
         fn main() {
             wrap(5);
             wrap(7);
             pair(0, 5);
             pair(0, 7);
         }",
    );
    assert_eq!(names, vec!["main", "pair$i64", "wrap$i64"]);
}

#[test]
fn a_call_with_a_literal_argument_is_an_ordinary_call() {
    let names = mono_names(
        "fn identity<T: any>(x: T) -> T { x }
         fn main() -> i64 { identity(5) }",
    );
    assert_eq!(names, vec!["identity$i64", "main"]);
}

#[test]
fn a_call_with_an_effectful_argument_keeps_it() {
    let names = mono_names(
        "fn konst<T: any>(x: T) -> i64 { 42 }
         fn side() -> i64 { 7 }
         fn main() -> i64 { konst(side()) }",
    );
    assert_eq!(names, vec!["konst$i64", "main", "side"]);
}

#[test]
fn concrete_arg_gives_the_literal_its_width() {
    // The receiver makes `T` a `u8`, so the literal is a `u8` — the "passing a literal to a
    // concrete int parameter still works" requirement, made executable.
    let names = mono_names(
        "fn value_or<T: any>(s: T?, d: T) -> T { d }
         fn main() -> i64 { let n: u8? = none; value_or(n, 0); 0 }",
    );
    assert_eq!(names, vec!["main", "value_or$u8"]);
}

#[test]
fn uninstantiated_generic_is_dropped() {
    let names = mono_names(
        "fn unused(v: any[]) -> i64 { 0 }
         fn main() -> i64 { 0 }",
    );
    assert_eq!(names, vec!["main"]);
}

#[test]
fn non_generic_program_is_unchanged() {
    let names = mono_names("fn helper(x: i64) -> i64 { x } fn main() -> i64 { helper(42) }");
    assert_eq!(names, vec!["helper", "main"]);
}

#[test]
fn empty_array_arg_is_an_array_of_the_default_type() {
    // An empty array nothing constrains is an `i64[]`, so `any[]` is instantiated at `i64`.
    let names = mono_names(
        "fn g(v: any[]) -> i64 { 0 }
         fn main() -> i64 { g([]) }",
    );
    assert_eq!(names, vec!["g$i64", "main"]);
}

#[test]
fn none_literal_is_an_optional_of_the_default_type() {
    // A bare `none` nothing constrains is an `i64?`, so `any?` is instantiated at `i64`.
    let names = mono_names(
        "fn is_present(x: any?) -> bool { match (x) { some(v) => true, none => false } }
         fn main() -> i64 { if (is_present(none)) { 1 } else { 0 } }",
    );
    assert_eq!(names, vec!["is_present$i64", "main"]);
}

#[test]
fn concrete_arg_decides_the_type_of_an_empty_array() {
    // The second argument makes `T` a `u8`, and the empty array is the `u8[]` its parameter wants.
    let names = mono_names(
        "fn g<T: any>(a: T[], b: T) -> i64 { 0 }
         fn main() -> i64 { let b: u8 = 7; g([], b) }",
    );
    assert_eq!(names, vec!["g$u8", "main"]);
}

#[test]
fn empty_and_none_in_separate_any_params_are_independent() {
    // Two anonymous `any` params are independent type variables, each settling on the default.
    let names = mono_names(
        "fn h(xs: any[], x: any?) -> i64 { 0 }
         fn main() -> i64 { h([], none) }",
    );
    assert_eq!(names, vec!["h$i64$i64", "main"]);
}

/// Every name a loaded program defines, in item order.
fn defined_names(p: &Program) -> Vec<String> {
    p.items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some(f.name.text.clone()),
            Item::Struct(s) => Some(s.name.text.clone()),
            Item::Variant(v) => Some(v.name.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn loading_a_program_twice_names_everything_alike() {
    // The builtins imported here are files of the program, and several hold
    // private items of their own (a test-support struct, a helper function) whose
    // names carry the file's index. That index is its place in the load order, so
    // the load order has to be a function of the program: the dogfood IR is
    // regenerated and compared as text, and a name that changed from one run to
    // the next would make it stale every time. Each load builds its own sets,
    // with their own seeds, so an order left to one would show up within a few.
    aipl::install_parser_hooks();
    let src =
        "import { is_err_and, map_err, map_ok, sort_by, value_or, value_or_err } from builtins;
               fn main() -> i64 {
                   let r = ok(1);
                   r.map_err(|e| e).map_ok(|v| v);
                   r.is_err_and(|e| e);
                   value_or_err(none, \"x\");
                   sort_by([1], |x| x);
                   value_or(none, 0)
               }";
    let load = || {
        let program =
            aipl::loader::load_program_sources(&[("./main.aipl", src)], DebugOptions::OFF)
                .expect("load");
        defined_names(&program)
    };
    let first = load();
    assert!(
        first.iter().any(|n| n.contains("LexError")),
        "this test needs builtin files with private items; got {first:?}"
    );
    for _ in 0..16 {
        assert_eq!(load(), first);
    }
}
