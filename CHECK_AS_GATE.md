# `aipl check` as the whole case gate: what is left

Status as of 2026-11-04. Done and removed from this list (git has the history): effect-free tests
(Phase 2a), forbidding imports of files that have sections (the earlier Phase 3), top-level `test`/`benchmark` blocks, the
`.performance` to `.benchmark` rename, root-relative `./` labels (DESIGN_PRINCIPLES.md §6 and §7), Phase 1 (the
parity gaps) and Phase 2b (reserving `main`); see "Done" below.

Each phase lands green via `cargo handoff` and is worth merging on its own.

**New direction (2026-11-03): the sections that are `check`'s own output become `--- check .. ---` sections.** See
"Direction" below and Phase 3. It changes the order of the remaining work: Phase 3 (those sections are `check` invocations), then the batch runner and the flip, which get smaller because
of it. The sections that observe a run of `main` (`stdout`, `stderr`, `exit code`, `expect file:`) and the inputs
(`cli`, `file:`) are not part of it and stay as they are.

## Goal
Validate the whole corpus (`tests/cases/**`, `crates/**`, `examples/`) in a few long-lived,
sharded `aipl check` processes instead of ~1183 per-case nextest processes, with `aipl check`
the single definition of "this file is correct" for the dev loop, handoff and user projects.
Success is measured against the current handoff **discovery run: ~410-460s** (a clean full
handoff: ~500s), taken in the same launch environment as handoff really runs in.

## Direction: the sections that are `check`'s output become `check` sections

**The idea.** `aipl check` is the whole gate (DESIGN_PRINCIPLES.md §2), and some of a file's expectations are
expectations about what `check` itself prints: the diagnostics a file must produce (`errors`), the output of its
tests (`check`, `check exit code`), the formatter's result (`formatted`). Those become one concept: a section
`--- check [flags] ---` whose body is what `aipl check [flags]` prints for that file. The flags are the CLI's
own, so a section is a subset of the gate named the way a person would run it.

**What does not change (decided).** `check` is for validations; `run` is for using the project. A file with
`stdout`, `stderr`, `exit code`, `expect file: p`, `cli` and `file: p` sections is a *test of what `main` does*,
and `check` already treats it as one: it runs `main` and compares. Those sections keep their names and
meaning, and `check` grows no `--run` flag. `aipl report` and `--- report ---` stay too: a report is a
measurement and not a validation, so it stays out of `check`; the section is a test of it, validated by `check` like
`stdout` is (decided).

**Why it is worth a multi-stage refactor.**
- One vocabulary for the sections that describe `check`'s own output: the flags on the command line and the
  headers in the files are the same words, so the reference is `aipl check --help`.
- The harness shrinks toward "run `check`, compare". Today `tests/cases.rs` (2100 lines) and `src/sections.rs`
  validate the same sections twice, differently; that is the parity gap list in Phase 1.
- Formatter fixtures stop being a second mechanism with its own runner (`tests/suites/fmt.rs`).
- `--fix`, the `handoff-fill:` lines and `AIPL_CASE` address a projection of `check`, not a zoo of kinds.

### Inventory: what exists today
| section | files | pins | validated by | proposed form |
|---|---|---|---|---|
| `errors` | 273 | compile diagnostics, rendered | `sections::errors` (check), `run_error_case` (harness) | `check --compile` |
| `check` | 7 | stdout of the *tests* run only | the harness only (`check_plan`) | `check --tests` (the bare name changes meaning, see DS6) |
| `check exit code` | 4 | that run's status | the harness only | gone: derived from the body (DS4) |
| `formatted` (in `tests/fmt`) | 27 | expected `format_source` output | `tests/suites/fmt.rs` only | deleted (DS5b) |
| `report` | 43 | `aipl report` text | `sections::report`, harness, the `report_agrees_with_the_pinned_report_*` tests | **unchanged** |
| `stdout` | 73 | the run's stdout | `sections::behavior`, `run_success_case` | **unchanged** |
| `stderr` | 1 | the run's stderr | same | **unchanged** |
| `exit code` | 12 | the run's status | same | **unchanged** |
| `expect file: p` | 2 | a file the run wrote | same | **unchanged** |
| inputs `cli` 5, `file: p` 37 | | args, companion files | staged by `in_staged_dir` | **unchanged** |

