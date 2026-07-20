# Working in this repo

## Committing
Don't ask whether to commit, and don't offer to — I always handle commits
myself. Finish a task at the green-and-formatted state (see the pre-handoff
sequence below) and stop there; leave the working tree uncommitted.

## Shell
Use the **Bash** tool for everything terminal-side: `cargo build`,
`cargo test`, `cargo fmt`, `git`, env-var-prefixed runs
(`AIPL_CASE='x' cargo test ...`), file operations.

## Test cadence
Avoid running the full test suite during development — it is the slowest part
of the dev loop. Prefer:
- A single test by file: `cargo test --test mono`
- A single test by name: `cargo test -- name_substring`
- A filtered case run: `AIPL_CASE='generics/' cargo test --test cases`
  (the cases harness intentionally fails when `AIPL_CASE` is set so a stray
  filter can't be mistaken for a green full suite)

But always finish a task with one full `cargo test` run as the pre-handoff
check — targeted runs alone can miss regressions in unrelated areas.

Let the tests do the verifying. Prefer running `cargo test` (targeted, then
full) over reading generated artifacts, diffs, or output by hand to convince
yourself a change is correct — the suite already asserts on all of it, so a
manual pass mostly burns tokens re-checking what a green run guarantees. Only
drop into manual inspection when a test actually fails and you need to
understand *why*, or for the rare thing no test covers.

## Formatting
Run `cargo fmt` at the end of every task, before handing the change back.
The pre-handoff sequence is: full `cargo test`, then `cargo fmt`.

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
and runs `aipl check` on it — so an untested or failing compiler-FFI function
fails the suite.

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
compiler runs on:

1. **Generate staged IR** — compiles each `.aipl` source with the new frontend
   and writes `*.clif.staged` files next to the live `*.clif` files:
   `cargo test --test dogfood_ir -- --ignored fill_staged_ir`

2. **Validate staged IR** — loads each `*.clif.staged` and calls its entry
   functions with known inputs; confirms the IR links and computes correctly:
   `cargo test --test dogfood_ir -- --ignored validate_staged_ir`

3. **Review the diff** — compare each `*.clif.staged` to the live `*.clif`
   manually. Pay extra attention to the **parser-hook engines**
   (`process_raw_string`, `parse_test_section_header`, `strip_test_sections`,
   `find_trailing_whitespace`): these functions are active during every parse,
   so a subtle bug in their IR affects the compiler's own source parsing.

4. **Promote staged → live** — validates again, copies `*.clif.staged` →
   `*.clif`, deletes the staged files, then fails intentionally so you review
   the final diff before committing:
   `cargo test --test dogfood_ir -- --ignored promote_staged_ir`

5. **Run full suite**: `cargo test`

**Invariant**: `cargo test` always fails while any `*.clif.staged` file exists
(the `no_staged_ir_pending` test in `tests/dogfood_ir.rs`). This ensures the
transition is never silently left half-done. To abort a staged workflow, delete
the `*.clif.staged` files from `crates/aipl-codegen/src/`.

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
