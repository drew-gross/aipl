# Working in this repo

## Committing
Don't ask whether to commit, and don't offer to — I always handle commits
myself. Finish a task at the green-and-formatted state (run `cargo handoff`,
see below) and stop there; leave the working tree uncommitted.

## Handoff (`cargo handoff`)
`cargo handoff` **is** the pre-handoff validation — run it once, at the end of a
task, as the whole finish-a-task gate. It runs the finish sequence in
dependency order and pays for the expensive regeneration steps only when a test
run proves they're needed: `cargo fmt` + `aipl fmt` the corpus, a discovery
run, then (only if that surfaced fillable staleness) scoped
`fill_expected` refills of exactly the mismatched cases and the staged
dogfood-IR regen/validate/promote flow, then a final run. It exits 0 on
green (printing what it refilled/regenerated and flagging behavioral-output
changes to review in the git diff), or stops with a pointed message naming the
step and why on any failure a refill can't fix.

**Every step is bounded by a wall-clock ceiling** — `HANDOFF_STEP_TIMEOUT`
seconds, default 1800, `0` disables. A wedged `nextest` produces no output and
makes no progress, so without a ceiling the gate simply hangs and gets
abandoned. On expiry it reports free RAM, swap usage and the busiest child
process, then kills the step's whole descendant tree, because the cause is nearly
always the machine rather than the tests: memory pressure (check swap — thrashing
pins processes at 0% CPU in `_dyld_start`), or a first-exec code-signing scan on a
freshly-linked test binary, which can idle ~2 minutes and looks exactly like a
hang. Raise the ceiling on a genuinely slow box; don't switch to hand-driving.
It also clears orphaned `nextest --list --format terse` children at
startup — an interrupted run leaves them blocked on a dead pipe, and they
accumulate across retries and slow each attempt for reasons that look unrelated.

The gate runs the suite with **`cargo nextest`** (each test in its own
process; requires `cargo install cargo-nextest`). One consequence it handles for
you: nextest defaults to fail-fast, so every whole-suite run there passes
`--no-fail-fast`. (Nextest also skips doctests; the repo has none left — the
documented examples are ordinary tests in `tests/ffi.rs`, so they run with the
rest of the suite.) Your own inner-loop runs can use either
runner — the cadence commands below are written for `cargo test`, and
`cargo nextest run -E 'test(<name>)'` is the nextest equivalent.

Every whole-suite step also passes **`--workspace --all-targets`**. `--workspace`
is the one that matters: without it cargo builds only the default package, whose
test targets are the three integration binaries, so every `#[cfg(test)]` module
under `crates/*/src/` went unrun — 57 tests, and nothing noticed when some of
them stopped *compiling*, because a test target that is never built cannot fail.
So a bare `cargo nextest run` is **not** the same set the gate runs; scope an
inner-loop run with `-E` rather than by dropping the flags.

**Where it lives.** `cargo handoff` is a cargo alias (`.cargo/config.toml`) for
`cargo run -p handoff`; the gate is the `handoff` crate. Its ordering rationale
lives in the module docs of `handoff/src/main.rs`, the output parsing that
decides which remediation steps run is in `handoff/src/discovery.rs` (with unit
tests — those parsers are a contract with nextest's JSON schema and the
harness's message format, so change one and its test tells you what else has to
move), and the step runner, watchdog and machine diagnostics are in
`handoff/src/runner.rs` and `handoff/src/machine.rs`.

**Don't hand-drive the sequence, and don't validate before it.** The gate is
the validation *and* the order. Running its steps yourself — `cargo fmt`,
`fill_case_tests`, `fill_expected`, the staged dogfood-IR flow — doesn't just
pay for the corpus twice; it reorders steps that depend on each other, and the
breakage that follows looks exactly like a real bug, so you then debug your own
sequencing.

**Never pipe a long-running command through a filter.** Run it bare:

    cargo handoff

Not `cargo handoff | tail -20`, not `2>&1 | tail`, not `| grep`, not `| head`.
The same goes for any whole-suite `cargo nextest run` or `cargo test`. A pipe
buffers the entire run, so the step-by-step output — `==> aipl fmt`,
`==> nextest (discovery)`, the elapsed times — appears only when the process
exits, ten to twenty minutes later. Until then the shells view shows nothing and
the run is indistinguishable from a hang. Watching a slow gate make progress is
the whole reason it prints its steps.

