---
name: add-builtin
description: Add a new builtin utility to AIPL (find_if, count_if, trim_while, …) — the AIPL-implemented builtin file, the two Rust registration points, the .doc/.test block, dogfooding it in the compiler's own sources, and the lint that flags ad-hoc reimplementations of it. Use when asked to add a builtin, a standard-library function, or a utility to `import { .. } from builtins`.
---

# Adding a builtin utility

A builtin is a function every file can `import { name } from builtins;`. Default
to implementing it **in AIPL** (`crates/aipl-mono/src/builtin_<name>.aipl`) — the
file is then the single source of both signature and body, and it doubles as its
own test suite and documentation. Reach for Rust only when AIPL can't express it.

Ship a builtin as three things, in this order: **the builtin**, **the sites in
this repo that should now use it**, and **the lint that keeps new ones from
appearing**. A utility nobody reaches for is dead weight, and the lint is what
makes the codebase converge on it — see "Lint against ad-hoc reimplementations".

## 1. Write the AIPL implementation

Copy the shape of a neighbour — `builtin_count_if.aipl` and `builtin_any.aipl`
are the best models for a predicate-taking utility, `builtin_first.aipl` for a
one-liner.

```aipl
import { equal as ==, filter, greater_than as > } from builtins;

// The AIPL implementation of the `find_if` builtin. Monomorphization registers
// this function under the canonical name `__builtin_find_if` (see
// `AIPL_BUILTIN_SOURCES` in `lib.rs`), so a user's `xs.find_if(pred)` resolves
// through the ordinary generic / higher-order machinery — the in-file name
// `find_if` is only for this file's own tests, which read exactly like user code.
pub fn find_if<T: any>(self: T[], pred: (T) -> bool) -> T? {
    for (let x : self) {
        if (pred(x)) {
            return some(x);
        };
    }
    none
}.doc("""
    What it returns, when it stops, and what an empty receiver gives. Say it is
    callable as a method, and cross-reference the siblings a reader might have
    wanted instead (`any` reports only *that* a match existed; `first` is the
    unconditional first element).
""").test({ .. })
```

Rules that bite:

- **First parameter is `self`**, so the builtin is method-callable
  (`xs.find_if(p)`); the free-call form `find_if(xs, p)` is the same AST.
- **Operators must be imported, always aliased** (`equal as ==`), including in
  this file. Bounds are `<T: any>` / `<T: ord>` / `<T: variant>` only.
- **`char` has no `<`/`>` yet** (TODO.txt item 1) — write char tests with `==`.
- Prefer a **void `main`** and, in predicates, the `fn is_x(self: T) -> bool`
  method form.

### The `.test` block is not optional

`compiler_aipl_files_are_tested_and_pass_check` (tests/suites/ffi.rs) fails any `.aipl`
under `crates/` with no `.test`. Cover, at minimum: the ordinary case; the
not-found / empty case (a *typed* empty is `[1, 2].filter(|x| x > 100)`); the
short-circuit or ordering guarantee the doc claims (first-vs-last match); the
free-call form; a non-i64 element type (`char`, `str`) and a `str` receiver if it
iterates chars; a named predicate; and a captured bound. Add a small
`fn is_nine(self: i64) -> bool { self == 9 }.test({ .. })` helper when you need a
named predicate — it needs its own `.test` too.

Run it in a second: `cargo run -q -- check crates/aipl-mono/src/builtin_<name>.aipl`.

## 2. Register it — exactly two Rust edits

| file | edit |
|---|---|
| `crates/aipl-mono/src/lib.rs` | add `("__builtin_<name>", "builtin_<name>.aipl")` to `AIPL_BUILTIN_SOURCES` |
| `crates/aipl-syntax/src/lib.rs` | add `"<name>"` to `IMPORTABLE_BUILTINS`, and to the `// NOTE:` list of AIPL-implemented builtins above it |

**Do not add a signature to `BUILTIN_SIGNATURES`** — that list is for builtins
whose body lives in Rust; the `.aipl` file is the source of the signature for
these, fed to the checker by `aipl_mono::aipl_builtin_sig_decls()`.

Nothing else needs touching. The per-case `#[test]` list
(`tests/support/case_tests.rs`), the `--- performance ---` section every `.aipl`
under `crates/` needs as a discovered case, and any IR regeneration are all
`cargo handoff`'s job — don't hand-drive them.

**If AIPL can't express it** (raw memory, a new runtime call), the path is
different and heavier: a declaration in `BUILTIN_SIGNATURES`, lowering in
`aipl-mono`/`aipl-codegen`, and matching runtime support in *both* the JIT and
the linker. Read an existing intrinsic end to end before starting.

