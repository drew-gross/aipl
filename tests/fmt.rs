//! Formatter tests.
//!
//! Two layers:
//! - **Fixtures** (`tests/fmt/*.aipl`): input source followed by a final
//!   `--- formatted ---` section holding the expected `format_source` output,
//!   byte for byte. Refresh with
//!   `cargo test --test fmt -- --ignored fill_expected_fmt` (scope with
//!   `AIPL_FMT_CASE=<substring>`), then review the diff.
//! - **Corpus invariants**: every parseable `.aipl` in the repo (test cases,
//!   dogfooded compiler sources, examples) must format without error, format
//!   *idempotently*, and preserve its tokens and comments exactly — imports
//!   excepted, which may reorder by design and are compared as multisets.

use std::path::{Path, PathBuf};

use aipl::fmt::{format_source, FmtOptions};
use aipl::{lex_tokens_and_comments, FmtTokenKind, Span};

fn setup() {
    aipl::install_parser_hooks();
}

/// All `.aipl` files under `dir`, recursively, sorted for stable output.
fn aipl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            aipl_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "aipl") {
            out.push(path);
        }
    }
}

/// The token stream of `src` split into import statements and everything
/// else: `(sorted import-statement token texts, the remaining token texts in
/// order, sorted comment texts)`. Imports may legitimately reorder (they are
/// hoisted and sorted), so each import statement's tokens are compared as one
/// sorted unit; every other token must survive in exact order.
fn token_fingerprint(src: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (toks, comments) = lex_tokens_and_comments(src).expect("lexes");
    // Collapse a raw-string / template *closing-delimiter* line's indentation:
    // the formatter re-aligns the closing `"""` / ``` ``` ``` under the opening
    // one, which is a deliberate (value-preserving) change, so it must not read
    // as a token difference here. Content lines keep their exact bytes.
    let text = |sp: &Span| -> String {
        let t = &src[sp.clone()];
        if let Some(nl) = t.rfind('\n') {
            let (head, last) = t.split_at(nl + 1);
            let trimmed = last.trim_start();
            if trimmed == "\"\"\"" || trimmed == "```" {
                return format!("{head}{trimmed}");
            }
        }
        t.to_string()
    };
    let mut imports: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let (kind, sp) = &toks[i];
        if text(sp) == "," {
            // Trailing commas are normalized by design (dropped when flat,
            // added when broken); commas don't participate in the check.
            i += 1;
            continue;
        }
        if *kind == FmtTokenKind::Plain(aipl::TokenKind::Keyword) && text(sp) == "import" {
            // Collect through the terminating `;`, sorting the names inside so
            // reordering within the list doesn't matter either.
            let mut stmt: Vec<String> = Vec::new();
            while i < toks.len() {
                let t = text(&toks[i].1);
                let done = t == ";";
                if t != "," {
                    stmt.push(t);
                }
                i += 1;
                if done {
                    break;
                }
            }
            stmt.sort();
            imports.push(stmt.join(" "));
        } else {
            rest.push(format!("{kind:?} {}", text(sp)));
            i += 1;
        }
    }
    imports.sort();
    let mut ctexts: Vec<String> = comments.iter().map(text).collect();
    ctexts.sort();
    (imports, rest, ctexts)
}

/// The `.aipl` files whose checked-in text is required to already be in
/// canonical format (see [`all_aipl_files_stay_formatted`] /
/// [`format_corpus`]). This is every `.aipl` in the repo *except* the
/// formatter's own `tests/fmt/` fixtures, whose inputs are deliberately
/// misformatted to exercise the formatter.
fn enforced_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in ["tests/cases", "crates", "examples", "tests/ffi_fixtures"] {
        aipl_files(Path::new(dir), &mut files);
    }
    files
}

/// Every checked-in `.aipl` file (outside the formatter's own fixtures) must
/// already be in canonical format — this is what keeps the corpus, and any new
/// file, formatted. A file that doesn't parse (a parse-error fixture) can't be
/// formatted and is exempt. Fix a failure with
/// `cargo test --test fmt -- --ignored format_corpus`.
#[test]
fn all_aipl_files_stay_formatted() {
    setup();
    let opts = FmtOptions::default();
    let mut unformatted: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for path in enforced_files() {
        let src = std::fs::read_to_string(&path).unwrap();
        if aipl::parse(aipl::strip_test_sections(&src)).is_err() {
            continue; // parse-error fixture; nothing to format
        }
        checked += 1;
        match format_source(&src, &opts) {
            Ok(formatted) if formatted == src => {}
            Ok(_) => unformatted.push(path.display().to_string()),
            Err(e) => unformatted.push(format!("{} (format error: {e})", path.display())),
        }
    }
    assert!(
        checked > 100,
        "enforcement checked too few files ({checked})"
    );
    assert!(
        unformatted.is_empty(),
        "{} file(s) are not canonically formatted; run \
         `cargo test --test fmt -- --ignored format_corpus` to fix:\n{}",
        unformatted.len(),
        unformatted.join("\n")
    );
}

