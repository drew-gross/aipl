//! A file's recorded expectations — the trailing `--- section ---` blocks —
//! checked against what the file actually does. This is the half of `aipl check`
//! that asks what a program *does*, beside the half that asks whether its
//! `.test` blocks pass.
//!
//! Each function returns its findings as rendered mismatch reports (an empty list
//! is a match) rather than failing at the first, because `check` reports
//! everything wrong with a file in one pass.
//!
//! Which sections are read here:
//!
//! | section | checked against |
//! |---|---|
//! | `errors` | the diagnostics compiling the file produces; the file must not compile |
//! | `stdout`, `stderr`, `exit code` | one run of the built `main` |
//! | `cli` | not compared: the arguments that run is given |
//! | `expect file: p` | the file `p` the run left in its working directory |
//!
//! `--- file: p ---` companions are not expectations; they are staged beside the
//! file by the caller so imports and fixtures resolve. `performance`, `check` and
//! `check exit code` are not read at all: the first belongs to the cases harness,
//! and the other two pin a run of `check` itself.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::ast::{Item, Program};
use crate::binary;
use crate::codegen::{normalize_output, ObjectCompilation};
use crate::{DebugOptions, Error, SpecFields};

/// Whether the file asks for its `main` to be run: any section that observes a
/// run. A file with none of them is a plain program, and checking it must not
/// execute `main` — that may be a server, or read its input from a terminal.
pub fn runs_main(spec: &SpecFields) -> bool {
    spec.stdout.is_some()
        || spec.stderr.is_some()
        || spec.exit_code.is_some()
        || !spec.cli.is_empty()
        || !spec.expect_files.is_empty()
}

/// A mismatch report for one section, or `None` when `actual` is what the file
/// recorded.
fn mismatch(section: &str, actual: &str, expected: &str) -> Option<String> {
    (actual != expected).then(|| {
        format!("`{section}` mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}")
    })
}

/// Sections that cannot sit together. `errors` describes a program that does not
/// compile, so a section that observes a run, or one that needs a compiled
/// program, contradicts it; the file is wrong, not the program, and neither
/// section could be checked against anything.
pub fn conflicts(spec: &SpecFields) -> Vec<String> {
    if spec.errors.is_none() {
        return Vec::new();
    }
    let mut found = Vec::new();
    if spec.stdout.is_some() || spec.stderr.is_some() || spec.exit_code.is_some() {
        found.push("`errors` section is mutually exclusive with stdout/stderr/exit code");
    }
    if spec.performance.is_some() {
        found.push(
            "`performance` section requires a running program, so it cannot coexist with `errors`",
        );
    }
    if spec.check.is_some() {
        found.push(
            "`check` section requires a compiling program, so it cannot coexist with `errors`",
        );
    }
    if !spec.expect_files.is_empty() {
        found.push(
            "`expect file:` section requires a running program, so it cannot coexist with `errors`",
        );
    }
    found.into_iter().map(str::to_string).collect()
}

/// Check an `errors` section: the file must fail to compile, with exactly the
/// recorded diagnostics. `compiled` is the outcome of compiling it; `label` is
/// the path the diagnostics should name, which is part of what the section
/// records.
///
/// Rendered against `spec.source` — the text with the sections cut off and its
/// trailing newlines dropped — so a diagnostic positioned at end of file lands
/// where the section's author saw it, not on a line that only exists because the
/// file went on to hold sections.
pub fn errors(spec: &SpecFields, compiled: Result<(), Vec<Error>>, label: &str) -> Vec<String> {
    let Some(expected) = spec.errors.as_deref() else {
        return Vec::new();
    };
    match compiled {
        // Nothing to compare: the section claims an error the compiler no longer
        // reports, so the section itself is what is wrong.
        Ok(()) => vec!["expected an error, but compilation succeeded".to_string()],
        Err(errs) => {
            let actual = Error::render_all(&errs, &spec.source, label);
            mismatch("errors", &actual, expected).into_iter().collect()
        }
    }
}

/// Build `program` and run its `main` once, then check the sections that observe
/// a run: `stdout`, `stderr`, `exit code` and every `expect file:`.
///
/// `Err` is a failure to produce a run at all — a compile or link error, a
/// program with no `main`, a binary that would not start — rendered for the
/// caller to print. A run that produced the wrong output is an `Ok` holding its
/// mismatch reports.
///
/// Built ahead of time and run as a child process, the way `aipl build` would,
/// rather than JIT-run in this process: the program's stdout is the thing being
/// compared, and a JIT run writes straight to this process's own.
///
/// The run happens in a scratch directory holding the file's companions, so
/// relative reads find their fixtures and anything it writes lands there rather
/// than in the tree `check` was started from.
pub fn behavior(
    program: &Program,
    spec: &SpecFields,
    stem: &str,
    label: &str,
    dbg: DebugOptions,
) -> Result<Vec<String>, String> {
    let has_main = program
        .items
        .iter()
        .any(|it| matches!(it, Item::Fn(f) if f.name == "main"));
    if !has_main {
        return Err(
            "a file with no `main` runs only its `.test` blocks — it can't have a \
             stdout/stderr/exit/cli/expect-file section"
                .to_string(),
        );
    }
    let render = |e: Vec<Error>| Error::render_all(&e, &spec.source, label);
    let obj = ObjectCompilation::new(program, stem, dbg, false)
        .and_then(|c| c.emit().map_err(Vec::from))
        .map_err(render)?;

    let dir = binary::scratch_dir("check-run").map_err(|e| e.to_string())?;
    let result = run_in(&dir, &obj, spec, stem);
    let _ = fs::remove_dir_all(&dir);
    result.map_err(|e| e.render(&spec.source, label))
}

/// The body of [`behavior`], with `dir` as its scratch directory so the caller
/// can remove it however this returns.
fn run_in(dir: &Path, obj: &[u8], spec: &SpecFields, stem: &str) -> Result<Vec<String>, Error> {
    crate::stage_companions(dir, &spec.extra_files)
        .map_err(|e| Error::msg(format!("stage companions in {}: {e}", dir.display())))?;
    let exe = dir.join(binary::default_exe_name(stem));
    binary::link(obj, &exe)?;
    let output = Command::new(&exe)
        .args(&spec.cli)
        .current_dir(dir)
        .output()
        .map_err(|e| Error::msg(format!("run {}: {e}", exe.display())))?;

    // Trailing whitespace and CRLF are not part of what a section records: a
    // one-line expectation is that line, not that line plus a newline.
    let stdout = normalize_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = normalize_output(&String::from_utf8_lossy(&output.stderr));
    let exit = output.status.code().unwrap_or(-1) & 0xff;
    let expected_exit = spec.exit_code.unwrap_or(0) & 0xff;

    // An absent section is not "unchecked": it records empty output and a clean
    // exit, which is what makes a stray `print` show up in a file that expects
    // silence.
    let mut found: Vec<String> = [
        mismatch("stdout", &stdout, spec.stdout.as_deref().unwrap_or("")),
        mismatch("stderr", &stderr, spec.stderr.as_deref().unwrap_or("")),
        mismatch("exit code", &exit.to_string(), &expected_exit.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();

    for (rel, expected) in &spec.expect_files {
        match fs::read_to_string(dir.join(rel)) {
            Ok(written) => found.extend(mismatch(
                &format!("expect file: {rel}"),
                &normalize_output(&written),
                expected,
            )),
            Err(e) => found.push(format!("expected output file {rel:?} was not written: {e}")),
        }
    }
    Ok(found)
}
