# Working in this repo

## Committing
Don't ask whether to commit, and don't offer to — I always handle commits
myself. Finish a task at the green-and-formatted state (run the handoff script,
see below) and stop there; leave the working tree uncommitted.

## Handoff (`scripts/handoff.sh`)
`scripts/handoff.sh` **is** the pre-handoff validation — run it once, at the
end of a task, as the whole finish-a-task gate. It runs the finish sequence in
dependency order and pays for the expensive regeneration steps only when a test
run proves they're needed: `cargo fmt` + `aipl fmt` the corpus, a discovery
run, then (only if that surfaced fillable staleness) scoped
`fill_expected` refills of exactly the mismatched cases and the staged
dogfood-IR regen/validate/promote flow, then a final run. It exits 0 on
green (printing what it refilled/regenerated and flagging behavioral-output
changes to review in the git diff), or stops with a pointed message naming the
step and why on any failure a refill can't fix.

The script runs the suite with **`cargo nextest`** (each test in its own
process; requires `cargo install cargo-nextest`). One consequence it handles for
you: nextest defaults to fail-fast, so every whole-suite run there passes
`--no-fail-fast`. (Nextest also skips doctests; the repo has none left — the
documented examples are ordinary tests in `tests/ffi.rs`, so they run with the
rest of the suite.) Your own inner-loop runs can use either
runner — the cadence commands below are written for `cargo test`, and
`cargo nextest run -E 'test(<name>)'` is the nextest equivalent.

**Don't hand-drive the sequence, and don't validate before it.** The script is
the validation *and* the order. Running its steps yourself — `cargo fmt`,
`fill_case_tests`, `fill_expected`, the staged dogfood-IR flow — doesn't just
pay for the corpus twice; it reorders steps that depend on each other, and the
breakage that follows looks exactly like a real bug, so you then debug your own
sequencing. The dependencies that bite:

- **Formatting shifts spans, so it must come first.** `aipl fmt` moves spans,
  string-literal data symbols are span-named, and the checked-in `.clif` plus
  every `--- performance ---`/`--- errors ---`/`--- check ---` section is
  span-sensitive. Stage IR (or refill sections) before formatting and what you
  just validated is already stale.
- **A new or deleted case file needs its `#[test]` list regenerated before a
  suite run means anything** — until then the case simply never runs.
- **Section refills come after the discovery run**, which is what establishes
  which sections are genuinely stale rather than refreshing on suspicion.

The script does all of this in dependency order and pays for the expensive
regeneration only when a test run proved it necessary. The only thing worth
running ahead of it is a **targeted** test for the code you just touched
(`cargo test -- name_substring`, `cargo test --test <file>`, or a scoped
`cargo test --test cases -- cases_generics`) as a fast inner-loop check — that's
a dev-loop signal, not the handoff gate. When the change is ready, run the
script and let it format, verify, and regenerate in one pass.

**The one sanctioned deviation.** When a change invalidates *hundreds* of
`--- performance ---` sections at once (anything that moves a counter in every
binary — a codegen change, a new baseline runtime call), handoff's per-case
`AIPL_CASE` refill loop spawns one `nextest` per mismatched case and becomes
pathological. Run the whole-corpus refill once
(`cargo test --test cases -- --ignored fill_expected`), then hand off. Two
guards on that: it's only worth it at that scale, and a blanket refill can bury
a real regression, so afterwards diff the corpus and confirm every changed line
is a metric — no `--- stdout ---`/`--- errors ---` body should move.

## Shell
Use the **Bash** tool for everything terminal-side: `cargo build`,
`cargo test`, `cargo fmt`, `git`, env-var-prefixed runs
(`AIPL_CASE='x' cargo test ... --ignored fill_expected`), file operations.

## Test cadence
Avoid running the full test suite during development — it is the slowest part
of the dev loop. Prefer:
- A single test by file: `cargo test --test mono`
- A single test by name: `cargo test -- name_substring`
- A filtered case run: `cargo test --test cases -- cases_generics`
  (every case is its own `#[test]`, named after its display path with the
  separators flattened — `cases/generics/ord_bound` is `cases_generics_ord_bound`
  — so libtest's own name filter scopes a run, and it reports how many it ran
  and how many it filtered out)

