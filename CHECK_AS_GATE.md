# `aipl check` as the whole case gate: what is left

Status as of 2026-11-03. Done and removed from this list (git has the history): effect-free tests
(Phase 2a), forbidding imports of sectioned files (Phase 3), top-level `test`/`benchmark` blocks, the
`.performance` to `.benchmark` rename, root-relative `./` labels (DESIGN_PRINCIPLES.md §6 and §7), and most of
Phase 2b, which reserved `main` for programs and tests of the real implementation of an effect.

Each phase lands green via `cargo handoff` and is worth merging on its own.

## Goal
Validate the whole corpus (`tests/cases/**`, `crates/**`, `examples/`) in a few long-lived,
sharded `aipl check` processes instead of ~1183 per-case nextest processes, with `aipl check`
the single definition of "this file is correct" for the dev loop, handoff and user projects.
Success is measured against the current handoff **discovery run: ~410-460s** (a clean full
handoff: ~500s), taken in the same launch environment as handoff really runs in.

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
- [ ] G2 leak gate for every file that has `main` or tests (library cases measure the synthesized
      test driver), not only `.benchmark`/`report`.
- [ ] G3 `check` / `check exit code` sections: run a child `aipl check --ignore-sections` and
      compare (about a dozen cases).
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
- [ ] G7 `check` and the harness compile different sets of functions. A mono-phase error (found while
      instantiating or checking code) is reported by `aipl check` with no `main`, but the harness's compile
      only reaches it from a `main`. Decide which is right (check's is the stricter) and make them agree.
      Pins 26 error fixtures to a `main` until then (see Phase 2b).
- [ ] Differential test: for every corpus file, old harness and new `check` agree on pass/fail, and fill
      mode yields byte-identical files and baseline rows.
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
- [ ] **26 error fixtures keep their `main`** because of G7; drop it once `check` is the gate or the harness
      compiles every function: `arrays/spread/err_spread_in_call, builtins/err_span_without_import,
      builtins/err_use_without_import, effects/err_random_main, effects/err_shim_partial_coverage,
      effects/err_shim_signature_mismatch, effects/err_shim_undeclared_effect,
      effects/err_shim_unshimmable_effect, generics/err_explicit_type_args,
      generics/err_return_only_type_param, generics/errors/err_ord_bound_violation,
      generics/errors/err_type_param_conflict, integers/err_literal_out_of_range, kwargs/err_duplicate_kw,
      kwargs/err_kw_for_positional, kwargs/err_kw_passed_positionally, kwargs/err_positional_after_kw,
      kwargs/err_unknown_kw, lambdas/err_operator_value_not_imported, structs/errors/err_generic_cannot_infer,
      syntax/err_call_missing_comma, syntax/err_missing_semicolon_before_refusal,
      syntax/err_operator_starts_expr, syntax/err_unexpected_token_in_expr,
      variants/err_payload_kw_for_positional, variants/err_payload_unknown_kw`.
- [ ] **Lint fixtures still in `tests/cases/lints/`** (58 files): the fixtures of the 10 lints still written in
      Rust (19 files: convert them into the lint's own `.test` assertions when each is ported, with
      `scripts/lint_fixtures_to_tests.py`), 24 files in groups named for a shape and not a lint (`slices`,
      `interpolations`, `struct_literals`, `decr_shapes`, ...: map each to its lint first), and 12 `_allow`
      fixtures that need a `struct`/`variant` or output a test block cannot hold.
- [ ] **Per batch:** `cargo handoff`, then review moved baseline rows (the `main` to test-driver switch moves
      `instructions` and `code bytes` for every converted case).
- [ ] **Update CLAUDE.md:** delete "Test `main` style" (its premise, a void `main` driving a case, is gone for
      ordinary cases); rewrite "Prefer the cases framework for tests" (it says a case is source plus expected
      `--- stdout ---`: say instead that behavior is pinned by `.test` blocks, and sections are for `errors`,
      `report`, and the real-implementation and entry-point-mechanics cases); update "Performance monitoring"
      (what a case carries; a `report` is of the test driver when there is no `main`); say where `main` may appear.
- [ ] **Update the skills:** `write-aipl` (its `main`/`stdout` examples and the capture-shim template) and
      any skill or doc that shows a `main` + `stdout` case as the way to write a test (`add-builtin` says
      "a case is for ... output a `main` prints": reword).
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
- **Verify a fixture change with the same compile path the gate uses**, not only `check` (G7), and read a
  freshly filled `--- errors ---` body by eye once: a refill pins whatever the compiler now says, including a
  different error.

Tools in `scripts/`, for the conversions above: `main_to_test.py` (print-only `main` to a capturing top-level
test), `main_to_asserts.py` (prints to asserts, for `report` cases), `errors_fixture_main.py` (take `main` out
of an `errors` fixture only if the messages stay the same), `lint_fixtures_to_tests.py`.

## Small follow-ups found on the way
- [ ] The "undeclared effect" error a test gets for an unshimmed effect names the function and effect but does
      not say to add a `shim`; worth saying.
- [ ] `.benchmark` blocks run in another process, so `check` counts them in its totals and prints their
      failures, but gives them no row of their own in the per-file listing. Needs the assertion tally to
      cross back from the instrumented run.
- [ ] The converted print-and-compare tests carry roughly 10-50 extra allocations from the capture shim. Where a
      function only printed out of convenience, rewrite it to return its value.
- [ ] `aipl doc` has not been checked against the placeholder function behind a top-level `test`/`benchmark`
      (private functions are not listed, so probably fine).

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
- The cases harness stages a case alone into a scratch dir with only its `file:` companions, so a case can
  never import a real sibling file.
