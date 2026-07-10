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
    generate_dogfood_artifact, Compilation, DOGFOOD_CLIF_FILE, DOGFOOD_ENTRIES, DOGFOOD_SOURCES,
};
use aipl::FfiValue;
use std::path::PathBuf;

const FILL_CMD: &str = "cargo test --test dogfood_ir -- --ignored fill_dogfood_ir";
const FILL_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored fill_staged_ir";
const VALIDATE_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored validate_staged_ir";
const PROMOTE_STAGED_CMD: &str = "cargo test --test dogfood_ir -- --ignored promote_staged_ir";

/// Path to the checked-in `.clif` artifact.
fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/aipl-codegen/src")
        .join(DOGFOOD_CLIF_FILE)
}

/// Path to the staged (candidate) `.clif.staged` artifact.
fn staged_artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/aipl-codegen/src")
        .join(format!("{DOGFOOD_CLIF_FILE}.staged"))
}

/// Generate the unified dogfood artifact via the live frontend.
///
/// Spawns a scoped thread with a 64 MiB stack: some dogfooded `.aipl` files
/// (e.g. `caret_block.aipl`) trigger deep recursion in the compiler that
/// overflows the default test-framework stack (8 MiB on macOS).
fn generate() -> String {
    for (path, _) in DOGFOOD_SOURCES {
        if !path.starts_with("./") {
            panic!("non-relative path: {path:?}")
        }
    }
    let mut result = None;
    std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(s, || {
                generate_dogfood_artifact(DOGFOOD_SOURCES, DOGFOOD_ENTRIES)
                    .unwrap_or_else(|e| panic!("generate dogfood IR: {e}"))
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
fn sanity_check(artifact: &str) {
    let comp = Compilation::from_artifact(artifact)
        .unwrap_or_else(|e| panic!("load regenerated {DOGFOOD_CLIF_FILE}: {e}"));

    let span = |start, end| {
        FfiValue::Struct(vec![
            ("start".to_string(), FfiValue::Int(start)),
            ("end".to_string(), FfiValue::Int(end)),
        ])
    };

    let out = comp
        .call_values(
            "process_raw_string",
            &[FfiValue::Str("\n    a\n    b\n    ".to_string())],
        )
        .unwrap();
    assert_eq!(out, FfiValue::Str("a\nb".to_string()));

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

    // Returns `LineAt { line, line_start, line_end }`: the 0-based line index,
    // byte offset of the line's first byte, and byte offset of the line's end
    // (the '\n' terminator or source.len() for the last line).
    let line_at = comp
        .call_values(
            "line_at",
            &[FfiValue::Str("hello\nworld".to_string()), FfiValue::Int(6)],
        )
        .unwrap();
    assert_eq!(
        line_at,
        FfiValue::Struct(vec![
            ("line".to_string(), FfiValue::Int(1)),
            ("line_start".to_string(), FfiValue::Int(6)),
            ("line_end".to_string(), FfiValue::Int(11)),
        ])
    );
    // Offset 0 always returns line 0, line_start 0.
    let line_at_first = comp
        .call_values(
            "line_at",
            &[FfiValue::Str("abc".to_string()), FfiValue::Int(0)],
        )
        .unwrap();
    assert_eq!(
        line_at_first,
        FfiValue::Struct(vec![
            ("line".to_string(), FfiValue::Int(0)),
            ("line_start".to_string(), FfiValue::Int(0)),
            ("line_end".to_string(), FfiValue::Int(3)),
        ])
    );

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

    // Section missing: appended after stripping trailing newlines.
    let fill_result = comp
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
        fill_result,
        FfiValue::Str("code\n--- stdout ---\nhi\n".to_string())
    );
    // Section present: same behavior as `fill_section`.
    let fill_replaced = comp
        .call_values(
            "fill_or_add_section",
            &[
                FfiValue::Str("a\n--- foo ---\nold\n".to_string()),
                FfiValue::Str("foo".to_string()),
                FfiValue::Str("new".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(
        fill_replaced,
        FfiValue::Str("a\n--- foo ---\nnew\n".to_string())
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

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checked_in_ir_is_current() {
    aipl::install_parser_hooks();
    let generated = generate();
    let path = artifact_path();
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

/// The checked-in IR must actually load and compute correctly (independent of
/// whether it's byte-current with source — that's `checked_in_ir_is_current`).
#[test]
fn checked_in_ir_loads_and_runs() {
    aipl::install_parser_hooks();
    let path = artifact_path();
    let checked_in = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing checked-in dogfood IR {}: {e}\nGenerate it with: {FILL_CMD}",
            path.display()
        )
    });
    sanity_check(&checked_in);
}

/// Fails if a `.clif.staged` file is present, signalling a staged IR workflow
/// is in progress. See CLAUDE.md for the full workflow.
#[test]
fn no_staged_ir_pending() {
    let staged = staged_artifact_path();
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
    let artifact = generate();
    sanity_check(&artifact);
    let path = staged_artifact_path();
    std::fs::write(&path, &artifact).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    eprintln!("wrote {}", path.display());
    panic!(
        "fill_staged_ir wrote staged IR — review the diff vs the live .clif file,\n\
         then validate with: {VALIDATE_STAGED_CMD}\n\
         then promote with:  {PROMOTE_STAGED_CMD}"
    );
}

/// Load and sanity-check `dogfood.clif.staged` without modifying anything. Run
/// after `fill_staged_ir` to confirm staged IR loads and computes correctly
/// before promoting it to live.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn validate_staged_ir() {
    let path = staged_artifact_path();
    let artifact = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
            path.display()
        )
    });
    sanity_check(&artifact);
    eprintln!("validated {}", path.display());
}

/// Promote staged IR to live: validates `dogfood.clif.staged`, copies it to the
/// live `dogfood.clif`, then deletes the staged file. Intentionally fails so the
/// resulting diff is reviewed and the suite is re-run before committing.
///
/// See CLAUDE.md for the full staged IR workflow.
#[test]
#[ignore = "author helper — see CLAUDE.md for staged IR workflow"]
fn promote_staged_ir() {
    let staged = staged_artifact_path();
    let artifact = std::fs::read_to_string(&staged).unwrap_or_else(|e| {
        panic!(
            "missing staged IR {}: {e}\nGenerate it with: {FILL_STAGED_CMD}",
            staged.display()
        )
    });
    sanity_check(&artifact);
    let live = artifact_path();
    std::fs::write(&live, &artifact)
        .unwrap_or_else(|e| panic!("write live {}: {e}", live.display()));
    std::fs::remove_file(&staged)
        .unwrap_or_else(|e| panic!("remove staged {}: {e}", staged.display()));
    eprintln!("promoted {} → {}", staged.display(), live.display());
    panic!(
        "promote_staged_ir updated the live .clif file — review the diff,\n\
         then run `cargo test` to confirm the suite is green before committing."
    );
}

#[test]
#[ignore = "author helper — run: cargo test --test dogfood_ir -- --ignored fill_dogfood_ir"]
fn fill_dogfood_ir() {
    aipl::install_parser_hooks();
    let artifact = generate();
    // Never write IR that won't link or run.
    sanity_check(&artifact);
    let path = artifact_path();
    std::fs::write(&path, &artifact).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    eprintln!("wrote {}", path.display());
    panic!(
        "fill_dogfood_ir regenerated the checked-in dogfood IR — review the diff, \
         then re-run the suite normally to confirm it's green."
    );
}