/// Authoring helper: rewrite every enforced `.aipl` file in place with its
/// canonical formatting, then fail so the run is visibly a bulk rewrite.
/// Review the diff (and refill any `--- performance ---`/`--- errors ---`
/// sections whose spans shifted) before committing.
#[test]
#[ignore]
fn format_corpus() {
    setup();
    let opts = FmtOptions::default();
    let mut changed = 0usize;
    for path in enforced_files() {
        let src = std::fs::read_to_string(&path).unwrap();
        if aipl::parse(aipl::strip_test_sections(&src)).is_err() {
            continue;
        }
        let formatted = format_source(&src, &opts)
            .unwrap_or_else(|e| panic!("[{}] format failed: {e}", path.display()));
        if formatted != src {
            std::fs::write(&path, &formatted).unwrap();
            eprintln!("[{}]: reformatted", path.display());
            changed += 1;
        }
    }
    panic!(
        "reformatted {changed} file(s); review the diff, refill any shifted \
         --- performance ---/--- errors ---/--- check --- sections \
         (`fill_expected`) and regenerate dogfood IR (`fill_dogfood_ir`), \
         then re-run the suite (this run fails intentionally)"
    );
}

/// Format every parseable `.aipl` in the repo and hold the formatter to its
/// invariants. Files that don't parse (error-case fixtures) are skipped —
/// there is nothing to format.
#[test]
fn corpus_formats_idempotently_and_losslessly() {
    setup();
    let mut files = Vec::new();
    for dir in ["tests/cases", "crates", "examples", "tests/fmt"] {
        aipl_files(Path::new(dir), &mut files);
    }
    assert!(files.len() > 100, "corpus walk found too few files");

    let opts = FmtOptions::default();
    let mut failures: Vec<String> = Vec::new();
    let mut formatted_count = 0usize;
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let prefix = aipl::strip_test_sections(&src);
        if aipl::parse(prefix).is_err() {
            continue; // an error-case fixture; nothing to format
        }
        let ctx = path.display();
        let once = match format_source(&src, &opts) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("[{ctx}] format failed: {e}"));
                continue;
            }
        };
        formatted_count += 1;
        match format_source(&once, &opts) {
            Ok(twice) => {
                if twice != once {
                    failures.push(format!(
                        "[{ctx}] not idempotent:\n--- first ---\n{once}\n--- second ---\n{twice}"
                    ));
                }
            }
            Err(e) => failures.push(format!("[{ctx}] reformat of own output failed: {e}")),
        }
        // Token/comment preservation, stronger than format_source's internal
        // multiset check: outside imports, order matters too.
        let before = token_fingerprint(prefix);
        let after = token_fingerprint(aipl::strip_test_sections(&once));
        if before != after {
            failures.push(format!("[{ctx}] token fingerprint changed"));
        }
        // Trailing sections ride along byte-for-byte.
        let sections = &src[prefix.len()..];
        if !sections.is_empty() && !once.ends_with(sections) {
            failures.push(format!("[{ctx}] trailing sections were not preserved"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} formatter corpus failure(s):\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert!(formatted_count > 100, "corpus formatted too few files");
}

// ---------- fixtures ----------

const FIXTURE_HEADER: &str = "\n--- formatted ---\n";

/// Split a fixture into (input, expected-output). The `--- formatted ---`
/// section is last in the file; everything before it (which may itself end
/// with other `--- .. ---` sections that must ride through formatting) is the
/// formatter's input.
fn split_fixture(contents: &str) -> Option<(&str, &str)> {
    let idx = contents.rfind(FIXTURE_HEADER)?;
    Some((
        &contents[..idx + 1], // keep the input's trailing newline
        &contents[idx + FIXTURE_HEADER.len()..],
    ))
}

fn fixture_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    aipl_files(Path::new("tests/fmt"), &mut files);
    assert!(!files.is_empty(), "no fixtures under tests/fmt");
    files
}

fn fixture_filter() -> Option<String> {
    std::env::var("AIPL_FMT_CASE").ok()
}