Filtering only ever serves the *agent's* convenience in reading the tool result;
it costs the human the one thing they need while a long run is in flight. If you
want a filtered view, run it bare and read or grep the recorded output file
afterwards — the harness saves it, and nothing is lost by waiting.

This is a repeat offence, not a hypothetical: in one session nearly every
`cargo handoff` went out as `cargo handoff 2>&1 | tail -N`, including the run
immediately after being corrected for it. Treat "am I about to pipe a
long-running command?" as a checkpoint before every invocation.

**If you find yourself wanting non-targeted validation before the handoff run,
that is a bug report about the gate — say so instead of working around it.**
Propose the improvement (and offer to implement it); don't build a parallel
procedure. This is not hypothetical: a wedged `nextest` once sent an agent off to
hand-roll the whole sequence, and it then kept using the hand-rolled version for
hours after the machine recovered — re-implementing the staged-IR flow three
times, at triple the cost, to reach conclusions one gate run would have given.
A blocker in the gate is worth fixing once; routing around it is worth nothing
and hides the blocker from the next person.

The dependencies that bite:

- **Formatting moves line numbers, so it must come first.** A rendered
  diagnostic embeds its own `--> path:line:col` and caret row, so every
  `--- errors ---`/`--- check ---` section is line-sensitive: reformat after
  refilling one and what you just validated is already stale. (Data symbols are
  *not* in this set — a string literal's symbol is a content hash, so it keeps
  its name when unrelated source above it moves, and `binary size` and the
  checked-in `.clif` no longer churn for an edit that touches no literal. See
  `StrLiterals` in `aipl-codegen/src/lib.rs`.)
- **A new or deleted case file needs its `#[test]` list regenerated before a
  suite run means anything** — until then the case simply never runs.
- **Section refills come after the discovery run**, which is what establishes
  which sections are genuinely stale rather than refreshing on suspicion.

The gate does all of this in dependency order and pays for the expensive
regeneration only when a test run proved it necessary. The only thing worth
running ahead of it is a **targeted** test for the code you just touched
(`cargo test -- name_substring`, `cargo test --test <file>`, or a scoped
`cargo test --test cases -- cases_generics`) as a fast inner-loop check — that's
a dev-loop signal, not the handoff gate. When the change is ready, run
`cargo handoff` and let it format, verify, and regenerate in one pass.

**Refills scale — you don't need to hand-drive them.** `AIPL_CASE` takes a
comma-separated list, and handoff passes every stale case in a *single*
`fill_expected` run, so a change that invalidates hundreds of
`--- performance ---` sections at once (a codegen change, a new baseline runtime
call) costs one invocation rather than hundreds. This used to be the one
sanctioned deviation from the gate: the refill loop spawned one `nextest` per
mismatched case, and since each invocation pays a fresh-binary startup that
dwarfs the refill itself, the advice was to run a whole-corpus refill by hand
first. That is no longer worth doing.

What survives from it is the review guard, which was never about speed: a
wholesale refill can bury a real regression, so after any large one, diff the
corpus and confirm every changed line is a metric — no `--- stdout ---` /
`--- errors ---` body should move.

## Shell
Use the **Bash** tool for everything terminal-side: `cargo build`,
`cargo test`, `cargo fmt`, `git`, env-var-prefixed runs
(`AIPL_CASE='x' cargo test ... --ignored fill_expected`), file operations.

## Test cadence
Avoid running the full test suite during development — it is the slowest part
of the dev loop. Prefer:
- A single suite: `cargo test --test compiler -- mono::` (suites are `mod`s of
  a merged target — see "Test targets are merged" below, so the suite name is a
  module prefix, not a `--test` argument)
- A single test by name: `cargo test -- name_substring`
- A filtered case run: `cargo test --test cases -- cases_generics`
  (every case is its own `#[test]`, named after its display path with the
  separators flattened — `cases/generics/ord_bound` is `cases_generics_ord_bound`
  — so libtest's own name filter scopes a run, and it reports how many it ran
  and how many it filtered out)

