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
- Crashes and runaway JIT are not mitigated (treated as top-priority bugs; fail loudly).
- Running `main` under the JIT is rejected: the AOT linker runtime is the product.

Open (resolve in the phase noted):
- [ ] D1 (Phase 0/2): where "real program" ends. Proposed: keep `main` when the case pins exit
      code, `cli`, stdin, `expect file:`, or process-level behavior; convert when it only prints.
- [ ] D2 (Phase 3): importing a file that has sections: **error** (recommended) or warning.
- [ ] D3 (Phase 1): what marks the project root so rooting does not depend on the cwd.
- [ ] D4 (end of Phase 0): speedup that justifies Phases 4-5 (suggestion: discovery well under
      half of 456s).

## Phase 0: measure and decide (no code)
- [ ] Time, over ~100 sampled cases, each step separately: per-test overhead, load/lint/mono,
      JIT, clang link, exec. Use `aipl check <dir>` (batch mode exists) and
      `scripts/flamegraph_check.sh`.
- [ ] Repeat in both launch environments (iTerm-exempt terminal; backgrounded Claude session).
- [ ] Census: files with `main` + sections, bucketed (print-only / exit code / cli,stdin,files).
- [ ] Census: how many files with sections are imported by other files (Phase 3 blast radius).
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

## Phase 2: convert dummy `main`s
- [ ] Design the capture helper (a `shim print`-based `capture_prints`-style function) so the
      expected text stays visible in the file; add it with `.test` blocks and a write-aipl skill entry.
- [ ] Codemod: `fn main() { body }` + `--- stdout --- X` becomes `fn scenario() { body }` plus a
      `.test` asserting the captured text equals X. Cases pinning exit code/`cli`/stdin/files keep `main`.
- [ ] Convert a directory at a time (list directories here as they land):
  - [ ] _(fill from the Phase 0 census)_
- [ ] Update CLAUDE.md "Test `main` style" and the `write-aipl` skill.
- [ ] `cargo handoff` per directory batch; review moved baseline rows (`main` -> test-driver switch).

## Phase 3: forbid importing files that have sections
- [ ] Loader error naming the section (resolve D2).
- [ ] Fix whatever the corpus failure list shows (per the repo's fanout rule, not by grepping ahead).
- [ ] Check what happens when a `file:` companion body itself contains a section marker.
- [ ] `cargo handoff` green.

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

## Session log
- 2026-11-01: plan written and approved; nothing implemented. Next: Phase 0.
