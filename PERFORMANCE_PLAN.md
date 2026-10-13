# Performance tracking redesign

Claude maintains this file: it is the plan and the progress record for replacing
the per-case `--- performance ---` sections. (`TODO.txt` is Drew's alone and is
not touched by this work.) Tick a box when its item has landed in a green
handoff, and update the design notes when a decision changes.

## Why

The `--- performance ---` sections pin every count exactly, including a
`functions:` block that gives each function's calls and bytes. That is about
13,800 per-function entries across 717 sections. Any codegen or dogfood tweak
moves them, so they now dominate every diff. In the last fifteen commits,
85–99% of changed `.aipl` lines were section refills (e.g. `3584d6e7`: 2,764 of
3,211; `9e4aa616`: 6,806 of 6,808).

They also fail at their own job. `fill_expected` rewrites every section, so a
property one case exists to protect (say, `allocations: 0` in a fusion test) is
silently re-recorded among hundreds of other refilled sections.

## Goals

1. A major performance change, regression **or improvement**, fails handoff so it
   gets reviewed. Handoff still finishes every other step, so all problems are
   found in one run.
2. Code can assert performance properties, such as "creating an array that fits
   inline allocates nothing", and a refill cannot paper over them.
3. A codegen or dogfood tweak does not produce a diff that dwarfs the change.
4. The same mechanism serves AIPL users: they can check their own code, report on
   it, and benchmark it, including wall time, which can be reported but never
   asserted.

## Design (settled)

### `.performance({ .. })` and `measure`

A second function attribute beside `.test`. Its block sets up inputs, measures,
and asserts:

```
fn decided_early(xs: i64[]) -> bool { xs.count(2) < 3 }.performance({
    let small = twos(10);
    let large = twos(10000);
    let a = measure("10 elements", small, |xs| decided_early(xs));
    let b = measure("10k elements", large, |xs| decided_early(xs));
    assert(a.allocations == 0);
    assert(b.instructions == a.instructions); // stops after 3 matches, at any length
})
```

- `measure(label, input, f)`:
  - `input` crosses an opaque boundary, so the optimizer cannot fold or hoist the
    measured work into the setup.
  - The result is treated as used and is dropped inside the measured window.
  - Setup outside the closure is never measured.
- `Measurement` holds deterministic fields only: allocations, bytes allocated,
  peak live bytes, instructions. **There are no call counts**; calls are never
  visible to user code. **Wall time is not a value in the language**, so it
  cannot be asserted on.
- **No effects, for now.** A `.performance` block, and every closure it measures,
  is effect-free. Using an effect there is a compile error. Allowing effects is a
  separate future project, and it also covers what the benchmark timing pass
  would do with repeated output.
- **Prefer exact counts when the code being measured is small.** The counts are
  exact and repeatable, so for a small closure the number is the claim:
  `allocations == 1` says what `0 < allocations` only hints at, and a loose bound
  lets a regression through. Few commits touch small code, so an exact claim about
  it seldom needs updating, and when it moves it is in the commit that changed the
  code, where touching those lines is the right outcome. For code many commits
  touch, an exact count would churn with each of them: state a comparison
  (`a.instructions == b.instructions`), which survives the churn, or a bound. A
  comparison is also right when the property is about a comparison rather than a
  size. (Every measurement carries a fixed overhead of 5 instructions, so a change
  to `measure` itself shifts every exact instruction count by a constant.)
- `measure` is legal only inside `.performance` blocks. Like `.test` blocks,
  `.performance` blocks are excluded from production builds and from the dogfood
  artifact.
- **`aipl check` runs them** (DESIGN_PRINCIPLES §2: bare `check` runs every
  validation). It builds them instrumented, and their failures sit beside test
  failures without suppressing anything else.

### `aipl benchmark <path>`

