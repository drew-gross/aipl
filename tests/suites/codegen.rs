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

/// Compile one source the way a real program is compiled: **through the
/// loader**, which is where imports are resolved. Parsing alone is not enough —
/// `x + y` parses to a call to the name `+`, and only the loader maps that to
/// the implementation the file imported. (It was enough back when an operator
/// was a node of its own, which is exactly what let these tests skip a phase
/// every real compile runs.)
fn compile(src: &str) -> Compilation {
    aipl::install_parser_hooks();
    let program = aipl::loader::load_program_sources(&[("./main.aipl", src)], DebugOptions::OFF)
        .expect("load");
    // A program with a `main` is rooted at it. One without has every function as an entry, since
    // the tests call whichever they define by name.
    let entries = if aipl::codegen::all_functions(&program).contains(&"main") {
        Vec::new()
    } else {
        aipl::codegen::all_functions(&program)
    };
    Compilation::new(&program, DebugOptions::OFF, &entries).unwrap()
}

/// The IR of the source at `path`, or `None` for one that does not compile (an error fixture).
fn ir_of_file(path: &std::path::Path) -> Option<String> {
    aipl::install_parser_hooks();
    let text = std::fs::read_to_string(path).expect("read a case");
    let src = aipl::strip_test_sections(&text);
    let program =
        aipl::loader::load_program_sources(&[("./main.aipl", src)], DebugOptions::OFF).ok()?;
    let entries = if aipl::codegen::all_functions(&program).contains(&"main") {
        Vec::new()
    } else {
        aipl::codegen::all_functions(&program)
    };
    let compilation = Compilation::new(&program, DebugOptions::OFF, &entries).ok()?;
    Some(compilation.ir().to_string())
}

/// What the compiler writes depends on the program alone: the same files compiled by several
/// threads at once, each in a different order, give the IR one thread gives. The optimizations
/// name the binders they introduce, and nothing about those names may follow the schedule.
#[test]
fn concurrent_compiles_write_the_same_ir_as_a_lone_one() {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases/optimizations"),
    )
    .expect("list the optimization cases")
    .map(|entry| entry.expect("a directory entry").path())
    .filter(|path| path.extension().is_some_and(|ext| ext == "aipl"))
    .collect();
    files.sort();
    files.truncate(16);
    let alone: Vec<Option<String>> = files.iter().map(|f| ir_of_file(f)).collect();
    assert!(alone.iter().any(Option::is_some), "no case compiled");

    const THREADS: usize = 4;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|offset| {
                let (files, alone) = (&files, &alone);
                std::thread::Builder::new()
                    .stack_size(256 * 1024 * 1024)
                    .spawn_scoped(scope, move || {
                        for step in 0..files.len() {
                            let at = (step + offset * 5) % files.len();
                            assert_eq!(
                                ir_of_file(&files[at]),
                                alone[at],
                                "{} compiled differently beside other compiles",
                                files[at].display()
                            );
                        }
                    })
                    .expect("spawn a compiling thread")
            })
            .collect();
        for handle in handles {
            handle.join().expect("a compiling thread panicked");
        }
    });
}

/// Run `src`'s `.test` blocks the way `check` does, on this thread, and hand back what they did.
fn run_tests(src: &str) -> aipl::codegen::TestRun {
    aipl::install_parser_hooks();
    let program = aipl::loader::load_program_sources(&[("./main.aipl", src)], DebugOptions::OFF)
        .expect("load");
    let driver = aipl::codegen::binary_entry_program(&aipl::perf::test_driver_as_main(&program))
        .expect("the test driver");
    let comp = Compilation::new(&driver, DebugOptions::OFF, &[]).expect("compile");
    aipl::codegen::set_quiet_summary(true);
    comp.run_cli("main", &[]).expect("run the tests");
    aipl::codegen::take_test_run()
}

/// What a thread's tests did is that thread's own: one running passing tests and another running a
/// failing one, at the same time, each get back their own counts and report.
#[test]
fn threads_running_tests_at_once_keep_their_results_apart() {
    const PASSING: &str = "import { equal as == } from builtins;
        fn a() -> i64 { 1 }.test({ assert(a() == 1); })
        fn b() -> i64 { 2 }.test({ assert(b() == 2); })
        fn c() -> i64 { 3 }.test({ assert(c() == 3); })";
    const FAILING: &str = "import { equal as == } from builtins;
        fn d() -> i64 { 4 }.test({ assert(d() == 5); })
        fn e() -> i64 { 5 }.test({ assert(e() == 5); })";
    let on_thread = |src: &'static str| {
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || (0..10).map(|_| run_tests(src)).collect::<Vec<_>>())
            .expect("spawn a testing thread")
    };
    let (passing, failing) = (on_thread(PASSING), on_thread(FAILING));
    for run in passing.join().expect("the passing thread") {
        assert_eq!((run.total, run.passed, run.failed), (3, 3, 0));
        assert_eq!(run.failure_report, "");
    }
    for run in failing.join().expect("the failing thread") {
        assert_eq!((run.total, run.passed, run.failed), (2, 1, 1));
        assert!(
            run.failure_report
                .starts_with("test d ... FAIL\n  assert failed at "),
            "{}",
            run.failure_report
        );
    }
}

