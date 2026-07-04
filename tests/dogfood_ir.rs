//! The compiler dogfoods AIPL by running *checked-in Cranelift IR* (the
//! `crates/aipl-codegen/src/*.clif` artifacts), not by recompiling the dogfooded
//! `.aipl` sources on every build. That decouples "can the compiler run" from
//! "can the compiler compile itself": a mid-change frontend that can't compile
//! the dogfooded sources still links and runs the checked-in IR.
//!
//! This mirrors the `--- performance ---` model in `tests/cases.rs`:
//!   - [`checked_in_ir_is_current`] (normal test) regenerates each artifact from
//!     source via the live frontend and asserts it matches the checked-in
//!     `.clif`. It only passes when the frontend is healthy — a mismatch
//!     mid-iteration is the *intended* signal, not a dogfood-path regression.
//!   - [`fill_dogfood_ir`] (`#[ignore]` author helper) regenerates each artifact,
//!     loads it back and sanity-calls the entries (so we never check in IR that
//!     won't link or run), writes the `.clif` files, then fails intentionally so
//!     the regenerated diff is reviewed before committing.
//!
//! Authoring workflow: break the frontend freely (the compiler still runs off the
//! checked-in IR) → fix it → `cargo test --test dogfood_ir -- --ignored
//! fill_dogfood_ir` → full `cargo test` (exercises the new IR end-to-end and this
//! verify test confirms the match) → commit, or revert the `.clif` if anything is
//! off.

use aipl::codegen::{dogfood_engines, generate_dogfood_artifact, Compilation, DogfoodEngine};
use aipl::FfiValue;
use std::path::PathBuf;

const FILL_CMD: &str = "cargo test --test dogfood_ir -- --ignored fill_dogfood_ir";
const FILL_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored fill_staged_ir";
const VALIDATE_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored validate_staged_ir";
const PROMOTE_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored promote_staged_ir";

/// Path to a dogfood engine's checked-in `.clif` artifact.
fn artifact_path(engine: &DogfoodEngine) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/aipl-codegen/src")
        .join(engine.clif_file)
}

/// Path to the staged (candidate) `.clif.staged` artifact for an engine.
fn staged_artifact_path(engine: &DogfoodEngine) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/aipl-codegen/src")
        .join(format!("{}.staged", engine.clif_file))
}

