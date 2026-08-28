# A parser library in AIPL

## Status

Long-running, interleaved with other work. Update the checkboxes as items land;
each is sized to be finishable in one session.

- [ ] 1 `ebnf.aipl` + FIRST sets
- [ ] 2 Highlighter generator (oracle: `tests/highlighting.rs`)
- [ ] 3 AIPL's grammar, differential-tested against gazelle
- [ ] 4 Formatter generator
- [ ] 5 Retire gazelle

The stages are strictly sequential.

**The library itself is built and proven**, and its plan is no longer here:
`grammar.aipl`, `cst.aipl` and `parse.aipl`, with three end-to-end toy
grammars — `grammar_sexp.aipl` (recursion and depth), `grammar_json.aipl`
(several terminal classes, `ListOf`, a heterogeneous AST) and
`grammar_calc.aipl` (`Climb`, lowered and then
*evaluated*, which is the assertion a wrong tree cannot survive). Every
`Rule` arm is covered. Those files and their `.test` blocks are the record of how
the library works; this document is now only what is left to build on top of it.

## Context

AIPL's syntax is currently described **three times, by hand, in three unrelated
formalisms**:

| Description | Where | Size | Produces |
|---|---|---|---|
| gazelle LR(1) grammar | `crates/aipl-parser/src/lib.rs:18-504` | ~254 non-comment lines, 81 rules, 238 alternatives | the compiler's AST |
| formatter token walker | `crates/aipl-codegen/src/walker.aipl` | 2644 code lines, ~160 functions | a `Doc` layout tree |
| TextMate grammar | `editors/vscode/syntaxes/aipl.tmLanguage.json` | 173 lines | editor scopes |

CLAUDE.md already names the cost of two of them: *"Adding a syntax form means
teaching two parsers: the gazelle grammar in `aipl-parser` **and** the
formatter's own token walker"* — a pairing with a documented bootstrap deadlock,
since handoff formats the corpus before it regenerates IR, so a formatter that
doesn't yet know the new syntax is the one asked to format sources written in
it. The TextMate grammar is a third copy that drifts silently until
`tests/highlighting.rs` catches it.

The goal is **one grammar, expressed as data**, from which the parser, the
formatter, and the highlighter are all derived. Eventually that grammar parses
AIPL itself and the gazelle dependency goes away.

The lexical half is already done in exactly this style:
`crates/aipl-codegen/src/lexer.aipl` is a generic data-driven lexer whose rule
set is plain data (`variant Matcher`, `TokenRule<K>[]`) interpreted by one
`lex<K>` driver, with `lex_aipl.aipl` as just a table. This library is that idea
one level up, and composes with it directly.

## The language work this design assumes

Pressure-testing the natural design against the compiler turned up a set of
blockers — each with a workaround, and each workaround a distortion of the
library. Building around them would have baked the compiler's limits into a file
meant to outlive them, so the language was fixed first. **That work is done, and
is no longer part of this project**; the library is written against the fixed
compiler and depends on all of it.

What it bought, in the shape the library actually needs: a case can be matched
without matching its payload (`same_case`, plus a `variant` bound), a generic
variant's type parameter is inferred from the expected type, recursive types may
recurse through an array, `match` arms may be statement blocks, and recursion is
tail-call eliminated. (That last one turns out **not** to reach the parse itself:
a recursive-descent call is never in tail position, so parse depth stays bounded
by the stack. Measured against the 8 MB an ordinary binary gets, `((( ... )))`
parses, renders and re-parses fine at 1,000 levels and segfaults before 2,000.)
Four shapes the design rests on that had no test anywhere in the repo were
confirmed to work and are locked in by cases — a fn value returning a boxed
recursive variant, an `A[]` parameter where `A` is boxed, an array of structs each
holding a boxed field, and a two-parameter generic variant.

**Building the library found four more gaps**, all in the same unexplored
corner — nobody had ever written a *generic recursive* variant, which `Rule<K>`
is — and all four are now fixed and locked in by
`tests/cases/generics/recursive_variant.aipl`:

- Recovering a generic instance's type arguments (`instance_args`, in
  `aipl-mono`'s `check.rs` and `lib.rs` alike) unified the template against the
  instance's own cases, so a case whose payload *is* that instance asked for the
  arguments being recovered and recursed until the stack ran out. Declaring
  `Rule<K>` at all crashed the compiler.
- A generic variant *constructed* inside a generic body (`Many(r, 0)` where
  `r: Rule<K>`) eagerly minted an instance named after the abstract type
  variable, which nothing else agreed with. The generic-*struct* path had always
  kept such an application unresolved; the variant path had never been asked,
  since before this every generic variant in the repo was only ever matched.
- A `match` on a generic instance read its payload types from the wrong map,
  missing the synthesized instances entirely and typing every binding as `i64`.
- An array literal of a concrete generic-variant instance (`Rule<Kind>[]`) was
  rejected as an invalid element type, for the same wrong-map reason.

Hash-backed dicts and sets are on TODO.txt rather than here. They are the largest
asymptotic win in the repo and would make packrat memoization affordable, but the
link pass (`link`, in `grammar.aipl`) already turns rule references into array
indices and FIRST sets (Stage 1) remove most of the need to memoize — so nothing
below waits on them.

## Errors