Targeted runs alone can miss regressions in unrelated areas, so they're the
inner-loop signal, not the finish check — finish a task by running the handoff
script (see the Handoff section above), which does the full `cargo test` (and
formatting/regeneration) for you.

Let the tests do the verifying. Prefer running `cargo test` (targeted, then
full) over reading generated artifacts, diffs, or output by hand to convince
yourself a change is correct — the suite already asserts on all of it, so a
manual pass mostly burns tokens re-checking what a green run guarantees. Only
drop into manual inspection when a test actually fails and you need to
understand *why*, or for the rare thing no test covers.

## Formatting
Every task ends formatted (`cargo fmt` for Rust, `aipl fmt` for `.aipl`). You
don't run these by hand as a separate step — the handoff script (see the Handoff
section above) formats first, then tests, so a single handoff run leaves the
tree both green and formatted.

## AIPL source formatting (`aipl fmt`)
Every checked-in `.aipl` file (outside the formatter's own `tests/fmt/`
fixtures) must be in canonical format — the `all_aipl_files_stay_formatted`
test in `tests/fmt.rs` enforces it over `tests/cases/`, `crates/`, `examples/`,
and `tests/ffi_fixtures/`. So a **new or edited `.aipl` file must be formatted**:
run `aipl fmt <file>` (in place; `--check` reports without writing), or reformat
the whole corpus at once with the author helper
`cargo test --test fmt -- --ignored format_corpus`.

The formatter (`aipl-fmt` crate, `aipl::fmt::format_source`) is canonical
(gofmt-style — it decides line breaks; width defaults to 100) and works off the
lexer token stream, so it preserves comments/literals verbatim and leaves
trailing `--- section ---` blocks byte-for-byte. Because it is span-driven,
reformatting shifts spans and therefore invalidates the usual downstream
artifacts — treat a corpus reformat like any span-shifting change: refill
`--- performance ---`/`--- errors ---`/`--- check ---` sections with
`fill_expected` afterward (the checked-in `dogfood.clif` is renumbered and
stays stable, so it normally needs no regeneration — the `checked_in_ir_is_current`
test will tell you if it does). Fixture files under `tests/fmt/*.aipl` are
*intentionally* misformatted inputs and are exempt from the enforcement test.

## Performance monitoring (non-deterministic)
Two separate perf tracks:
- **Deterministic, asserted**: the per-case `--- performance ---` sections
  (allocations, instructions executed, binary size). These have hard assertions
  and are filled/refreshed with the `fill_expected` ignored test (see below).
- **Non-deterministic, *not* asserted**: `tests/performance_metrics.md` — a
  checked-in table of wall-clock (measured in-process, so process spawn/teardown
  is excluded), build time, and peak RSS per case. These drift run-to-run, so
  nothing asserts on them; they exist only to track trends.

Refresh the non-deterministic table with:
`cargo test --test cases -- --ignored refresh_perfmon`
It runs serially, rewrites `tests/performance_metrics.md`, and prints an
overall (summed-across-tests) change plus per-test wall-clock outliers and
added/removed tests, then fails intentionally so the summary is visible. Review
the printed summary, then commit the regenerated file. The runtime self-times
only when `AIPL_PERFMON_STATS` points at a file, so normal runs and `aipl build`
binaries are unaffected.

The two author-helper "refresh" modes are `#[ignore]`d tests, not env vars: a
normal `cargo test` skips them; opt in by name with `-- --ignored <name>`. The
relevant failure messages (a missing/stale `--- performance ---` section, a
perf mismatch, a stale perfmon table) print the exact command to run.

`fill_expected` always overwrites every section with current values — no need
to set bodies to `?` first.

