//! The compiler dogfoods AIPL by running one checked-in Cranelift IR artifact
//! (`crates/aipl-codegen/src/dogfood.clif`, compiled from every `.aipl` file
//! listed in [`DOGFOOD_SOURCES`]), not by recompiling the dogfooded `.aipl`
//! sources on every build. That decouples "can the compiler run" from "can the
//! compiler compile itself": a mid-change frontend that can't compile the
//! dogfooded sources still links and runs the checked-in IR.
//!
//! This mirrors the `--- performance ---` model in `tests/cases.rs`:
//!   - [`checked_in_ir_is_current`] (normal test) regenerates the artifact from
//!     source via the live frontend and asserts it matches the checked-in
//!     `.clif`. It only passes when the frontend is healthy — a mismatch
//!     mid-iteration is the *intended* signal, not a dogfood-path regression.
//!   - [`fill_dogfood_ir`] (`#[ignore]` author helper) regenerates the artifact,
//!     loads it back and sanity-calls every entry (so we never check in IR that
//!     won't link or run), writes `dogfood.clif`, then fails intentionally so
//!     the regenerated diff is reviewed before committing.
//!
//! Authoring workflow: break the frontend freely (the compiler still runs off the
//! checked-in IR) → fix it → `cargo test --test dogfood_ir -- --ignored
//! fill_dogfood_ir` → full `cargo test` (exercises the new IR end-to-end and this
//! verify test confirms the match) → commit, or revert `dogfood.clif` if
//! anything is off.

use aipl::codegen::{
    generate_dogfood_artifact, read_dogfood_sources, source_refs, Compilation, DOGFOOD_CLIF_FILE,
    DOGFOOD_ENTRIES, DOGFOOD_IR_ENV, DOGFOOD_SOURCE_FILES, FMT_CLIF_FILE, FMT_ENTRIES, FMT_IR_ENV,
    FMT_SOURCE_FILES,
};
use aipl::FfiValue;
use std::path::PathBuf;

const FILL_CMD: &str = "cargo test --test dogfood_ir -- --ignored fill_dogfood_ir";
const FILL_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored fill_staged_ir";
const VALIDATE_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored validate_staged_ir";
const PROMOTE_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored promote_staged_ir";

/// The real validation command: run the whole suite with the compiler itself
/// linking the staged IR (`AIPL_DOGFOOD_IR`), so every parse in the corpus
/// exercises the candidate. The path is **absolute** because the cases harness
/// spawns the compiler as a subprocess whose working directory isn't the repo
/// root — a relative path wouldn't resolve there.
fn validate_staged_corpus_cmd() -> String {
    format!(
        "AIPL_DOGFOOD_IR={} AIPL_FMT_IR={} cargo test",
        staged_path_of(&ARTIFACTS[0]).display(),
        staged_path_of(&ARTIFACTS[1]).display(),
    )
}

/// One checked-in artifact: the sources it is generated from, the FFI entries it
/// must export, its filename, and the env var that overrides it for a staged
/// run. There are two — the parser-hook engine and the formatter engine — linked
/// independently so an ordinary compile never pays to link the walker.
struct Artifact {
    file: &'static str,
    env: &'static str,
    /// The artifact's `.aipl` module names; the sources themselves are read
    /// from disk (`read_dogfood_sources`) at the point of use, so editing one
    /// doesn't rebuild the compiler.
    sources: &'static [&'static str],
    entries: &'static [&'static str],
}

const ARTIFACTS: &[Artifact] = &[
    Artifact {
        file: DOGFOOD_CLIF_FILE,
        env: DOGFOOD_IR_ENV,
        sources: DOGFOOD_SOURCE_FILES,
        entries: DOGFOOD_ENTRIES,
    },
    Artifact {
        file: FMT_CLIF_FILE,
        env: FMT_IR_ENV,
        sources: FMT_SOURCE_FILES,
        entries: FMT_ENTRIES,
    },
];

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/aipl-codegen/src")
}

/// Path to a checked-in `.clif` artifact.
fn artifact_path_of(a: &Artifact) -> PathBuf {
    src_dir().join(a.file)
}

/// Path to a staged (candidate) `.clif.staged` artifact.
fn staged_path_of(a: &Artifact) -> PathBuf {
    src_dir().join(format!("{}.staged", a.file))
}