424 of 1157 `.aipl` files carry sections; none of them are under `crates/`, so the compiler's own sources never
parse a section (the dogfooded section parsers are not on the bootstrap path of the compiler's own build). About
311 files change under this design (273 + 11 + 27).

Commands today (`usage()` in `src/main.rs`): `run`, `ir`, `doc`, `docs`, `build`, `check`, `report`, `lsp`.
Formatting is already `check --fix` (there is no `fmt` command; `aipl::fmt::format_source` is the library the
LSP and the tests call). `run`, `build`, `ir`, `doc`, `docs`, `lsp` and `report` are not subsets of `check` and
stay.

### Proposed design
- **A `check` section is a projection.** `check` builds one structured result for a file (formatting findings,
  compile diagnostics, test rows and failures, benchmark outcomes, the run, leak and baseline findings). Each
  flag selects parts of it and a renderer prints them. A section's body is that rendering for the file alone (no
  file heading, trailing newlines stripped), compared byte for byte. One structured result serves every section of
  the file, so a case with several sections costs one `check`, not several.
- **A section never validates itself.** The result is computed with the file's sections ignored (the existing
  `--ignore-sections` rule), then each section is compared to its projection. The run-observing sections are
  compared against the run in the same result, as `sections::behavior` does now.
- **Exit status** is derived for the check projections (non-zero exactly when the projection reports a
  failure), so `check exit code` disappears. The program's own exit code stays an `exit code` section.
- **One output stream** for everything `check` reports about a file, in a fixed order, so a section can pin
  diagnostics (DS3). Today tests go to stdout and diagnostics, mismatches, format and trace notices to stderr.
- **The baseline table stays central** (`tests/performance_metrics.md`): a codegen change costs one changed
  file, which per-file sections would not. `check --baseline` compares against it (G1); it is not a section.
- **Not scheduled: a `--sections` flag**, the complement of `--ignore-sections`: validate a file's sections and
  skip formatting, lint and the rest. It falls out of the projection model (a selector that names the
  expectations and nothing else); define it after stage 3.3 if it is wanted.

### Decisions to settle before Phase 3 starts
Each has a recommendation; none is made yet.
- [x] **DS1 header grammar.** `--- check [flags] ---`, flags exactly as on the command line, space separated,
      written in a canonical order so one projection has one spelling; an unknown flag, or the same projection
      twice, is an error (the existing rule that a malformed header is the file's error, not a dropped
      expectation). Recommended.
- [x] **DS2 projections and flag names.** Selector flags choose what `check` prints and verifies; none given means
      everything. Recommended set: `--format`, `--compile` (diagnostics), `--tests`, `--benchmarks`, plus
      `--baseline` and `--leaks` when G1/G2 land. No `--run`, no `--report`. `--fix` and `--ignore-sections` keep their
      meaning. Alternative: one full-output section per file (simpler, but any unrelated rendering change churns
      every file). Rejected for churn; sections should pin narrow projections.
- [x] **DS3 one stream.** All of `check`'s report on a file goes to stdout in a defined order; stderr is for
      usage and I/O errors only. Costs: about 40 expectations in `tests/suites/check.rs` and the convention for
      `trace` refusals and "needs formatting" notices. Recommended.
- [x] **DS4 exit status.** Derived for check projections; no separate section. Alternative: a trailing
      `exit: N` line. Recommended: derive.
- [x] **DS5 `report` (decided).** It is a measurement, not a validation: `aipl report` and `--- report ---` stay as they
      are and `check` gains no `--report`. The report tests and the `report` rows of the migration are out of this
      project.
