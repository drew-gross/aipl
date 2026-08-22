//! AIPL — facade crate.
//!
//! The compiler is split across several workspace crates so `cargo build
//! --timings` can attribute build time to each piece (notably the
//! gazelle-macro parser and the cranelift codegen, the two heavy
//! dependencies). This crate re-exports them under the original `aipl::*`
//! paths so downstream code — the CLI and the integration tests — sees a
//! single unified API.

// Core types: spans, errors, debug tracing, and the AST.
pub use aipl_syntax::{ast, DebugOptions, Error, Span};

// Lexer + parser surface.
pub use aipl_parser::{
    companion_files, lex_aipl, lex_aipl_stripped, lex_signatures_and_comments, lex_tokens,
    lex_tokens_and_comments, parse, parse_test_section_header, split_test_sections,
    stage_companions, strip_test_sections, FmtTokenKind, LexedError, LexedOutput, LexedStrStyle,
    LexedToken, LexedTokenKind, TokenKind,
};

// Compiler passes and backends, each re-exported as a module so existing
// `aipl::codegen::…`, `aipl::mono::…`, etc. paths keep resolving.
pub use aipl_codegen as codegen;
pub use aipl_codegen::{parse_spec, FfiValue, SpecFields};
pub use aipl_fmt as fmt;
pub use aipl_linker as binary;
pub use aipl_loader as loader;
pub use aipl_mono as mono;

use std::path::Path;

/// Install the compiler's parser hooks: the dogfooded-AIPL lexer, section
/// stripper, test-section-header parser, trailing-whitespace finder, and the
/// error-render/checker helpers, each run through the FFI (see
/// [`codegen::install_parser_hooks`] for the full list). Call this once before
/// parsing any source — the parser has no native fallback for these. The
/// [`Engine`] constructors do this for you; the CLI and test harnesses call it
/// directly. Idempotent.
pub fn install_parser_hooks() {
    aipl_codegen::install_parser_hooks();
}

/// Embedding FFI: JIT-compile AIPL source and call its functions from Rust.
///
/// An `Engine` owns the JIT-compiled program (code stays mapped for the
/// engine's lifetime). Functions are called by name with `i64` arguments and
/// result — the runtime representation of every scalar AIPL value (`bool` is
/// `0`/`1`, `char` is a codepoint). `str`, arrays, and other composite
/// arguments/returns aren't marshalable across the FFI yet.
///
/// Verified by `doc_example_engine` in `tests/ffi.rs`.
///
/// ```ignore
/// let src = "import { wrapping_add as + } from builtins; pub fn add(a: i64, b: i64) -> i64 { a + b }";
/// let engine = aipl::Engine::compile(src)?;
/// assert_eq!(engine.call("add", &[2, 3])?, 5);
/// # Ok::<(), Vec<aipl::Error>>(())
/// ```
pub struct Engine {
    comp: codegen::Compilation,
}

impl Engine {
    /// Compile AIPL source held in memory. `from "..."` path imports resolve
    /// relative to the current directory; `from builtins` works as usual.
    pub fn compile(source: &str) -> Result<Engine, Vec<Error>> {
        install_parser_hooks();
        let dbg = DebugOptions::new(false);
        let program = loader::load_program_str(source, dbg)?;
        Ok(Engine {
            comp: codegen::Compilation::new(&program, dbg)?,
        })
    }

