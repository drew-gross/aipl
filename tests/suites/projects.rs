//! The projects under `projects/` are standalone AIPL projects that happen to
//! live in this repository.
//!
//! Each one carries the compiler that builds it (DESIGN_PRINCIPLES.md §4), so
//! there is nothing to resolve and nothing to activate: `projects/word_count/`
//! holds its source and an `aipl`, and building it is running that `aipl`.
//!
//! Which leaves this suite two things to check, and they are opposite halves of
//! the same claim. [`checked_in_compilers_are_current`] asks whether the copy was
//! built from the compiler sources as they stand now — a policy for the projects
//! here rather than a property of the design, because these double as the
//! language's worked examples and one pinned to a superseded compiler would be
//! demonstrating superseded AIPL. [`projects_pass_their_own_check`] then asks
//! whether the project actually works *through that copy*, run the way a user
//! runs it, which is the only thing here that executes the shipped compiler at
//! all.
//!
//! # Why a fingerprint rather than the bytes
//!
//! The obvious check is to compare the project's `aipl` against
//! `CARGO_BIN_EXE_aipl` byte for byte. That does not work, and the way it fails
//! is silent: **cargo writes two different `target/debug/aipl` binaries
//! depending on how it was invoked.** A plain `cargo build` and the
//! `cargo nextest run --workspace --all-targets` the gate uses produce files of
//! different sizes (20.6 MB against 28.4 MB, measured, reproducibly) at the same
//! path, because `--all-targets` pulls the dev-dependencies into feature
//! resolution. Each overwrites the other. A byte comparison would therefore pass
//! or fail on which cargo command happened to run last, which is not a property
//! of the projects at all.
//!
//! So what is recorded beside each compiler is a fingerprint of *what it was
//! built from* — every file the `aipl` binary is compiled out of — in an
//! `aipl.build` file. That is stable across build modes and across machines,
//! where the bytes are neither, and it is the same shape as the
//! `; source-fingerprint` line that ties `dogfood.o` to the AIPL it was
//! generated from. It inherits that pattern's one weakness too: the fingerprint
//! is a claim about the binary rather than a measurement of it, so a hand-edited
//! `aipl.build` would lie. [`projects_pass_their_own_check`] is the backstop —
//! it runs the thing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The `fill_*` helper that fixes a stale copy, named in every failure it would
/// fix. `cargo handoff` runs it for you.
const REGENERATE: &str = "cargo test --test compiler -- --ignored projects::fill_project_compilers";

/// The line `aipl.build` records, matching `dogfood.manifest`'s spelling.
const FINGERPRINT_PREFIX: &str = "source-fingerprint ";

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn projects_dir() -> PathBuf {
    repo().join("projects")
}

/// Every project directory, by name, in sorted order.
///
/// A project is any directory directly under `projects/`; `projects/README.md`
/// and anything else loose in there is not one. Sorted so a failure names them
/// in a stable order.
fn projects() -> Vec<PathBuf> {
    let dir = projects_dir();
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    found.sort();
    assert!(
        !found.is_empty(),
        "no projects found under {} — this suite would pass by checking nothing",
        dir.display()
    );
    found
}

/// Where a project keeps its compiler. Flat, beside the source: the principle is
/// that it is not somewhere a person has to be told about.
fn compiler_in(project: &Path) -> PathBuf {
    project.join("aipl")
}

/// Where a project records which compiler that is.
fn build_record_in(project: &Path) -> PathBuf {
    project.join("aipl.build")
}

/// Every file the `aipl` binary is built out of, sorted by path relative to the
/// repository root.
///
/// `src/` and `crates/` whole, rather than a list of extensions: the compiler is
/// built from more than its `.rs` — the dogfooded `.aipl` sources, the
/// checked-in `dogfood.o` and its manifest, each crate's `Cargo.toml`, the build
/// scripts — and naming extensions is how one of those comes to be silently
/// excluded. `Cargo.lock` and the root manifest are in for the dependency
/// versions, which are as much a part of what got built as our own code.
///
/// `handoff/` is deliberately out: it is this repo's gate, not the compiler.
fn compiler_sources() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in ["src", "crates"] {
        walk(&repo().join(dir), &mut files);
    }
    for file in ["Cargo.toml", "Cargo.lock"] {
        files.push(repo().join(file));
    }
    files.sort();
    assert!(
        files.len() > 100,
        "only {} compiler source files found — the walk is not finding the tree",
        files.len()
    );
    files
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// A fingerprint of the compiler's whole source tree: FNV-1a over each file's
/// repo-relative path and contents, in sorted order.
///
/// Hand-rolled, and specified rather than merely currently-stable, for the same
/// reason `aipl_artifact::fingerprint` is: the value is written to a file in one
/// run and compared in another, so a hasher that is free to change between
/// releases would turn every project stale for no reason. The path goes into the
/// hash as well as the contents so that moving a file is a change even when
/// nothing in it moved.
fn compiler_fingerprint() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for path in compiler_sources() {
        let rel = path.strip_prefix(repo()).unwrap_or(&path);
        // Forward slashes, so the fingerprint does not depend on the separator
        // the host happens to use.
        eat(rel.to_string_lossy().replace('\\', "/").as_bytes());
        eat(&std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
    }
    h
}

/// The fingerprint recorded in an `aipl.build`, or `None` when the file is
/// missing or carries no such line.
fn recorded_fingerprint(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix(FINGERPRINT_PREFIX))
        .and_then(|v| v.trim().parse().ok())
}

