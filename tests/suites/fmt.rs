//! Formatter tests.
//!
//! - **Corpus invariants**: every parseable `.aipl` in the repo (test cases,
//!   dogfooded compiler sources, examples) must format without error and format
//!   *idempotently*. Token and comment preservation is not checked here — it is
//!   `format_source`'s own precondition, which refuses rather than emits, so
//!   "formatted without error" already covers it (`verify_same_tokens` /
//!   `TokenFingerprint` in `format_source.aipl`).

use std::path::{Path, PathBuf};

use aipl::codegen::{format_source, FORMAT_WIDTH};

fn setup() {
    aipl::install_parser_hooks();
}

/// All `.aipl` files under `dir`, recursively, sorted for stable output —
/// walked by the dogfooded AIPL `find_files` (see [`aipl::codegen::find_files`]),
/// not `std::fs`. A directory that can't be walked is a broken discovery, not an
/// empty one, so it panics rather than silently enforcing nothing.
fn aipl_files(dir: &str, out: &mut Vec<PathBuf>) {
    let found = aipl::codegen::find_files(dir, ".aipl")
        .unwrap_or_else(|e| panic!("find_files({dir:?}): {e}"));
    out.extend(found.into_iter().map(PathBuf::from));
}

/// The `.aipl` files whose checked-in text is required to already be in
/// canonical format (see [`all_aipl_files_stay_formatted`] /
/// [`format_corpus`]): every `.aipl` in the repo.
fn enforced_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in [
        "tests/cases",
        "crates",
        "examples",
        "projects",
        "tests/ffi_fixtures",
    ] {
        aipl_files(dir, &mut files);
    }
    files
}

