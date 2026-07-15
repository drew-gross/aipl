//! Codegen tests that don't fit the cases harness — IR-string assertions,
//! `Error::render` formatting, and ObjectCompilation internals. Everything
//! else (run+check, expect-error cases) lives under `tests/cases/`, grouped
//! into folders by language feature.

use aipl::codegen::{Compilation, ObjectCompilation};
use aipl::{DebugOptions, Error};

/// Parse, first installing the (idempotent) parser hooks the dogfooded
/// section-header / raw-string helpers require — there's no native fallback.
fn parse(src: &str) -> Result<aipl::ast::Program, Error> {
    aipl::install_parser_hooks();
    aipl::parse(src)
}

fn compile(src: &str) -> Compilation {
    let program = parse(src).unwrap();
    Compilation::new(&program, DebugOptions::OFF).unwrap()
}

#[test]
fn empty_program_compiles() {
    // Empty source has no main; cases harness requires one, so this
    // stays as a JIT-only sanity check.
    let _ = compile("");
}

#[test]
fn struct_decl_alone_compiles() {
    let _ = compile("struct Point { x: i64, y: i64 }");
}

#[test]
fn ir_contains_function_body() {
    let comp = compile("fn add(x: i64, y: i64) -> i64 { x + y }");
    let ir = comp.ir();
    assert!(ir.contains("function"), "missing 'function' header: {ir}");
    assert!(ir.contains("i64"), "missing i64 type: {ir}");
    assert!(ir.contains("iadd"), "missing iadd op: {ir}");
}

#[test]
fn render_points_at_offending_expression() {
    let src = "fn f(c: i64) -> i64 { if (c) { 1 } else { 2 } }";
    let program = parse(src).unwrap();
    let Err(err): Result<_, Error> = Compilation::new(&program, DebugOptions::OFF) else {
        panic!("expected error");
    };
    // The `c` in the if condition is at byte 26 → column 27.
    let expected = r"error: if condition: expected bool, got i64
 --> input:1:27
  |
1 | fn f(c: i64) -> i64 { if (c) { 1 } else { 2 } }
  |                           ^";
    assert_eq!(err.render(src, "input"), expected);
}

#[test]
fn render_multiline_source_picks_correct_line() {
    let src = "fn f() -> i64 {\n    bogus\n}";
    let program = parse(src).unwrap();
    let Err(err): Result<_, Error> = Compilation::new(&program, DebugOptions::OFF) else {
        panic!("expected error");
    };
    // `bogus` spans bytes 20..25 → line 2, col 5.
    let expected = r#"error: unknown identifier "bogus"
 --> input:2:5
  |
2 |     bogus
  |     ^^^^^"#;
    assert_eq!(err.render(src, "input"), expected);
}

#[test]
fn unreached_functions_are_dropped() {
    // Lazy seeding starts from `main`: a function it never calls (directly or
    // transitively) is dropped before codegen, so its body never lowers. The
    // distinctive constant `12345` marks `dead`'s body in the IR.
    let comp = compile("fn dead() -> i64 { 12345 } fn main() -> i64 { 7 }");
    let ir = comp.ir();
    assert!(
        !ir.contains("12345"),
        "unreached `dead` should be dropped:\n{ir}"
    );
    assert!(
        ir.contains("7"),
        "reachable `main` should be emitted:\n{ir}"
    );
}

#[test]
fn single_use_private_helper_is_inlined() {
    // A private function called exactly once is inlined into its caller and
    // dropped: only `main` is emitted, with the helper's body folded in.
    let comp = compile(
        "import { wrapping_add as + } from builtins;
         fn helper(x: i64) -> i64 { x + 4242 }
         fn main() -> i64 { helper(1) }",
    );
    let ir = comp.ir();
    assert!(ir.contains("; main"), "main should be emitted:\n{ir}");
    assert!(
        !ir.contains("; helper"),
        "single-use `helper` should be inlined away:\n{ir}"
    );
    assert_eq!(
        ir.matches("function ").count(),
        1,
        "only `main` should remain:\n{ir}"
    );
}

#[test]
fn concat_arg_emits_concat_specialized_instance() {
    // `str + str` lowers to a lazy concat node (`aipl_concat_lazy`), and passing
    // that concat value to a `str` parameter emits a distinct concat-specialized
    // instance (`label$c0`) alongside the plain `label` used for a plain-str arg.
    // Each variant is called twice so the post-monomorphization inlining pass
    // (which folds single-use functions away) keeps both as standalone instances.
    let comp = compile(
        "import { wrapping_add as +, +++ } from builtins;
         fn label(s: str) -> i64 { 0 }
         fn main() -> i64 {
             label(\"abcdefgh\" +++ \"ijklmnop\") + label(\"qrstuvwx\" +++ \"yz012345\")
             + label(\"plainval\") + label(\"another0\")
         }",
    );
    let ir = comp.ir();
    assert!(
        ir.contains("; label$c0"),
        "concat-specialized instance `label$c0` should be emitted:\n{ir}"
    );
    assert!(
        ir.contains("; label\n"),
        "plain `label` instance should be emitted:\n{ir}"
    );
}

#[test]
fn object_compilation_emits_object_file_with_renamed_main() {
    let program = parse("fn main() -> i64 { 42 }").unwrap();
    let comp = ObjectCompilation::new(&program, "test", DebugOptions::OFF, false).unwrap();
    let bytes = comp.emit().unwrap();
    assert!(!bytes.is_empty(), "object emit produced no bytes");
    // Either ELF, Mach-O, or COFF header — we don't care which, just that
    // it's not empty and round-trips through cranelift-object.
    // Symbol-rename check: the user's `main` is exported as the wrapper
    // symbol so the runtime's `int main()` can call into it.
    let needle = aipl::codegen::BINARY_USER_MAIN.as_bytes();
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(
        found,
        "expected {:?} to appear in the object file's symbol table",
        aipl::codegen::BINARY_USER_MAIN
    );
}

#[test]
fn object_compilation_requires_main() {
    let program = parse("fn other() -> i64 { 1 }").unwrap();
    let Err(err) = ObjectCompilation::new(&program, "test", DebugOptions::OFF, false) else {
        panic!("expected error");
    };
    assert!(
        err.message.contains("main"),
        "expected error mentioning main, got: {err}"
    );
}
