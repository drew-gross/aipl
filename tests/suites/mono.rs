//! Unit tests for the monomorphization pass. These run on ASTs (parse →
//! monomorphize) without the codegen/cases harness, so they're fast. Generic
//! bodies here avoid builtins (whose names are only canonicalized by the
//! loader) — the instantiation logic is driven purely by call-site argument
//! types.

use aipl::ast::{Item, Program};
use aipl::mono::{inline_single_use, monomorphize, use_counts};
use aipl::DebugOptions;
use std::collections::HashMap;

/// Parse, first installing the (idempotent) parser hooks the dogfooded
/// section-header / raw-string helpers require — there's no native fallback.
fn parse(src: &str) -> Result<Program, aipl::Error> {
    aipl::install_parser_hooks();
    aipl::parse(src)
}

/// Sorted function names in a program.
fn fn_names(p: &Program) -> Vec<String> {
    let mut names: Vec<String> = p
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some(f.name.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

/// Sorted function names in a monomorphized program.
fn mono_fn_names(p: &aipl::mono::MonoProgram) -> Vec<String> {
    let mut names: Vec<String> = p.fns.iter().map(|f| f.name.clone()).collect();
    names.sort();
    names
}

fn mono_names(src: &str) -> Vec<String> {
    let prog = parse(src).expect("parse");
    let mono = monomorphize(&prog, DebugOptions::OFF).expect("monomorphize");
    mono_fn_names(&mono)
}

/// Per-function reference counts for `src` (groundwork for inlining). Operates on
/// the parsed AST — no type-checking — so cases need only parse.
fn counts(src: &str) -> HashMap<String, usize> {
    use_counts(&parse(src).expect("parse"))
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
    fn_names(&inline_single_use(&parse(src).expect("parse")))
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
fn inline_keeps_early_return() {
    // An early `return` in the body would escape into the caller if inlined.
    let names = inlined_names(
        "fn helper(x: i64) -> i64 { return x; }
         fn main() -> i64 { helper(5) }",
    );
    assert!(names.contains(&"helper".to_string()));
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
        "fn g(v: any[]) -> i64 { 0 }
         fn main() -> i64 { g([1, 2]) + g(['a']) }",
    );
    // The generic template is dropped; one concrete instance per element type.
    assert_eq!(names, vec!["g$char", "g$i64", "main"]);
}

#[test]
fn same_element_type_is_memoized() {
    let names = mono_names(
        "fn g(v: any[]) -> i64 { 0 }
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
         fn main() -> i64 { let r = f(make_str()); 0 }",
    );
    assert!(names.contains(&"f$str$own0".to_string()), "{names:?}");
}

#[test]
fn concat_arg_makes_distinct_monomorphization() {
    // A `str + str` value carries the concat-str representation; passing it to a
    // `str` parameter emits a distinct, concat-specialized instance (`$c0`)
    // alongside the plain instance used for a plain-str argument.
    let names = mono_names(
        "fn label(s: str) -> str { s }
         fn main() {
             let g = \"a\" +++ \"b\";
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
        "fn g(x: any?) -> i64 { 0 }
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
        "fn f(a: any[], b: any[]) -> i64 { 0 }
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
         fn main() -> i64 { value_or(some(1), 2) + value_or(some('a'), 'b') }",
    );
    assert_eq!(names, vec!["main", "value_or$char", "value_or$i64"]);
}

#[test]
fn named_type_param_inferred_from_other_argument() {
    // A bare `none` for `s: T?` carries no type; `T` is pinned by `d`.
    let names = mono_names(
        "fn value_or<T: any>(s: T?, d: T) -> T { d }
         fn main() -> i64 { value_or(none, 5) }",
    );
    assert_eq!(names, vec!["main", "value_or$i64"]);
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
fn empty_array_arg_pins_any_array_to_pseudo_type() {
    // `any[]` with only an empty-array argument monomorphizes to the
    // `EmptyArray` pseudo-type instance.
    let names = mono_names(
        "fn g(v: any[]) -> i64 { 0 }
         fn main() -> i64 { g([]) }",
    );
    assert_eq!(names, vec!["g$EmptyArray", "main"]);
}

#[test]
fn none_literal_pins_any_optional_to_pseudo_type() {
    // `any?` with only a bare-`none` argument monomorphizes to the
    // `NoneLiteral` pseudo-type instance.
    let names = mono_names(
        "fn is_present(x: any?) -> bool { match (x) { some(v) => true, none => false } }
         fn main() -> i64 { if (is_present(none)) { 1 } else { 0 } }",
    );
    assert_eq!(names, vec!["is_present$NoneLiteral", "main"]);
}

#[test]
fn concrete_arg_still_wins_over_empty_marker() {
    // For multi-arg generics, an empty/`none` argument only fills in a type
    // variable that no concrete argument pinned. Here the second arg pins T
    // to i64, so the instance is `g$i64`, not `g$EmptyArray`.
    let names = mono_names(
        "fn g<T: any>(a: T[], b: T) -> i64 { 0 }
         fn main() -> i64 { g([], 7) }",
    );
    assert_eq!(names, vec!["g$i64", "main"]);
}

#[test]
fn empty_and_none_in_separate_any_params_get_distinct_markers() {
    // Two anonymous `any` params are independent type variables; each is
    // pinned to its own pseudo-type marker.
    let names = mono_names(
        "fn h(xs: any[], x: any?) -> i64 { 0 }
         fn main() -> i64 { h([], none) }",
    );
    assert_eq!(names, vec!["h$EmptyArray$NoneLiteral", "main"]);
}