- It has two passes:
  1. **Counting pass**: the instrumented build, the same as `check`. It records
     each label's `Measurement`.
  2. **Timing pass**: the production build. Each `measure` warms up, then repeats
     its closure until the timings settle, and reports the median and spread.
     It **replays** the recorded `Measurement` for each label, so the block's
     code sees identical values in both passes.
- Labels must be unique per block; a duplicate is an error.
- Only `.performance` blocks are timed. `main` is timed only if `main` itself
  has a `.performance` block. Until effects are allowed, that is possible only
  for an effect-free `main`.

### `aipl report <file.aipl>`

Whole-program counts (including peak live bytes) plus the per-function
calls/bytes breakdown that used to live in `functions:`. It is a compiler
command, so users have it, and by default it is on-demand, not checked in. It
replaces reading `--- performance ---` sections when debugging a regression.

**`--- report ---` section.** A file may pin its `aipl report` output, and
`check` validates it like any other section. This is for the few properties only
the function list can show: an instance not minted, a call inlined away. It is
opt-in per case. Use it only where `measure` cannot express the property;
otherwise it brings back the old churn.

### The baseline: `tests/performance_metrics.md`

- One row per case, with the deterministic columns (allocations, bytes
  allocated, peak live bytes, instructions, code bytes) beside the existing wall-clock / build /
  peak RSS columns. It keeps the same format and the same generator.
- **`refresh_perfmon` writes exact deterministic counts**, which corrects any
  drift below the threshold. It is the only thing that writes the table.
  `AIPL_CASE` scopes it to some rows, which is how one accepts specific moves.
- Each case's test compares its measured counts with its row. A move past the
  threshold **in either direction** fails that case and names the scoped refresh
  command. Smaller moves pass, and the file does not change, so a tweak costs no
  diff.
- Handoff treats these failures as not fillable but not fatal. It finishes every
  other remediation and the final run, then exits non-zero listing every moved
  case.
- **Dropped:** whole-corpus totals and geomean lines. Per-case thresholds are the
  broad net.

### `aipl check` validates every section

`check` reads a file's trailing sections (stdout, stderr, exit code, cli, errors,
expect file, report, file companions) and validates them. `--- check ---` is
redefined as the `.test` runner's report and renamed to say so. With that, the cases harness becomes
"`aipl check` this case, then compare its baseline row". A fill flag on `check`
replaces `fill_expected`, which gives users the same snapshot workflow.

### Whole-compiler metric

This is a `.performance` block on the dogfood entry points (parse, format) over
fixed representative inputs, not a separate harness.

## Phases

The phases are units of work, **not a sequence**. Drew picks the order, likely
parts of 7, then 3, then more of 7, and so on. Each checkbox is meant to land on
its own where it can. Every landing ends at a green `cargo handoff` and stops so
Drew can commit; unrelated work never shares a tree.

The dependencies that do hold:

| phase | needs | why |
|---|---|---|
| 2 (baseline) | 1 | the `functions:` block should be reachable through `aipl report` before the sections that hold it are deleted |
| 4 (migrate) | 3; the `--- report ---` items of 1 + 7 | claims are written as `.performance` blocks; calls-only properties as `--- report ---` |
| 5 (compiler metric) | 3 | it is a `.performance` block |
| 6 (benchmark) | 3 | it runs `.performance` blocks |
| 7 `--- report ---` item | 1 | it pins `aipl report` output |

**Phase 7 while `--- performance ---` still exists:** `check` does not learn that
section. The harness keeps validating it, and the baseline once Phase 2 lands,
until Phase 4 deletes it. Otherwise `check` gains a section only to lose it
again.

### Phase 1: measurement in the compiler, `aipl report`