- [x] **DS5b formatter fixtures (decided: no `--fix` in a section header).** Formatting is covered by every `.aipl` in the tree having to be canonical (`all_aipl_files_stay_formatted`), so the 27 `tests/fmt` fixtures, `fixtures_match_expected` and `fill_expected_fmt` are deleted rather than moved. Not adopted: `check --format --fix` as a section, which
      would have meant "the file after the fix", the one projection whose body is a file and not a report.
- [x] **DS6 the stale `check` name.** Bare `--- check ---` becomes "everything", so the 7+4 existing
      `check`/`check exit code` files must move to `check --tests` first (stage 3.8 orders it). Recommended.
- [x] **DS7 editor and LSP support (decided: none for now).** Nothing new is added; what works today keeps working, which
      is that the editor and LSP do not report sections as errors to be fixed.

## Background
Two separate costs sit under "process startup":
1. **Per-test process overhead** (nextest process, test-binary start, ~0.2s dogfood engine
   load; ~0.9s overhead per ~0.6s of work). In-process batching removes it.
2. **macOS fresh-executable assessment** (~0.19s per newly linked exe, serialized, ~1.7 exes
   per case, ~350s floor; ~2ms under iTerm; see memory `project_handoff_slow_exec_assessment`).
   Batching alone does not remove it; fewer `main`s do (Phase 2b is what cut them).

Facts that shape the remaining phases:
- `aipl check` already validates `stdout`, `stderr`, `exit code`, `errors`, `report`,
  `expect file:`, `cli`, `file:` companions (`src/sections.rs`, `check_file` in `src/main.rs`),
  and `--fix` writes them with the same `fill_or_add_sections_file` as `fill_expected`.
- `.test` blocks run in an in-process JIT; `main` and `report` run as linked executables.
- Serial `aipl check crates` took ~123s for 144 files (~0.85s/file, compute not startup), so a
  serial single process is **not** faster than parallel nextest: the batch must be sharded.
- `check` is serial in-process (`env::set_current_dir`, global test tallies, global shim slots).
  Tests no longer have real effects, so the JIT test run no longer needs a scratch cwd or staged
  companions; only `main` runs do. That is most of what made `set_current_dir` necessary.
- `check` runs `main` only when a run-observing section (`stdout`, `stderr`, `exit code`, non-empty
  `cli`, `expect file:`) exists; there is no implicit "silent and exit 0" assertion.

Non-goals (decided): crashes and runaway JIT are not mitigated (treated as top-priority bugs; fail
loudly); running `main` under the JIT is rejected (the AOT linker runtime is the product).

## Open decisions
- [x] **D3 (Phase 1):** the project root is the directory the tool is run in; no marker file (see G4).
- [ ] **D4 (end of Phase 0):** the speedup that justifies Phases 4-5 (suggestion: discovery well under
      half of the current number).
- [x] **DS1-DS7:** all settled (see "Decisions" above).
- [ ] **D7:** how, if at all, to enforce "`main` only for programs, real-builtin coverage and
      entry-point mechanics". Directory-based enforcement was rejected; no mechanism chosen. Until then it
      is held by review.

## Phase 0: measure and decide (no code)
- [ ] Time, over ~100 sampled cases, each step separately: per-test overhead, load/lint/mono,
      JIT, clang link, exec. Use `aipl check <dir>` (batch mode exists) and
      `scripts/flamegraph_check.sh`.
- [ ] Repeat in both launch environments (iTerm-exempt terminal; backgrounded Claude session).
- [ ] Check what "binary size" in `aipl report` reads (object vs linked exe).
- [ ] Check that a `Compilation` frees its JIT code on drop (memory growth in a long process).
- [ ] Write the timing table into this doc; resolve D4; go/no-go for Phases 4-5.

