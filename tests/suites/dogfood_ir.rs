//! The compiler dogfoods AIPL by running one checked-in artifact, compiled from
//! every `.aipl` file listed in [`DOGFOOD_SOURCES`], rather than by recompiling
//! those sources on every build. That decouples "can the compiler run" from "can
//! the compiler compile itself": a mid-change frontend that cannot compile the
//! dogfooded sources still links and runs the checked-in artifact.
//!
//! **What is checked in is the compiled object**, `dogfood.o`, beside
//! `dogfood.manifest` — the `;`-comment header carrying the FFI signatures and
//! struct layouts the runtime marshals against, plus a `; source-fingerprint`
//! line naming the IR it was built from. The Cranelift IR itself is a
//! *working-tree intermediate*: it is what the artifact is generated as, and
//! what the staged workflow JITs to validate a candidate, but it is not stored.
//! That keeps 40 MB of generated text out of every revision (the object is
//! ~7 MB) and means no build has to lower it.
//!
//! This mirrors the `--- performance ---` model in `tests/cases.rs`:
//!   - [`checked_in_ir_is_current`] (normal test) regenerates the IR from source
//!     via the live frontend and checks its fingerprint against the one the
//!     manifest records. It only passes when the frontend is healthy — a
//!     mismatch mid-iteration is the *intended* signal, not a dogfood-path
//!     regression.
//!   - [`fill_dogfood_ir`] (`#[ignore]` author helper) regenerates the IR, loads
//!     it back and sanity-calls every entry (so we never check in an artifact
//!     that won't link or run), compiles and writes `dogfood.o` plus its
//!     manifest, then fails intentionally so the result is reviewed before
//!     committing.
//!
//! Authoring workflow: break the frontend freely (the compiler still runs off the
//! checked-in object) → fix it → `cargo test --test dogfood --
//! --ignored dogfood_ir::fill_dogfood_ir` → full `cargo test` (exercises the new
//! artifact end-to-end and this verify test confirms the match) → commit, or
//! revert `dogfood.o` and `dogfood.manifest` if anything is off.

use aipl::codegen::{
    emit_object, generate_dogfood_artifact, manifest_with_fingerprint, read_dogfood_sources,
    source_refs, ArtifactFiles, Compilation, DOGFOOD, DOGFOOD_CLIF_FILE, DOGFOOD_ENTRIES,
    DOGFOOD_IR_ENV, DOGFOOD_SOURCE_FILES,
};
use aipl::FfiValue;
use std::path::PathBuf;

const FILL_CMD: &str = "cargo test --test dogfood -- --ignored dogfood_ir::fill_dogfood_ir";
const FILL_STAGED_CMD: &str = "cargo test --test dogfood -- --ignored dogfood_ir::fill_staged_ir";
const VALIDATE_STAGED_CMD: &str =
    "cargo test --test dogfood -- --ignored dogfood_ir::validate_staged_ir";
const PROMOTE_STAGED_CMD: &str =
    "cargo test --test dogfood -- --ignored dogfood_ir::promote_staged_ir";

/// The real validation command: run the whole suite with the compiler itself
/// linking the staged IR (`AIPL_DOGFOOD_IR`), so every parse in the corpus
/// exercises the candidate. The path is **absolute** because the cases harness
/// spawns the compiler as a subprocess whose working directory isn't the repo
/// root — a relative path wouldn't resolve there.
fn validate_staged_corpus_cmd() -> String {
    format!(
        "AIPL_DOGFOOD_IR={} cargo test",
        staged_path_of(&ARTIFACTS[0]).display(),
    )
}

/// One checked-in artifact: the sources it is generated from, the FFI entries it
/// must export, its filename, and the env var that overrides it for a staged
/// run. There is one — the formatter used to be a second, linked separately so
/// an ordinary compile never paid to link the walker, until the artifact became
/// a prebuilt object and nothing was linked at run time any more. The tests
/// still loop over the list, so a second costs an entry here and nothing else.
struct Artifact {
    file: &'static str,
    /// The compiled files this artifact is checked in as — the object the build
    /// links, and the manifest header beside it. The `.clif` the two are
    /// generated from is a working-tree intermediate, not a checked-in file.
    files: ArtifactFiles,
    env: &'static str,
    /// The artifact's `.aipl` module names; the sources themselves are read
    /// from disk (`read_dogfood_sources`) at the point of use, so editing one
    /// doesn't rebuild the compiler.
    sources: &'static [&'static str],
    entries: &'static [&'static str],
}