- [x] Move the instrumented build/link/run and stats parsing (`run_performance_check`,
      `measure_perf_stats`, `PerfStats`, `BinSizes`, `merge_fn_sizes` in
      `tests/cases.rs`) into the `aipl` library, so the harness and the CLI share
      one implementation. Landed as `src/perf.rs` (`aipl::perf`): `measure`,
      `measured_program`, `PerfStats` (with `render`/`parse`), `FnStat`,
      `BinSizes`, `MeasureError`. The caller builds the production object
      (the harness also runs it for behavior) and passes it with its symbol
      names; `measure` does the instrumented compile, link, run and parse. The
      harness keeps the `--- performance ---` comparison, the leak gate and fill.
      `object` is now a regular dependency of the root crate. One behavior
      change: `measure` no longer fails on a nonzero exit by itself, only when
      the run never reported (the harness's behavior checks already cover an
      unexpected exit).
- [x] Runtime: track peak live bytes (a high-water mark of allocated minus freed),
      in both the JIT and AOT runtimes. (Done in the instrumented AOT runtime; the
      JIT never instruments. See Phase 3.)
- [x] `aipl report <file.aipl> [-- args]`: runs `main`, or the `.test` driver for a
      library file, instrumented. It prints the counts (including peak live
      bytes), the binary size split, and per-function calls/bytes, and flags a
      leak (allocations ≠ deallocations). Landed in `report_cmd` over
      `aipl::perf::measure`. Decisions:
      - Arguments after `--` go to the program; otherwise the file's `--- cli ---`.
      - The file is staged as `check` stages one (companions beside it, a scratch
        directory the program runs in) but rooted at **its own directory**, not the
        working directory: an `assert`'s file label is baked into the binary's
        data, so any other root makes `binary size` depend on where `report` ran
        and disagree with the harness. With it, `report`'s numbers equal a case's
        recorded section byte for byte.
      - A leak prints a `leak:` line on stderr and exits 1; the counts still print.
      - `PerfStats` gained `peak_live_bytes`, read from a new `peak live bytes:`
        line the instrumented runtime writes. **`render()` (the `--- performance
        ---` body) does not include it**, so no section churns; `render_machine()`
        and `render_report()` do.
- [x] Machine-readable output mode for the harness and handoff: `--machine`, the
      `--- performance ---` form plus the peak line, which `PerfStats::parse` reads
      back whole. (The harness calls `aipl::perf` directly; this is for tools that
      shell out, handoff in Phase 2.)
- [x] `report` suite in `tests/suites/` (plus its `mod` line) covering the output:
      parity with the recorded section on four cases (cli section, library,
      file-writing, companions), independence from the working directory, both
      output forms, the peak, `--` arguments overriding `--- cli ---`, and the
      refusals.
- [x] Usage text; README and CLAUDE.md mention.

### Phase 2: baseline table, sections become optional (the churn win)

- [ ] Add deterministic columns (allocations, bytes allocated, peak live bytes,
      instructions, code bytes) to `performance_metrics.md`. The header prose says
      which columns are checked and which are not.
- [ ] `refresh_perfmon` builds both variants per case and writes exact counts. Its
      summary covers the deterministic moves too.
- [ ] `AIPL_CASE` scoping for `refresh_perfmon`: rewrite only the matched rows,
      and always prune rows whose case no longer exists. It still fails
      intentionally to show its summary.
- [ ] Per-case comparison with the row, both directions. Thresholds: Drew will
      decide them when this phase starts. The placeholder is |Δ| ≥ 10% **and**
      |Δ| ≥ a per-metric floor. Keep the constants in one place. The message
      gives the metric, old → new, %, regression/improvement, and the scoped
      refresh command.
- [ ] A case with no row fails, naming the scoped refresh.
- [x] The leak gate stays per case — and now holds for *every* running case,
      pinned or not (`run_performance_check` always measures; it used to run only
      because `performance` was mandatory except for `examples/`, which therefore
      escaped it). It also gates a file's `.performance` blocks in `check`. The
      tests that prove the gates fire use a test-only runtime switch,
      `AIPL_INJECT_LEAK`.