## Done: Phases 1 and 2b (2026-11-04), and what they leave behind
Git has the history; these are the facts later phases lean on.
- **Baseline (G1).** `aipl check --baseline <table.md>` holds each file's run (its `main`, or its tests' driver when
  it has none, measured as `aipl report` does) to its row, the five asserted columns exactly, and prints
  `BaselineMoved`/`BaselineMissing` as `handoff-fill:` lines. Opt-in, so the tool does not carry this repository's
  table path. Rows are keyed by the path from the root without `.aipl` (`tests/cases/arrays/slice`);
  `perf::baseline_moves` is the one comparison, shared with the harness. `--fix` does not touch the table (many
  files share it); the reader of the fill line rewrites it. Open for Phase 4: skip the second measurement when the
  leak gate already ran the same program.
- **Leak gate (G2).** `check` runs the tests' synthesized driver once, instrumented, for every file with tests, and
  `main` too when it is being run anyway because the file records what it does; either leaking fails the file. A
  plain program's `main` is never run (it may be a server), so a `main` with no run-observing section is not gated
  by `check`; the harness still measures every case's `main`. Until the harness delegates (3.7) a case's tests are
  built and run twice.
- **Rooting (G4, D3).** The project root is the directory the tool is run in; no marker file. Harness, `check` and
  `aipl report` all label a file `./path/from/root.aipl` (loader errors, caret blocks, baked `assert`/`trace`). A
  file with `file:` sections is staged under `<out>/<case>/case-root/` at the path it has in the repository.
  Not done: the report == pinned section check from a second cwd (a report differs by design from another directory).
- **`file:` on an in-place source (G5)** is not an error in `check` (the rule would put this repository's directory
  names into the tool users run); both stage any file with `file:` sections.
- **Fill lines (G6).** A section finding that a rewrite repairs prints `Fill::Section` on its own stderr line after
  the message, with the file's path from the root as the case; findings no rewrite repairs print none. The names are
  not yet the harness's display paths (`cases/x/y`); Phase 5 settles that.
- **Which functions `check` compiles (G7).** Same as the harness: load, then `Compilation::new`. Errors found while
  instantiating generic code are found only for code reachable from a root (`main` or a test), under both. Fixing
  that belongs to checking generic functions at their definition (`CLAUDE_TODO.txt`). Fixtures that pin such an
  error keep a root.
- **Dropped:** the differential test (the corpus would run twice) and the canary set (`tests/suites/check.rs` has a
  failing fixture per rejection).
- **`main` is reserved** (CLAUDE.md, "Where `main` may appear"). 47 files keep a `main` with sections, all
  deliberate: entry mechanics and real effects (`cli/`, `results/`, `functions/exit_code`,
  `functions/void/main_returns_nothing`, `execute_program/`, `files/`, `builtins/{read_*,terminal,now_nanos,
  monotonic_now}`, `debug/trace`), whole-program `report` pins (`report/`, `optimizations/inline_inspected`,
  `strings/concat/print_rope`), `strings/int_parse`, `effects/{shim_clock,random,random_provider_block}`, and 17
  `err_` fixtures whose messages change when `main` goes. `tests/cases/lints/` holds only the two squelching tests.
- **Converting a print-only `main`** (the shape if one is ever needed): prefer a returned value and an assert; if the
  printing is the behavior, capture it with `shim prints { print = .., put = .. }` in a top-level `test({ .. })` and
  assert the captured text. Verify a fixture change by compiling the file and reading the errors it prints, never
  through `--fix` (it leaves a section alone when the file compiles).
- Open follow-ups are in `CLAUDE_TODO.txt`.

## Phase 3: `check`'s own output is written as `check` sections
Order matters, and the reasons are in "Why this order" below. Each numbered item is a stage that lands green on
its own; the corpus migration (3.9) is one handoff per kind. The run-observing sections and the inputs are not
touched anywhere in this phase except where `check` reads them into the structured result.

**3.1 Structure first (no behavior change)** (2026-11-04)
- [x] `check` produces a `CheckResult` per file (`src/check.rs`): the formatting verdict (`Formatting`), the tests'
      rows, and either a rendered load/compile failure or a `Checked` (driver exit code, traces, section findings
      left after `--fix`, the sections `--fix` wrote and why it could not, whether tests ran). `check_path` in
      `main.rs` produces it and prints only the formatting line, first, so it still precedes whatever the run
      prints; the single-file and batch modes then print the result in the orders they always used (the batch
      prints `fixed` after the rows, the single report before them).