/// What `fill_project_compilers` writes beside each copied compiler.
fn build_record(fingerprint: u64) -> String {
    format!(
        "# The `aipl` beside this file is the compiler that builds this project\n\
         # (DESIGN_PRINCIPLES.md §4). This records which compiler it is: a\n\
         # fingerprint of every file the binary was built from.\n\
         #\n\
         # It exists because the projects in *this* repository are held to the\n\
         # current compiler — a project of your own would not have one. Checked by\n\
         # `projects::checked_in_compilers_are_current`; rewritten, with the\n\
         # binary, by `cargo handoff`.\n\
         {FINGERPRINT_PREFIX}{fingerprint}\n"
    )
}

/// Every project's checked-in compiler must have been built from the compiler
/// sources as they stand now.
///
/// Any change to the compiler — Rust half or dogfooded AIPL — makes every
/// project stale at once. That is the intended reading of "the projects here are
/// always on an up-to-date compiler", and it is why `cargo handoff` re-copies
/// rather than stopping.
#[test]
fn checked_in_compilers_are_current() {
    let want = compiler_fingerprint();
    let mut stale: Vec<String> = Vec::new();
    for project in projects() {
        let compiler = compiler_in(&project);
        if !compiler.exists() {
            stale.push(format!(
                "  {}: missing — the project has no compiler at all",
                rel(&compiler)
            ));
            continue;
        }
        let record = build_record_in(&project);
        match recorded_fingerprint(&record) {
            Some(have) if have == want => {}
            Some(have) => stale.push(format!(
                "  {}: built from source fingerprint {have}, against {want} now",
                rel(&compiler),
            )),
            None => stale.push(format!(
                "  {}: missing or unreadable, so which compiler {} is cannot be told",
                rel(&record),
                rel(&compiler),
            )),
        }
    }
    assert!(
        stale.is_empty(),
        "{} project compiler(s) are not built from this repository's current \
         sources. Re-copy with:\n    {REGENERATE}\n{}",
        stale.len(),
        stale.join("\n"),
    );
}

/// Every project must pass its own `aipl check`, run through its own compiler.
///
/// This is the project's test suite — its `.test` blocks — and running it through
/// the checked-in copy rather than through `CARGO_BIN_EXE_aipl` is the point:
/// that copy is what a user would run, and nothing else here would ever execute
/// it. It is also what keeps [`checked_in_compilers_are_current`] honest, since a
/// recorded fingerprint is a claim about the binary and this actually runs it. A
/// project whose compiler is missing, is built for another platform, or cannot
/// parse its own source fails here rather than silently not being tested.
///
/// `current_dir` is the project, so the paths in the output are the ones a user
/// standing in that directory would see, and `check` with no argument picks up
/// exactly the project's own tree.
#[test]
fn projects_pass_their_own_check() {
    let mut failures: Vec<String> = Vec::new();
    for project in projects() {
        let compiler = compiler_in(&project);
        if !compiler.exists() {
            // `checked_in_compilers_are_current` reports this properly; failing
            // twice for one cause would just be noise.
            continue;
        }
        let out = Command::new(&compiler)
            .current_dir(&project)
            .arg("check")
            .output()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", rel(&compiler)));
        if !out.status.success() {
            failures.push(format!(
                "  {} failed `./aipl check`:\n{}{}",
                rel(&project),
                indent(&String::from_utf8_lossy(&out.stdout)),
                indent(&String::from_utf8_lossy(&out.stderr)),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} project(s) do not pass their own tests:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

/// Author helper: copy the built compiler into every project, record what it was
/// built from, then fail so the run is visibly a bulk rewrite — the same shape as
/// the other `fill_*` helpers. `cargo handoff` runs this for you when the check
/// above fails.
///
/// `CARGO_BIN_EXE_aipl` is the binary cargo built for this test target, so the
/// copy is of a compiler that is current by construction, whichever of the two
/// build modes produced it (see the module docs). The fingerprint written beside
/// it is of the sources, not of that file, which is what makes the record mean
/// the same thing in both modes.
#[test]
#[ignore]
fn fill_project_compilers() {
    let src = Path::new(env!("CARGO_BIN_EXE_aipl"));
    let record = build_record(compiler_fingerprint());
    let mut copied: Vec<String> = Vec::new();
    for project in projects() {
        let dst = compiler_in(&project);
        // Remove first: overwriting a *running* executable in place fails on
        // some platforms, and an unlink-then-write always works. `fs::copy`
        // carries the permission bits, so the copy stays executable.
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(src, &dst)
            .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src.display(), rel(&dst)));
        let note = build_record_in(&project);
        std::fs::write(&note, &record).unwrap_or_else(|e| panic!("{}: {e}", rel(&note)));
        copied.push(rel(&dst));
    }
    panic!(
        "copied the built compiler into {} project(s) — review the diff:\n  {}",
        copied.len(),
        copied.join("\n  "),
    );
}

/// `path` relative to the repository root, for messages a reader can act on.
fn rel(path: &Path) -> String {
    path.strip_prefix(repo())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Indent captured subprocess output so it reads as nested under the failure
/// that quotes it.
fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}\n"))
        .collect::<Vec<_>>()
        .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fingerprint has to be the same value twice in a row, or every run
    /// would report every project stale.
    #[test]
    fn the_fingerprint_is_stable() {
        assert_eq!(compiler_fingerprint(), compiler_fingerprint());
    }

    /// And it has to be readable back out of exactly what the filler writes.
    #[test]
    fn a_written_record_reads_back() {
        let dir = std::env::temp_dir().join(format!("aipl-projects-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = build_record_in(&dir);
        std::fs::write(&path, build_record(1234567890)).expect("write record");
        assert_eq!(recorded_fingerprint(&path), Some(1234567890));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing record is "cannot tell", not a panic and not a pass.
    #[test]
    fn a_missing_record_has_no_fingerprint() {
        assert_eq!(
            recorded_fingerprint(Path::new("/nonexistent/aipl.build")),
            None
        );
    }
}
