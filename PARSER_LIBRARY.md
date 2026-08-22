# A parser library in AIPL

## Status

Long-running, interleaved with other work. Update the checkboxes as items land;
each is sized to be finishable in one session.

**Stage 1 — language work** — done, except one deferred item that blocks nothing
- [ ] 1.0 *(deferred, not required)* hash-backed dicts/sets — TODO.txt D1

**Stage 2 — the library**
- [ ] 2.0 `grammar.aipl`, `cst.aipl`, `parse.aipl` + per-arm unit tests
- [ ] 2.1 S-expressions end to end (incl. deep-nesting test as a built binary)
- [ ] 2.2 JSON end to end

**Stages 3-7**
- [ ] 3 `ebnf.aipl` + FIRST sets
- [ ] 4 Highlighter generator (oracle: `tests/highlighting.rs`)
- [ ] 5 AIPL's grammar, differential-tested against gazelle
- [ ] 6 Formatter generator
- [ ] 7 Retire gazelle

The one remaining Stage 1 item is deferred and blocks nothing. Stage 2 onward is
strictly sequential.

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

## Why the language work came first

Pressure-testing the natural design against the compiler turned up a set of
blockers — each with a workaround, and each workaround a distortion of the
library. Building around them would have baked the compiler's limits into a file
meant to outlive them, so the language was fixed first. **That work is done**;
the design below is written against the fixed compiler.

What it bought, in the shape the library actually needs: a case can be matched
without matching its payload (`same_case`, plus a `variant` bound), a generic
variant's type parameter is inferred from the expected type, recursive types may
recurse through an array, `match` arms may be statement blocks, and recursion is
tail-call eliminated so parse depth is bounded by the input rather than the
stack. Four shapes the design rests on that had no test anywhere in the repo
were confirmed to work and are now locked in by cases — a fn value returning a
boxed recursive variant, an `A[]` parameter where `A` is boxed, an array of
structs each holding a boxed field, and a two-parameter generic variant. What is
left below is one deferred performance item that blocks nothing.

## Stage 1 — Language work

### 1.0 Deferred: hash-backed dicts and sets

TODO.txt D1 — dicts and sets are linear scans (`lib.rs:2816`, `dict_find`), which
is the largest asymptotic win available in the repo, and would make both rule
lookup by name and packrat memoization affordable. **Not required**: the link
pass (Stage 2) turns rule references into array indices, and FIRST sets (Stage 4)
remove most of the need to memoize. Listed so the dependency is explicit if the
parser later wants memoization.

## Stage 2 — The library, proven on two toy grammars

New files, flat in `crates/aipl-codegen/src/` alongside `lexer.aipl` and
`doc.aipl` (where every AIPL library file lives; a subdirectory only makes
imports uglier). Nothing is added to `DOGFOOD_SOURCE_FILES` or
`FMT_SOURCE_FILES` (`crates/aipl-codegen/src/lib.rs:3352,3391`), so **no `.clif`
regeneration is involved** — the files are picked up automatically as library
cases by the harness and by `aipl check crates`.

```
// grammar.aipl
variant Rule<K> =
    Term(K)                    // any token of this kind (`same_case`)
  | Spelling(str)              // one token with this exact source text
  | Then(Rule<K>[])            // sequence
  | OneOf(Rule<K>[])           // ordered choice (PEG)
  | Many(Rule<K>, u64)         // min count: 0 = *, 1 = +
  | Maybe(Rule<K>)
  | Named(str)                 // rule reference; `link` rewrites to NamedIdx
  | NamedIdx(u64)
  | ListOf(Rule<K>, str, bool) // item, separator spelling, allow trailing
  | Climb(Rule<K>, Prec[])     // precedence climbing over an atom rule

struct Prec { power: u64, right: bool, ops: str[] }

struct Production<K, A> {
    name: str,        // rule name; also the CST node name
    rule: Rule<K>,
    expect_as: str,   // error display label; "" = transparent
    scope: str,       // highlighter scope hint
    layout: Layout,   // formatter shape
    build: Build<A>,  // AST lowering
}

variant Build<A> = Mk((Cst, A[]) -> A!ParseError)
struct Grammar<K, A> { prods: Production<K, A>[] }

// cst.aipl — lossless: CTrivia puts comments in the tree, so concatenated
// spans reconstruct the source byte-for-byte.
variant Cst = CLeaf(Span) | CTrivia(Span) | CTree(u64, Cst[])
```

`Build` has a single arm that always pins `A` — deliberately no `NoLower` arm,
since a payload-free arm pins no type parameter of its own
(there is nothing to infer *from*). Productions with nothing to lower pass a
trivial named function.

Function values carry three restrictions that shape `build`: they must be
**effect-free** (`check.rs:1649`), **non-generic** (`check.rs:1639`), and
**non-capturing** (`check.rs:1825`). So each is a named top-level
`fn lower_<prod>(cst: Cst, kids: A[]) -> A!ParseError`. They also cannot be
compared with `==` (`check.rs:3059`), so a `.test` block asserts on parse
*output*, never on the grammar value — the same constraint `lexer.aipl` lives
with.