- [x] Renderers, one per part, in `check.rs` with unit tests: `render_test_rows`, `render_findings` (with the
      `handoff-fill:` line), `Formatting::line`, `CheckResult::render_fixed` and `render_summary`. Compile
      diagnostics and traces are already rendered strings.
- [x] The runtime's tallies are data too: `aipl::codegen::take_test_run()` hands over a `TestRun` (records, counts,
      failure text) and starts the next file from zero; the global totals, `take_failure_report` and the
      `add_*` helpers are gone. A file's `.benchmark` blocks and leak checks are added to its own `TestRun`, and a
      batch is `BatchTotals`, the sum of its files' results, rendered by `BatchTotals::render` (tested). One
      change in behavior, a fix: the driver's exit code used to be cumulative over a batch, so a file after a
      failing one reported a failing driver.

**3.2 One output stream (DS3)** (2026-11-04)
- [x] Compile diagnostics, section mismatches (with their `handoff-fill:` lines), format notices, trace refusals and
      `fix` lines are all on stdout. stderr is for usage and I/O errors only (no such path, an unreadable
      baseline table, an empty tree). The order is fixed, one for every mode:
      1. the formatting notice, first, because it is reported before the file runs;
      2. whatever the run itself prints (`trace(..)` values);
      3. the file's rows, then either its compile failure or (what `--fix` wrote, the `trace` refusal, each section
         finding), as `CheckResult::render`;
      4. after the last file: the failing tests' text under `test failures:`, then the count line (a batch's
         `N files, T tests` line and its one line per way a file failed).
      Moved from the old single-file order: the `fixed` line is now after the rows (as in a batch), and the
      traces and findings are now before the failure text and the summary instead of after.
- [x] Expectations updated: `tests/suites/check.rs` (37 tests), `fmt::cli_check_formats_with_fix`, and the
      `--- check ---` section of `tests/cases/debug/trace_fails_check.aipl`, which now also pins the diagnostic.
      The handoff reads `handoff-fill:` lines from stdout.

**3.3 Selector flags (DS2)** (2026-11-04)
- [x] `--format`, `--compile`, `--tests`, `--benchmarks` (`Parts` in `src/check.rs`): composable, none means all. No
      `--run`, no `--report`. `--format` is the canonical-format check alone and does not load the file. `--compile`
      loads and compiles (syntax and type errors, lints, the benchmark refusals, the `trace` refusal) and runs
      nothing. `--tests` and `--benchmarks` also run those blocks, and report compile failures too, since a file
      that does not compile has nothing to run. The tests' leak gate is part of `--tests`. One test per flag and
      for the combinations in `tests/suites/check.rs`; `usage()` and the long help describe them.
- [x] A selector leaves the file's recorded sections and `--baseline` out (they describe a run of `main` and of the
      whole file, which no selector names), so naming a part implies `--ignore-sections`.
- [x] `--fix` with a part named: the only thing there is to rewrite is formatting, so it applies to `--format`;
      sections are repaired only by an unselected run.
- [x] Exit status: non-zero exactly when a chosen part reports a failure (unformatted, a compile diagnostic, a
      failing test or benchmark, a leak, a `trace`).

**3.4 The header grammar and the data model (additive; old names keep working)** (2026-11-04)
Settled with it, from the recommendations: DS1 (the header is `--- check <flags> ---`, one canonical spelling per
projection), DS4 (exit status derived; no section for it) and DS6 (bare `--- check ---` keeps its old meaning,
`--tests`, until 3.8 migrates the 7+4 files).
- [x] `parse_spec` (`parse_spec.aipl`) accepts `check` plus flags. The flags are `--format --compile --tests
      --benchmarks`, each once, in that order, single-spaced; an unknown flag, a repeat, any other spelling and two
      sections for one projection are errors that say what to write. Flags on any other section stay "unknown
      section". Tests in the AIPL file and `tests/suites/parser.rs`.