Targeted runs alone can miss regressions in unrelated areas, so they're the
inner-loop signal, not the finish check — finish a task by running
`cargo handoff` (see the Handoff section above), which does the full
`cargo test` (and formatting/regeneration) for you.

Let the tests do the verifying. Prefer running `cargo test` (targeted, then
full) over reading generated artifacts, diffs, or output by hand to convince
yourself a change is correct — the suite already asserts on all of it, so a
manual pass mostly burns tokens re-checking what a green run guarantees. Only
drop into manual inspection when a test actually fails and you need to
understand *why*, or for the rare thing no test covers.

## Formatting
Every task ends formatted (`cargo fmt` for Rust, `aipl fmt` for `.aipl`). You
don't run these by hand as a separate step — `cargo handoff` (see the Handoff
section above) formats first, then tests, so a single handoff run leaves the
tree both green and formatted.

## AIPL source formatting (`aipl fmt`)
Every checked-in `.aipl` file (outside the formatter's own `tests/fmt/`
fixtures) must be in canonical format — the `all_aipl_files_stay_formatted`
test in `tests/fmt.rs` enforces it over `tests/cases/`, `crates/`, `examples/`,
and `tests/ffi_fixtures/`. So a **new or edited `.aipl` file must be formatted**:
run `aipl fmt <file>` (in place; `--check` reports without writing), or reformat
the whole corpus at once with the author helper
`cargo test --test compiler -- --ignored fmt::format_corpus`.

`aipl check` reports it too, so a project using `check` as its whole handoff gate
gets one command for "is this tree ready". An unformatted file is a `check`
failure but not a fatal one — it is named on stderr, its tests still run, and the
batch summary ends with `N files need formatting`.

The formatter (`aipl-fmt` crate, `aipl::fmt::format_source`) is canonical
(gofmt-style — it decides line breaks; width defaults to 100) and works off the
lexer token stream, so it preserves comments/literals verbatim and leaves
trailing `--- section ---` blocks byte-for-byte. Reformatting moves line numbers,
which invalidates any section holding a rendered diagnostic: refill
`--- errors ---`/`--- check ---` sections with `fill_expected` afterward. It does
not move `--- performance ---` on its own (see the handoff note above on
content-hashed literal symbols), and the checked-in `dogfood.clif` is renumbered
and stays stable, so neither normally needs regenerating — the
`checked_in_ir_is_current` test will tell you if the IR does. Fixture files under
`tests/fmt/*.aipl` are *intentionally* misformatted inputs and are exempt from
the enforcement test.