## Prefer the cases framework for tests
Default to the `tests/cases/**/*.aipl` framework over Rust unit tests in
`tests/*.rs`. A case file is just real AIPL source plus the expected
`--- stdout ---` / `--- exit code ---` / `--- errors ---`, so it doubles as
documentation: a reviewer sees exactly what a user would write and exactly
what they'd get back. Rust unit tests that embed source as escaped string
literals (e.g. `"fn f() { \"\"\"...\"\"\" }"`) and assert on internal AST
shapes are hard to read and hide the user-facing behavior — avoid them for
language features. To check an exact string value from a case, `print` it
(wrap in markers like `"[" + s + "]"` to make whitespace/empty visible) or
compare with `==` and return a distinguishing exit code. Reserve `tests/*.rs`
for things the cases framework genuinely can't express (e.g. asserting on a
parser/loader API directly).

**Adding or deleting a case file needs one extra step — which handoff takes for
you.** Every case gets its own `#[test]`, and `#[test]` is compile-time while
cases are discovered at run time — so the list lives in the checked-in
`tests/support/case_tests.rs`. `scripts/handoff.sh` regenerates it when the tree
and the list disagree, so adding or deleting a case is not a reason to run
anything by hand. The command exists for when you want it directly:
`cargo test --test cases -- --ignored fill_case_tests`. You don't have to
remember either way: the `every_case_has_a_test` test fails when the list and the
tree disagree (in either direction) and prints that command. It's a hard failure
precisely because a case with no `#[test]` would otherwise never run.

Note that every `.aipl` under `crates/` is *also* discovered as a case, so
adding a dogfooded source adds a case too — it needs the same `#[test]` entry
and the same `--- performance ---` section any case does. Handoff fills both.

## Operators must be imported
Operators are not ambient — a file that uses `==`, `<`, `&&`, unary `-`/`!`, etc.
must import each by spelling: `import { ==, < } from builtins;`. The `+` operator
is special: it's the `wrapping_add` builtin aliased to `+`, so it's imported as
`import { wrapping_add as + } from builtins;` (a bare `import { + }` is an error).
The
loader gates operator *usage* per file against its imports (unimported → compile
error). So every new `.aipl` (test case, example, embedded compiler source, and
each `--- file:` companion) that uses operators needs the matching import — and
since the import shifts line numbers, refill any `--- errors ---`/`--- check ---`
/`--- performance ---` sections (string-literal data symbols are span-named, so
`binary size` shifts too).

## Predicate methods (`is_*` functions)
Boolean predicates should be written as methods on their receiver, not
free functions — `c.is_digit()` reads more naturally than `is_digit(c)`.
Spell them as `fn is_name(self: T) -> bool`. When calling, use the method
syntax: `my_var.is_digit()` (or the equivalent function form `is_digit(my_var)`,
but prefer the method). This applies to all predicate functions, not just
compiler internals.

## AIPL functions used in the compiler must be well-tested
The compiler dogfoods AIPL via the FFI: some `.aipl` files under `crates/*/src/`
are JIT-compiled and called during compilation (e.g.
`crates/aipl-codegen/src/add.aipl`). Every such function must be well-tested —
attach `.test({ assert(...) })` blocks covering its real behavior (including the
shapes the compiler actually calls it with). These tests run via `aipl check`,
and the `compiler_aipl_files_are_tested_and_pass_check` test in `tests/ffi.rs`
discovers every `.aipl` under `crates/`, requires each to carry a `.test` block,
and runs one `aipl check crates` over the whole directory — so an untested or
failing compiler-FFI function fails the suite. Re-run that check by hand with
`cargo run -q -- check crates`, or narrow it to one file with
`cargo run -q -- check <file.aipl>`.

## No native fallbacks for dogfooded functions
A dogfooded AIPL function is the **single source of truth** — never write a
native (Rust) reimplementation of its logic as a fallback for when the engine
isn't available. If the dogfooded engine can't be reached (e.g. its hook isn't
installed, or the checked-in IR fails to load), **fail loudly** (panic) rather
than silently substituting a Rust version. The parser reaches the dogfooded
`process_raw_string` / `parse_test_section_header` through installable hooks with
no fallback, so any in-process parse must `install_parser_hooks()` first — tests
that parse directly do this (e.g. via a `parse` wrapper or in `setup_cases`).
Keeping one implementation avoids the two drifting apart and keeps the AIPL
genuinely exercised.

