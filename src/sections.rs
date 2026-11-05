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
//! | `check --compile` | the diagnostics compiling the file produces; the file must not compile |
//! | `check <flags>` | what `check` prints for those parts |
//! | `stdout`, `stderr`, `exit code` | one run of the built `main` |
//! | `cli` | not compared: the arguments that run is given |
//! | `report` | what `aipl report` prints for the file |
//! | `expect file: p` | the file `p` the run left in its working directory |
//!
//! `--- file: p ---` companions are not expectations; they are staged beside the
//! file by the caller so imports and fixtures resolve. `check` sections pin what `check` itself prints for some of its parts; they are compared with
//! a run of `check` for those parts (see [`projections`]).

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::ast::{Item, Program};
use crate::binary;
use crate::check::Parts;
use crate::codegen::{normalize_output, ObjectCompilation};
use crate::{DebugOptions, Error, SpecFields};
use handoff::protocol::Fill;

/// One way a file disagrees with its recorded sections.
///
/// `fix` is the section body that would make the finding go away — `(section
/// name, new body)`, in the form `aipl check --fix` writes back — when there is
/// one. A mismatch always has: the section should say what the file now does.
/// Things no rewrite can repair (an expected file that was never written, a
/// `main` that does not exist, a leak, two sections that contradict each other)
/// have none, and stay failures under `--fix`.
pub struct Finding {
    pub message: String,
    pub fix: Option<(String, String)>,
}

impl Finding {
    /// A finding with nothing to rewrite.
    pub fn unfixable(message: impl Into<String>) -> Self {
        Finding {
            message: message.into(),
            fix: None,
        }
    }
}

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
/// recorded. Its fix is the section rewritten to say `actual`.
fn mismatch(section: &str, actual: &str, expected: &str) -> Option<Finding> {
    (actual != expected).then(|| Finding {
        message: format!(
            "`{section}` mismatch\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
        ),
        fix: Some((section.to_string(), actual.to_string())),
    })
}

/// The parts of `check` that the file's own `check` sections pin. `check` does not run them in this
/// process: each section is compared with a run of `check` for just those parts (see [`projections`]),
/// and a pinned part that failed on purpose is the section's to say, not the file's to fail on.
/// `--compile` is not among them: it is validated where the file is compiled. A section with no
/// flags pins every part.
pub fn pinned_parts(spec: &SpecFields) -> Parts {
    let pinned = |flag: &str| {
        spec.checks.iter().any(|(projection, _)| {
            projection.is_empty() || projection.split(' ').any(|named| named == flag)
        })
    };
    Parts {
        format: pinned("--format"),
        compile: false,
        tests: pinned("--tests"),
        benchmarks: pinned("--benchmarks"),
    }
}

/// The header a projection is written under: `check --tests`, or `check` for all of it.
fn header(projection: &str) -> String {
    if projection.is_empty() {
        "check".to_string()
    } else {
        format!("check {projection}")
    }
}

/// Check every `check` section other than `--compile` against `run_check`, which runs `check` for
/// the named flags and gives back what it printed.
///
/// A section's body is that output byte for byte, so it records the trace values a run printed and
/// the refusal after them as well as the test rows. The status is not recorded: it follows from the
/// body.
pub fn projections(
    spec: &SpecFields,
    mut run_check: impl FnMut(&str) -> Result<String, String>,
) -> Vec<Finding> {
    let mut found = Vec::new();
    for (projection, body) in spec
        .checks
        .iter()
        .filter(|(projection, _)| projection != "--compile")
    {
        match run_check(projection) {
            Ok(output) => found.extend(mismatch(&header(projection), &output, body)),
            Err(message) => found.push(Finding::unfixable(message)),
        }
    }
    found
}

/// Sections that cannot sit together. `errors` describes a program that does not
/// compile, so a section that observes a run, or one that needs a compiled
/// program, contradicts it; the file is wrong, not the program, and neither
/// section could be checked against anything.
pub fn conflicts(spec: &SpecFields) -> Vec<Finding> {
    if spec.errors.is_none() {
        return Vec::new();
    }
    let mut found = Vec::new();
    if spec.stdout.is_some() || spec.stderr.is_some() || spec.exit_code.is_some() {
        found.push("`check --compile` section is mutually exclusive with stdout/stderr/exit code");
    }
    if spec.report.is_some() {
        found.push(
            "`report` section requires a running program, so it cannot coexist with `check --compile`",
        );
    }
    if spec
        .checks
        .iter()
        .any(|(projection, _)| projection != "--compile" && projection != "--format")
    {
        found.push(
            "`check` section for tests or benchmarks requires a compiling program, so it cannot coexist with `check --compile`",
        );
    }
    if !spec.expect_files.is_empty() {
        found.push(
            "`expect file:` section requires a running program, so it cannot coexist with `check --compile`",
        );
    }
    found.into_iter().map(Finding::unfixable).collect()
}

