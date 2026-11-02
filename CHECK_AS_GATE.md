# `aipl check` as the whole case gate (multi-session plan)

Living plan. Tick boxes as work lands, append to the **Session log** at the end of every
session, and keep **Decisions** current. Each phase lands green via `cargo handoff` and is
worth merging on its own; Phases 1-3 stand even if Phase 0 says to stop before Phase 4.

## Goal
Validate the whole corpus (`tests/cases/**`, `crates/**`, `examples/`) in a few long-lived,
sharded `aipl check` processes instead of ~1183 per-case nextest processes, with `aipl check`
the single definition of "this file is correct" for the dev loop, handoff and user projects.
Success is measured against the current handoff **discovery run: 456s** (final-gate-only
handoff: 546s total), taken in the same launch environment as handoff really runs in.

## Background (read before starting)
Two separate costs sit under "process startup":
1. **Per-test process overhead** (nextest process, test-binary start, ~0.2s dogfood engine
   load; ~0.9s overhead per ~0.6s of work). In-process batching removes it.
2. **macOS fresh-executable assessment** (~0.19s per newly linked exe, serialized, ~1.7 exes
   per case, ~350s floor; ~2ms under iTerm; see memory `project_handoff_slow_exec_assessment`).
   Batching alone does not remove it; fewer `main`s (Phase 2) do.

Facts established by reading (nothing timed yet):
- `aipl check` already validates `stdout`, `stderr`, `exit code`, `errors`, `report`,
  `expect file:`, `cli`, `file:` companions (`src/sections.rs`, `check_file` in `src/main.rs`),
  and `--fix` writes them with the same `fill_or_add_sections_file` as `fill_expected`.
- `.test` blocks run in an in-process JIT; `main` and `report` run as linked executables.
- Serial `aipl check crates` took ~123s for 144 files (~0.85s/file, compute not startup), so a
  serial single process is **not** faster than parallel nextest: the batch must be sharded.
- `check` is serial in-process (`env::set_current_dir`, global test tallies, global shim slots).
- Imported files' sections, `.test` bodies and `file:` companions are silently dropped today.

## Decisions
Settled:
- `main` is reserved for the entry point of a program that is itself the subject. Files that have
  `main` + sections only as a hook for printing/asserting are converted (Phase 2).
- Because of that, `check` keeps its rule of running `main` only when a run-observing section
  (`stdout`, `stderr`, `exit code`, non-empty `cli`, `expect file:`) exists. No implicit
  "silent and exit 0" assertion for section-less `main`s.
- Paths in diagnostics and baked `assert`/`trace` labels are relative to the **project root**
  (cmd+clickable). The harness and `aipl report` move to it; `check` already works that way.
- **Tests and performance blocks may not let an effect escape**: every effect a `.test` body uses
  (directly or through any function it calls) must be discharged by a `shim` inside the body. Tests
  that exercise the real (unshimmed) filesystem, clock, stdin or process APIs are real programs: they
  keep a `main` and pin behavior with `stdout` / `file:` / `expect file:` sections.
- Crashes and runaway JIT are not mitigated (treated as top-priority bugs; fail loudly).
- Running `main` under the JIT is rejected: the AOT linker runtime is the product.

Open (resolve in the phase noted):
- [ ] D1 (Phase 0/2): where "real program" ends. Proposed: keep `main` when the case pins exit
      code, `cli`, stdin, `expect file:`, or process-level behavior; convert when it only prints.
- [x] D2 (Phase 3): importing a file that has sections is an **error** (done 2026-11-01).
- [ ] D3 (Phase 1): what marks the project root so rooting does not depend on the cwd.
- [x] D5 (Phase 2a): no shared capture helpers — each test writes its shim **inline** (decided
      2026-11-02). No builtin, no library file, no in-memory filesystem builtin; the shim pattern
      stays visible at the use, as in `tests/cases/effects/shim_closures.aipl`.
- [ ] D4 (end of Phase 0): speedup that justifies Phases 4-5 (suggestion: discovery well under
      half of 456s).

