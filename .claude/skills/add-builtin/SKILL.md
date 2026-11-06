---
name: add-builtin
description: Add a new builtin utility to AIPL (find_if, count_if, trim_while, …) — the AIPL-implemented builtin file, its Rust registration, the `# ..` documentation and `.test` block, dogfooding it in the compiler's own sources, and the lint that flags ad-hoc reimplementations of it. Use when asked to add a builtin, a standard-library function, or a utility to `import { .. } from builtins`.
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

# The AIPL implementation of the `find_if` builtin. The loader names this
# function by the canonical name `__builtin_find_if` (see
# `aipl_syntax::builtin`), so a user's `xs.find_if(pred)` resolves
# through the ordinary generic / higher-order machinery — the in-file name
# `find_if` is only for this file's own tests, which read exactly like user code.
# What it returns, when it stops, and what an empty receiver gives. Say it is
# callable as a method, and cross-reference the siblings a reader might have
# wanted instead (`any` reports only *that* a match existed; `first` is the
# unconditional first element).
pub fn find_if<T: any>(self: T[], pred: (T) -> bool) -> T? {
    for (let x : self) {
        if (pred(x)) {
            return some(x);
        };
    }
    none
}.test({ .. })
```

Rules that bite:

- **First parameter is `self`**, so the builtin is method-callable
  (`xs.find_if(p)`); the free-call form `find_if(xs, p)` is the same AST.
- **Operators must be imported, always aliased** (`equal as ==`), including in
  this file. Bounds are `<T: any>` / `<T: ord>` / `<T: variant>` only.
- **`char` has no `<`/`>` yet** (TODO.txt item 1) — write char tests with `==`.
- **A builtin can be internal** — usable by `builtin_*.aipl` sources only —
  by listing it in `INTERNAL_BUILTINS` (`aipl-syntax`); the loader refuses the
  import anywhere else. `reserve` is one, pending a decision on exposing it.
- **It is an ordinary file.** The loader loads it as one more file of every
  program that reaches it, so it is checked, linted and compiled like the
  program's own files: it may import other builtins, hold private helpers,
  structs and constants, use context-typed calls (`narrow`, `to_set`), and take
  keyword and variadic parameters (`sep: U* = [], final_sep: U* = sep`, as
  `builtin_map_join.aipl` declares). Only its `pub fn` is special — it is named
  `__builtin_<name>`. Its `.test` and `.benchmark` blocks run when the file
  itself is checked, not in the programs that use it.
- In predicates, use the `fn is_x(self: T) -> bool` method form. Don't add a
  `main`; tests are `.test` blocks.

### The `.test` block is not optional

`compiler_aipl_files_are_tested` (tests/suites/ffi.rs) fails any `.aipl` under
`crates/` with no `.test`, and the case each one is (`cases_aipl_mono_src_builtin_<name>`)
fails if the block does not pass. Cover, at minimum: the ordinary case; the
not-found / empty case (a *typed* empty is `[1, 2].filter(|x| x > 100)`); the
short-circuit or ordering guarantee the doc claims (first-vs-last match); the
free-call form; a non-i64 element type (`char`, `str`) and a `str` receiver if it
iterates chars; a named predicate; and a captured bound. Add a small
`fn is_nine(self: i64) -> bool { self == 9 }.test({ .. })` helper when you need a
named predicate — it needs its own `.test` too.

Run it in a second: `cargo run -q -- check crates/aipl-mono/src/builtin_<name>.aipl`.

## 2. Register it

| file | edit |
|---|---|
| `crates/aipl-syntax/src/callee.rs` | a `Callee::<Name>` arm and its `"__builtin_<name>"` in `Callee::name` |
| `crates/aipl-syntax/src/builtin.rs` | `Builtin::aipl(Callee::<Name>, "builtin_<name>.aipl")` in `BUILTINS` — the one table of everything about a builtin: which callee, and where its signature and docs are |
| `crates/aipl-syntax/src/lib.rs` | `<name>` in the `// NOTE:` list of AIPL-implemented builtins in `BUILTIN_SIGNATURES` |

**Do not add a signature to `BUILTIN_SIGNATURES`** — that list is for builtins
whose body lives in Rust; the `.aipl` file is the source of the signature for
these.

**A builtin no file names needs a `companions` rule.** The loader loads what a
program imports. One that a pass after the checker introduces — a fusion's
target (`map_find_if`), a lowering's (`reduce`, `extend_optional`) — is loaded
through `aipl_builtins::companions`, which says which callees make it reachable.
Forget the rule and monomorphization stops on the first program that reaches
it, naming the table.

Nothing else needs touching. The per-case `#[test]` list
(`tests/support/case_tests.rs`), the row in `tests/performance_baseline.md` every new
case needs (handoff adds it), and any IR regeneration are all
`cargo handoff`'s job — don't hand-drive them.

**If AIPL can't express it** (raw memory, a new runtime call), the path is
different and heavier: a declaration in `BUILTIN_SIGNATURES`, lowering in
`aipl-mono`/`aipl-codegen`, and matching runtime support in *both* the JIT and
the linker. Read an existing intrinsic end to end before starting.