const ARTIFACTS: &[Artifact] = &[Artifact {
    file: DOGFOOD_CLIF_FILE,
    files: DOGFOOD,
    env: DOGFOOD_IR_ENV,
    sources: DOGFOOD_SOURCE_FILES,
    entries: DOGFOOD_ENTRIES,
}];

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/aipl-codegen/src")
}

/// Path to a staged (candidate) `.clif.staged` artifact.
fn staged_path_of(a: &Artifact) -> PathBuf {
    src_dir().join(format!("{}.staged", a.file))
}

/// Path to the checked-in compiled object.
fn object_path_of(a: &Artifact) -> PathBuf {
    src_dir().join(a.files.object)
}

/// Path to the checked-in manifest header.
fn manifest_path_of(a: &Artifact) -> PathBuf {
    src_dir().join(a.files.manifest)
}

/// Compile `text` and write the two checked-in files, returning whether either
/// changed.
///
/// "Changed" is compared on content rather than assumed, because the object's
/// mtime is a build input: rewriting identical bytes would re-archive it,
/// relink every test binary and — on macOS — queue each for Gatekeeper's
/// first-exec scan, which is ten minutes of nothing. The gate reads the
/// `(unchanged)` this produces and skips its rebuild step.
fn write_compiled_of(a: &Artifact, text: &str) -> bool {
    let (bytes, header) = emit_object(text, a.files.name, a.files.prefix)
        .unwrap_or_else(|e| panic!("compile {}: {e}", a.file));
    let manifest = manifest_with_fingerprint(&header, text);
    let obj_path = object_path_of(a);
    let man_path = manifest_path_of(a);
    let obj_same = std::fs::read(&obj_path).is_ok_and(|old| old == bytes);
    let man_same = std::fs::read_to_string(&man_path).is_ok_and(|old| lf(&old) == lf(&manifest));
    if !obj_same {
        std::fs::write(&obj_path, &bytes)
            .unwrap_or_else(|e| panic!("write {}: {e}", obj_path.display()));
    }
    if !man_same {
        std::fs::write(&man_path, &manifest)
            .unwrap_or_else(|e| panic!("write {}: {e}", man_path.display()));
    }
    !(obj_same && man_same)
}