- [ ] Handoff (`handoff/src/discovery.rs` + unit tests, `main.rs`): recognize
      baseline-move failures by a distinct marker. Keep going through refills, IR
      regen and the final run; exit non-zero with the list. After the final run,
      run a scoped refresh for added or deleted cases. This is bookkeeping, not
      a reviewed move.
- [ ] `--- performance ---` becomes optional (remove the "missing required"
      failure).
- [ ] Audit which cases protect a real property and keep their section until
      Phase 4: `optimizations/`, `perf/`, `performance/`, plus the ~90 whose prose
      cites allocations or instructions. Record the list below, marking each case
      as expressible with `measure` or needing `--- report ---`.
- [ ] Delete the section from every other case, including all 111 under `crates/`.
- [ ] Docs: CLAUDE.md (Performance monitoring, Test `main` style, the handoff
      notes about performance refills) and the `tests/cases.rs` module docs.

### Phase 3: `.performance` and `measure` (syntax bootstrap: two handoffs)

- [x] `grammar_aipl.aipl`: add `.performance({ .. })` as a second `fn_attr`, with
      its parsed structures (`item.aipl`, `parsed.aipl`). `walker.aipl` lays it out
      the way it lays out `.test`. Write both in syntax the checked-in parser
      already knows. (`walker.aipl` needed no change: `fn_attrs` already lays out
      any `.name({ .. })` generically.)
- [x] Hand off; this regenerates the artifact. Confirm the form round-trips
      through `aipl fmt` before writing it anywhere else.
- [x] Treat `.performance` as `.test` for now (Drew's call): `check` runs it and
      `assert` works in it. Design, since the Rust AST has no field for it:
      - The parser keeps the attribute on `Function` (`performance:
        PerformanceBlock?`, parser-side only). The bridge (`ffi_ast.rs::item`)
        turns it into a second item right after the function: a private
        `__performance$<f>` with no parameters, a unit body, and the block as its
        `.test` body and helpers. Everything downstream then handles it as the
        test code it is: `check` runs it, an imported file's is dropped, `assert`
        is baked, helpers are hoisted.
      - Names: `aipl_syntax::performance_fn_name` / `performance_source_name`;
        `is_test_body` / `is_test_helper` understand the `__test$__performance$<f>`
        owner. `aipl check` reports it as `f.performance`; `aipl-index` skips the
        stand-in.
      - Later phases tell a performance block from a test by that name (`measure`
        legal only in one; excluded from production), the way `is_test_body`
        already drives `?` and `assert`.
      - Parser messages for a block's helpers still say `.test` block (they share
        a production); only the duplicate-helper message names the attribute.
- [x] `measure` builtin and `Measurement` struct (allocations, bytes allocated,
      peak live bytes, instructions). `builtin_measure.aipl` (AIPL, generic), over
      four internal builtins: `measure_begin`, `measure_consume`, `measure_end`,
      `measure_read(i)`. Decisions:
      - **Effect.** `measure` and the four carry the internal `!measure` effect
        (in `KNOWN_EFFECTS`). It is what makes "only in a `.performance` block"
        and "no other effect in one" fall out of the existing effect discipline:
        the performance test functions declare `measure` and nothing else
        (`build_performance_program`), the ordinary test functions declare
        everything *except* it. `undeclared_effect_message` turns the two cases
        into targeted messages.
      - **Window.** Instructions are counted per basic block on entry, so `begin`
        and `end` get blocks of their own in codegen (`Callee::MeasureBegin |
        MeasureEnd`); otherwise work inside one block counts nothing. A
        measurement includes a fixed overhead (an identity closure costs 5
        instructions) that cancels in comparisons.
      - **Result dropped inside the window**: `measure_consume(f(input))` is a
        generic intrinsic whose codegen arm releases a fresh temporary there, and
        which the optimizer cannot remove (an unused pure `let` is eliminated
        outright; verified).
      - **Opaque input**: no explicit barrier. The lambda-specialized `measure`
        instance is a call boundary, and constant inputs are not folded (verified:
        identity 5, `n + 1` 7, a polynomial 12). Add a barrier if a future
        optimization specializes on constant arguments.
      - **Peak live bytes** are tracked by the *instrumented* AOT runtime only: each
        block gets a 16-byte size header there (`bytes allocated` still counts the
        request), and the peak restarts from the live total when a window opens.
        The JIT never instruments, so its `measure_*` are no-ops reading 0; the
        Phase 1 "both runtimes" item is done in that sense.
      - `label` is not used by the runtime yet (reserved for `aipl benchmark`'s
        replay). Computed labels are not checked for duplicates.