/// Generate the artifact for one engine via the live frontend.
///
/// Spawns a scoped thread with a 64 MiB stack: some dogfooded `.aipl` files
/// (e.g. `caret_block.aipl`) trigger deep recursion in the compiler that
/// overflows the default test-framework stack (8 MiB on macOS).
fn generate(engine: &DogfoodEngine) -> String {
    let mut result = None;
    for (path, _) in engine.sources {
        if !path.starts_with("./") {
            panic!("non-relative path: {path:?}")
        }
    }
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(s, || {
                generate_dogfood_artifact(engine.sources, engine.entries)
                    .unwrap_or_else(|e| panic!("generate dogfood IR for {}: {e}", engine.clif_file))
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

/// Round-trip sanity: load an artifact through `from_artifact` and call its
/// entries, so `fill` never writes IR that won't link or compute correctly.
fn sanity_check(engine: &DogfoodEngine, artifact: &str) {
    let comp = Compilation::from_artifact(artifact)
        .unwrap_or_else(|e| panic!("load regenerated {}: {e}", engine.clif_file));
    match engine.clif_file {
        "add.clif" => {
            assert_eq!(comp.call("add", &[2, 3]).unwrap(), 5);
            assert_eq!(comp.call("add", &[-7, -8]).unwrap(), -15);
            assert_eq!(comp.call("add", &[8, 24]).unwrap(), 32);
        }
        "process_raw_string.clif" => {
            let out = comp
                .call_values(
                    "process_raw_string",
                    &[FfiValue::Str("\n    a\n    b\n    ".to_string())],
                )
                .unwrap();
            assert_eq!(out, FfiValue::Str("a\nb".to_string()));
        }
        "parse_test_section_header.clif" => {
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
        }
        "strip_test_sections.clif" => {
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
        }
        "find_trailing_whitespace.clif" => {
            // Returns `Span?`: `some(span)` (the first trailing-ws run's byte range)
            // or `none` when clean. Exercises an optional-of-struct return marshaled
            // back through the dogfood `from_artifact` path.
            let dirty = comp
                .call_values(
                    "find_trailing_whitespace",
                    &[FfiValue::Str("bad \nok".to_string())],
                )
                .unwrap();
            assert_eq!(
                dirty,
                FfiValue::Opt(Some(Box::new(FfiValue::Struct(vec![
                    ("start".to_string(), FfiValue::Int(3)),
                    ("end".to_string(), FfiValue::Int(4)),
                ]))))
            );
            let clean = comp
                .call_values(
                    "find_trailing_whitespace",
                    &[FfiValue::Str("a\nb\nc".to_string())],
                )
                .unwrap();
            assert_eq!(clean, FfiValue::Opt(None));
        }
        "line_at.clif" => {
            // Returns `LineAt { line, line_start, line_end }`: the 0-based line
            // index, byte offset of the line's first byte, and byte offset of the
            // line's end (the '\n' terminator or source.len() for the last line).
            let result = comp
                .call_values(
                    "line_at",
                    &[FfiValue::Str("hello\nworld".to_string()), FfiValue::Int(6)],
                )
                .unwrap();
            assert_eq!(
                result,
                FfiValue::Struct(vec![
                    ("line".to_string(), FfiValue::Int(1)),
                    ("line_start".to_string(), FfiValue::Int(6)),
                    ("line_end".to_string(), FfiValue::Int(11)),
                ])
            );
            // Offset 0 always returns line 0, line_start 0.
            let first = comp
                .call_values(
                    "line_at",
                    &[FfiValue::Str("abc".to_string()), FfiValue::Int(0)],
                )
                .unwrap();
            assert_eq!(
                first,
                FfiValue::Struct(vec![
                    ("line".to_string(), FfiValue::Int(0)),
                    ("line_start".to_string(), FfiValue::Int(0)),
                    ("line_end".to_string(), FfiValue::Int(3)),
                ])
            );
        }
        "caret_block.clif" => {
            // Returns the rustc-style location + caret underline block for a span.
            // Third arg is the filename that appears in the ` --> ` line.
            let span = |start, end| {
                FfiValue::Struct(vec![
                    ("start".to_string(), FfiValue::Int(start)),
                    ("end".to_string(), FfiValue::Int(end)),
                ])
            };
            let result = comp
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
                result,
                FfiValue::Str(" --> input:1:1\n  |\n1 | hello world\n  | ^^^^^".to_string())
            );
            // Multi-line source: span on second line.
            let line2 = comp
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
                line2,
                FfiValue::Str(" --> input:2:1\n  |\n2 | world\n  | ^^^^^".to_string())
            );
            // Filename appears in output when a real path is passed.
            let with_name = comp
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
                with_name,
                FfiValue::Str(" --> foo.aipl:1:1\n  |\n1 | hello world\n  | ^^^^^".to_string())
            );
        }
        "fill_section.clif" => {
            // Replaces an existing section's body, leaving the rest untouched.
            let result = comp
                .call_values(
                    "fill_section",
                    &[
                        FfiValue::Str("a\n--- foo ---\nold\n--- bar ---\nb\n".to_string()),
                        FfiValue::Str("foo".to_string()),
                        FfiValue::Str("new".to_string()),
                    ],
                )
                .unwrap();
            assert_eq!(
                result,
                FfiValue::Str("a\n--- foo ---\nnew\n--- bar ---\nb\n".to_string())
            );
        }
        "fill_or_add_section.clif" => {
            // Section missing: appended after stripping trailing newlines.
            let result = comp
                .call_values(
                    "fill_or_add_section",
                    &[
                        FfiValue::Str("code\n".to_string()),
                        FfiValue::Str("stdout".to_string()),
                        FfiValue::Str("hi".to_string()),
                    ],
                )
                .unwrap();
            assert_eq!(
                result,
                FfiValue::Str("code\n--- stdout ---\nhi\n".to_string())
            );
            // Section present: same behavior as `fill_section`.
            let replaced = comp
                .call_values(
                    "fill_or_add_section",
                    &[
                        FfiValue::Str("a\n--- foo ---\nold\n".to_string()),
                        FfiValue::Str("foo".to_string()),
                        FfiValue::Str("new".to_string()),
                    ],
                )
                .unwrap();
            assert_eq!(replaced, FfiValue::Str("a\n--- foo ---\nnew\n".to_string()));
        }
        other => panic!("no sanity check defined for dogfood engine {other}"),
    }
}