## Multiple runtime representations: classify + `match`, don't `is_*`
A single source type can have several runtime representations chosen by context
(the first is `str`: inline / heap / view / concat — see the "Representation
dispatch" sections in `crates/aipl-codegen/src/lib.rs` and the linker runtime,
kept byte-for-byte identical). When a runtime helper branches on *which*
representation a value is, classify it once into the representation `enum` (e.g.
`str_repr(v) -> StrRepr`) and **`match`** on it — do **not** chain ad-hoc
`is_inline()`/`is_view()`/`is_concat()` boolean checks. The `match` is
exhaustive, so adding a representation makes the compiler flag every dispatch
site that doesn't yet handle it, instead of silently falling through to a
heap/`else` arm. Spell variants out (group with `|`, e.g. `Null | Heap`) rather
than using a bare `_`, so a new representation still forces a decision at each
site. Reserve a plain `is_*`/`matches!` boolean only where a `match` genuinely
doesn't fit and the advantage is clear. This pattern is meant to generalize to
future multi-representation types, not just `str`.

## Test `main` style: prefer a void `main`
When a test case's `main` exists only to drive the program (its return value
isn't the thing under test), write a **void** `main` — `fn main() { ... }` — not
`fn main() -> i64 { ...; 0 }`. A trailing literal `0` is extraneous: a void
`main` already exits 0. Reserve `fn main() -> i64 { ... }` for cases whose point
*is* the return value (e.g. an `--- exit code ---` test, or a `main` that returns
a computed expression being checked). This applies to new cases and to edits;
note that switching a `main` between `-> i64 { ...; 0 }` and `{ ... }` shifts the
`instructions executed` / `binary size` counters, so refill `--- performance ---`.

## Fanout updates from test failures
When a language change forces fixture/example edits across many files, don't
grep-and-patch them up-front. Make the language change first, then let the
final-pass `cargo test` enumerate the failing fixtures — the failure list is
authoritative (catches files the grep would miss and skips ones it would
falsely match). Update fixtures from that list, then re-run.

The same goes for *estimating* a change's blast radius: the best way to learn
how many tests a change impacts is to make the change and run the suite, not to
scan every test ahead of time. A grep over fixtures consistently over- or
under-counts (e.g. a token may appear in `--- stdout ---` or source, not the
assertion that actually breaks), so don't bother — just implement, run, and read
the failure list.

## Staged IR workflow for dogfood IR changes

Any change that affects how the compiler generates Cranelift IR (new builtins,
type layout changes, codegen restructuring, etc.) also invalidates the checked-in
`*.clif` artifacts. Use the staged IR workflow instead of calling `fill_dogfood_ir`
directly — it lets you validate candidate IR before it becomes the live IR the
compiler runs on.

> **`scripts/handoff.sh` already runs this entire flow**, in its correct place
> in the sequence: *after* the corpus is formatted (spans settled) and *after* a
> discovery run has shown the IR is actually stale. Adding a dogfooded `.aipl`
> or changing codegen is **not** a reason to run the steps below by hand — just
> hand off. Reach for them only to localize a failure handoff already reported,
> or to inspect a candidate artifact on purpose. Driving them yourself ahead of
> a handoff is how you stage IR from unformatted source and then validate an
> artifact that is stale before you finish.

**The one case handoff genuinely cannot bootstrap: a change to the formatter's
own grammar.** `crates/aipl-codegen/src/walker.aipl` compiles into `fmt.clif`,
and handoff formats the corpus (step 1) *before* regenerating IR (step 5) — so
the live formatter, which doesn't know the new syntax yet, is what formats
sources written in it, and the run stops at `aipl fmt`. Adding a syntax form
means teaching two parsers: the gazelle grammar in `aipl-parser` *and* the
formatter's own token walker. To get out of the loop:

