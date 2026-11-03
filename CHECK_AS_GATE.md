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
- `main` is reserved for exactly two things (decided 2026-11-02, broad scope): **(a)** the entry point of an
  actual program (`projects/*`, `examples/`), and **(b)** a handful of tests that validate the behavior of the
  *un-shimmed* builtins (real file I/O, printing, stdin, clock, `execute_program`, terminal), which need some
  coverage of the real implementation. Everything else stops pinning the behavior of "regular" functions through
  `main` / `print` / sections: it becomes `.test` blocks (asserts on returned values, or on prints captured with an
  inline `shim`). This is the **broad** set of the Phase 2b census (~650 files), not just empty placeholders.
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
- [x] D1 (Phase 0/2): "real program" = (a) and (b) above, nothing else (decided 2026-11-02).
- [x] D6 (Phase 2b): tests of the entry-point mechanics (the 55 `main` files that pin `exit code`, `cli`, `stderr`,
      `file:` / `expect file:`) **stay `main` tests** (decided 2026-11-02). They are not moved: no directory is
      created for them.
- [ ] D7 (Phase 2b): how, if at all, to enforce "`main` only for programs and real-builtin coverage". Directory-based
      enforcement was rejected (decided 2026-11-02: skip enforcement for now; the user has not settled on a
      mechanism). Until then the rule is held by review and by the checklist below.
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

### Phase 2b: reserve `main` for programs and real-builtin coverage
Target state: `fn main` appears only in (a) real programs (`projects/`, `examples/`), (b) tests of the un-shimmed builtins,
and (c) the entry-point-mechanics tests (exit code, `cli`, `stderr`, `file:`, `expect file:`, D6). Nothing enforces it
yet (D7); no directory layout is imposed.

Census (2026-11-02; `fn main` plus at least one section, over `tests/cases`, `crates`, `examples`): 704 files.
| group | files | disposition |
|---|---|---|
| empty `fn main() {}` placeholder | 55 | delete the `main` |
| non-empty `main` + `.test` blocks + sections | 56 | fold the prints into the existing tests |
| `main`, no `.test`, `stdout` only | 309 | convert: prints become a captured-output assert, values become asserts |
| `main`, no `.test`, `errors` (compile-error fixtures) | 197 | delete the `main` unless it is the thing that errors; the `errors` section stays (it pins the compiler, not a function) |
| `main`, no `.test`, `exit code` / `cli` / `file:` / `expect file:` / `stderr` | 55 | keep as `main` tests (D6), where they are |
| `main`, no `.test`, `report` (+ `stdout`) | 32 | the report is of what the case costs: keep the section, drive the cost from the test driver instead of `main` |
| `main`, no sections | 15 | decide per file (delete, or it is example/program code) |

How to convert (the shape a reader should see):
- **Prefer a returned value.** If a function only prints because it was convenient, make it return the value
  and assert on that: `assert(describe(3) == "three")`. No shim.
- **If the printing is the behavior** (a function with `!prints`, a formatter that emits), capture it inline in a
  **top-level `test({ .. })`** (no dummy function: see DESIGN_PRINCIPLES.md §6 and "Top-level `test`" below). `print`
  adds a newline and `put` does not, so the stand-in models both, and the expected text stays visible:
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
  (Watch `--- ` markers inside a `"""` block; that a `"""` block is dedented, drops trailing whitespace on a line, and
  needs a blank line before the closing quotes for the final newline.) Boilerplate is the accepted cost of D5; if
  ~300 copies of this prove too noisy, revisit D5 with that number in hand.
- The old `--- stdout ---` text is the oracle: the new assert must say the same bytes, so a codemod slip fails a test
  instead of passing quietly. Read each batch's diff on a sample.

Checklist:
- [ ] ~~Allowlist rule~~ skipped for now (D7).
- [x] D6 answered: the 55 entry-point-mechanics tests keep their `main`, in place.
- [ ] Choose the real-builtin coverage set (kept in place, no new directory): one `main` case per un-shimmed builtin
      surface (print/put, `read_file_to_string`, `write_string_to_file` + `expect file:`, `list_files`,
      `read_line`/`read_byte`, `now_nanos`/`monotonic_now`, `execute_program`, terminal). Start from what exists:
      `tests/cases/files/*`, `builtins/now_nanos`, `builtins/monotonic_now` (already main + `stdout`),
      `execute_program/*`. Keep each tiny; list them here. Everything else in those directories converts.
