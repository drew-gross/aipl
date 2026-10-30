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
Rust-side recipe for the fixtures, the one-line hit span and `#[allow]`
squelching is in the "Authoring a lint" section of CLAUDE.md and in the
add-builtin skill's step 4; it applies unchanged.

## Porting a Rust lint

Do not transliterate it. The Rust lint predates these rules, so a port applies
all of sections 1–3 to it: look for shared helpers (the Rust lint's private
functions often duplicate ones already in the lint libraries), return
`UnfilledHit[]` if its message names a builtin (rather than carrying an import
parameter or calling `imported_as` itself), and inline helpers with one caller.
Anything the port moves into a library gets its `.test` blocks and, if the Rust
lints still use the original, a note to the user that the Rust copy is now a
duplicate to delete once its lint is ported.

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

## 4. Finish

Each lint file needs `.test` blocks (compiler AIPL must be well-tested), the
three fixtures under `tests/cases/lints/` (`err_<lint>`, `<lint>_allow`,
`<lint>_not_flagged`), a registration, and a `DOGFOOD_SOURCE_FILES` entry. Then
`cargo handoff` once.

Bootstrap trap: removing a Rust lint and adding its AIPL replacement in one
change fails discovery with `#[allow] squelches nothing` on the markers that
the Rust lint used to consume, until the artifact is promoted. Strip those
markers, run the staged-IR flow by hand once, restore them, then hand off.
Say that you did.