## 2b. Add it to the `write-aipl` skill

`.claude/skills/write-aipl/SKILL.md` carries a one-line-per-category builtin list that every
task loads. Add the new builtin's name to the right category (and keep it terse: name only),
or the next author will not know it exists.

## 3. Dogfood it

Grep the repo for the shape the builtin replaces and convert the real sites —
starting with `crates/**/*.aipl`, which is compiler source. This is the honest
test of the signature (`find_prec` in `crates/aipl-codegen/src/parse.aipl` was
the shape that motivated `find_if`, and converting it proved the lambda captured
two outer bindings fine). The builtin's own `.test` block is already a test as
user code — the file is checked like any other — so it needs no duplicate in
`tests/cases/`. A case is for what the file cannot hold: how the builtin
interacts with something else (a fusion, a lint, a `shim`) or a real effect.

Expect the conversion to show up as a small instruction-count shift in every
case that transitively compiles the converted file. Handoff refreshes those
baseline rows itself and lists them in its closing report: review them in the
diff of `tests/performance_baseline.md`. That is the expected cost of the
conversion, not a regression.

## 4. Lint against ad-hoc reimplementations

**Recommended, not automatic.** A lint is how a builtin actually lands — the
utility makes the short spelling possible, the lint makes it the one people
write — so write one by default. But it is a real cost, and two questions decide
whether to pay it:

- **Would following it improve the caller?** If the longhand and the rewrite read
  about the same, the lint buys churn and a squelch marker, not clarity. Skip it.
- **Is the lint going to be complicated?** Every line of shape-matching is a line
  that can flag something it shouldn't, and a mis-advised lint is worse than a
  missing one. Complexity is a reason to weigh, not an automatic no: what makes
  it worth paying is a *decisive* signal that the shape is the one you mean. In
  `return_loop_find_index` the extra bulk is all fold-scaffolding — an indexed
  `for` reaches the lint desugared into a synthetic `__idx$N` counter, a
  per-iteration rebind and an appended bump — and it earned its keep, because
  `$` is not an identifier character, so a `__idx$` match provably cannot be a
  user's own loop. Bulk that buys precision is worth it; bulk spent guessing is
  not.

When you skip the lint, say so in the builtin's own file comment and why, so the
next person doesn't re-litigate it.

Precedents to copy — `push_loop_pipeline` (→ `map`/`filter`),
`return_loop_find_if` (→ `find_if`), `return_loop_find_index` (→ `find_index`,
and the worked example of matching a *desugared* shape), `match_value_or`,
`match_is_some_and`, `match_map_ok`/`match_map_err`, `len_gt_zero`
(→ `is_nonempty`), `incr_by_one` (→ `++`).

**Layout.** One lint per file, in AIPL: `crates/aipl-codegen/src/lint/<name>.aipl`, registered in
`lint/lint.aipl` and listed in `DOGFOOD_SOURCE_FILES`. The add-lint skill is the recipe, and the
helpers more than one lint shares live in `lint_hit.aipl` (`quote`, `spans_its_text`,
`imported_as`, `bound_to`, `text_after`) and `places.aipl`. Name the lint `<bad shape>_<replacement>`.

**Match exactly one shape, and bow out of every near-miss.** Before flagging,
ask what the rewrite would silently change: work the builtin has nowhere to put
(a second statement, an `else`, a fall-through that isn't the not-found answer);
an expression lifted into a lambda that mentions state the rewrite deletes or
propagates with `?` (`lambda_safe`); an effect that would be reordered. A shape
you can't advise precisely is one to leave alone — a mis-advised lint is worse
than a missing one. Put each near-miss in the `_not_flagged` fixture with a
comment saying *why*.

**The hit's span must be one line that survives `aipl check --fix`.** `#[allow]` is
line-scoped, so a span covering a multi-line construct can never be squelched —
and an unsquelchable lint that fires on an AIPL-implemented builtin **is a compile
error in every program that reaches that builtin**. Point at a short
statement inside the shape (the `mut acc = [];` seed; the `return some(x);`).
Your own new builtin will usually trip the new lint — it *is* the longhand — so
squelch it there with a trailing `#[allow]` and a comment saying why, then
confirm `aipl check --fix` keeps the marker on that line.

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

**No per-lint fixtures:** the flagged, not-flagged and `#[allow]`-agreement cases are `.test` assertions in the
lint's own file (add-lint skill, "Tests belong in the lint").

**Fanout:** don't grep-and-patch the corpus ahead of time; implement, run, read
the failure list. The one grep worth doing first is over `crates/**/*.aipl` for
the shape, because a compiler-source hit is the one that breaks every program
reaching it if the span turns out to be unsquelchable.

## 5. Finish

`cargo handoff`, once, at the end. Expect it to regenerate the case `#[test]`
list and add a baseline row for each new case, and refresh any row that moved.
Review the moved rows in its closing report (a builtin that was meant to be free
should not move them), and confirm no `--- stdout ---` or error *message* body
moved.