/// An artifact's override path, if a staged-IR validation run set its env var.
fn ir_override(a: &Artifact) -> Option<PathBuf> {
    match std::env::var(a.env) {
        Ok(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => None,
    }
}

/// The artifact this run should verify against: the env override when set (a
/// staged-IR validation run — the compiler is itself linking that file, so the
/// source-vs-artifact checks must target it too), else the live checked-in one.
fn active_path_of(a: &Artifact) -> PathBuf {
    ir_override(a).unwrap_or_else(|| artifact_path_of(a))
}

/// A dogfood source failed the combined frontend. Pin the offender by parsing
/// each source on its own, then panic with the rendered error and the exact
/// command to iterate on just that file. Falls back to the raw error if no
/// single file reproduces the failure (e.g. a cross-file resolution or codegen
/// error, not a parse error).
fn blame_dogfood_failure(errs: Vec<aipl::Error>) -> ! {
    for (name, src) in read_dogfood_sources(DOGFOOD_SOURCE_FILES) {
        let stripped = aipl::strip_test_sections(&src);
        let Err(e) = aipl::parse(stripped) else {
            continue;
        };
        // Every dogfood source lives under this crate's `src/` directory.
        let rel = format!("crates/aipl-codegen/src/{}", name.trim_start_matches("./"));
        panic!(
            "dogfood source failed to parse:\n{}\n\n\
             To test just this file, run:\n    aipl check {rel}",
            e.render(stripped, &rel),
        );
    }
    panic!("generate dogfood IR:\n{}", aipl::Error::display_all(&errs));
}

/// Generate the unified dogfood artifact via the live frontend.
///
/// Spawns a scoped thread with a 64 MiB stack: some dogfooded `.aipl` files
/// (e.g. `caret_block.aipl`) trigger deep recursion in the compiler that
/// overflows the default test-framework stack (8 MiB on macOS).
fn generate_for(a: &Artifact) -> String {
    for path in a.sources {
        if !path.starts_with("./") {
            panic!("non-relative path: {path:?}")
        }
    }
    let owned = read_dogfood_sources(a.sources);
    let sources = source_refs(&owned);
    let mut result = None;
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(s, || {
                generate_dogfood_artifact(&sources, a.entries)
                    .unwrap_or_else(|e| blame_dogfood_failure(e))
            })
            .expect("spawn scoped thread");
        result = Some(handle.join().expect("generate thread panicked"));
    });
    result.unwrap()
}

/// Normalize line endings so a CRLF checkout (git `autocrlf`) compares equal to
/// the LF-generated text.
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Round-trip sanity: load the artifact through `from_artifact` and call every
/// entry, so `fill` never writes IR that won't link or compute correctly.
/// Entry-level check for one artifact: load it and call its entries with known
/// inputs, so a candidate that links but computes the wrong thing is caught
/// before the full corpus run.
fn sanity_check_of(a: &Artifact, artifact: &str) {
    if a.file == FMT_CLIF_FILE {
        sanity_check_fmt(artifact);
    } else {
        sanity_check(artifact);
    }
}

/// The formatter artifact's single entry. Messy input on purpose, so this proves
/// the walker round-trips rather than merely returning something.
fn sanity_check_fmt(artifact: &str) {
    let comp = Compilation::from_artifact(artifact)
        .unwrap_or_else(|e| panic!("load regenerated {FMT_CLIF_FILE}: {e}"));
    let formatted = comp
        .call_values(
            "format_program",
            &[
                FfiValue::Str("fn  f (  a : i64 )->i64{ a }".to_string()),
                FfiValue::Int(100),
            ],
        )
        .unwrap();
    assert_eq!(
        formatted,
        FfiValue::Res(Ok(Box::new(FfiValue::Str(
            "fn f(a: i64) -> i64 { a }".to_string()
        ))))
    );
}