- [x] `measure` outside a `.performance` block is a compile error ("fn \"measure\"
      can only be called from a `.performance` block"); a duplicate *literal* label
      in one block is an error (`aipl_syntax::performance_diagnostics`, reported by
      `check` before anything runs). Labels are per block.
- [x] No effects: the performance test functions declare only `!measure`, so a
      block or a measured closure that uses an effect gets "effects are not
      supported in a `.performance` block yet: fn \"print\" has effect
      \"!prints\"". Covers a named function passed as the closure too.
- [x] `.performance` blocks are excluded from production builds and from
      `dogfood.o`, the same as `.test`: a program with a `measure`-using block
      builds to a byte-identical IR and binary (checked by hand). No dogfooded
      source uses `.performance` yet; Phase 5 will, and goes through the same
      path.
- [x] `aipl check` runs them instrumented. They run apart from the `.test` blocks,
      in an AOT build of `build_performance_program` linked against the
      instrumented runtime (`aipl::perf::run_performance_tests`), because the
      in-process JIT does not instrument. The AOT runtime's test hooks now report
      failures in the JIT's exact format when `AIPL_TEST_REPORT` is set (its value
      is the batch file label) and write their tally to `AIPL_TEST_TALLY`; `check`
      folds it into the one aggregate. Single-file mode now prints the summary
      itself, after both kinds have run. A failing or uncompilable block fails the
      file and stops nothing else. **A file with a `.performance` block therefore
      needs the same linker `aipl build` does.**
- [x] Cases under `tests/cases/performance/`: `measure_claims` (no allocation for
      inline values, an early-exit relative claim, allocation and peak, peak
      relative to what is already live, an absolute bound) and
      `measure_claim_fails` (a failing claim pinned by `--- check ---`). The
      compile-time refusals (outside a block, effects, duplicate label) are in the
      Rust `check` suite, since their messages are on stderr.
- [x] Docs: CLAUDE.md ("`.performance` blocks and `measure`"), README, `check`'s
      usage text, and the `# ..` documentation on `measure` itself, which `aipl docs`
      renders.

### Phase 4: migrate property cases, retire `--- performance ---`

- [ ] Convert each audited case (list below) to `.performance` claims, or to a
      `--- report ---` section where only the function list shows the property
      (`*_not_minted`, inlining).
- [ ] Remove the section kind: `Spec::performance`, `PerfStats` rendering/parsing,
      the fill path, `EXTERNAL_BYTES`, and the docs.

### Phase 5: whole-compiler metric

- [ ] `.performance` blocks on the dogfood entry points: parse (`aipl_parse_file`)
      and format (`format_source`) over representative inputs embedded as `"""`
      literals, which keeps them effect-free. Use absolute bounds with headroom.
- [ ] Confirm the blocks stay out of `dogfood.o`. The source change still
      regenerates the IR through handoff.

### Phase 6: `aipl benchmark`

- [ ] Counting pass records label → `Measurement`. The timing pass runs the
      production build and replays the recorded values by label, with warmup and
      an adaptive iteration count. It reports the median and spread per label
      beside the counts.
- [ ] Only `.performance` blocks; `main` only when it has one.
- [ ] `--save <file>` / `--compare <file>` for user-side trend tracking (not
      asserted).