#[test]
fn fixtures_match_expected() {
    setup();
    let filter = fixture_filter();
    let opts = FmtOptions::default();
    let mut failures = Vec::new();
    let mut ran = 0usize;
    for path in fixture_files() {
        if let Some(f) = &filter {
            if !path.to_string_lossy().contains(f.as_str()) {
                continue;
            }
        }
        ran += 1;
        let contents = std::fs::read_to_string(&path).unwrap();
        let Some((input, expected)) = split_fixture(&contents) else {
            failures.push(format!(
                "[{}] missing `--- formatted ---` section; add one with a `?` body and run \
                 `cargo test --test fmt -- --ignored fill_expected_fmt`",
                path.display()
            ));
            continue;
        };
        let actual = match format_source(input, &opts) {
            Ok(a) => a,
            Err(e) => {
                failures.push(format!("[{}] format failed: {e}", path.display()));
                continue;
            }
        };
        if expected.trim() == "?" {
            failures.push(format!(
                "[{}] expected body is `?`; run `cargo test --test fmt -- --ignored \
                 fill_expected_fmt` to fill it. Actual:\n{actual}",
                path.display()
            ));
            continue;
        }
        if actual != expected {
            failures.push(format!(
                "[{}] output mismatch\n--- expected ---\n{expected}--- actual ---\n{actual}",
                path.display()
            ));
        }
    }
    if filter.is_some() {
        // Mirror the cases harness: a focused run fails on purpose so a stray
        // filter can't masquerade as a green suite.
        assert!(
            failures.is_empty(),
            "{} fixture failure(s):\n{}",
            failures.len(),
            failures.join("\n\n")
        );
        panic!(
            "AIPL_FMT_CASE filter active: ran {ran} fixture(s), all passing — unset it to run \
             the full suite"
        );
    }
    assert!(
        failures.is_empty(),
        "{} fixture failure(s):\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Authoring helper: rewrite every fixture's `--- formatted ---` section with
/// the formatter's actual output, then fail so the run is visibly a refresh.
/// Scope with `AIPL_FMT_CASE=<substring>`.
#[test]
#[ignore]
fn fill_expected_fmt() {
    setup();
    let filter = fixture_filter();
    let opts = FmtOptions::default();
    let mut filled = 0usize;
    for path in fixture_files() {
        if let Some(f) = &filter {
            if !path.to_string_lossy().contains(f.as_str()) {
                continue;
            }
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        let input = match split_fixture(&contents) {
            Some((input, _)) => input.to_string(),
            None => contents.clone(),
        };
        let actual = format_source(&input, &opts)
            .unwrap_or_else(|e| panic!("[{}] format failed: {e}", path.display()));
        let new_contents = format!("{input}--- formatted ---\n{actual}");
        std::fs::write(&path, new_contents).unwrap();
        eprintln!("[{}]: filled expected formatting", path.display());
        filled += 1;
    }
    panic!(
        "filled {filled} fixture(s); review the diff, then re-run `cargo test --test fmt` \
         normally (this run fails intentionally so the refresh is visible)"
    );
}

/// The `aipl fmt` CLI: rewrites in place; `--check` reports without writing.
/// (A CLI surface the `.aipl` cases framework, which only `run`s/`check`s,
/// can't exercise.)
#[test]
fn cli_fmt_and_check() {
    let path = std::env::temp_dir().join(format!("aipl_fmt_{}.aipl", std::process::id()));
    let src = "fn main()   ->   i64 { 42 }\n";
    std::fs::write(&path, src).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_aipl"))
            .arg("fmt")
            .args(args)
            .arg(&path)
            .output()
            .expect("spawn aipl fmt")
    };
    // --check on an unformatted file: exit 1, file untouched.
    let out = run(&["--check"]);
    assert!(!out.status.success(), "--check should fail on unformatted");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), src);
    // fmt rewrites in place.
    let out = run(&[]);
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn main() -> i64 { 42 }\n"
    );
    // --check on the formatted file: exit 0.
    let out = run(&["--check"]);
    assert!(out.status.success(), "--check should pass when formatted");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn width_is_configurable() {
    setup();
    let src = "fn f(aaaa: i64, bbbb: i64, cccc: i64) -> i64 { aaaa }\n";
    let wide = format_source(src, &FmtOptions { max_width: 100 }).unwrap();
    assert_eq!(
        wide,
        "fn f(aaaa: i64, bbbb: i64, cccc: i64) -> i64 { aaaa }\n"
    );
    // The parameter list exceeds 30 columns and breaks (with a trailing
    // comma); the one-expression body still fits flat after it.
    let narrow = format_source(src, &FmtOptions { max_width: 30 }).unwrap();
    assert_eq!(
        narrow,
        "fn f(\n    aaaa: i64,\n    bbbb: i64,\n    cccc: i64,\n) -> i64 { aaaa }\n"
    );
}
