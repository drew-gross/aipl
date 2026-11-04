# `aipl check` as the whole case gate: what is left

Status as of 2026-11-03. Done and removed from this list (git has the history): effect-free tests
(Phase 2a), forbidding imports of files that have sections (the earlier Phase 3), top-level `test`/`benchmark` blocks, the
`.performance` to `.benchmark` rename, root-relative `./` labels (DESIGN_PRINCIPLES.md §6 and §7), and most of
Phase 2b, which reserved `main` for programs and tests of the real implementation of an effect.

Each phase lands green via `cargo handoff` and is worth merging on its own.

**New direction (2026-11-03): the sections that are `check`'s own output become `--- check .. ---` sections.** See
"Direction" below and Phase 3. It changes the order of the remaining work: Phase 1 (parity gaps) first, then
Phase 3 (those sections are `check` invocations), then the batch runner and the flip, which get smaller because
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
| `formatted` (in `tests/fmt`) | 27 | expected `format_source` output | `tests/suites/fmt.rs` only | `check --format --fix` (DS5b) |
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
- [ ] **DS1 header grammar.** `--- check [flags] ---`, flags exactly as on the command line, space separated,
      written in a canonical order so one projection has one spelling; an unknown flag, or the same projection
      twice, is an error (the existing rule that a malformed header is the file's error, not a dropped
      expectation). Recommended.
- [ ] **DS2 projections and flag names.** Selector flags choose what `check` prints and verifies; none given means
      everything. Recommended set: `--format`, `--compile` (diagnostics), `--tests`, `--benchmarks`, plus
      `--baseline` and `--leaks` when G1/G2 land. No `--run`, no `--report`. `--fix` and `--ignore-sections` keep their
      meaning. Alternative: one full-output section per file (simpler, but any unrelated rendering change churns
      every file). Rejected for churn; sections should pin narrow projections.
- [ ] **DS3 one stream.** All of `check`'s report on a file goes to stdout in a defined order; stderr is for
      usage and I/O errors only. Costs: about 40 expectations in `tests/suites/check.rs` and the convention for
      `trace` refusals and "needs formatting" notices. Recommended.
- [ ] **DS4 exit status.** Derived for check projections; no separate section. Alternative: a trailing
      `exit: N` line. Recommended: derive.
- [x] **DS5 `report` (decided).** It is a measurement, not a validation: `aipl report` and `--- report ---` stay as they
      are and `check` gains no `--report`. The report tests and the `report` rows of the migration are out of this
      project.
- [ ] **DS5b formatter fixtures.** `check --format --fix` as a section means "the file after the fix", the one
      projection whose body is a file and not a report, so the 27 `tests/fmt` fixtures move into the same
      mechanism and `fmt.rs`'s fixture runner goes. Alternative: leave formatter fixtures outside the scheme.
      Recommended: move them in.
- [ ] **DS6 the stale `check` name.** Bare `--- check ---` becomes "everything", so the 7+4 existing
      `check`/`check exit code` files must move to `check --tests` first (stage 3.8 orders it). Recommended.
- [ ] **DS7 editor and LSP support** for section headers (highlighting, a diagnostic for a malformed header):
      optional, after the rest.

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
- [ ] **D3 (Phase 1):** what marks the project root, so rooting does not depend on the cwd. A directory
      under `projects/` is its own project with its own root, not the repository's (DESIGN_PRINCIPLES.md
      §7), so the marker has to find `projects/word_count` and not walk up to the repository.
      `aipl.build` sits at each project's root today.
- [ ] **D4 (end of Phase 0):** the speedup that justifies Phases 4-5 (suggestion: discovery well under
      half of the current number).
- [ ] **DS1-DS7:** see "Direction" above (DS5 is decided); settle the rest before Phase 3 starts.
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

## Phase 1: close the parity gaps in `check`
Each gap gets a failing-fixture test in `tests/suites/check.rs` (model:
`the_leak_gate_runs_for_a_case_with_no_sections`).
- [ ] G1 baseline row compare vs `tests/performance_metrics.md` (5 asserted columns), with
      `BaselineMoved`/`BaselineMissing` outcomes.
