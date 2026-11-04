---
name: add-lint
description: Add or port a lint in AIPL (crates/aipl-codegen/src/lint/) — the lint file, its registration in lint.aipl, DOGFOOD_SOURCE_FILES, the fixtures, and the rules for sharing helpers and naming a builtin in advice. Use when asked to write, add or port a lint, including a "don't reimplement foo()" lint for a new builtin.
---

# Adding a lint

Lints are AIPL: one file per lint in `crates/aipl-codegen/src/lint/<name>.aipl`,
registered in `lint/lint.aipl`, listed in `DOGFOOD_SOURCE_FILES`
(`crates/aipl-codegen/src/lib.rs`), run on the written-form AST and returning
hits. Read a neighbour first: `match_value_or.aipl` (a lint over shared matching
helpers), `case_of_equal_is_case.aipl` (a lint that names a builtin). The
one-line hit span and `#[allow]` squelching are in the "Authoring a lint" section of
CLAUDE.md and in the add-builtin skill's step 4.

## 1. Check for existing utilities before writing any

DRY is critical here. Before writing a helper, search for it:

- `lint/lint_hit.aipl` — `hit`, `UnfilledHit`, `imported_as`, `on_functions`,
  `on_functions_unfilled`, the `lint_*` test drivers.
- `lint/field_uses.aipl`, `lint/slice_bounds.aipl`, `lint/option_match.aipl`,
  `lint/constant_default.aipl`, `lint/accumulation.aipl` (the seeded-empty-then-filled-by-a-loop
  shape and its pipeline advice), `lint/interpolation.aipl` — shared shape recognizers.
- The builtins and `crates/aipl-codegen/src/*.aipl` (expr walkers, `source`
  quoting, spans). `body.collect_expr_cases(Slice)` is every `Slice` under `body`,
  typed as that case alone, so a lambda can destructure it directly.

If it exists in a library, use it. **If the thing you need already exists as a
private helper inside another lint's file, move it into a library file (or a
new shared one) and have both lints import it** — do not copy it, and do not
leave it private to one lint. When you extract, make it `pub`, keep its `.test`
blocks with it, and add any new file to `DOGFOOD_SOURCE_FILES`.

## 2. Naming a builtin in advice: use `UnfilledHit`

A "don't reimplement `foo()`" lint tells the author to call a function. The
lint cannot know how the linted file spells it (alias, or not imported at all),
so it must **not** hard-code the name in the message and must not take an
import parameter. Return `UnfilledHit[]` — `unfilled(builtin, before, after,
span)` — and let the caller hydrate it with `filled` / `on_functions_unfilled`,
which fills in the file's spelling and adds the ", importing `X` from builtins"
hint when it is not imported. Register the lint in `lint.aipl` in its
unfilled form.

This covers **every builtin the message names, not only the one being
introduced**: advice such as `write "x.value_or(d)"` or `"x.is_some_and(|v| ..)"`
spells a builtin in a string, and so does one that names two (`.map(..).value_or(..)`).
`match_value_or`, `match_is_some_and` and `match_map_value_or` hard-coded their
names this way and had to be fixed. Before finishing, read every message string
you wrote and ask whether it contains a builtin's name; if it does, that name is a
segment the caller fills in.

## 3. No single-call-site functions without a good reason

Do not split a body into a helper that has exactly one caller; inline it. A
function earns its existence by being called from more than one place (or being
a deliberate, tested, `pub` library entry). The one accepted reason is a missing
language feature that forces the split (for example, a construct the checked-in
parser cannot express in one body). When you do that:

- put a comment at the helper naming the missing feature, and
- **report it to the user in your task summary**, so it can be undone when the
  feature lands.

## 4. Tests belong in the lint, not in fixtures

**A lint's tests are `.test` assertions in its own file.** Each assertion runs the lint's driver
(`lint_function_messages`, `lint_function_unfilled_messages`, `lint_messages`, ...) on one small source
string and checks the messages, so the flagged shape, the near-miss and the message text sit beside the
code that produces them and run in-process with the file's other tests. The three kinds:

- **Flagged:** `assert(driver("fn f(..) {..}", lint_fn)? == [message(..)])`, what the lint says of the shape.
- **Not flagged:** `assert(driver("fn f(..) {..}", lint_fn)? == [])` for every near-miss, each with a comment
  saying *why* it must be left alone. These are most of a lint's tests; write the near-misses first.
- **`#[allow]` and the rewrite agree:** helper functions declared in the test block, the longhand carrying
  `#[allow]`, and an assert that both spellings give the same answer. Keep this only when the advice could be
  wrong about meaning; a struct or variant it needs cannot be declared in a test block, so such a case stays a
  fixture.

A source string with `"""` raw blocks holds quotes and backslashes without escapes. The lint function never
sees `#[allow]` (the squelch is applied by the driver in `lint.aipl`), so a snippet that carries the marker
does not belong in these assertions.

**Do not add the per-lint fixtures** (`err_<lint>`, `<lint>_allow`, `<lint>_not_flagged` under
`tests/cases/lints/`). What stays under `tests/cases/lints/` covers the *infrastructure* only: that a hit is
rendered as a diagnostic with the import hint filled in, that `#[allow]` squelches a line and survives
`aipl check --fix`, and that a lint's hit points at one line. `allow_squelch.aipl` and the
`return_loop_find_if` triple are those; add one only for a behavior the drivers in `lint_hit.aipl` do not
exercise.

## 5. Finish

Each lint file needs `.test` blocks (compiler AIPL must be well-tested; section 4 says what they
hold), a registration, and a `DOGFOOD_SOURCE_FILES` entry. No per-lint fixtures. Then `cargo handoff` once.