/// No checked-in AIPL file names a place on one machine (DESIGN_PRINCIPLES.md, "Paths are
/// relative to the project root"). A pinned `--- check --compile ---` or `--- check ---` section that
/// holds `/Users/someone/...` passes where it was recorded and fails on every other checkout.
/// A fixture that needs to show an absolute path writes one that is plainly not real.
#[test]
fn no_pinned_file_names_a_path_on_one_machine() {
    // Where a home directory or a temporary directory begins on the systems this runs on.
    const MACHINE_PATHS: [&str; 5] = [
        "/Users/",
        "/home/",
        "/private/var/",
        "/var/folders/",
        ":\\Users\\",
    ];
    let mut offenders: Vec<String> = Vec::new();
    for path in enforced_files() {
        let text = std::fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            if MACHINE_PATHS.iter().any(|p| line.contains(p)) {
                offenders.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "{} line(s) name a path on one machine; output is rendered relative to the project root:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// Every checked-in `.aipl` file (outside the formatter's own fixtures) must
/// already be in canonical format — this is what keeps the corpus, and any new
/// file, formatted. A file that doesn't parse (a parse-error fixture) can't be
/// formatted and is exempt. Fix a failure with
/// `cargo test --test compiler -- --ignored fmt::format_corpus`.
#[test]
fn all_aipl_files_stay_formatted() {
    setup();
    let mut unformatted: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for outcome in par_map(&enforced_files(), |path| {
        let src = std::fs::read_to_string(path).unwrap();
        if aipl::parse(aipl::strip_test_sections(&src)).is_err() {
            return None; // parse-error fixture; nothing to format
        }
        Some(match format_source(&src, FORMAT_WIDTH) {
            Ok(formatted) if formatted == src => None,
            Ok(_) => Some(path.display().to_string()),
            Err(e) => Some(format!("{} (format error: {e})", path.display())),
        })
    }) {
        let Some(verdict) = outcome else {
            continue;
        };
        checked += 1;
        unformatted.extend(verdict);
    }
    assert!(
        checked > 100,
        "enforcement checked too few files ({checked})"
    );
    assert!(
        unformatted.is_empty(),
        "{} file(s) are not canonically formatted; run \
         `cargo test --test compiler -- --ignored fmt::format_corpus` to fix:\n{}",
        unformatted.len(),
        unformatted.join("\n")
    );
}

/// The section names the parser refuses (`errors`, `check exit code`, `formatted`) appear on a line
/// of their own in no file that carries sections: the parser's message is the first anyone hears
/// of them, so a leftover in the tree is a file that cannot be read at all.
#[test]
fn no_retired_section_name_remains_in_the_tree() {
    setup();
    const RETIRED: [&str; 3] = [
        "--- errors ---",
        "--- check exit code ---",
        "--- formatted ---",
    ];
    let mut files = Vec::new();
    for dir in ["tests/cases", "examples", "projects", "tests/ffi_fixtures"] {
        aipl_files(dir, &mut files);
    }
    let found: Vec<String> = files
        .iter()
        .filter(|path| {
            std::fs::read_to_string(path)
                .is_ok_and(|text| text.lines().any(|line| RETIRED.contains(&line)))
        })
        .map(|path| path.display().to_string())
        .collect();
    assert!(
        found.is_empty(),
        "a retired section name remains in:\n{}",
        found.join("\n")
    );
}

/// Authoring helper: rewrite every enforced `.aipl` file in place with its
/// canonical formatting, then fail so the run is visibly a bulk rewrite.
/// Review the diff (and refill any `--- report ---`/`--- check --compile ---`
/// sections whose spans shifted) before committing.
#[test]
#[ignore]
fn format_corpus() {
    setup();
    let changed = par_map(&enforced_files(), |path| {
        let src = std::fs::read_to_string(path).unwrap();
        if aipl::parse(aipl::strip_test_sections(&src)).is_err() {
            return false;
        }
        let formatted = format_source(&src, FORMAT_WIDTH)
            .unwrap_or_else(|e| panic!("[{}] format failed: {e}", path.display()));
        if formatted == src {
            return false;
        }
        std::fs::write(path, &formatted).unwrap();
        eprintln!("[{}]: reformatted", path.display());
        true
    })
    .into_iter()
    .filter(|&c| c)
    .count();
    panic!(
        "reformatted {changed} file(s); review the diff, refill any shifted \
         --- report ---/--- check .. --- sections \
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
    for dir in ["tests/cases", "crates", "examples"] {
        aipl_files(dir, &mut files);
    }
    assert!(files.len() > 100, "corpus walk found too few files");

    let mut failures: Vec<String> = Vec::new();
    let mut formatted_count = 0usize;
    // Per file: whether it was formatted at all, and every invariant it broke.
    for (formatted, broke) in par_map(&files, |path| {
        let mut failures: Vec<String> = Vec::new();
        let src = std::fs::read_to_string(path).unwrap();
        let (prefix, sections) = aipl::split_test_sections(&src);
        if aipl::parse(prefix).is_err() {
            return (false, failures); // an error-case fixture; nothing to format
        }
        let ctx = path.display();
        let once = match format_source(&src, FORMAT_WIDTH) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("[{ctx}] format failed: {e}"));
                return (false, failures);
            }
        };
        match format_source(&once, FORMAT_WIDTH) {
            Ok(twice) => {
                if twice != once {
                    failures.push(format!(
                        "[{ctx}] not idempotent:\n--- first ---\n{once}\n--- second ---\n{twice}"
                    ));
                }
            }
            Err(e) => failures.push(format!("[{ctx}] reformat of own output failed: {e}")),
        }
        // Token/comment preservation is `format_source`'s own precondition —
        // it refuses rather than emits (see `verify_same_tokens` /
        // `TokenFingerprint` in `format_source.aipl`), so reaching this point
        // already means the tokens survived, both times.
        //
        // Trailing sections ride along byte-for-byte.
        if !sections.is_empty() && !once.ends_with(sections) {
            failures.push(format!("[{ctx}] trailing sections were not preserved"));
        }
        (true, failures)
    }) {
        formatted_count += usize::from(formatted);
        failures.extend(broke);
    }
    assert!(
        failures.is_empty(),
        "{} formatter corpus failure(s):\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert!(formatted_count > 100, "corpus formatted too few files");
}

/// `f` over every path, on every core, results in input order.
///
/// The corpus is ~600 files and each is parsed and laid out independently, so
/// the three whole-corpus tests here are embarrassingly parallel — and were
/// paying ~80s each single-threaded on a 12-core machine. The dogfood engine
/// is thread-local (one lazily built per worker, off the prebuilt object), so
/// a worker needs no setup beyond the process-wide hooks the caller installed.
fn par_map<T: Send>(paths: &[PathBuf], f: impl Fn(&Path) -> T + Sync) -> Vec<T> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<T>>> =
        paths.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..workers.min(paths.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(path) = paths.get(i) else {
                    break;
                };
                *slots[i].lock().unwrap() = Some(f(path));
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap().expect("every path was visited"))
        .collect()
}

/// Formatting through the CLI: `aipl check` reports an unformatted file without
/// touching it, `aipl check --fix` rewrites it in place. (A CLI
/// surface the `.aipl` cases framework, which only `run`s/`check`s, can't
/// exercise.)
#[test]
fn cli_check_formats_with_fix() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("fmt_cli");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("wide.aipl");
    let src = "fn main()   ->   i64 { 42 }\n";
    std::fs::write(&path, src).unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_aipl"))
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("spawn aipl");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code(),
        )
    };
    // Plain `check`: reported, exit 1, file untouched.
    assert_eq!(
        run(&["check", "wide.aipl"]),
        (
            "./wide.aipl: needs formatting (run `aipl check --fix ./wide.aipl`)\n\
             0 tests: 0 passed, 0 failed\n"
                .to_string(),
            String::new(),
            Some(1)
        )
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), src);
    // `--fix` rewrites it in place and says so.
    assert_eq!(
        run(&["check", "--fix", "wide.aipl"]),
        (
            "formatted ./wide.aipl\n0 tests: 0 passed, 0 failed\n".to_string(),
            String::new(),
            Some(0)
        )
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn main() -> i64 { 42 }\n"
    );
    // And now `check` passes.
    assert_eq!(
        run(&["check", "wide.aipl"]),
        (
            "0 tests: 0 passed, 0 failed\n".to_string(),
            String::new(),
            Some(0)
        )
    );
    let _ = std::fs::remove_file(&path);
}

/// Author helper: lay out the file named by `AIPL_FMT_PROBE` with the AIPL
/// formatter and report which way it went. Handy for measuring how much native
/// stack the walker needs on a given file — pair it with `RUST_MIN_STACK`.
#[test]
#[ignore = "author helper — measures AIPL walker stack depth"]
fn aipl_fmt_probe() {
    setup();
    let path = std::env::var("AIPL_FMT_PROBE").expect("AIPL_FMT_PROBE");
    let src = std::fs::read_to_string(&path).unwrap();
    match aipl::codegen::format_source(&src, 100) {
        Ok(_) => eprintln!("PROBE-OK"),
        Err(e) => eprintln!("PROBE-ERR {e}"),
    }
}