- [x] `Spec.checks: Check[]` is the ordered list of `(projection, body)`; `errors` is `--compile` and bare `check` is
      `--tests`, so the old names are in the same list. Additive: `errors`, `check` and `check_exit_code` keep their
      fields, and `check --compile` / `check --tests` also fill the field of their old spelling, so the existing
      validation holds them. Rust: `SpecFields.checks` and its FFI marshalling. Not touched until 3.7: the harness's
      own `Spec` in `tests/cases.rs`.
- [x] A `check` section for any other projection is reported by `check` ("not validated yet", `sections::
      unvalidated_checks`) instead of being parsed and ignored. 3.5 deletes this guard when it validates them.
- [x] `fill_or_add_sections` finds and adds a section by its canonical header (tested). `parse_test_section_header`,
      `split_test_sections`, `strip_test_sections` and `companion_files` treat a flagged header like any other
      (tested in each file), and so do the loader's refusal to import a file with sections (names the header as
      written; `check.rs`) and the formatter (preserves the tail byte for byte; `fmt.rs`).
- Shipped in two handoffs because the live artifact's `Spec` had no `checks` field: the AIPL change first (the Rust
  side looks fields up by name, so the extra one was harmless), then the Rust side that reads it.

**3.5 Validation of the new sections in `check`** (2026-11-05)
Settled before it, from the recommendations or by decision: DS1, DS2, DS3, DS4, DS6 as recommended; DS5b no `--fix` in a
section header, and the 27 `tests/fmt` fixtures, `fixtures_match_expected` and `fill_expected_fmt` are deleted
(formatting is held by every `.aipl` in the tree having to be canonical); DS7 no editor or LSP support for now.
- [x] Each `check` section is compared with what `check` prints for its flags with the file's sections ignored
      (`sections::projections`). It is a child run, `aipl check --ignore-sections <flags> <file>` from the project root,
      the way the harness did it: a `trace` value goes straight to the process's stdout and is part of what a section
      records, so an in-process render could not hold it. A file's `--compile` section stays on the in-process path
      (`errors`). Cost: one child per pinned projection, which is 7 + 2 files today.
- [x] The parts a file pins are left out of its own run (`sections::pinned_parts`, `Parts::without`): a section that
      records a failing run is not the file failing, and a trace refusal belongs to the run that is pinned.
- [x] `conflicts()` knows the new forms: a `check` section for the tests or benchmarks cannot sit with `errors`.
- [x] `--- check --tests ---` is the replacement for the old `check`. Bare `--- check ---` is not "everything" yet: it keeps
      meaning `--tests` until 3.8.
- [x] `--fix` rewrites and adds them, under the header the file has (a file with the old bare `check` is rewritten
      under that name). The old `check exit code` is compared with the status of the `--tests` run; the new sections
      have no status section.
- [x] Old names are aliases: both spellings validate through the same code (tested side by side).
- [x] Deleted the "not validated yet" guard (`unvalidated_checks`).
- Two fixtures that pinned a benchmark failure under the old `check` (`benchmark/block_failure`,
  `benchmark/measure_claim_fails`) moved early to `--- check --tests --benchmarks ---`, since the old name now means
  `--tests` alone. The harness (`check_plan`) counts any `check` section as a pin, so it no longer requires those cases'
  `check` run to succeed; 3.7 deletes that code.

**3.6 Prove the two validators agree**
- [ ] Differential test over the whole corpus: the old harness verdict equals the new `check` verdict for every
      file (pass or fail, and the message), and `--fix` produces byte-identical files and baseline rows.
- [ ] A migrated twin of every old-style file validates identically to the original (the migration script's
      acceptance test, run before any file is written).
- [ ] Canaries, per projection: wrong body, missing section for output the file produces, unknown flag, duplicate
      projection, a section that contradicts another, a section on a file that does not compile.