#[test]
fn checked_in_ir_is_current() {
    aipl::install_parser_hooks();
    for engine in dogfood_engines() {
        let generated = generate(&engine);
        let path = artifact_path(&engine);
        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing checked-in dogfood IR {}: {e}\nGenerate it with: {FILL_CMD}",
                path.display()
            )
        });
        assert_eq!(
            lf(&generated),
            lf(&checked_in),
            "checked-in dogfood IR {} is stale. Regenerate with: {FILL_CMD}",
            path.display()
        );
    }
}

/// The checked-in IR must actually load and compute correctly (independent of
/// whether it's byte-current with source — that's `checked_in_ir_is_current`).
#[test]
fn checked_in_ir_loads_and_runs() {
    aipl::install_parser_hooks();
    for engine in dogfood_engines() {
        let path = artifact_path(&engine);
        let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing checked-in dogfood IR {}: {e}\nGenerate it with: {FILL_CMD}",
                path.display()
            )
        });
        sanity_check(&engine, &checked_in);
    }
}

/// Fails if any `.clif.staged` file is present, signalling a staged IR
/// workflow is in progress. See CLAUDE.md for the full workflow.
#[test]
fn no_staged_ir_pending() {
    let pending: Vec<_> = dogfood_engines()
        .into_iter()
        .filter(|e| staged_artifact_path(e).exists())
        .map(|e| format!("{}.staged", e.clif_file))
        .collect();
    if !pending.is_empty() {
        panic!(
            "staged IR pending for: {}\n\
             Validate with:  {VALIDATE_STAGED_CMD}\n\
             Then promote:   {PROMOTE_STAGED_CMD}\n\
             To abort:       delete the .staged files.",
            pending.join(", ")
        );
    }
}

/// Generate staged (candidate) IR from source — writes `*.clif.staged` files
/// next to the live `*.clif` files. Sanity-checks each artifact before writing
/// so only working IR is staged. Intentionally fails so the diff is reviewed
/// before promoting.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn fill_staged_ir() {
    aipl::install_parser_hooks();
    for engine in dogfood_engines() {
        let artifact = generate(&engine);
        sanity_check(&engine, &artifact);
        let path = staged_artifact_path(&engine);
        std::fs::write(&path, &artifact)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {}", path.display());
    }
    panic!(
        "fill_staged_ir wrote staged IR — review the diff vs the live .clif files,\n\
         then validate with: {VALIDATE_STAGED_CMD}\n\
         then promote with:  {PROMOTE_STAGED_CMD}"
    );
}

/// Load and sanity-check each `*.clif.staged` file without modifying anything.
/// Run after `fill_staged_ir` to confirm staged IR loads and computes correctly
/// before promoting it to live.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn validate_staged_ir() {
    for engine in dogfood_engines() {
        let path = staged_artifact_path(&engine);
        let artifact = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
                path.display()
            )
        });
        sanity_check(&engine, &artifact);
        eprintln!("validated {}", path.display());
    }
}

/// Promote staged IR to live: validates each `*.clif.staged`, copies it to the
/// live `*.clif`, then deletes the staged file. Intentionally fails so the
/// resulting diff is reviewed and the suite is re-run before committing.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn promote_staged_ir() {
    for engine in dogfood_engines() {
        let staged = staged_artifact_path(&engine);
        let artifact = std::fs::read_to_string(&staged).unwrap_or_else(|e| {
            panic!(
                "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
                staged.display()
            )
        });
        sanity_check(&engine, &artifact);
        let live = artifact_path(&engine);
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
    for engine in dogfood_engines() {
        let artifact = generate(&engine);
        // Never write IR that won't link or run.
        sanity_check(&engine, &artifact);
        let path = artifact_path(&engine);
        std::fs::write(&path, &artifact)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {}", path.display());
    }
    panic!(
        "fill_dogfood_ir regenerated the checked-in dogfood IR — review the diff, \
         then re-run the suite normally to confirm it's green."
    );
}