The bar is `friendly_syntax_error` (`crates/aipl-parser/src/lib.rs:3556-3597`):
the found token spelled as source, a full expected *set*, humanized, deduped,
Oxford-joined. The load-bearing piece is `SYMBOL_DISPLAY_NAMES`
(`lib.rs:3472-3552`), which collapses `expr`/`term`/`unary`/`postfix`/`atom` all
to `"expression"`. `expect_as` is the reified equivalent; `""` means transparent,
so plumbing rules never surface. This matters more under PEG than LR: ordered
choice lets a failed, more-specific alternative own the furthest position, so
without aggressive collapsing the messages get *worse* than today's. Rendering is
already dogfooded — `caret_block.aipl:13` produces the `--> path:line:col | ^^^`
block — so `struct ParseError { message: str, span: Span }` mirrors
`lexer.aipl:100`'s `LexError` and feeds straight in.

## The stages

Each new file goes flat in `crates/aipl-codegen/src/` alongside `lexer.aipl` and
the library's own files (where every AIPL library file lives; a subdirectory only
makes imports uglier), and is picked up automatically as a library case by the
harness and by `aipl check crates`. Nothing joins `DOGFOOD_SOURCE_FILES` or
`FMT_SOURCE_FILES` (`crates/aipl-codegen/src/lib.rs:3352,3391`) until Stage 3, so
until then **no `.clif` regeneration is involved**.

**1 — Introspection, proven.** `ebnf.aipl` dumps the grammar as EBNF; FIRST-set
computation lets `OneOf` pick a branch without backtracking (also why packrat
memoization is not needed while dicts remain linear scans).

**2 — The highlighter generator.** Generate
`editors/vscode/syntaxes/aipl.tmLanguage.json` from the grammar. First because it
**already has a strict oracle**: `tests/highlighting.rs` validates that file
against every token of every case file and example. The target is regex- and
line-based, so the generator emits token classification plus contextual patterns
for the easy declarations — what the hand-written grammar does today.

**3 — AIPL's own grammar**, differential-tested against gazelle across the whole
corpus, in the shape of
`tests/lexer_dogfood.rs::dogfood_lex_hook_matches_fresh_compile_on_corpus`.

**4 — The formatter generator**, targeting the existing `Doc`. `Layout` starts
from `walker.aipl`'s vocabulary (`ListStyle`, `comma_list_docs`, `match_expr`'s
hard-broken block) with a `Custom(str)` escape hatch naming a hand-written layout
function. Honest risk: most of `walker.aipl`'s bulk is *heuristics* — comment
attachment, call hugging, chain breaking, import sorting — and those will not
fall out of a grammar. The realistic win is retiring the mechanical two thirds
and shrinking the "teach two parsers" problem, not deleting the file.

**5 — Retire gazelle.**

Stages 3-5 change the compiler's own parse path and pull these files into
`DOGFOOD_SOURCE_FILES`; that is where IR regeneration, relink cost, and the
formatter bootstrap deadlock start to matter. Stages 1-2 touch none of it.

## Naming

Checked mechanically against all 132 constructors declared across
`crates/**/*.aipl`: **zero collisions**. Constructors are mangled per file
(`crates/aipl-loader/src/lib.rs:310-372`), so cross-file reuse is fine —
`Ident`/`Op`/`Punct` are each already declared twice today. The two real hazards
are intra-file: a constructor colliding with another top-level name in the *same*
file is **silently dropped** from the bare view (`aipl-loader/src/lib.rs:356-370`),
and an import colliding with a local is a hard error. So do not declare a local
type named `Token` (imported from `lexer.aipl`), and do not name a test-only
token variant `Kind` — `lexer.aipl:780` does, and copying that pattern into a
file that also declares a `Kind` constructor trips the silent drop.

## Verification

1. **Shape cases** — the four previously-unproven shapes are locked in by
   `variants/fn_value_returns_boxed`, `variants/boxed_array_param`,
   `structs/array_of_boxed_field_structs` and `generics/two_param_variant`. Their
   `--- performance ---` sections assert balanced allocations, which is what
   would move first if boxing regressed.
2. **Corpus after a change that moves metrics everywhere** — anything touching
   codegen does. That is CLAUDE.md's "one sanctioned deviation": whole-corpus
   `fill_expected` once rather than handoff's per-case loop, then confirm from
   the diff that no `--- stdout ---`/`--- errors ---` body moved, only metrics.
3. **Per-file inner loop** — `cargo run -q -- check crates/aipl-codegen/src/parse.aipl`
   runs that file's `.test` blocks alone; this is also what enforces the
   `.test`-block requirement (`tests/ffi.rs:1109`).
4. **Scoped case run** — `cargo test --test cases -- crates_aipl_codegen_src_parse`.
5. **Losslessness** — assert that the CST's concatenated spans reconstruct the
   source byte-for-byte. Stage 4 depends on this; it is the cheapest place to
   catch a regression.
6. **Round-trip on each toy** — parse → lower → render → parse, which each of
   the three asserts. The calculator renders fully parenthesized, so its round
   trip is exact *and* its text shows the tree; a grammar added later should
   carry the same pair. The deep-nesting case (200 levels of `((((...))))`) lives
   with the S-expressions and runs as a **built binary**, not just under
   `aipl check` — otherwise it is measured against the CLI's 256 MB thread stack
   instead of the 8 MB a shipped binary gets.
7. **Stage 2 oracle** — `cargo test --test highlighting` against the generated
   `.tmLanguage.json`, unchanged.
8. **Finish** — `cargo handoff`, which regenerates the `#[test]` list for
   new case files and fills their `--- performance ---` sections. Expect churn: a
   library case's perf section measures its `.test` driver, and for scale
   `walker.aipl` records 3,075,003 instructions.