## 3. Dogfood it

Grep the repo for the shape the builtin replaces and convert the real sites —
starting with `crates/**/*.aipl`, which is compiler source. This is the honest
test of the signature (`find_prec` in `crates/aipl-codegen/src/parse.aipl` was
the shape that motivated `find_if`, and converting it proved the lambda captured
two outer bindings fine). Add a `tests/cases/` case for the builtin as user code
too — `tests/cases/lambdas/<name>.aipl` for a higher-order one.

Expect the conversion to show up in perf sections as a new
`__builtin_<name>$…$lam<N>` entry and a small instruction-count shift in every
case that transitively compiles the converted file. That's the refill handoff
does, not a regression.

## 4. Lint against ad-hoc reimplementations

**Standard guidance: every new utility gets a lint that flags the longhand it
replaces.** That is how the builtin actually lands: the utility makes the short
spelling possible, the lint makes it the one people write. Precedents to copy —
`push_loop_pipeline` (→ `map`/`filter`), `return_loop_find_if` (→ `find_if`),
`match_value_or`, `match_is_some_and`, `match_map_ok`/`match_map_err`,
`len_gt_zero` (→ `is_nonempty`), `incr_by_one` (→ `++`).

**Layout.** One lint per file: `crates/aipl-syntax/src/lint/<name>.rs`, holding a
`pub(super) fn <name>(..)` and that lint's own helpers. Add `mod <name>;`, a
`use self::<name>::<name>;`, and a call in `check` — all in
`crates/aipl-syntax/src/lint.rs`, which also holds the helpers more than one lint
shares: `line_of`, `lone_stmt`/`is_block_tail`, `spans_its_text`, `lambda_safe`,
`imported_as`, `end_is_receiver_len`. Name the lint `<bad shape>_<replacement>`.

**Match exactly one shape, and bow out of every near-miss.** Before flagging,
ask what the rewrite would silently change: work the builtin has nowhere to put
(a second statement, an `else`, a fall-through that isn't the not-found answer);
an expression lifted into a lambda that mentions state the rewrite deletes or
propagates with `?` (`lambda_safe`); an effect that would be reordered. A shape
you can't advise precisely is one to leave alone — a mis-advised lint is worse
than a missing one. Put each near-miss in the `_not_flagged` fixture with a
comment saying *why*.

**The hit's span must be one line that survives `aipl fmt`.** `#[allow]` is
line-scoped, so a span covering a multi-line construct can never be squelched —
and an unsquelchable lint that fires on an AIPL-implemented builtin **panics the
compiler for every program that reaches that builtin**. Point at a short
statement inside the shape (the `mut acc = [];` seed; the `return some(x);`).
Your own new builtin will usually trip the new lint — it *is* the longhand — so
squelch it there with a trailing `#[allow]` and a comment saying why, then
confirm `aipl fmt` keeps the marker on that line.

**Message shape**, matching the others:

```
"<what this code is> — write \"<the rewrite>\" and drop the loop\
 {, importing `find_if` from builtins} (or append #[allow] to this line to keep it)"
```

Resolve the builtin's *local* name with `imported_as(program, "find_if")` so an
alias is followed, and name the import when the file lacks one. Splice a receiver
back into the advice only when `spans_its_text` says its span covers its text —
otherwise write the `<iterable>` placeholder, because a call's span stops before
its closing paren.

**Fixtures — three, under `tests/cases/lints/`:**

| file | holds |
|---|---|
| `err_<lint>.aipl` | every firing variant, with a `--- errors ---` section |
| `<lint>_allow.aipl` | the `#[allow]` squelch **and** the rewrite side by side, so both spellings run and are proven to agree |
| `<lint>_not_flagged.aipl` | every near-miss, each with a comment saying why |

Never hand-write the `--- errors ---` body — write `?` and let handoff (or
`AIPL_CASE='lints/err_<lint>' cargo test --test cases -- --ignored fill_expected`)
render it. Add the two companion names to the lists in `err_lints.aipl`; that
edit shifts its line numbers, so its error block gets refilled — expected.

**Fanout:** don't grep-and-patch the corpus ahead of time; implement, run, read
the failure list. The one grep worth doing first is over `crates/**/*.aipl` for
the shape, because a compiler-source hit is the one that panics if the span
turns out to be unsquelchable.

## 5. Finish

`cargo handoff`, once, at the end. Expect it to regenerate the case `#[test]`
list and refill `--- performance ---` widely. Then review the diff and confirm
every changed line is a metric or a rendered line number — no `--- stdout ---` or
error *message* body should move.