## Performance monitoring (non-deterministic)
Two separate perf tracks:
- **Deterministic, asserted**: the per-case `--- performance ---` sections
  (allocations, instructions executed, binary size, and a `functions:` block
  giving every function's call count and byte size in one list — builtins
  included, at `bytes: (external)`, since only this object's code is measured).
  These have hard assertions and are filled/refreshed with the `fill_expected`
  ignored test (see below).
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

## Test targets are merged (`tests/compiler.rs`, `tests/dogfood.rs`)

There are **three** integration-test targets, not one per suite. Every extra
target is a full static link of the ~77-crate compiler closure — measured at
~6s each, and that link, not compiling the test source, is what dominates build
time (`tests/shims.rs`, 73 lines, cost exactly as much as `tests/cases.rs`,
1914 lines). The suites live in `tests/suites/*.rs` and are pulled in as `mod`s:

| target | suites |
|---|---|
| `cases` | the `tests/cases/**/*.aipl` corpus (own target — 560 tests, filtered constantly) |
| `compiler` | check, codegen, doc_cmd, fmt, highlighting, mono, parser, shims |
| `dogfood` | dogfood_ir, ffi, lexer_dogfood |

Two consequences:

- **Test names are module-qualified.** `format_corpus` is `fmt::format_corpus`;
  `fill_staged_ir` is `dogfood_ir::fill_staged_ir`. Names in `cases` are
  unchanged. Filter one suite with `cargo test --test compiler -- fmt::`.
- **`tests/suites/*.rs` are not auto-discovered** (cargo only discovers
  `tests/*.rs` and `tests/*/main.rs`), so **adding a suite means adding a `mod`
  line** to the relevant target — otherwise it silently never runs, exactly like
  a case with no `#[test]`.

They are `mod`s rather than `include!`s deliberately: splicing into one crate
root collides on duplicate `use` statements (11 of them across these files —
`PathBuf`, `Program`, `HashMap`, …), and each is a hard E0252 that any future
import could reintroduce. `mod` keeps each suite's imports isolated.

Paths inside a moved suite are now one level deeper — anchor fixtures at
`env!("CARGO_MANIFEST_DIR")` rather than writing source-relative
`include_str!("ffi_fixtures/…")`.

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
`tests/support/case_tests.rs`. `cargo handoff` regenerates it when the tree
and the list disagree, so adding or deleting a case is not a reason to run
anything by hand. The command exists for when you want it directly:
`cargo test --test cases -- --ignored fill_case_tests`. You don't have to
remember either way: the `every_case_has_a_test` test fails when the list and the
tree disagree (in either direction) and prints that command. It's a hard failure
precisely because a case with no `#[test]` would otherwise never run.

Note that every `.aipl` under `crates/` is *also* discovered as a case, so
adding a dogfooded source adds a case too — it needs the same `#[test]` entry
and the same `--- performance ---` section any case does. Handoff fills both.

## Operators must be imported — always as `name as op`
Operators are not ambient, and **none of them has a bare form**. Every operator is
imported by aliasing its named builtin:
`import { equal as ==, less_than as < } from builtins;`. A bare
`import { ==, < }` is a compile error, exactly as `import { + }` is.

Where an operator has more than one semantics the alias is what records the
choice (`wrapping_add as +` vs `saturating_add as +`). Where it has only one, the
alias buys uniformity: an import list shows every operator a file uses in one
shape, so a reader never has to know which operators happen to be ambiguous.

| operator | import as | operator | import as |
|---|---|---|---|
| `+` | `wrapping_add` / `saturating_add` | `==` | `equal` |
| `-` | `wrapping_sub` / `saturating_sub` | `!=` | `not_equal` |
| `*` | `wrapping_mul` | `<` | `less_than` |
| `++` | `wrapping_increment` / `saturating_increment` | `>` | `greater_than` |
| `/` | `saturating_divide` | `<=` | `less_than_or_equal` |
| `%` | `remainder` | `>=` | `greater_than_or_equal` |
| `+++` | `concat` | `&&` | `logical_and` |
| `!` | `logical_not` | `\|\|` | `logical_or` |

`OPERATOR_BUILTINS` (`crates/aipl-syntax/src/lib.rs`) is the single place these
are declared — extend it rather than adding a per-operator special case. The
loader's refusal message is generated from that table, so a new operator or
flavor keeps its own diagnostic correct: one named form yields
"import it aliased: `equal as ==`", several yields "pick a semantics, e.g.
`wrapping_add as +` or `saturating_add as +`".

The loader gates operator *usage* per file against its imports (unimported →
compile error). So every new `.aipl` (test case, example, embedded compiler
source, and each `--- file:` companion) that uses operators needs the matching
import — and since the import shifts line numbers, refill any
`--- errors ---`/`--- check ---` sections. `--- performance ---` does *not* need
one: string-literal symbols are content-hashed, so a line shift on its own leaves
`binary size` alone.

When rewriting imports across the corpus, anchor the match to line start
(`^import {`): `walker.aipl`'s formatter tests hold `"import { == } from
builtins;"` inside *string literals*, and rewriting those corrupts fixtures —
both the input and its expected output.

## Match patterns: name only what you read, and never repeat an arm body
Two rules, and between them no arm should ever have to spell out a payload it
ignores or duplicate another arm's body:

- **`Ctor(..)` matches a case and ignores what it carries.** No binder names, no
  arity to keep in step with the variant. It is deliberately *not* the same as
  the nullary `Ctor`, which asserts the case has no payload and stays an error
  when it does — the two spellings are what tell a reader whether a case carries
  anything, without going to look. `..` on a payload-free case is refused for the
  same reason. Prefer it to `Ctor(_, _)`; reach for `_` per slot only when some
  slots *are* read (`Ctor(x, _)`).
- **Alternatives may bind.** `A | B => body` requires only that every alternative
  binds the same names in the same order, so two cases whose payloads line up
  share one arm: `Between(o, c) | Nested(o, c) => ..`. Mixing is fine —
  `Point | Circle(..)` binds nothing either way. What is refused is alternatives
  that disagree (`some(v) | none`), which would leave `v` unbound in one branch.

So a `match` with two identical arm bodies is a `match` with one arm missing a
`|`. The exception is width: a 16-way alternation runs past 100 columns and the
formatter then breaks it one-per-line — back to the arms it replaced — so
`is_operator_name.aipl` and `walker.aipl`'s `fmt_kind` group *by meaning* across
a few lines on purpose.

Grouping is source-level only: each alternative becomes its own `MatchArm` in the
parser, and `Ctor(..)` expands to one `_` binder per slot in monomorphization
(the first pass that knows the case's arity). Every pass downstream —
exhaustiveness, codegen, the lints — only ever sees one pattern per arm with its
binders named.

## `match` is either a statement or an expression, never both
An arm's body may be a single expression or a brace-delimited statement block
(statements, then an optional trailing expression that is the arm's value). Which
kind the whole `match` is follows from its arms, and each kind carries one
obligation — they're complementary, so no `match` satisfies both:

| arms produce | kind | obligation |
|---|---|---|
| nothing | statement | may assign freely; must sit where its value is discarded — i.e. ends in `;` |
| a value | expression | its value must be used; no arm may assign to a binding declared **outside** the `match` |

So a `match` run for effect is written `match (x) { .. };` — with the semicolon.
The point is that `let v = match (..) {..}` tells you the whole effect: it
computed `v`. Anything that also mutates is spelled as a statement, where a
reader expects effects.

"Outside" is the operative word: an arm may declare and drive its own `mut`
freely (that's how a block arm computes its value), and a *statement* `match`
nested inside an expression arm is fine. Only a `set` reaching a binding declared
outside the `match` is rejected.

## Struct spread — `T { ..base, field: value }`
Every field not given explicitly comes from `base`. Prefer it to restating
fields, and especially to the functional-update longhand:

```
fn advance(self: W, end: u64) -> W {     fn advance(self: W, end: u64) -> W {
    mut w = self;                   →        W { ..self, pos: self.pos + 1, last_end: end }
    set w.pos = w.pos + 1;               }
    set w.last_end = end;
    w
}
```

Rules worth knowing before reaching for it:

- **The spread comes first, and there may be only one.** A trailing spread reads
  as "and the rest from base"; a leading one as "start from base, then override".
  Only the unambiguous order is accepted.
- **It earns its keep on wide structs.** Copying one field of two (`WDoc { ..v,
  doc: X }`) saves nothing and *loses* information — the explicit `w: v.w` says
  which field came from where. Use it where it replaces several reads.
- **A full copy is not a spread.** `T { ..x }` where every field comes from `x`
  is just `x`.
- **The operand is evaluated once**, so a call is safe as a base — the expansion
  mentions it per field, so the loader binds it to a synthetic `let` first.
- **The operand must be the same struct.** `FmtError { ..e }` where `e` is a
  `LexError` is rejected even though the fields line up: the shapes agreeing is a
  coincidence, not a contract, and a spread crossing between them would silently
  change meaning the moment either struct gained or renamed a field. Converting
  between two struct types stays spelled out field by field.
  (Enforced by the type annotation on the binding the desugaring introduces —
  which is why a *generic* target can't be pinned: `Box` names a template, not a
  type. Its field types are still checked against the instance, so a mismatched
  instance is caught; a different struct sharing the field names is not.)
- It does **not** work in the fn-body shorthand (`fn f() -> T { ..x }`): that
  production is led by a field name, so `..` can't start one.

## Authoring a lint: the hit's span must be one line that survives `aipl fmt`
**One lint, one file.** Each lint lives in `crates/aipl-syntax/src/lint/<name>.rs`,
named after the lint function it holds (plus that lint's own private helpers), so
a fuzzy file search on the name in a diagnostic lands on its implementation.
`lint.rs` is the driver: the `check` entry point that runs every lint and drops
`#[allow]`-squelched hits, plus the two helpers more than one lint shares
(`line_of`, `end_is_receiver_len`). Adding a lint is a new file, a `mod` line,
and a `use self::<name>::<name>;`; a lint fn (and any type the driver passes it)
is `pub(super)`.

`#[allow]` is line-scoped — it squelches hits whose span *starts* on the marker's
line. So a lint whose span covers a multi-line construct can never be squelched:
the only line able to carry the marker is the construct's first, and `aipl fmt`
relocates a marker written there (e.g. inside a `match (..) {` header) onto its
own line, where it squelches nothing.

Point the hit at a single-line sub-expression that identifies the shape — for
`match_is_some_and`, the `none => false` arm rather than the whole `match`. Then
verify by squelching it somewhere real and re-running `aipl fmt`.

This matters more than it sounds: an unsquelchable lint that fires on an
AIPL-implemented builtin (`crates/aipl-mono/src/builtin_*.aipl`) **panics the
compiler** — "AIPL-implemented builtin sources are valid AIPL" — so every program
reaching that builtin dies. One such lint took out 66 cases at once.

## Two literal gotchas when building strings

- **A `char` interpolates bare, but `to_str` still quotes it.** `` `{c}` `` on a
  `char` renders `x`, exactly as a `str` interpolates without its double quotes —
  but `to_str('x')` is still the *literal* `'x'`, and so is a char nested in a
  rendered array or struct (`['a', 'b']`). Note `s[i]` is a `char?`, not a
  `char`, so it gets neither treatment — it renders `some('x')`. Indexing a
  `str`'s bytes for text still wants the one-byte **slice** `s[i..i + 1]`.
- **A `"""` block is raw *and* dedented.** No escape processing, and the common
  leading indent is stripped — so on a single-line block a leading space is
  removed while a trailing one is kept (`""" x """` is `"x "`). It is the right
  form for text that *contains* backslashes or quotes (JSON, regexes, expected
  compiler output); it is the wrong form when the exact leading whitespace
  matters, where an ordinary escaped literal is predictable.

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

> **`cargo handoff` already runs this entire flow**, in its correct place
> in the sequence: *after* the corpus is formatted (the sources settled) and *after* a
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
formatter's own token walker.

Expect this on **any** change to `walker.aipl`'s grammar — it is routine, not
exotic (two separate features hit it in a single session). Two symptoms to
recognize: the run stops at `aipl fmt` with a parse error on a file using the new
syntax; or, more insidiously, the live formatter *silently rewrites* the new form
into something that means something else — a struct spread `T { ..base, x: 1 }`
became `T { .., base, x: 1 }` before the walker knew the form. Always confirm the
new syntax round-trips through `aipl fmt` before trusting a formatted corpus.

To get out of the loop:

1. `fill_staged_ir` — builds a `fmt.clif.staged` that understands the new form.
2. Format the corpus with the staged formatter:
   `AIPL_FMT_IR=<abs>/fmt.clif.staged AIPL_DOGFOOD_IR=<abs>/dogfood.clif.staged cargo test --test compiler -- --ignored fmt::format_corpus`
3. `fill_staged_ir` **again** — step 2 rewrote the very sources the IR is
   generated from, so the artifact from step 1 no longer matches them.
4. Validate + promote as below, then run handoff normally for the refills.

This is not a licence to hand-drive generally: it announces itself as a hard
`aipl fmt` failure, and it's the only ordering the gate can't express.

The steps, for those cases:

1. **Generate staged IR** — compiles each `.aipl` source with the new frontend
   and writes `*.clif.staged` files next to the live `*.clif` files:
   `cargo test --test dogfood -- --ignored dogfood_ir::fill_staged_ir`

2. **Entry-level pre-check (fast)** — loads each `*.clif.staged` and calls its
   entry functions with known inputs; confirms the IR links and each entry
   computes correctly. This does *not* run the compiler on the staged IR — it's
   just the quick gate before the corpus run:
   `cargo test --test dogfood -- --ignored dogfood_ir::validate_staged_ir`

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
   `cargo test --test dogfood -- --ignored dogfood_ir::promote_staged_ir`

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