**3.7 The harness delegates (`tests/cases.rs`)**
- [ ] Replace its per-section code (`run_error_case`, the section checks in `run_success_case`, `check_plan`,
      `run_benchmark_check`, `check_or_fill`'s per-name logic) with one `aipl check` run per case, for the new
      sections and the run-observing ones alike (`check` already validates the latter). It keeps the baseline
      compare and `refresh_perfmon` until G1 moves them into `check`.
- [ ] `fill_expected` becomes a wrapper over `check --fix`, scoped by `AIPL_CASE`.

**3.8 Make the old `check` name free**
- [ ] Migrate the 7 `--- check ---` and 4 `--- check exit code ---` files to `--- check --tests ---` first; the
      exit code is derived, so the second section goes. Only then does bare `--- check ---` mean everything.

**3.9 Migrate the corpus, one kind per handoff, by script**
Each script rewrites headers, runs both validators and refuses to write a file whose verdict or `--fix` result
changes (the lesson from the error-fixture pass: a refill pins whatever the compiler now says). Each leaves the
differential test green.
- [ ] `errors` (273) to `check --compile`.
- [x] ~~The `formatted` fixtures to `check --format --fix`~~ Not migrated (DS5b): they are deleted in 3.5.
- [ ] Files with these sections outside `tests/cases/` and `tests/fmt` (`examples/`, `projects/`).

**3.10 Remove the old names**
- [ ] The parser refuses `errors`, `check exit code` and `formatted` with a message naming the replacement, as
      `.performance` does. `stdout`, `stderr`, `exit code`, `expect file`, `report`, `cli` and `file:` are not touched.
- [ ] Delete the alias layer, the old `Spec` fields, the old `sections.rs` paths for these and the harness leftovers.
- [ ] A guard test that no retired section name remains anywhere in the tree.

**3.11 Commands**
- [ ] Confirm `check --fix` is the only formatting entry (the `aipl::fmt::format_source` library stays for the LSP
      and tests), and that `usage()`, `--help` and the long help in `main.rs` describe the flags. `aipl report` is
      untouched.

**3.12 Handoff protocol**
- [ ] `Fill::Section { case, section }` carries the canonical header for the new sections;
      `handoff/src/discovery.rs` and its unit tests, `scoped_fill_cmd`, and the `handoff-fill:` lines `check` prints
      (G6) follow.

**3.13 Docs and tooling**
- [ ] CLAUDE.md: Handoff, formatting and `--fix`, "Prefer the cases framework", "Authoring error-case fixtures"
      (`--- errors ---` becomes `--- check --compile ---`), the benchmark fixtures paragraph, and the Performance
      monitoring text where it names `check` sections.
- [ ] DESIGN_PRINCIPLES.md §2: `check`'s own output is writable as `check` sections; one stream; the projections;
      and that run-observing sections make a file a test of `main`.
- [ ] README, `projects/README.md`, the skills (`write-aipl`, `add-lint`, `add-builtin`), the headers of
      `tests/cases.rs` and `tests/suites/fmt.rs`, `aipl --help`.
- [x] ~~Optional (DS7)~~ Not done: no editor or LSP support for now.

**3.14 Done when**
- [ ] The corpus holds only `check ..` sections, the run-observing sections, `report`, `cli` and `file:`; the guard
      test is green.
- [ ] `cargo handoff` green, then once more with a refill, to show `check --fix` and the fill lines end to end.

**Why this order.** 3.1 first because every later stage reads or renders the structured result, and Phase 1's
parity gaps (leak, baseline, the harness/`check` compile difference) become parts of it. 3.2 before the flags
because a section cannot pin output that goes to two streams. The header grammar and validation (3.4, 3.5) are
additive and aliased so the old and new spellings can be compared (3.6) before anything in the corpus moves,
and the harness delegates (3.7) while both spellings still exist. The old `check` name is vacated (3.8) before the
bare name is given a new meaning. Corpus moves (3.9) come last and are mechanical, with the old names refused
only after the whole tree has moved (3.10), so a half-migrated tree is never green by accident.

**Risks specific to this phase**
- The one-stream change (3.2) alters what every user of `aipl check` sees, including the projects that use it as
  their gate; it lands alone so a moved test has one cause.
- Migration churn: about 311 files change. Keep it to scripts that verify, one kind per handoff.
- During 3.4-3.9 two validators exist. The differential test is what stops them drifting; do not skip it.
- Section bodies embed root-relative paths (DESIGN_PRINCIPLES.md §7): a moved file changes its pinned text, as it
  does for `errors` today. Not new, but now true of every `check` section.
- Which functions `check` compiles (G7) decides what `--compile` can report. Decide it in Phase 1, before the projection
  is built.

## Phase 4: batch runner
- [ ] `aipl check --shard i/n` over a file list; deterministic assignment.
- [ ] Progress line naming the file in flight (a crash must say where it died).
- [ ] One writer for `tests/performance_metrics.md`: shards report rows; handoff (or an unsharded
      `check --fix`) merges.
- [ ] Memoize pure loader work (parse+lint of builtins and shared crate files) across files in a
      process; profile first.
- [ ] Audit the process-global state a batch run leans on. The JIT's cached constants and shim cells leaked
      between files and are fixed (`forget_jit_constants`); shim slots, test tallies and `set_current_dir` are
      still unaudited.
- [ ] Shuffled-order test: results identical to the fixed order (cross-file state leakage).
- [ ] Re-measure against the Phase 0 table; record in this doc; confirm D4 still holds.

## Phase 5: flip
The harness already delegates its section checking to `check` (Phase 3, 3.7), so the flip is about the per-case nextest
structure and handoff's scoping, not about section code.
- [ ] Handoff runs the shards and reads `handoff-fill:` lines; replace `case_test_names_from`,
      `final_run_scope_from`, `scoped_filter_expr` with a scoped `aipl check <stale paths>`.
- [ ] `settle_baseline` uses the instrumented run's five asserted columns; `refresh_perfmon` stays
      a separate command for wall-clock/RSS.
- [ ] Keep the old per-case harness behind `--ignored` for one cycle as the cross-check.
- [ ] Delete `tests/support/case_tests.rs`, `fill_case_tests`, `every_case_has_a_test`; fold
      `fill_expected` into `check --fix`.
- [ ] Update CLAUDE.md (Handoff, Test cadence, cases sections) and skills naming `fill_expected`.
- [ ] A clean `cargo handoff`, then one that needs a refill, to show the `handoff-fill:` path.

## Phase 6 (only if Phase 4 measurements demand it)
- [ ] Link a shard's instrumented objects into one executable and `fork` per case, so exec
      assessment is paid once. Needs per-case symbol prefixing in `ObjectCompilation`.

## Must keep running, unchanged
`fmt::all_aipl_files_stay_formatted` (also covers `projects/`, `tests/ffi_fixtures`),
`fmt::no_pinned_file_names_a_path_on_one_machine`, the five `report_agrees_with_the_pinned_report_*`
tests, `compiler_aipl_files_are_tested`, `docs_site`, `checked_in_ir_is_current`.

## Risks to watch
- Cross-file state (shim slots, counters, allocator stats, thread-locals) making results order-dependent.
- JIT memory growth across a long batch; recycle shards by file count if it does not free.
- Losing nextest names/filters/retries/timeouts per case: the dev-loop replacement is
  `aipl check <path>`.
- Baseline rows shift once from rooting and again per converted directory; keep these in separate
  handoffs so a moved row has one cause.
- Phase 3 touches the section parsers that `check --fix` calls through the dogfood artifact: keep every change
  additive until the corpus has moved, and expect `checked_in_ir_is_current` to want a regeneration each time.
- The cases harness stages a case alone into a scratch dir with only its `file:` companions, so a case can
  never import a real sibling file.