- [x] G2 leak gate (2026-11-03). `check` runs the tests' synthesized driver once in an instrumented build for every
      file with tests, and `main` too when it is being run anyway because the file records what it does; either
      leaking fails the file (`test memory leak in tests|main ... FAIL`). A plain program's `main` is never run
      (it may be a server or read a terminal), so a `main` with no run-observing section is not gated by `check`.
      Open: the harness still measures `main` of every case, including `examples/` with no sections; to move that
      into `check`, such a file would need a section (even `--- exit code ---` / `0`), which makes it a test of
      `main` and so safe to run. While the harness also measures, a case's tests are built and run twice (once
      in `check --ignore-sections`, once by the harness); that duplication ends when the harness delegates (3.7).
- [x] ~~G3 `check` / `check exit code` sections~~ subsumed by Phase 3 (a `check` section is validated by the same
      mechanism as every other section).
- [ ] G4 root the cases harness at the project root (resolve D3). Today it roots each case at its own
      directory, so a loader error opens with `./err_x.aipl` while its caret block says
      `./tests/cases/…/err_x.aipl`: two roots in one message. Rooting moves every baked `assert`/`trace`
      label, so baseline `code bytes`/`data` rows and pinned reports move once; do it in its own handoff.
      Keep `report_does_not_depend_on_where_it_is_run_from` (tests/suites/report.rs) meaningful, and test
      that `check`'s `report` == `aipl report` == the pinned section from different cwds.
      Related: `aipl check <dir>` on `tests/cases/imports/` reports 9 `errors` mismatches for cases with
      `file:` companions (their labels use the scratch dir), so the label rule for staged files needs settling.
- [ ] G5 keep the "`file:` section on an in-place source is an error" rule in `check`.
- [ ] G6 `check` prints `handoff-fill:` lines via `handoff::protocol::Fill` (one definition).
- [ ] G7 **which functions does `check` compile?** (Corrected 2026-11-03: `check` and the cases harness compile
      error fixtures identically; the "parity gap" recorded here earlier was a verification mistake, since `--fix`
      leaves a section alone when the file compiles.) The real question is reachability. Errors found while
      instantiating generic code (`conflicting types for "T"`) are only found for code reachable from a root
      (`main`, or a test), so an uncalled function with such a call compiles clean under both. Principle #2
      says `check` reports everything it can: make every non-generic function a mono root for `check`'s compile
      (recommended; measure the cost over `crates/` first), or accept it and keep a root in those fixtures. Today
      it pins `generics/errors/err_type_param_conflict` to a `main`.
- [ ] Differential test: for every corpus file, old harness and new `check` agree on pass/fail, and fill
      mode yields byte-identical files and baseline rows. (Phase 3 extends it to the migrated section names.)
- [ ] Canary set of deliberately broken copies that both must reject: wrong stdout, wrong exit code, leak,
      moved baseline, missing baseline, stale report, errors mismatch, trace left in, lint hit,
      unformatted file.
- [ ] `cargo handoff` green; baseline rows shifted by the rooting change reviewed.

## Phase 2b: what is left of reserving `main`
Target state: `fn main` appears only in (a) real programs (`projects/`, `examples/`), (b) tests of the real
implementation of an effect (stdin, terminal, process, real clock, real files), and (c) the
entry-point-mechanics tests (exit code, `cli`, `stderr`, `file:`, `expect file:`).

- [ ] **Recount the files with a `main`, sections and tests** (the 64 found on 2026-11-03 are converted; the 4 kept are
      `files/{error_type,write_file}` and `builtins/{now_nanos,monotonic_now}`, tests of the real filesystem and clock).
- [x] The 26 error fixtures that kept their `main` (2026-11-03): 24 had it renamed `subject` once the conversion was
      verified by compiling the file and reading its errors (the earlier verification went through `--fix`, which
      leaves a section untouched when the file compiles, so it called a deletion "unchanged" when it had removed the
      error). Two keep it: `effects/err_random_main` (the error is that `main` cannot declare `!random`) and
      `generics/errors/err_type_param_conflict` (needs a root: G7).
- [ ] **Lint fixtures still in `tests/cases/lints/`** (58 files): the fixtures of the 10 lints still written in
      Rust (19 files: convert them into the lint's own `.test` assertions when each is ported, with
      `scripts/lint_fixtures_to_tests.py`), 24 files in groups named for a shape and not a lint (`slices`,
      `interpolations`, `struct_literals`, `decr_shapes`, ...: map each to its lint first), and 12 `_allow`
      fixtures that need a `struct`/`variant` or output a test block cannot hold.