## Phase 0: measure and decide (no code)
- [ ] Time, over ~100 sampled cases, each step separately: per-test overhead, load/lint/mono,
      JIT, clang link, exec. Use `aipl check <dir>` (batch mode exists) and
      `scripts/flamegraph_check.sh`.
- [ ] Repeat in both launch environments (iTerm-exempt terminal; backgrounded Claude session).
- [ ] Census: files with `main` + sections, bucketed (print-only / exit code / cli,stdin,files).
- [x] Census: how many files with sections are imported by other files: 0 (Phase 3 found none).
- [ ] Check that a `Compilation` frees its JIT code on drop (memory growth in a long process).
- [ ] Check what "binary size" in `aipl report` reads (object vs linked exe).
- [ ] Write the timing table and census into this doc; resolve D1 and D4; go/no-go.

## Phase 1: close the parity gaps in `check`
Each gap gets a failing-fixture test in `tests/suites/check.rs` (model:
`the_leak_gate_runs_for_a_case_with_no_sections`).
- [ ] G1 baseline row compare vs `tests/performance_metrics.md` (5 asserted columns), with
      `BaselineMoved`/`BaselineMissing` outcomes.
- [ ] G2 leak gate for every file that has `main` or tests (library cases measure the synthesized
      test driver), not only `.performance`/`report`.
- [ ] G3 `check` / `check exit code` sections: run a child `aipl check --ignore-sections` and
      compare (8 + 4 cases).
- [ ] G4 project-root rooting (resolve D3): move the harness and `aipl report` to it; keep
      `report_does_not_depend_on_where_it_is_run_from` (tests/suites/report.rs) meaningful; test
      that `check`'s `report` == `aipl report` == the pinned section from different cwds.
- [ ] G5 keep the "`file:` section on an in-place source is an error" rule in `check`.
- [ ] G6 `check` prints `handoff-fill:` lines via `handoff::protocol::Fill` (one definition).
- [ ] Differential test: for every corpus file, old harness and new `check` agree on pass/fail,
      and fill mode yields byte-identical files and baseline rows.
- [ ] Canary set of deliberately broken copies that both must reject: wrong stdout, wrong exit
      code, leak, moved baseline, missing baseline, stale report, errors mismatch, trace left in,
      lint hit, unformatted file.
- [ ] `cargo handoff` green; baseline rows shifted by the rooting change reviewed.

## Phase 2: tests may not let effects escape, then convert dummy `main`s

### Phase 2a: effect-free tests (do first; it defines what a converted test may do)
Findings (subagent read of the code; line numbers approximate):
- The checker has no notion of "test". `.test` bodies get every effect only because
  `synthesize_driver` in `crates/aipl-codegen/src/lib.rs` (`all_effects`, ~1520-1535) declares
  `prints, read_files, read_stdin, read_stdin_raw, terminal, write_files, list_files,
  execute_program, clock` on each synthesized `__test$<fn>` and on `__test_main` (`random` and
  `measure` are already excluded). `.performance` blocks already declare only `measure`, so they
  are effect-free today.
- The rule is therefore: declare nothing for `TestKind::Tests`. An effectful call outside a `shim`
  then fails with the existing "undeclared effect" error, and `check_shim` already discharges a
  shimmed effect for its body, transitively. Shim functions inside a test must themselves be
  effect-free (capture-only closures), because the enclosing fn must declare any effect they have.
- `trace` and `assert` carry no effect, so they stay usable.
- `aipl check` runs `.test` blocks in the in-process JIT with real effects today: prints go to the
  process's stdout, files to the cwd (a scratch dir for cases, the repo root for in-place files).
- No capture helper, in-memory filesystem or fake clock exists. `tests/cases/effects/shim_closures.aipl`
  shows the inline pattern (capture prints into a `str[]`, script `read_line`, fix the clock). A shim must bind
  every operation of its effect, so a filesystem stand-in binds `read_file_to_string`,
  `write_string_to_file` and `list_files` together.