/// An artifact's override path, if a staged-IR validation run set its env var.
fn ir_override(a: &Artifact) -> Option<PathBuf> {
    match std::env::var(a.env) {
        Ok(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => None,
    }
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
/// entry with known inputs, so `fill` never writes IR that won't link or
/// compute correctly, and a candidate that links but computes the wrong thing
/// is caught before the full corpus run.
fn sanity_check_of(a: &Artifact, artifact: &str) {
    let comp = Compilation::from_artifact(artifact)
        .unwrap_or_else(|e| panic!("load regenerated {}: {e}", a.file));
    sanity_check_entries(a, &comp);
}

/// [`sanity_check_of`] against an already-built engine, whichever way it was
/// built — a JIT link of candidate text, or the object linked into this binary.
fn sanity_check_entries(_a: &Artifact, comp: &Compilation) {
    // The formatter, on messy input on purpose, so this proves the walker
    // round-trips rather than merely returning something — and with a trailing
    // section, which the pipeline carries through untouched.
    let formatted = comp
        .call_values(
            "format_source",
            &[
                FfiValue::Str("fn  f (  a : i64 )->i64{ a }\n--- stdout ---\nx  \n".to_string()),
                FfiValue::Int(100),
            ],
        )
        .unwrap();
    assert_eq!(
        formatted,
        FfiValue::Res(Ok(Box::new(FfiValue::Str(
            "fn f(a: i64) -> i64 { a }\n--- stdout ---\nx  \n".to_string()
        ))))
    );

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

    // The parser: a whole file to an AST, with its `#[allow]` spans. Rebuilt
    // through the same bridge the compiler uses, so this proves the artifact's
    // parser and the Rust-side reconstruction agree, not merely that a call
    // returns.
    let parsed = comp
        .call_values(
            "aipl_parse_file",
            &[FfiValue::Str(
                "fn f(x: i64) -> i64 { x } #[allow]\n".to_string(),
            )],
        )
        .unwrap();
    let (program, allows) = aipl::ffi_ast::parse_file_from_ffi(&parsed)
        .unwrap_or_else(|e| panic!("dogfooded aipl_parse_file did not rebuild: {e}"));
    assert_eq!(program.items.len(), 1);
    assert_eq!(allows, vec![26..34]);
    let refused = comp
        .call_values("aipl_parse_file", &[FfiValue::Str("fn f( {".to_string())])
        .unwrap();
    assert!(
        aipl::ffi_ast::parse_file_from_ffi(&refused).is_err(),
        "a syntax error must come back as one"
    );

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
    // A `Token<AiplTok>`: its kind, plus the `SpanStr` carrying the matched text
    // and the span it covers. The text is sliced out of `src` rather than
    // written as a literal, so an expectation cannot claim text the source does
    // not hold at that span — the invariant `span_str` establishes in
    // `lexer.aipl` is the one this mirrors.
    let tok = |src: &str, case: &str, payload: Vec<FfiValue>, s: usize, e: usize| {
        FfiValue::Struct(vec![
            (
                "kind".to_string(),
                FfiValue::Variant(case.to_string(), payload),
            ),
            (
                "text".to_string(),
                FfiValue::Struct(vec![
                    ("text".to_string(), FfiValue::Str(src[s..e].to_string())),
                    ("span".to_string(), span(s as i64, e as i64)),
                ]),
            ),
        ])
    };
    let lex_src = "let x = 42; // note";
    let lexed = comp
        .call_values("lex_aipl", &[FfiValue::Str(lex_src.to_string())])
        .unwrap();
    assert_eq!(
        lexed,
        FfiValue::Res(Ok(Box::new(FfiValue::Struct(vec![
            (
                "tokens".to_string(),
                FfiValue::Array(vec![
                    tok(lex_src, "Let", vec![], 0, 3),
                    tok(lex_src, "Name", vec![FfiValue::Str("x".to_string())], 4, 5),
                    tok(lex_src, "Eq", vec![], 6, 7),
                    tok(lex_src, "IntLit", vec![FfiValue::Int(42)], 8, 10),
                    tok(lex_src, "Semi", vec![], 10, 11),
                ]),
            ),
            (
                "trivia".to_string(),
                FfiValue::Array(vec![tok(lex_src, "LineComment", vec![], 12, 19)]),
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
    let strip_src = "let x = 1\n--- stdout ---\nfoo";
    let stripped = comp
        .call_values("lex_aipl_stripped", &[FfiValue::Str(strip_src.to_string())])
        .unwrap();
    assert_eq!(
        stripped,
        FfiValue::Res(Ok(Box::new(FfiValue::Struct(vec![
            (
                "tokens".to_string(),
                FfiValue::Array(vec![
                    tok(strip_src, "Let", vec![], 0, 3),
                    tok(
                        strip_src,
                        "Name",
                        vec![FfiValue::Str("x".to_string())],
                        4,
                        5
                    ),
                    tok(strip_src, "Eq", vec![], 6, 7),
                    tok(strip_src, "IntLit", vec![FfiValue::Int(1)], 8, 9),
                ]),
            ),
            ("trivia".to_string(), FfiValue::Array(vec![])),
        ]))))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The checked-in artifact must still be what the AIPL sources compile to.
///
/// The IR itself is not checked in — the object is — so this compares
/// *fingerprints* rather than text: regenerate the IR in memory, hash it, and
/// check it against the `; source-fingerprint` line the manifest carries. That
/// is the same invariant the old text comparison enforced, without keeping 40 MB
/// of generated IR in the repository. Regeneration is deterministic, which is
/// what makes a hash comparison sound; a mismatch means the sources moved.
#[test]
fn checked_in_ir_is_current() {
    aipl::install_parser_hooks();
    for a in ARTIFACTS {
        let generated = generate_for(a);
        // In a staged-IR validation run the compiler is linking the staged
        // `.clif`, so "current with source" must be checked against *that* —
        // the checked-in object is intentionally behind until promotion.
        if let Some(staged) = ir_override(a) {
            let text = std::fs::read_to_string(&staged).unwrap_or_else(|e| {
                panic!(
                    "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
                    staged.display()
                )
            });
            // IR is too large to print on error, so don't use assert_eq!
            assert!(
                lf(&generated) == lf(&text),
                "staged IR {} is stale. Regenerate with: {FILL_STAGED_CMD}",
                staged.display()
            );
            continue;
        }
        let path = manifest_path_of(a);
        let manifest = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing manifest {}: {e}\nGenerate it with: {FILL_CMD}",
                path.display()
            )
        });
        let recorded = aipl::codegen::source_fingerprint(&manifest).unwrap_or_else(|| {
            panic!(
                "{} carries no source fingerprint. Regenerate with: {FILL_CMD}",
                path.display()
            )
        });
        assert_eq!(
            aipl::codegen::artifact_fingerprint(&generated),
            recorded,
            "the checked-in artifact is stale — the AIPL sources no longer compile \
             to the IR {} was built from. Regenerate with: {FILL_CMD}",
            a.files.object
        );
    }
}

/// The object linked into this binary must be the one checked in — not an
/// older copy of it.
///
/// Ordinary runs execute the dogfood entries straight out of the binary
/// (`Compilation::from_prebuilt`), so a stale link means the compiler is quietly
/// parsing with superseded AIPL while `checked_in_ir_is_current` still reports
/// the artifact as fine. Cargo is supposed to make this impossible — `build.rs`
/// lists the object and its manifest as `rerun-if-changed` inputs — and this
/// test is here to make the failure loud rather than to distrust it.
///
/// Compares against the *checked-in* manifest even under a staged run: the
/// linked object came from it, and the staged candidate deliberately hasn't been
/// promoted yet.
#[test]
fn prebuilt_object_matches_checked_in_ir() {
    for a in ARTIFACTS {
        let path = manifest_path_of(a);
        let manifest = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing manifest {}: {e}", path.display()));
        let checked_in = aipl::codegen::source_fingerprint(&manifest)
            .unwrap_or_else(|| panic!("{} carries no source fingerprint", path.display()));
        let linked = aipl::codegen::prebuilt_fingerprint(a.files.name)
            .unwrap_or_else(|| panic!("no prebuilt object for {}", a.files.name));
        assert_eq!(
            linked, checked_in,
            "the object linked into this binary is not the one checked in as {}. \
             Rebuild (`cargo build`) to pick up the current artifact.",
            a.files.object
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
        // The engine this process would really use: the linked object normally,
        // or the JIT of the staged `.clif` under a validation run. Stronger than
        // the text round-trip this used to do — it exercises the artifact as
        // shipped rather than a fresh compile of something equal to it.
        sanity_check_entries(a, &aipl::codegen::dogfood_compilation());
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
/// beside the checked-in object. Sanity-checks it before writing so only working
/// IR is staged. Intentionally fails so the candidate is validated before
/// promoting.
///
/// The staged candidate stays *IR*, not an object: the corpus validation run
/// points `AIPL_DOGFOOD_IR` at it and the compiler JITs it, which is the whole
/// point of staging — the object in the binary was built from the live artifact
/// and cannot speak for a candidate.
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

/// Promote staged IR to live: validates `dogfood.clif.staged`, *compiles* it to
/// the checked-in `dogfood.o` and manifest, then deletes the staged file.
/// Intentionally fails so the result is reviewed and the suite re-run before
/// committing.
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
        // Promotion is a *compile*: the staged `.clif` is the candidate, and
        // what gets checked in is the object it lowers to plus the manifest
        // header beside it. The IR itself is not stored — see `write_compiled_of`
        // and the manifest's `source-fingerprint` line, which is what still ties
        // the artifact to its AIPL sources.
        let changed = write_compiled_of(a, &artifact);
        std::fs::remove_file(&staged)
            .unwrap_or_else(|e| panic!("remove staged {}: {e}", staged.display()));
        let live = object_path_of(a);
        if changed {
            eprintln!("promoted {} → {}", staged.display(), live.display());
        } else {
            eprintln!(
                "promoted {} → {} (unchanged: identical to the live artifact)",
                staged.display(),
                live.display()
            );
        }
    }
    panic!(
        "promote_staged_ir updated the checked-in artifact — review the diff,\n\
         then run `cargo test` to confirm the suite is green before committing."
    );
}

#[test]
#[ignore = "author helper — run: cargo test --test dogfood -- --ignored dogfood_ir::fill_dogfood_ir"]
fn fill_dogfood_ir() {
    aipl::install_parser_hooks();
    for a in ARTIFACTS {
        let artifact = generate_for(a);
        // Never write an artifact that won't link or run.
        sanity_check_of(a, &artifact);
        write_compiled_of(a, &artifact);
        eprintln!("wrote {}", object_path_of(a).display());
    }
    panic!(
        "fill_dogfood_ir regenerated the checked-in artifact — review the diff, \
         then re-run the suite normally to confirm it's green."
    );
}
