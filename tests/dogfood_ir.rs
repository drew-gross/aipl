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

/// Path to a dogfood engine's checked-in `.clif` artifact.
fn artifact_path(engine: &DogfoodEngine) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/aipl-codegen/src")
        .join(engine.clif_file)
}

/// Generate the artifact for one engine via the live frontend.
fn generate(engine: &DogfoodEngine) -> String {
    generate_dogfood_artifact(engine.sources, engine.entries)
        .unwrap_or_else(|e| panic!("generate dogfood IR for {}: {e}", engine.clif_file))
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