    /// Compile a set of in-memory virtual files, supplied as `(name, source)`
    /// pairs — typically each `include_str!`'d into the host binary, so the AIPL
    /// is embedded at build time and nothing is read from disk at run time. The
    /// **first** pair is the root (its functions are callable by name); the rest
    /// are reached through `import { f } from "./name.aipl"` clauses, resolved
    /// against the supplied names by *exact match*.
    ///
    /// Each name must be a relative path starting with `./`, spelled the same way
    /// in the import that reaches it — nothing is normalized, and a name without
    /// the prefix is an error. That is also what lets the same sources load
    /// unchanged from disk via [`compile_file`], where the `./` is what makes the
    /// import relative in the first place.
    ///
    /// Verified by `doc_example_compile_sources` in `tests/ffi.rs`.
    ///
    /// ```ignore
    /// // In real use each source is `include_str!("aipl/<name>.aipl")`.
    /// let engine = aipl::Engine::compile_sources(&[
    ///     ("./calc.aipl", "import { square } from \"./mathlib.aipl\";\n\
    ///                      import { wrapping_add as + } from builtins;\n\
    ///                      pub fn sum_of_squares(a: i64, b: i64) -> i64 { square(a) + square(b) }"),
    ///     ("./mathlib.aipl", "import { wrapping_mul as * } from builtins; pub fn square(n: i64) -> i64 { n * n }"),
    /// ])?;
    /// assert_eq!(engine.call("sum_of_squares", &[3, 4])?, 25);
    /// # Ok::<(), Vec<aipl::Error>>(())
    /// ```
    ///
    /// [`compile_file`]: Engine::compile_file
    pub fn compile_sources(sources: &[(&str, &str)]) -> Result<Engine, Vec<Error>> {
        install_parser_hooks();
        let dbg = DebugOptions::new(false);
        let program = loader::load_program_sources(sources, dbg)?;
        Ok(Engine {
            comp: codegen::Compilation::new(&program, dbg)?,
        })
    }

    /// Compile an AIPL file and the files it imports, from disk. This is how a
    /// host keeps AIPL functions across separate `.aipl` files: `path` is the
    /// root/API file (its functions are callable by name via [`call`]); it
    /// reaches helpers in other files through ordinary `import { f } from
    /// "./other.aipl"` clauses, so only the root file's functions form the FFI
    /// surface and helpers are invoked transitively.
    ///
    /// [`call`]: Engine::call
    pub fn compile_file(path: &Path) -> Result<Engine, Vec<Error>> {
        install_parser_hooks();
        let dbg = DebugOptions::new(false);
        let program = loader::load_program(path, dbg)?;
        Ok(Engine {
            comp: codegen::Compilation::new(&program, dbg)?,
        })
    }

    /// Call AIPL function `name` with `i64` arguments, returning its `i64`
    /// result. Errors if the function is missing, isn't a plain user function,
    /// has the wrong arity, or has a non-scalar parameter/return type.
    pub fn call(&self, name: &str, args: &[i64]) -> Result<i64, Error> {
        self.comp.call(name, args)
    }

    /// Call AIPL function `name`, marshaling composites as well as scalars (see
    /// [`FfiValue`]). Each argument's variant must match the parameter type —
    /// `Int` for `i64`/`bool`/`char`, `Str` for `str`, `Array` for an array, and
    /// so on for structs, variants, optionals and results, to any nesting depth —
    /// and the result is marshaled by the function's declared return type. Heap
    /// values the host builds for an argument are borrowed for the duration of
    /// the call.
    ///
    /// Verified by `doc_example_call_values` in `tests/ffi.rs`.
    ///
    /// ```ignore
    /// let src = "import { wrapping_add as +, ==, && } from builtins;\n\
    ///            fn go(a: str, b: str, i: i64) -> i64 {\n\
    ///              match (a[i]) {\n\
    ///                some(x) => match (b[i]) {\n\
    ///                  some(y) => if (x == ' ' && y == ' ') { go(a, b, i + 1) } else { i }, none => i },\n\
    ///                none => i } }\n\
    ///            pub fn common_space_prefix(a: str, b: str) -> i64 { go(a, b, 0) }";
    /// let engine = aipl::Engine::compile(src)?;
    /// use aipl::FfiValue;
    /// let n = engine.call_values(
    ///     "common_space_prefix",
    ///     &[FfiValue::Str("    x".into()), FfiValue::Str("  y".into())],
    /// )?;
    /// assert_eq!(n, FfiValue::Int(2));
    /// # Ok::<(), Vec<aipl::Error>>(())
    /// ```
    pub fn call_values(&self, name: &str, args: &[FfiValue]) -> Result<FfiValue, Error> {
        self.comp.call_values(name, args)
    }
}