fn sanity_check(artifact: &str) {
    let comp = Compilation::from_artifact(artifact)
        .unwrap_or_else(|e| panic!("load regenerated {DOGFOOD_CLIF_FILE}: {e}"));

    let span = |start, end| {
        FfiValue::Struct(vec![
            ("start".to_string(), FfiValue::Int(start)),
            ("end".to_string(), FfiValue::Int(end)),
        ])
    };

    // Returns `str?`: a marker is `some(name)`, a non-marker is `none`.
    let marker = comp
        .call_values(
            "parse_test_section_header",
            &[FfiValue::Str("--- performance ---".to_string())],
        )
        .unwrap();
    assert_eq!(
        marker,
        FfiValue::Opt(Some(Box::new(FfiValue::Str("performance".to_string()))))
    );
    let plain = comp
        .call_values(
            "parse_test_section_header",
            &[FfiValue::Str("fn main() {".to_string())],
        )
        .unwrap();
    assert_eq!(plain, FfiValue::Opt(None));

    // Returns the kept prefix (everything before the first marker line).
    let kept = comp
        .call_values(
            "strip_test_sections",
            &[FfiValue::Str("code\n--- stdout ---\nfoo".to_string())],
        )
        .unwrap();
    assert_eq!(kept, FfiValue::Str("code\n".to_string()));
    let whole = comp
        .call_values(
            "strip_test_sections",
            &[FfiValue::Str("fn main() {}\n".to_string())],
        )
        .unwrap();
    assert_eq!(whole, FfiValue::Str("fn main() {}\n".to_string())); // no marker → keep all

    // Returns `Span?`: `some(span)` (the first trailing-ws run's byte range)
    // or `none` when clean. Exercises an optional-of-struct return marshaled
    // back through the dogfood `from_artifact` path.
    let dirty = comp
        .call_values(
            "find_trailing_whitespace",
            &[FfiValue::Str("bad \nok".to_string())],
        )
        .unwrap();
    assert_eq!(dirty, FfiValue::Opt(Some(Box::new(span(3, 4)))));
    let clean = comp
        .call_values(
            "find_trailing_whitespace",
            &[FfiValue::Str("a\nb\nc".to_string())],
        )
        .unwrap();
    assert_eq!(clean, FfiValue::Opt(None));

    // Formats `input:LINE: TEXT` (1-based line, trimmed condition text).
    let loc = comp
        .call_values(
            "assert_loc",
            &[FfiValue::Str("assert(x == 1)".to_string()), span(7, 13)],
        )
        .unwrap();
    assert_eq!(loc, FfiValue::Str("input:1: x == 1".to_string()));
    let loc2 = comp
        .call_values(
            "assert_loc",
            &[FfiValue::Str("a\nassert(y)".to_string()), span(9, 10)],
        )
        .unwrap();
    assert_eq!(loc2, FfiValue::Str("input:2: y".to_string()));

    // Returns the rustc-style location + caret underline block for a span.
    // Third arg is the filename that appears in the ` --> ` line.
    let caret = comp
        .call_values(
            "caret_block",
            &[
                FfiValue::Str("hello world".to_string()),
                span(0, 5),
                FfiValue::Str("input".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(
        caret,
        FfiValue::Str(" --> input:1:1\n  |\n1 | hello world\n  | ^^^^^".to_string())
    );
    // Multi-line source: span on second line.
    let caret_line2 = comp
        .call_values(
            "caret_block",
            &[
                FfiValue::Str("hello\nworld".to_string()),
                span(6, 11),
                FfiValue::Str("input".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(
        caret_line2,
        FfiValue::Str(" --> input:2:1\n  |\n2 | world\n  | ^^^^^".to_string())
    );
    // Filename appears in output when a real path is passed.
    let caret_with_name = comp
        .call_values(
            "caret_block",
            &[
                FfiValue::Str("hello world".to_string()),
                span(0, 5),
                FfiValue::Str("foo.aipl".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(
        caret_with_name,
        FfiValue::Str(" --> foo.aipl:1:1\n  |\n1 | hello world\n  | ^^^^^".to_string())
    );

    // Real file I/O, so stage it under the OS temp dir (never the repo tree)
    // and clean up after.
    let dir = std::env::temp_dir().join(format!(
        "aipl-dogfood-fill-or-add-section-file-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir sanity-check staging");
    let path = dir.join("case.txt");
    std::fs::write(&path, "code\n--- stdout ---\nold\n").expect("write staged file");
    let path_str = path.to_str().expect("utf-8 temp path").to_string();

    let file_result = comp
        .call_values(
            "fill_or_add_section_file",
            &[
                FfiValue::Str(path_str.clone()),
                FfiValue::Str("stdout".to_string()),
                FfiValue::Str("new".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(file_result, FfiValue::Res(Ok(Box::new(FfiValue::Int(0)))));
    let written = std::fs::read_to_string(&path).expect("read back staged file");
    assert_eq!(written, "code\n--- stdout ---\nnew\n");

    // A missing file surfaces the builtin `Error`'s message.
    let missing = dir.join("no_such_file.txt");
    let file_err = comp
        .call_values(
            "fill_or_add_section_file",
            &[
                FfiValue::Str(missing.to_str().unwrap().to_string()),
                FfiValue::Str("stdout".to_string()),
                FfiValue::Str("new".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(
        file_err,
        FfiValue::Res(Err(Box::new(FfiValue::Str(
            "could not read file".to_string()
        ))))
    );

    // Collapses CRLF to LF, then strips the trailing `\n`/`\r` run.
    let normalized = comp
        .call_values(
            "normalize_output",
            &[FfiValue::Str("a\r\nb\r\n".to_string())],
        )
        .unwrap();
    assert_eq!(normalized, FfiValue::Str("a\nb".to_string()));

    // Range-checks a flexible integer literal against a type name; `bool` rides
    // back as `Int(0|1)`. In-range at the edge fits; just over does not; a
    // non-integer name never fits.
    let fits = comp
        .call_values(
            "int_fits",
            &[FfiValue::Int(255), FfiValue::Str("u8".to_string())],
        )
        .unwrap();
    assert_eq!(fits, FfiValue::Int(1));
    let overflows = comp
        .call_values(
            "int_fits",
            &[FfiValue::Int(256), FfiValue::Str("u8".to_string())],
        )
        .unwrap();
    assert_eq!(overflows, FfiValue::Int(0));
    let not_int = comp
        .call_values(
            "int_fits",
            &[FfiValue::Int(0), FfiValue::Str("bool".to_string())],
        )
        .unwrap();
    assert_eq!(not_int, FfiValue::Int(0));

    // Whether a string spells a built-in operator; `bool` rides back as `Int(0|1)`.
    let is_op = comp
        .call_values("is_operator_name", &[FfiValue::Str("==".to_string())])
        .unwrap();
    assert_eq!(is_op, FfiValue::Int(1));
    let not_op = comp
        .call_values("is_operator_name", &[FfiValue::Str("map".to_string())])
        .unwrap();
    assert_eq!(not_op, FfiValue::Int(0));

    // Lexes AIPL source into `LexResult<AiplTok>`: the typed token stream plus
    // the trivia side-channel (comments and `#[allow]` markers). This is the
    // richest entry the artifact serves — a result of a generic struct of
    // arrays of structs whose `kind` field is a variant.
    let tok = |case: &str, payload: Vec<FfiValue>, s, e| {
        FfiValue::Struct(vec![
            (
                "kind".to_string(),
                FfiValue::Variant(case.to_string(), payload),
            ),
            ("span".to_string(), span(s, e)),
        ])
    };
    let lexed = comp
        .call_values(
            "lex_aipl",
            &[FfiValue::Str("let x = 42; // note".to_string())],
        )
        .unwrap();
    assert_eq!(
        lexed,
        FfiValue::Res(Ok(Box::new(FfiValue::Struct(vec![
            (
                "tokens".to_string(),
                FfiValue::Array(vec![
                    tok("Let", vec![], 0, 3),
                    tok("Name", vec![FfiValue::Str("x".to_string())], 4, 5),
                    tok("Eq", vec![], 6, 7),
                    tok("IntLit", vec![FfiValue::Int(42)], 8, 10),
                    tok("Semi", vec![], 10, 11),
                ]),
            ),
            (
                "trivia".to_string(),
                FfiValue::Array(vec![tok("LineComment", vec![], 12, 19)]),
            ),
        ]))))
    );
    // A byte no rule matches is a hard `LexError` with its span.
    let lex_err = comp
        .call_values("lex_aipl", &[FfiValue::Str("@".to_string())])
        .unwrap();
    assert_eq!(
        lex_err,
        FfiValue::Res(Err(Box::new(FfiValue::Struct(vec![
            (
                "message".to_string(),
                FfiValue::Str("unexpected character".to_string()),
            ),
            ("span".to_string(), span(0, 1)),
        ]))))
    );

    // `lex_aipl_stripped` drops trailing `--- section ---` blocks before lexing
    // (one FFI crossing for strip + lex), and kept tokens keep their original
    // spans.
    let stripped = comp
        .call_values(
            "lex_aipl_stripped",
            &[FfiValue::Str("let x = 1\n--- stdout ---\nfoo".to_string())],
        )
        .unwrap();
    assert_eq!(
        stripped,
        FfiValue::Res(Ok(Box::new(FfiValue::Struct(vec![
            (
                "tokens".to_string(),
                FfiValue::Array(vec![
                    tok("Let", vec![], 0, 3),
                    tok("Name", vec![FfiValue::Str("x".to_string())], 4, 5),
                    tok("Eq", vec![], 6, 7),
                    tok("IntLit", vec![FfiValue::Int(1)], 8, 9),
                ]),
            ),
            ("trivia".to_string(), FfiValue::Array(vec![])),
        ]))))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checked_in_ir_is_current() {
    aipl::install_parser_hooks();
    for a in ARTIFACTS {
        let generated = generate_for(a);
        // In a staged-IR validation run the compiler is linking the staged file,
        // so "current with source" must be checked against *that* file — the
        // live `.clif` is intentionally behind until promotion.
        let path = active_path_of(a);
        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing IR {}: {e}\nGenerate it with: {FILL_CMD}",
                path.display()
            )
        });
        // IR is too large to print on error, so don't use assert_eq!
        assert!(
            lf(&generated) == lf(&checked_in),
            "IR {} is stale. Regenerate with: {FILL_CMD}",
            path.display()
        );
    }
}

/// The prebuilt object in this binary must have been built from the checked-in
/// `.clif` — not an older copy of it.
///
/// Ordinary runs execute the dogfood entries straight out of the binary
/// (`Compilation::from_prebuilt`), so a stale object means the compiler is
/// quietly parsing with superseded AIPL while `checked_in_ir_is_current` still
/// reports the artifact as fine. Cargo is supposed to make this impossible —
/// `build.rs` lists both artifacts as `rerun-if-changed` inputs — and this test
/// is here to make the failure loud rather than to distrust it.
///
/// Compares against the *live* artifact even under a staged run: the object was
/// built from the live file, and the staged one deliberately hasn't been
/// promoted yet.
#[test]
fn prebuilt_object_matches_checked_in_ir() {
    for a in ARTIFACTS {
        let path = artifact_path_of(a);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing IR {}: {e}", path.display()));
        let built = aipl::codegen::prebuilt_fingerprint(a.file)
            .unwrap_or_else(|| panic!("no prebuilt object for {}", a.file));
        assert_eq!(
            built,
            aipl::codegen::artifact_fingerprint(&text),
            "the prebuilt object for {} was built from a different version of it \
             than the one checked in. Rebuild (`cargo build`) to pick up the \
             current artifact.",
            a.file
        );
    }
}

/// Every checked-in artifact the compiler runs on must actually load and compute
/// correctly (independent of whether it's byte-current with source — that's
/// `checked_in_ir_is_current`). Targets each artifact's env override when set,
/// so a staged validation run sanity-checks the staged files.
#[test]
fn checked_in_ir_loads_and_runs() {
    aipl::install_parser_hooks();
    for a in ARTIFACTS {
        let path = active_path_of(a);
        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing IR {}: {e}\nGenerate it with: {FILL_CMD}",
                path.display()
            )
        });
        sanity_check_of(a, &checked_in);
    }
}

/// AIPL source whose entry returns the rich shapes the artifact manifest must
/// describe beyond flat structs: a generic struct instance holding arrays of
/// structs whose fields are a variant (with str/i64/char and nullary cases)
/// and a nested struct, under a result whose err side is itself a struct —
/// the exact shape of the dogfooded lexer's `LexResult<AiplTok>!LexError`.
const RICH_TYPES_SRC: &str = r#"
variant Kind = Space | Name(str) | Num(i64) | Ch(char)

struct Pos { start: i64, end: i64 }

struct Tok<K> { kind: K, pos: Pos }

struct Out<K> { tokens: Tok<K>[], trivia: Tok<K>[] }

struct Lerr { message: str, pos: Pos }

pub fn rich(flag: bool) -> Out<Kind>!Lerr {
    if (flag) {
        ok(Out {
            tokens: [
                Tok { kind: Name("hello"), pos: Pos { start: 0, end: 5 } },
                Tok { kind: Num(42), pos: Pos { start: 6, end: 8 } },
                Tok { kind: Ch('x'), pos: Pos { start: 9, end: 12 } },
                Tok { kind: Space, pos: Pos { start: 12, end: 13 } },
            ],
            trivia: [Tok { kind: Name("t"), pos: Pos { start: 1, end: 2 } }],
        })
    } else {
        err(Lerr { message: "boom", pos: Pos { start: 2, end: 3 } })
    }
}
"#;

/// The artifact manifest round-trips variants, arrays, generic-struct
/// instances, and nested structs: an entry returning them computes the same
/// [`FfiValue`] through `from_artifact` (manifest-reconstructed layouts) as
/// through the live-frontend engine, and the values are the expected ones.
#[test]
fn artifact_round_trips_rich_types() {
    aipl::install_parser_hooks();
    let sources: &[(&str, &str)] = &[("./rich.aipl", RICH_TYPES_SRC)];
    let artifact = generate_dogfood_artifact(sources, &["rich"]).unwrap_or_else(|e| {
        panic!(
            "generate rich-types artifact: {}",
            aipl::Error::display_all(&e)
        )
    });
    let comp = Compilation::from_artifact(&artifact)
        .unwrap_or_else(|e| panic!("load rich-types artifact: {e}"));

    let engine = aipl::Engine::compile_sources(sources).expect("frontend-compile rich types");

    let pos = |start, end| {
        FfiValue::Struct(vec![
            ("start".to_string(), FfiValue::Int(start)),
            ("end".to_string(), FfiValue::Int(end)),
        ])
    };
    let tok = |kind, p| FfiValue::Struct(vec![("kind".to_string(), kind), ("pos".to_string(), p)]);
    let expected_ok = FfiValue::Res(Ok(Box::new(FfiValue::Struct(vec![
        (
            "tokens".to_string(),
            FfiValue::Array(vec![
                tok(
                    FfiValue::Variant("Name".to_string(), vec![FfiValue::Str("hello".to_string())]),
                    pos(0, 5),
                ),
                tok(
                    FfiValue::Variant("Num".to_string(), vec![FfiValue::Int(42)]),
                    pos(6, 8),
                ),
                // A `char` payload rides its scalar ABI: `Int` of the codepoint.
                tok(
                    FfiValue::Variant("Ch".to_string(), vec![FfiValue::Int('x' as i64)]),
                    pos(9, 12),
                ),
                tok(FfiValue::Variant("Space".to_string(), vec![]), pos(12, 13)),
            ]),
        ),
        (
            "trivia".to_string(),
            FfiValue::Array(vec![tok(
                FfiValue::Variant("Name".to_string(), vec![FfiValue::Str("t".to_string())]),
                pos(1, 2),
            )]),
        ),
    ]))));
    let expected_err = FfiValue::Res(Err(Box::new(FfiValue::Struct(vec![
        ("message".to_string(), FfiValue::Str("boom".to_string())),
        ("pos".to_string(), pos(2, 3)),
    ]))));

    for (comp_name, ok_val, err_val) in [
        (
            "artifact",
            comp.call_values("rich", &[FfiValue::Int(1)]).unwrap(),
            comp.call_values("rich", &[FfiValue::Int(0)]).unwrap(),
        ),
        (
            "frontend",
            engine.call_values("rich", &[FfiValue::Int(1)]).unwrap(),
            engine.call_values("rich", &[FfiValue::Int(0)]).unwrap(),
        ),
    ] {
        assert_eq!(ok_val, expected_ok, "{comp_name} path, ok case");
        assert_eq!(err_val, expected_err, "{comp_name} path, err case");
    }
}

/// Fails if a `.clif.staged` file is present, signalling a staged IR workflow
/// is in progress. See CLAUDE.md for the full workflow.
///
/// Suppressed during a staged-IR validation run (`AIPL_DOGFOOD_IR` set): that
/// run's whole point is to exercise the corpus against the still-pending staged
/// file, so the pending file is expected — this check would otherwise turn a
/// clean validation run red for the very reason it exists.
#[test]
fn no_staged_ir_pending() {
    for a in ARTIFACTS {
        if ir_override(a).is_some() {
            continue;
        }
        let staged = staged_path_of(a);
        if staged.exists() {
            panic!(
                "staged IR pending for: {}\n\
                 Validate with:  {VALIDATE_STAGED_CMD}\n\
                 Then promote:   {PROMOTE_STAGED_CMD}\n\
                 To abort:       delete the .staged file.",
                staged.display()
            );
        }
    }
}

/// Generate staged (candidate) IR from source — writes `dogfood.clif.staged`
/// next to the live `dogfood.clif`. Sanity-checks the artifact before writing
/// so only working IR is staged. Intentionally fails so the diff is reviewed
/// before promoting.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn fill_staged_ir() {
    aipl::install_parser_hooks();
    for a in ARTIFACTS {
        let artifact = generate_for(a);
        sanity_check_of(a, &artifact);
        let path = staged_path_of(a);
        std::fs::write(&path, &artifact)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {}", path.display());
    }
    panic!(
        "fill_staged_ir wrote staged IR — then validate it by running the whole\n\
         suite with the compiler linking the staged files:\n    {}\n\
         ({VALIDATE_STAGED_CMD} is a faster entry-level pre-check.)\n\
         Once green, promote with: {PROMOTE_STAGED_CMD}",
        validate_staged_corpus_cmd()
    );
}

/// Entry-level pre-check: load `dogfood.clif.staged` and sanity-call every
/// entry, without modifying anything. This is the *fast* gate — it confirms the
/// staged IR links and each entry computes correctly, but it does not exercise
/// the compiler running on it. The real validation is running the whole corpus
/// against the staged IR via [`validate_staged_corpus_cmd`] (the compiler links
/// the staged file through `AIPL_DOGFOOD_IR`), which this message points at.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn validate_staged_ir() {
    for a in ARTIFACTS {
        let path = staged_path_of(a);
        let artifact = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
                path.display()
            )
        });
        sanity_check_of(a, &artifact);
        eprintln!("entry-level check passed for {}.", path.display());
    }
    eprintln!(
        "Now run the full corpus against them:\n    {}",
        validate_staged_corpus_cmd()
    );
}

/// Promote staged IR to live: validates `dogfood.clif.staged`, copies it to the
/// live `dogfood.clif`, then deletes the staged file. Intentionally fails so the
/// resulting diff is reviewed and the suite is re-run before committing.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn promote_staged_ir() {
    for a in ARTIFACTS {
        let staged = staged_path_of(a);
        let artifact = std::fs::read_to_string(&staged).unwrap_or_else(|e| {
            panic!(
                "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
                staged.display()
            )
        });
        sanity_check_of(a, &artifact);
        let live = artifact_path_of(a);
        std::fs::write(&live, &artifact)
            .unwrap_or_else(|e| panic!("write live {}: {e}", live.display()));
        std::fs::remove_file(&staged)
            .unwrap_or_else(|e| panic!("remove staged {}: {e}", staged.display()));
        eprintln!("promoted {} → {}", staged.display(), live.display());
    }
    panic!(
        "promote_staged_ir updated the live .clif files — review the diff,\n\
         then run `cargo test` to confirm the suite is green before committing."
    );
}

#[test]
#[ignore = "author helper — run: cargo test --test dogfood_ir -- --ignored fill_dogfood_ir"]
fn fill_dogfood_ir() {
    aipl::install_parser_hooks();
    for a in ARTIFACTS {
        let artifact = generate_for(a);
        // Never write IR that won't link or run.
        sanity_check_of(a, &artifact);
        let path = artifact_path_of(a);
        std::fs::write(&path, &artifact)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {}", path.display());
    }
    panic!(
        "fill_dogfood_ir regenerated the checked-in IR — review the diff, \
         then re-run the suite normally to confirm it's green."
    );
}