- [ ] **Per batch:** `cargo handoff`, then review moved baseline rows (the `main` to test-driver switch moves
      `instructions` and `code bytes` for every converted case).
- [ ] **Update CLAUDE.md for `main` only, now:** delete "Test `main` style" (its premise, a void `main` driving a
      case, is gone for ordinary cases); say where `main` may appear; say that behavior is pinned by `.test` blocks
      and sections are for diagnostics, reports and the real-implementation and entry-point cases. The section
      *vocabulary* in CLAUDE.md changes again in Phase 3, so leave it for 3.13 and do not rewrite it twice.
- [ ] **Update the skills for `main` only, now:** `write-aipl` (its `main`/`stdout` examples and the capture-shim
      template) and `add-builtin` ("a case is for ... output a `main` prints"). Section names in the skills wait for 3.13.
- [ ] **Re-count** the cases that link a production executable after the last conversion; it should be small
      (this is what Phase 0's "fewer executables" saving rests on). Record it here.

How to convert (the shape a reader should see):
- **Prefer a returned value.** If a function only prints because it was convenient, make it return the value
  and assert on that: `assert(describe(3) == "three")`. No shim.
- **If the printing is the behavior**, capture it inline in a top-level `test({ .. })` (no dummy function).
  `print` adds a newline and `put` does not, so the stand-in models both, and the expected text stays visible:
  ```
  test({
      mut out = "";
      shim prints {
          print = |text| { set out = `{out}{text}\n`; },
          put = |text| { set out = `{out}{text}`; },
      } {
          demo();            // the old main's body, verbatim
      };
      assert(out == """
          expected text, copied from the old `--- stdout ---`
          """);
  })
  ```
  A `"""` block is dedented, drops trailing whitespace on a line, and needs a blank line before the closing
  quotes for the final newline; use an escaped string when the output has trailing whitespace or blank lines
  at the end. Watch `--- ` markers inside a `"""` block.
- **A `report` case** pins the whole program's call counts, so print-capture adds rows of its own: turn the
  prints into asserts (`print(X)` to `assert(X == "line")`) and keep the capture only where helper functions
  print.
- The old `--- stdout ---` text is the oracle: the new assert must say the same bytes, so a codemod slip fails a
  test and does not pass quietly. Read a sample of each batch's diff.
- **Verify a fixture change by compiling the file and reading the errors it really prints**, never through
  `--fix`: it leaves a section alone when the file compiles, so it compares the old text with itself. A refill
  pins whatever the compiler now says, including a different error; read a freshly filled `--- errors ---` body by
  eye once.

Tools in `scripts/`, for the conversions above: `main_to_test.py` (print-only `main` to a capturing top-level
test), `main_to_asserts.py` (prints to asserts, for `report` cases), `errors_fixture_main.py` (take `main` out
of an `errors` fixture only if the messages stay the same), `lint_fixtures_to_tests.py`.

## Small follow-ups found on the way
- [x] **A leak the new gate found: `projects/minesweeper`'s tests leaked** (51 of 7028 allocations). Not minesweeper's
      and not the shim's: any struct with an array field, a `mut self` method that is not inlined (called twice),
      and a move of the binding afterwards (`let f = a;`, or an inlined callee taking it) leaked one block. After
      the call the binding's slot named the call's own result buffer, which the call also tracks for release; the
      move empties a binding's buffer field by field, so the call's release found nothing. The mutating-method arm
      of `compile_call_expr` now gives a struct receiver a buffer of its own. Pinned by
      `tests/cases/structs/mut_method_then_move.aipl`.
- [ ] The "undeclared effect" error a test gets for an unshimmed effect names the function and effect but does
      not say to add a `shim`; worth saying.
- [ ] `.benchmark` blocks run in another process, so `check` counts them in its totals and prints their
      failures, but gives them no row of their own in the per-file listing. Needs the assertion tally to
      cross back from the instrumented run.
- [ ] The converted print-and-compare tests carry roughly 10-50 extra allocations from the capture shim. Where a
      function only printed out of convenience, rewrite it to return its value.
- [ ] `aipl doc` has not been checked against the placeholder function behind a top-level `test`/`benchmark`
      (private functions are not listed, so probably fine).

## Phase 3: `check`'s own output is written as `check` sections
Order matters, and the reasons are in "Why this order" below. Each numbered item is a stage that lands green on
its own; the corpus migration (3.9) is one handoff per kind. The run-observing sections and the inputs are not
touched anywhere in this phase except where `check` reads them into the structured result.

**3.1 Structure first (no behavior change)**
- [ ] Split `check_file` (`src/main.rs`) into producing a structured `CheckResult` (formatting findings, compile
      diagnostics, test records, benchmark outcome, the run, the report, trace refusals, later leak and baseline)
      and rendering it. Existing tests stay green unchanged; this is the stage that makes the rest cheap.
- [ ] Give each part of the result a stable, testable renderer (rows, failures, diagnostics), used by both the
      CLI and, later, section comparison.

**3.2 One output stream (DS3)**
- [ ] Move compile diagnostics, section mismatches, format notices, trace refusals and `fix` lines into the
      rendered report on stdout, in a fixed order per file; stderr only for usage and I/O errors.
- [ ] Update every expectation that reads stderr: `tests/suites/check.rs` (about 40), `fmt.rs`
      (`cli_check_formats_with_fix`), `report.rs`, `source_extension.rs`, the 8 `--- check ---` cases, and the
      scripts that run `check`. Own handoff: it moves all of those at once.

**3.3 Selector flags (DS2)**
- [ ] `--format`, `--compile`, `--tests`, `--benchmarks`: composable; none means all. A test per flag and per
      combination in `check.rs`; `usage()` and `--help` text. (No `--run` and no `--report`.)
- [ ] `--fix` per selector: what it rewrites and what it may add (today it formats, refreshes recorded sections,
      and adds a run section only for output the file's own run produced).
- [ ] Exit status per selector (non-zero exactly when the selected parts report a failure).

**3.4 The header grammar and the data model (additive; old names keep working)**
- [ ] `parse_test_section_header` (AIPL) accepts `check` plus flags; canonical spelling; unknown flag, duplicate
      projection, and flags on any other section are errors. `.test` blocks in the AIPL file, and
      `tests/suites/parser.rs`.
- [ ] `parse_spec` (AIPL): keep the fields for the run-observing sections and inputs; replace `errors`, `check` and
      `check_exit_code` with an ordered list of `(projection, body)` expectations; the old names parse into the same
      list as aliases. The Rust `SpecFields` in `src/lib.rs`, the FFI marshalling in `aipl-codegen/src/lib.rs`
      (~2317) and the `Spec` in `tests/cases.rs` follow.
- [ ] `fill_or_add_sections` / `fill_or_add_sections_file` (AIPL) address a `check` section by its canonical header.
- [ ] `split_test_sections`, `strip_test_sections`, `companion_files`, the formatter's byte-for-byte preservation,
      the loader's `refuse_sectioned_import`, `aipl-index` and `lex_aipl` keep working with a header that has flags
      (tests for each). The compiler's own sources have no sections, so the bootstrap risk is only the live
      artifact's `parse_spec`, which `check --fix` calls: keep it additive until the corpus has moved.

**3.5 Validation of the new sections in `check`**
- [ ] Compare each `check` section to its projection of the one `CheckResult` computed with sections ignored.
- [ ] Re-express `conflicts()` (`sections.rs`) for the new forms: a `--compile` section that holds diagnostics cannot
      sit with a run-observing section, or with `report`; a run section on a file with no `main` is still reported.
- [ ] A `--- check ---` over everything, and `--- check --tests ---` as the replacement for the old `check`.
- [ ] `--fix` rewrites and adds them.
- [ ] Old names are aliases of the new projections inside `check`, so both spellings validate identically.

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
- [ ] The `formatted` fixtures (27 in `tests/fmt`) to `check --format --fix` (DS5b), and port the fixture runner and
      `fill_expected_fmt` in `tests/suites/fmt.rs`.
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
- [ ] `scripts/`: `errors_fixture_main.py` and `lint_fixtures_to_tests.py` name `errors` sections; update or retire.
- [ ] Optional (DS7): highlighting for section header lines in the editor grammar, and an LSP diagnostic for a
      malformed header.

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