/// A shim installed by one thread's program is not in force for another's.
#[test]
fn a_shim_on_one_thread_does_not_reach_a_program_on_another() {
    const SHIMMED: &str = "import { equal as ==, print } from builtins;
        fn shout() !prints { print(\"x\") }.test({
            mut said: str[] = [];
            shim prints { print = |text| { set said = [text]; }, put = |text| { set said = [text]; } } {
                shout();
            };
            assert(said == [\"x\"]);
        })";
    let on_thread = || {
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| (0..10).map(|_| run_tests(SHIMMED)).collect::<Vec<_>>())
            .expect("spawn a testing thread")
    };
    let threads: Vec<_> = (0..3).map(|_| on_thread()).collect();
    for thread in threads {
        for run in thread.join().expect("a thread") {
            assert_eq!((run.total, run.failed), (1, 0), "{}", run.failure_report);
        }
    }
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
    let comp = compile(
        "import { wrapping_add as + } from builtins;
         fn add(x: i64, y: i64) -> i64 { x + y }",
    );
    let ir = comp.ir();
    assert!(ir.contains("function"), "missing 'function' header: {ir}");
    assert!(ir.contains("i64"), "missing i64 type: {ir}");
    assert!(ir.contains("iadd"), "missing iadd op: {ir}");
}

#[test]
fn render_points_at_offending_expression() {
    let src = "fn f(c: i64) -> i64 { if (c) { 1 } else { 2 } }";
    let program = parse(src).unwrap();
    let Err(errs): Result<_, Vec<Error>> = Compilation::new(
        &program,
        DebugOptions::OFF,
        &aipl::codegen::all_functions(&program),
    ) else {
        panic!("expected error");
    };
    // The `c` in the if condition is at byte 26 → column 27.
    let expected = r"error: if condition: expected bool, got i64
 --> input:1:27
  |
1 | fn f(c: i64) -> i64 { if (c) { 1 } else { 2 } }
  |                           ^";
    assert_eq!(Error::render_all(&errs, src, "input"), expected);
}

#[test]
fn render_multiline_source_picks_correct_line() {
    let src = "fn f() -> i64 {\n    bogus\n}";
    let program = parse(src).unwrap();
    let Err(errs): Result<_, Vec<Error>> = Compilation::new(
        &program,
        DebugOptions::OFF,
        &aipl::codegen::all_functions(&program),
    ) else {
        panic!("expected error");
    };
    // `bogus` spans bytes 20..25 → line 2, col 5.
    let expected = r#"error: unknown identifier "bogus"
 --> input:2:5
  |
2 |     bogus
  |     ^^^^^"#;
    assert_eq!(Error::render_all(&errs, src, "input"), expected);
}

#[test]
fn unreached_functions_are_dropped() {
    // Lazy seeding starts from `main`: a function it never calls (directly or
    // transitively) is dropped before codegen, so its body never lowers. The
    // distinctive constant `12345` marks `dead`'s body in the IR.
    let comp = compile("fn dead() -> i64 { 12345 } #[allow]\nfn main() -> i64 { 7 }");
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
    //
    // `label` has to survive as a real function for there to be an instance to
    // look at, so it is written past both inlining passes: called more than
    // once, which `inline_single_use` requires, and with a body of nine
    // non-leaf expressions, which is past `inline_small`'s threshold of four
    // (`DEFAULT_INLINE_MAX_EXPRS`). A plain `{ 0 }` body would simply be
    // inlined into `main` and no `label` instance of either kind would be
    // emitted. The body reads `s`, so constant folding cannot shrink it back
    // under the threshold for a later round of the pass manager to pick up.
    //
    // It used to earn its keep with an early `return` instead, which the
    // inliner refused outright. That is no longer a reason — a guard clause is
    // rewritten and inlined like anything else — so the size is the reason
    // now, and it is the more honest one: it is a property of the body rather
    // than of a shape the inliner happened not to handle.
    //
    // `rope_str` names the representation: the argument here *is* "a concat-str",
    // so the fixture says that instead of arranging for some operation to build one.
    let comp = compile(
        "import { len, wrapping_add as +, rope_str } from builtins;
         fn label(s: str) -> u64 { s.len() + s.len() + s.len() + s.len() + s.len() }
         fn main() -> u64 {
             label(rope_str(\"abcdefgh\", \"ijklmnop\"))
             + label(rope_str(\"qrstuvwx\", \"yz012345\"))
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
    let Err(errs) = ObjectCompilation::new(&program, "test", DebugOptions::OFF, false) else {
        panic!("expected error");
    };
    assert!(
        errs.iter().any(|e| e.message.contains("main")),
        "expected error mentioning main, got: {errs:?}"
    );
}