/// Check a `check --compile` section: the file must fail to compile, with exactly the
/// recorded diagnostics. `compiled` is the outcome of compiling it; `label` is
/// the path the diagnostics should name, which is part of what the section
/// records.
///
/// Rendered against `spec.source` — the text with the sections cut off and its
/// trailing newlines dropped — so a diagnostic positioned at end of file lands
/// where the section's author saw it, not on a line that only exists because the
/// file went on to hold sections.
pub fn errors(spec: &SpecFields, compiled: Result<(), Vec<Error>>, label: &str) -> Vec<Finding> {
    let Some(expected) = spec.errors.as_deref() else {
        return Vec::new();
    };
    match compiled {
        // Nothing to compare: the section claims an error the compiler no longer
        // reports, so the section itself is what is wrong.
        Ok(()) => vec![Finding::unfixable(
            "expected an error, but compilation succeeded",
        )],
        Err(errs) => {
            let actual = Error::render_all(&errs, &spec.source, label);
            mismatch("check --compile", &actual, expected)
                .into_iter()
                .collect()
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
) -> Result<Vec<Finding>, String> {
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
fn run_in(dir: &Path, obj: &[u8], spec: &SpecFields, stem: &str) -> Result<Vec<Finding>, Error> {
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
    let mut found: Vec<Finding> = [
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
            Err(e) => found.push(Finding::unfixable(format!(
                "expected output file {rel:?} was not written: {e}"
            ))),
        }
    }
    Ok(found)
}

/// Check a `report` section: the file's `aipl report`, as that command prints it.
///
/// The file is built both ways and run once, in a scratch directory holding its
/// companions and with `cli` as its arguments — exactly what `aipl report` does,
/// through the same function, so the section can be written by running that
/// command. A program that leaks is a finding of its own, as it is to `report`.
///
/// `Err` is a failure to produce a report at all (it does not compile for the
/// instrumented build, or cannot be linked), rendered for the caller to print.
pub fn report(
    program: &Program,
    spec: &SpecFields,
    stem: &str,
    label: &str,
    dbg: DebugOptions,
) -> Result<Vec<Finding>, String> {
    let Some(expected) = spec.report.as_deref() else {
        return Ok(Vec::new());
    };
    let dir = binary::scratch_dir("check-report").map_err(|e| e.to_string())?;
    let measured = crate::stage_companions(&dir, &spec.extra_files)
        .map_err(|e| format!("stage companions in {}: {e}", dir.display()))
        .and_then(|()| {
            crate::perf::measure_program(program, stem, &spec.cli, &dir, dbg)
                .map_err(|e| e.render(&spec.source, label))
        });
    let _ = fs::remove_dir_all(&dir);
    let stats = measured?;

    // A section body has its trailing newlines stripped, and a report ends in one.
    let mut found: Vec<Finding> = mismatch("report", stats.render_report().trim_end(), expected)
        .into_iter()
        .collect();
    if let Some(n) = stats.leaked() {
        // A report of a leaking run is not one to record: the fix would bake the
        // leak into the section, which is why the harness refuses the same thing.
        for f in &mut found {
            f.fix = None;
        }
        found.push(Finding::unfixable(format!(
            "leak: {n} of {} allocations were never freed ({} deallocations)",
            stats.allocations, stats.deallocations
        )));
    }
    Ok(found)
}

/// A baseline table to hold a file's run to: where it was read from, for the messages, and its rows.
pub struct Baseline {
    pub path: String,
    pub rows: std::collections::HashMap<String, [u64; 5]>,
}

/// Hold a file's run to its row in the baseline table: every asserted column must match
/// exactly, and a file with no row fails too. `key` is the row's name, the file's path from
/// the root without its extension; `case` is the file's path from the root, which a
/// `handoff-fill:` line names it by.
///
/// The file is measured the way the harness measures a case and `aipl report` a file: its
/// `main`, or when it has none the run of its tests, built both ways in a scratch directory
/// holding its companions and run with `cli` as its arguments. Neither finding has a rewrite
/// that `--fix` applies, because the table is one file that many files' rows share. It is
/// refreshed by whoever reads the `handoff-fill:` line.
///
/// A program that leaks has no row worth comparing (the leak is reported by the leak gate),
/// so it is left to that.
pub fn baseline(
    table: &Baseline,
    key: &str,
    case: &str,
    program: &Program,
    spec: &SpecFields,
    stem: &str,
    label: &str,
    dbg: DebugOptions,
) -> Result<Vec<Finding>, String> {
    let dir = binary::scratch_dir("check-baseline").map_err(|e| e.to_string())?;
    let measured = crate::stage_companions(&dir, &spec.extra_files)
        .map_err(|e| format!("stage companions in {}: {e}", dir.display()))
        .and_then(|()| {
            crate::perf::measure_program(program, stem, &spec.cli, &dir, dbg)
                .map_err(|e| e.render(&spec.source, label))
        });
    let _ = fs::remove_dir_all(&dir);
    let stats = measured?;
    // Every running case is held to freeing what it allocates, whether or not it pins anything;
    // a count that includes a leak is not one to compare, so no row is judged.
    if let Some(leaked) = stats.leaked() {
        return Ok(vec![Finding::unfixable(format!(
            "memory leak: {leaked} of {} allocations were never freed ({} deallocations)",
            stats.allocations, stats.deallocations
        ))]);
    }
    let Some(row) = table.rows.get(key) else {
        return Ok(vec![Finding::unfixable(format!(
            "performance baseline missing: {} has no row for `{key}`\n{}",
            table.path,
            Fill::BaselineMissing {
                case: case.to_string()
            }
            .marker()
        ))]);
    };
    let moves = crate::perf::baseline_moves(*row, &stats);
    if moves.is_empty() {
        return Ok(Vec::new());
    }
    let shown: Vec<String> = moves.iter().map(|m| format!("  {m}")).collect();
    Ok(vec![Finding::unfixable(format!(
        "performance baseline moved ({}):\n{}\n{}",
        table.path,
        shown.join("\n"),
        Fill::BaselineMoved {
            case: case.to_string(),
            moves
        }
        .marker()
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(checks: &[(&str, &str)]) -> SpecFields {
        SpecFields {
            checks: checks
                .iter()
                .map(|(projection, body)| (projection.to_string(), body.to_string()))
                .collect(),
            ..SpecFields::default()
        }
    }

    #[test]
    fn a_section_pins_every_part_its_header_names_and_never_the_compile() {
        let pinned = pinned_parts(&spec(&[("--tests --benchmarks", ""), ("--compile", "")]));
        assert_eq!(
            (
                pinned.format,
                pinned.compile,
                pinned.tests,
                pinned.benchmarks
            ),
            (false, false, true, true)
        );
    }

    #[test]
    fn a_section_with_no_flags_pins_every_part() {
        let pinned = pinned_parts(&spec(&[("", "")]));
        assert_eq!(
            (
                pinned.format,
                pinned.compile,
                pinned.tests,
                pinned.benchmarks
            ),
            (true, false, true, true)
        );
        assert!(!pinned_parts(&spec(&[])).tests);
    }

    #[test]
    fn each_section_is_compared_with_the_run_for_its_own_flags() {
        let mut asked = Vec::new();
        let found = projections(
            &spec(&[("--tests", "same"), ("--format", "other")]),
            |flags| {
                asked.push(flags.to_string());
                Ok("same".to_string())
            },
        );
        assert_eq!(asked, ["--tests", "--format"]);
        assert_eq!(found.len(), 1);
        assert!(found[0].message.starts_with("`check --format` mismatch"));
        assert_eq!(
            found[0].fix,
            Some(("check --format".to_string(), "same".to_string()))
        );
    }

    #[test]
    fn a_section_with_no_flags_is_written_as_bare_check() {
        let found = projections(&spec(&[("", "old")]), |_| Ok("new".to_string()));
        assert_eq!(found[0].fix, Some(("check".to_string(), "new".to_string())));
    }

    #[test]
    fn a_run_that_could_not_start_is_a_finding_without_a_fix() {
        let found = projections(&spec(&[("--tests", "")]), |_| Err("no binary".to_string()));
        assert_eq!(found.len(), 1);
        assert!(found[0].fix.is_none());
    }
}