- Census (lower bound: it does not follow effects reached through imported functions; ~889 `.test`
  blocks in 500 files): about 6 files use a real effect in a test body today:
  `tests/cases/files/write_file.aipl`, `tests/cases/files/error_type.aipl`,
  `crates/aipl-codegen/src/fill_or_add_sections_file.aipl`, `crates/aipl-codegen/src/find_files.aipl`,
  `tests/cases/builtins/now_nanos.aipl`, `tests/cases/builtins/monotonic_now.aipl` (which also runs
  `sleep 1`). `projects/minesweeper/main.aipl:516` already shims `random`. No test body prints or reads
  stdin directly. The real number comes from the checker once the rule is on (fanout rule: change it, read the failure list).
- Touchpoints: `build_test_program` / `build_performance_program` / `synthesize_driver`; the LSP's
  use of `build_test_program` (`crates/aipl-lsp/src/server.rs` ~721) and `src/perf.rs` ~41 if they
  assume the saturated set; stale comments at `aipl-codegen/src/lib.rs` ~1481 and ~1514-1519 ("A test body may
  call anything") and `aipl-syntax/src/lib.rs` ~4120; DESIGN_PRINCIPLES.md (add the rule beside the shim text).

Effects on other phases:
- **Phase 4 gets easier:** with no real effects in `.test` blocks, the JIT test run no longer needs a
  scratch cwd or staged companions; only `main` runs (child processes) do. That removes the main reason
  `check` calls `set_current_dir`, which is what forced it to be serial per process.
- **Phase 2b gets a clear target:** a converted dummy `main` becomes a test that captures prints with a shim,
  because the test is no longer allowed to print for real.
- Tests of the real filesystem/clock/stdin/process APIs stay as `main` cases (they count as the
  "real program" bucket of decision D1), so this fits Prerequisite A.
- Coverage is not lost, but moves: real-IO assertions inside `crates/**` `.test` blocks
  (`fill_or_add_sections_file`, `find_files`) become shimmed tests, and the real-API check lives in a
  `main` case with sections.

Checklist:
- [x] Capture helpers: none (D5) — shims are written inline in each test. The write-aipl skill and
      DESIGN_PRINCIPLES.md show the inline patterns (print capture, scripted stdin, fixed clock,
      a filesystem stand-in binding all three operations).
- [x] Turn the rule on: empty `all_effects` for `TestKind::Tests` in `synthesize_driver`; fix the
      stale comments; confirm LSP and `src/perf.rs` still build their test programs.
- [x] Fix the failure list: shim the effects in each offending test, or move it to a `main` case with
      `stdout` / `file:` / `expect file:` sections when the real API is the point (the 6 above are the
      expected starting set).
- [x] Refusal pinned by `a_test_cannot_let_an_effect_escape` in `tests/suites/check.rs` (a test-compile error
      prints to stderr, which no case section can pin). The message names the function and effect but does not
      say to shim it; worth improving.
- [x] `write-aipl` skill states the rule and the inline patterns (DESIGN_PRINCIPLES.md left alone: it is not a numbered principle).
- [ ] `cargo handoff` green.

### Phase 2b: convert dummy `main`s
- [ ] Settle the inline print-capture shape (D5: no helper) so the expected text stays visible in the
      file; document it in the write-aipl skill.
- [ ] Codemod: `fn main() { body }` + `--- stdout --- X` becomes `fn scenario() { body }` plus a
      `.test` asserting the captured text equals X. Cases pinning exit code/`cli`/stdin/files keep `main`.
- [ ] Convert a directory at a time (list directories here as they land):
  - [ ] _(fill from the Phase 0 census)_
- [ ] Update CLAUDE.md "Test `main` style" and the `write-aipl` skill.
- [ ] `cargo handoff` per directory batch; review moved baseline rows (`main` -> test-driver switch).

## Phase 3: forbid importing files that have sections
- [x] Loader error naming the section (`refuse_sectioned_import` in `crates/aipl-loader/src/lib.rs`, raised at the import site while resolving path imports; covered by the Rust test `check::a_file_with_sections_cannot_be_imported` in `tests/suites/check.rs`; a case fixture cannot express it, because the cases harness stages only `file:` companions and a companion cannot hold sections). Virtual (FFI-supplied) files are not covered.
- [x] Fix whatever the corpus failure list shows: nothing failed, no corpus file imported a sectioned file.
- [x] A `file:` companion body that contains a section marker: the marker ends the companion and starts a section of the main file, with no error (the rest of the intended companion becomes e.g. the main file's `stdout`). Pinned by `parser::a_section_marker_inside_a_companion_ends_it_and_belongs_to_the_main_file`. A companion therefore can never hold sections, which is what makes the import rule complete for staged files.
- [x] `cargo handoff` green (1019s; 1 new baseline row).

## Phase 4: batch runner
- [ ] `aipl check --shard i/n` over a file list; deterministic assignment.
- [ ] Progress line naming the file in flight (a crash must say where it died).
- [ ] One writer for `tests/performance_metrics.md`: shards report rows; handoff (or an unsharded
      `check --fix`) merges.
- [ ] Memoize pure loader work (parse+lint of builtins and shared crate files) across files in a
      process; profile first.
- [ ] Shuffled-order test: results identical to the fixed order (cross-file state leakage).
- [ ] Re-measure against the Phase 0 table; record in this doc; confirm D4 still holds.

## Phase 5: flip
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
`fmt::all_aipl_files_stay_formatted` (also covers `projects/`, `tests/ffi_fixtures`), the five
`report_agrees_with_the_pinned_report_*` tests, `compiler_aipl_files_are_tested`, `docs_site`,
`checked_in_ir_is_current`.

## Risks to watch
- Cross-file state (shim slots, counters, allocator stats, thread-locals) making results order-dependent.
- JIT memory growth across a long batch; recycle shards by file count if it does not free.
- Losing nextest names/filters/retries/timeouts per case: the dev-loop replacement is
  `aipl check <path>`.
- Baseline rows shift once from rooting and again per converted directory; keep these in separate
  handoffs so a moved row has one cause.

## Findings to carry into other phases
- Phase 1 (G4/G6): `aipl check <dir>` on `tests/cases/imports/` reports 9 `errors` mismatches for
  cases with `file:` companions: their labels use the scratch dir
  (`/private/var/folders/.../aipl-check-N/util.aipl`, or `./util.aipl` expected) instead of the
  harness's labels. Single-file `check` of a companion case may differ the same way. The differential
  test must cover companion cases, and the label rule for staged files needs settling.
- The batch `check` also printed `fn "print" has effect` style errors only for real faults; no other surprises.

- The cases harness stages a case alone into a scratch dir with only its `file:` companions, so a
  case can never import a real sibling file. A first fixture for Phase 3 was refilled by handoff with a
  file-not-found error and looked green; check a freshly filled `--- errors ---` body by eye once.

## Session log
- 2026-11-01: plan written and approved; nothing implemented. Next: Phase 0.
- 2026-11-01 (session 2): Phase 3 done out of order. Loader refuses importing a file with sections; handoff green (1019s: discovery 488s, final 495s), blast radius zero. Companion-marker behavior pinned (marker ends the companion, silently belongs to the main file). Phase 3 complete. Next: Phase 0 or Phase 1.
- 2026-11-01 (session 2): added Phase 2a (tests and performance blocks may not let effects escape) after the design discussion; code read only, nothing implemented for it. Census says ~6 files need migration; rule is mostly a one-line change in `synthesize_driver`.

- 2026-11-02: Phase 2a rule on; 6 tests migrated. Finding for Phase 4: one `aipl check crates examples ...` batch
  reported 52 test failures (lint/*, format_source, grammar_aipl) that pass alone and do not fail as cases —
  cross-file state leakage in a long-lived process. Cause found and fixed: the JIT's thread-local constant/shim-cell
  slots (`CONST_SLOTS`) were never cleared between compilations, so one file's cached constant was read by the next
  (`forget_jit_constants`, pinned by `one_files_cached_constants_do_not_reach_the_next`). Other process-global state
  (shim slots, tallies, `set_current_dir`) is still unaudited.