The driver (`parse.aipl`) yields both the CST and the lowered AST in one pass;
`parse_cst` skips `build` for consumers that only want the tree. Three things it
must get right: **`link(grammar)`** runs once, rewriting `Named(str)` →
`NamedIdx(u64)` and erroring on unresolved references; **a progress guard on
`Many`**, since an inner rule matching empty input loops forever and AIPL has no
`break` (`lexer.aipl:194-196` documents the lexer's version of the same
invariant); and **furthest-failure tracking** for errors.

### 2.1 S-expressions — the smallest grammar that is still recursive

```
// grammar_sexp.aipl — two productions, and every risky thing exercised once.
"list" -> Then([Spelling("("), Many(Named("item"), 0), Spelling(")")])
"item" -> OneOf([Term(Atom), Named("list")])

variant Sexp = SAtom(str) | SList(Sexp[])
```

Deliberately the floor, not an arbitrary warm-up. `Named("list")` reaching
itself through `item` is what forces a **recursive CST** (boxing through an
array) and **unbounded parse depth** (tail calls), and lowering to `Sexp` is
what exercises `(Cst, A[]) -> A!ParseError` with a **boxed `A`** — the shape with
no precedent anywhere in the repo. A non-recursive toy (a comma-separated integer
list, key/value lines) would exercise none of those, which is the entire reason
to have a first target at all.

What it deliberately omits: precedence, more than one terminal class, separators,
and any error-message subtlety. If the driver is wrong, it is wrong here in two
productions rather than in ten.

Before even that, the library's own `.test` blocks should cover each `Rule` arm
in isolation against a hand-built token array — the equivalent of
`walker.aipl`'s `toks_of`/`walker_of` fixtures (`walker.aipl:2253`).

### 2.2 JSON

Adds what S-expressions leave out, still without precedence: several terminal
classes (string, number, the three keywords), `ListOf` with a separator and a
trailing-comma rule, `OneOf` across six alternatives, and a heterogeneous AST.
Complete but tiny, and `lexer.aipl`'s own test block already drives a
JSON-flavored rule set to crib the token rules from. `grammar_json.aipl` supplies
those rules, ~10 productions, and lowering to a `variant Json`.

Precedence (`Climb`) has no exercise in either toy — it arrives with AIPL's
grammar in Stage 5. If it needs proving sooner, the cheapest vehicle is a
four-operator arithmetic grammar bolted onto the S-expression lexer.

### Errors

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

## Stages 3-6

**3 — Introspection, proven.** `ebnf.aipl` dumps the grammar as EBNF; FIRST-set
computation lets `OneOf` pick a branch without backtracking (also the answer to
packrat being unaffordable while dicts are linear scans).

**4 — The highlighter generator.** Generate
`editors/vscode/syntaxes/aipl.tmLanguage.json` from the grammar. First because it
**already has a strict oracle**: `tests/highlighting.rs` validates that file
against every token of every case file and example. The target is regex- and
line-based, so the generator emits token classification plus contextual patterns
for the easy declarations — what the hand-written grammar does today.

**5 — AIPL's own grammar**, differential-tested against gazelle across the whole
corpus, in the shape of
`tests/lexer_dogfood.rs::dogfood_lex_hook_matches_fresh_compile_on_corpus`.

**6 — The formatter generator**, targeting the existing `Doc`. `Layout` starts
from `walker.aipl`'s vocabulary (`ListStyle`, `comma_list_docs`, `match_expr`'s
hard-broken block) with a `Custom(str)` escape hatch naming a hand-written layout
function. Honest risk: most of `walker.aipl`'s bulk is *heuristics* — comment
attachment, call hugging, chain breaking, import sorting — and those will not
fall out of a grammar. The realistic win is retiring the mechanical two thirds
and shrinking the "teach two parsers" problem, not deleting the file.

**7 — Retire gazelle.**

Stages 5-7 change the compiler's own parse path and pull these files into
`DOGFOOD_SOURCE_FILES`; that is where IR regeneration, relink cost, and the
formatter bootstrap deadlock start to matter. Stages 2-4 touch none of it.

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
   source byte-for-byte. Stage 6 depends on this; it is the cheapest place to
   catch a regression.
6. **Round-trip on each toy** — parse → lower → render → parse, for
   S-expressions first and then JSON. The deep-nesting case (~200 levels of
   `((((...))))`) belongs with S-expressions, since that is the first grammar
   that can nest, and it must run as a **built binary**, not just under
   `aipl check` — otherwise it is measured against 256 MB instead of the 8 MB a
   shipped binary gets, and tail-call elimination goes untested.
7. **Stage 4 oracle** — `cargo test --test highlighting` against the generated
   `.tmLanguage.json`, unchanged.
8. **Finish** — `scripts/handoff.sh`, which regenerates the `#[test]` list for
   new case files and fills their `--- performance ---` sections. Expect churn: a
   library case's perf section measures its `.test` driver, and for scale
   `walker.aipl` records 3,075,003 instructions.