1. `fill_staged_ir` — builds a `fmt.clif.staged` that understands the new form.
2. Format the corpus with the staged formatter:
   `AIPL_FMT_IR=<abs>/fmt.clif.staged AIPL_DOGFOOD_IR=<abs>/dogfood.clif.staged cargo test --test fmt -- --ignored format_corpus`
3. `fill_staged_ir` **again** — step 2 shifted spans, so the IR from step 1 is
   already stale.
4. Validate + promote as below, then run handoff normally for the refills.

This is not a licence to hand-drive generally: it announces itself as a hard
`aipl fmt` failure, and it's the only ordering the script can't express.

The steps, for those cases:

1. **Generate staged IR** — compiles each `.aipl` source with the new frontend
   and writes `*.clif.staged` files next to the live `*.clif` files:
   `cargo test --test dogfood_ir -- --ignored fill_staged_ir`

2. **Entry-level pre-check (fast)** — loads each `*.clif.staged` and calls its
   entry functions with known inputs; confirms the IR links and each entry
   computes correctly. This does *not* run the compiler on the staged IR — it's
   just the quick gate before the corpus run:
   `cargo test --test dogfood_ir -- --ignored validate_staged_ir`

3. **Validate by running the corpus against the staged IR, not by reading the
   diff** — the real check is running the whole suite with the compiler itself
   linking the staged files, via the `AIPL_DOGFOOD_IR` and `AIPL_FMT_IR` env
   vars (each points one engine at an alternate `.clif` instead of the baked-in
   one), so every parse in the corpus — including the compiler parsing its own
   source — exercises the candidates:
   `AIPL_DOGFOOD_IR=<abs>/dogfood.clif.staged AIPL_FMT_IR=<abs>/fmt.clif.staged cargo test`
   The path **must be absolute** — the cases harness spawns the compiler as a
   subprocess whose CWD isn't the repo root, so a relative path won't resolve
   there. (The `fill_staged_ir` / `validate_staged_ir` messages print the exact
   command with the absolute path filled in.) Under this env var
   `no_staged_ir_pending` is suppressed and `checked_in_ir_is_current` compares
   source against the staged file, so a good candidate is a clean green run.
   Manual review of the `.staged` vs live diff is only worthwhile when this run
   *fails* — then diff to localize, paying attention to the **parser-hook
   engines** (`process_raw_string`, `parse_test_section_header`,
   `strip_test_sections`, `find_trailing_whitespace`, `lex_aipl`), which are
   active during every parse of the compiler's own source.

4. **Promote staged → live** — validates again, copies `*.clif.staged` →
   `*.clif`, deletes the staged files, then fails intentionally so you review
   the final diff before committing:
   `cargo test --test dogfood_ir -- --ignored promote_staged_ir`

5. **Run full suite**: `cargo test`

**Invariant**: a plain `cargo test` always fails while any `*.clif.staged` file
exists (the `no_staged_ir_pending` test in `tests/dogfood_ir.rs`) — so the
transition is never silently left half-done. The only exception is the step-3
validation run, where `AIPL_DOGFOOD_IR` is set and that check is deliberately
suppressed. To abort a staged workflow, delete the `*.clif.staged` files from
`crates/aipl-codegen/src/`.

`fill_dogfood_ir` (writes directly to live) is still available for obviously-safe
regenerations (e.g. a source comment changed the artifact with no logic change),
but prefer the staged workflow for any change that touches IR generation logic.

## Authoring error-case fixtures
Never hand-write the expected error block in a `tests/cases/` error fixture.
The expected text must match the compiler's `Error::render` byte-for-byte —
caret columns, and even a trailing space on an empty source line — so
transcribing it by hand is error-prone. Instead write a `--- errors ---`
section (any body is fine) and run the `fill_expected` helper, scoped to the
fixture with `AIPL_CASE`:
`AIPL_CASE='structs/err_foo' cargo test --test cases -- --ignored fill_expected`.
The harness writes the actual rendered error back into the fixture (and fails
that run intentionally); review it, then re-run normally to confirm it passes.
This also avoids a rendering mismatch: the harness renders against
`spec.source` (trailing newlines stripped), which differs from `aipl run
<file>` for EOF-positioned errors.