- [ ] Delete the 55 empty `main`s (and the 197 `errors` fixtures' `main`s that are not the error site). Mechanical;
      handoff refills line-sensitive `errors` sections.
- [ ] Convert the 56 `main` + `.test` files (fold the prints into their tests).
- [ ] Convert the 309 `stdout`-only programs, in the batches below (counts per `tests/cases/` directory; one
      `cargo handoff` per batch; tick a batch when it lands). Each batch is ~20-35 files so a diff stays
      reviewable. Batch 1 is the small-directories warm-up, which is where the capture template gets shaken out.
  - [x] 1. Warm-up, 35 files (redone as top-level `test` blocks after that form landed) (the census said 33; it missed `bindings/*/` and `functions/void/`): `bindings` 5, `conditionals` 1, `constants` 2, `chars` 2, `compound_assign` 2, `integers` 2,
        `loops` 2, `results` 2, `syntax` 1, `options` 3, `methods` 3, `functions` 6, `dicts` 4. Template settled (below).
  - [x] 2. `strings`, 28 (27 converted; `small_string_optimization` is left, see D8). (Mind `named_representations`: it must keep asking for each representation by name.)
  - [x] 3. `arrays`, 28.
  - [x] 4. `structs` 15 + `variants` 13, 28.
  - [x] 5. `tuples` 12 + `sets` 11, 23.
  - [x] 6. `generics` 13 + `match_arms` 9 + `lambdas` 8, 30.
  - [x] 7. Small leftovers, 19: `kwargs` 7, `grids` 4, `tail_calls` 4, `imports` 1, `docs` 1, `performance` 1, `perf` 1.
  - [x] 8. `optimizations`, 33 (26 converted; the 4 on the D8 list and 3 held back by D9, below). Take care: these exist to pin what codegen does to a program, so check each converted
        case still exercises the same optimization (the `report` and baseline row are the evidence), not just the output.
  - [x] 9-11. `lints`: done a different way (decided 2026-11-03). A lint's flagged / not-flagged / allow-agreement cases are
        `.test` assertions in the lint's own `.aipl` file (`scripts/lint_fixtures_to_tests.py`), not fixtures; the add-lint skill
        says so for new and ported lints. 118 files in `tests/cases/lints/` became 58. Left there: the `allow_squelch` and
        `return_loop_find_if` triple (infrastructure), 12 `_allow` fixtures converted to top-level tests (they need a
        `struct`/`variant` or a loop's output a test block cannot hold), and the fixtures of the 10 lints still in Rust
        (19 files) plus 24 files in groups named for a shape rather than a lint (`slices`, `interpolations`, `struct_literals`,
        `decr_shapes`, ...). Convert the Rust ones when each lint is ported; map the shape-named ones to their lint first.
  - [ ] 12. Decide before converting, 18: `effects` 7, `builtins` 8, `execute_program` 2, `cli` 1. Some of these *are*
        the real-builtin coverage set (b) and keep their `main`; the `shim_effects`-style ones already test shims and
        convert. Settle the kept set here, then convert the rest.
- [ ] **Left alone for now (D8, decided 2026-11-02):** the 11 files that hang a `.performance` block off `main` keep
      their `main` and their `stdout` section untouched until D8 is settled (a top-level `performance({ .. })`, or attach
      each block to the function it measures): `strings/small_string_optimization`, `constants/computed`,
      `arrays/{reduce,reverse_allocates_nothing,slice_allocates_nothing,inline_empty}`,
      `optimizations/{join_emptiness_fusion,map_map_fusion,fold_emptiness_and_index,map_reduce_fusion}`,
      `performance/refcount_balance`. Skip them in every batch below (the codemod already skips what it cannot match;
      check each batch's list for these names). They are the known exceptions in the final recount.
- [ ] **Left alone, D9 (compiler bug, found 2026-11-03):** `optimizations/{fusion_named_effects,loop_map_fusion_effects,predicate_fusion_effects}`
      pass an effectful function (or a closure that prints) to a higher-order builtin. Under a test's `shim` that is rejected with
      "fn print has effect !prints but the calling function does not declare it" at the `print` inside the passed function, even when
      the call sits in a `fn .. !prints` the shim calls. The same code in a `main` that declares `!prints` works. Looks like mono
      specialises `map(loud)` with the shim-reduced effect set of the function it is instantiated from. Reproducers:
      a test whose shim body is `[1, 2].map(loud)` with `fn loud(x: i64) !prints -> i64`. Fix in `aipl-mono` (effect accounting for
      function values under a shim), then convert these three.
- [ ] Convert the 32 `report` cases: confirm what `aipl report` / the harness measure when there is no `main`
      (the synthesized test driver), re-pin the section, check the baseline row moves are explained.
- [ ] The 15 section-less `main` files: decide each.
- [ ] Per directory batch: `cargo handoff`, then review moved baseline rows (the `main` -> test-driver switch moves
      `instructions` and `code bytes` for every converted case, as the old "void main" note says).
- [ ] Update **CLAUDE.md**: delete the "Test `main` style" section (its premise, a void `main` driving a case, is
      gone for ordinary cases); rewrite "Prefer the cases framework for tests" (it says a case is source plus
      expected `--- stdout ---`: say instead that behavior is pinned by `.test` blocks, and sections are for
      `errors`, `report`, and the real-builtin and entry-point-mechanics cases); update "Performance monitoring"
      (what a case carries; `report` without a `main`); say where `main` may appear (programs, real-builtin coverage, entry-point mechanics).
- [ ] Update the `write-aipl` skill (the `main`/`stdout` examples, the capture-shim template above) and any other
      skill or doc that shows a `main` + `stdout` case as the way to write a test (`add-builtin`, `add-lint` say
      "a case is for ... output a `main` prints": reword).
- [ ] Re-count the census after the last directory; the number of cases that link a production executable should
      be small (this is what Phase 0's "fewer executables" saving rests on): record it here.

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

- 2026-11-02: Phase 2b scope set to the broad reading (~650 files); D1 resolved; D6 resolved (entry-point tests keep `main`, in place); enforcement skipped (D7 open, no directory-based rule).

- 2026-11-02: Phase 2b batch 1 (35 files) converted with `scripts/main_to_test.py` (rename `main` to `scenario`, capture
  prints with an inline shim, assert the old stdout text); 34 passed first time, 1 needed a hand edit. Findings:
  a `"""` block drops trailing whitespace on a line (use an escaped string when the output has any), and a blank line
  before the closing `"""` is how the expected text gets its final newline. The converted files are print-and-compare,
  not returned-value asserts; the returned-value rewrite is a follow-up where a function only printed out of convenience.
- 2026-11-02: top-level `test({ .. })` added (decided mid-2b: converting `main` to a `fn scenario()` only swapped one dummy
  function for another). Parser (`grammar_aipl.aipl` `test_item` / `lower_test_item`, naming in `fn_names.aipl`
  `unattached_test_name`), formatter (`walker.aipl` `test_item`), driver (`synthesize_driver` drops the placeholder fn and
  reports `line N`; `aipl_syntax::unattached_test_line`), Rust test `check::a_top_level_test_block_runs_and_is_reported_by_its_line`,
  DESIGN_PRINCIPLES.md §6 (rare by design: coverage will be measured by attachment). One handoff for the grammar bootstrap
  (1671s; refresh_perfmon dominated: 49 compiler-source rows moved). Batch 1 redone with the new form; `scripts/main_to_test.py`
  now emits it. The symbol index (LSP outline) listed the placeholder; `aipl-index` now skips it (`corpus::indexes_the_whole_repository_with_correct_spans` caught it). `aipl doc` not checked (private functions are not listed).

### Top-level `test({ .. })` (language addition, 2026-11-02)
Syntax: an item `test({ .. })`, the body of a `.test` (helpers allowed). Lowered by the parser to a private function
`__test_block_<line>_<offset>` with an empty body whose `.test` body is the block; never part of `run`/`build` output (checked:
`aipl report` is byte-identical with and without one). A `# ..` block directly above it is an error. Reported as `test line N`.

- 2026-11-02: `aipl check` output is now grouped by file (`file:` then `function: N assertions passed[, M failed]`, one row
  `standalone tests` for the top-level `test` blocks) with every failure gathered under `test failures:` at the bottom,
  before the summary lines. The JIT test runtime counts assertions per test (`TestRecord`, `take_test_records`) and buffers
  failure text (`take_failure_report`). A `.performance` block runs in another process, so it is counted in the totals and
  its failures land under `test failures:` but it has no row of its own yet. 17 `check::` assertions updated.

- 2026-11-02: Phase 2b batch 2 (`strings`): 27 of 28 converted; `main_to_test.py` gained an escaped-string fallback for output
  with trailing whitespace. **D8 (open; the 11 are left alone, see the Phase 2b checklist):** 11 files hang a `.performance` block off `main`
  (`strings/small_string_optimization`, `constants/computed`, `arrays/{reduce,reverse_allocates_nothing,slice_allocates_nothing,inline_empty}`,
  `optimizations/{join_emptiness_fusion,map_map_fusion,fold_emptiness_and_index,map_reduce_fusion}`, `performance/refcount_balance`).
  The performance block is not about `main`, but `.performance` attaches to a function and has no top-level form. Options: a
  top-level `performance({ .. })` like `test` (same rarity argument), or attach each to the function it measures. Unconverted until decided.

- 2026-11-03: Phase 2b batches 3-7 converted together (125 files: `arrays` minus its 4 `.performance`-on-`main` files, `structs`,
  `variants`, `tuples`, `sets`, `generics`, `match_arms`, `lambdas`, `kwargs`, `grids`, `tail_calls`, `imports`, `docs`, `perf`).
  The codemod matched every one; all 125 passed `aipl check` first time. (`performance/refcount_balance` is on the D8 list.)

- 2026-11-03: lint fixtures moved into the lints' own tests (27 AIPL lints; see Phase 2b batches 9-11 above). The script reads the
  messages off the running lint rather than typing them; its comments come from the fixture. `.performance`/Rust-lint fixtures untouched.

- 2026-11-03: Phase 2b batch 8 (`optimizations`): 26 converted; 3 files held back by a mono bug (D9 in the checklist), 4 by D8.