- [ ] Tests: the deterministic half exactly; the timing half by shape only (labels
      and columns present).

### Phase 7: `aipl check` validates every section

Each item below can land separately.

- [x] `check` parses trailing sections and validates stdout, stderr, exit code,
      cli, errors, expect file. A mismatch is a check failure that does not
      suppress the others. Staging `--- file: ---` companions into a scratch dir
      is part of this item, since `import`s and `expect file` can't resolve
      without it. Landed in `src/sections.rs` and `check_cmd`; decisions:
      - `main` is run only when the file has a section that observes a run
        (`stdout`, `stderr`, `exit code`, `cli`, `expect file:`). A file with none
        is a plain program and is not executed. Once a run happens, an absent
        `stdout`/`stderr`/`exit code` means empty/empty/0, as in the harness.
      - A file with companions is loaded from a staged copy of its source rooted
        at the scratch dir; one without is loaded in place. An `errors` file is
        compiled from `spec.source`, so end-of-file diagnostics render as the
        harness renders them.
      - `errors` diagnostics name the file `./`-relative to where `check` started,
        however it was pointed at the file.
      - `--ignore-sections` checks the tests alone. The harness passes it until
        the harness item below makes `check` own the sections.
      - `performance`, `check` and `check exit code` are not validated by `check`.
- [ ] Rename `--- check ---` / `--- check exit code ---` to a name that says they
      pin the `.test` runner's report (choose when implementing) and migrate the
      handful of cases that use them.
- [x] `--- report ---`: `check` validates a pinned `aipl report`. The body is the
      human form `aipl report` prints, trailing newlines stripped, compared as
      written. Landed across: `parse_spec.aipl` (`report` field; a regeneration of
      the artifact before the Rust bridge reads it), `SpecFields`, `sections::report`
      (built through `perf::measure_program`, the function `aipl report` now also
      uses), the harness (`Spec.report`; measured once with `performance`;
      refilled where present, never added; exclusive with `errors`). A leak is a
      finding of its own. Ported from Rust tests into cases under
      `tests/cases/report/`: the smallest program, a program that calls into the
      runtime (with a `--- cli ---`), and a library's test run. They still carry the
      mandatory `--- performance ---`, so for now each pins one run twice; Phase 2
      and 4 make that section optional. The `*_not_minted` and inlining cases are
      the intended users and move over in Phase 4.
- [ ] Leak gate: `check` flags allocations ≠ deallocations on the runs it already
      performs. First check whether the allocation tally is cheap enough in the
      production runtime; otherwise flag leaks only where an instrumented run
      happens.
- [ ] Fill flag on `check` (name TBD) rewrites mismatched sections and replaces
      `fill_expected`.
- [ ] Harness: each case becomes `aipl check <case>` plus whatever `check` doesn't
      own yet (`--- performance ---` before Phase 4, the baseline comparison).
      Delete the duplicated section logic from `tests/cases.rs`.
- [ ] Handoff: the refill step calls `check`'s fill flag on the stale cases.
      Update the discovery parsing and its unit tests.
- [ ] Docs: CLAUDE.md ("Authoring error-case fixtures", every `fill_expected`
      mention), `tests/cases.rs` module docs.

## Decided

- **Calls-only properties** are pinned with a `--- report ---` section that `check`
  validates.
- **Effects** are not allowed in `.performance` blocks for now. Allowing them is a
  future project.
- **Peak live bytes** is a `Measurement` field, a baseline column, and part of
  `aipl report`.
- **`--- check ---`** becomes the `.test` runner's report under a clearer name.
- **Thresholds** will be decided by Drew at the start of Phase 2.
- **Order** is flexible (see Phases).

## Future (out of scope here)

- Effects in `.performance` blocks and measured closures, including what the
  benchmark timing pass does with repeated output.

## Audit: cases that keep `--- performance ---` until Phase 4

_(filled in during Phase 2)_
