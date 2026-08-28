# A parser library in AIPL

## Status

Long-running, interleaved with other work. Update the checkboxes as items land;
each is sized to be finishable in one session.

**Stage 1 — the library**
- [x] 1.0 `grammar.aipl`, `cst.aipl`, `parse.aipl` + per-arm unit tests
- [x] 1.1 S-expressions end to end (incl. deep-nesting test as a built binary)
- [x] 1.2 JSON end to end
- [x] 1.3 A calculator end to end — `Climb`, lowered and evaluated

**Stages 2-6**
- [ ] 2 `ebnf.aipl` + FIRST sets
- [ ] 3 Highlighter generator (oracle: `tests/highlighting.rs`)
- [ ] 4 AIPL's grammar, differential-tested against gazelle
- [ ] 5 Formatter generator
- [ ] 6 Retire gazelle

The stages are strictly sequential.

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
is no longer part of this project**; the design below is written against the
fixed compiler and depends on all of it.

What it bought, in the shape the library actually needs: a case can be matched
without matching its payload (`same_case`, plus a `variant` bound), a generic
variant's type parameter is inferred from the expected type, recursive types may
recurse through an array, `match` arms may be statement blocks, and recursion is
tail-call eliminated. (That last one turns out **not** to reach the parse itself:
a recursive-descent call is never in tail position, so parse depth stays bounded
by the stack — see 1.1 below for the measured limit.) Four shapes the design
rests on that had no test anywhere in the repo were confirmed to work and are
locked in by cases — a fn value returning a boxed recursive variant, an `A[]`
parameter where `A` is boxed, an array of structs each holding a boxed field, and
a two-parameter generic variant.

**Stage 1.0 found four more gaps**, all in the same unexplored corner — nobody
had ever written a *generic recursive* variant, which `Rule<K>` is — and all four
are now fixed and locked in by `tests/cases/generics/recursive_variant.aipl`:

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
link pass (Stage 1) turns rule references into array indices and FIRST sets
(Stage 2) remove most of the need to memoize — so nothing below waits on them.

## Stage 1 — The library, proven on two toy grammars

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
    build: Build<A>,  // AST lowering
}                     // (`layout: Layout` joins this in Stage 5)

variant Build<A> = Mk((str, Cst, A[]) -> A!ParseError)
struct Grammar<K, A> { prods: Production<K, A>[] }

// cst.aipl — lossless: CTrivia puts comments in the tree, so concatenated
// spans reconstruct the source byte-for-byte.
variant Cst = CLeaf(Span) | CTrivia(Span) | CTree(u64, Cst[])
```

`Build` has a single arm that always pins `A` — deliberately no `NoLower` arm,
since a payload-free arm pins no type parameter of its own
(there is nothing to infer *from*). Productions with nothing to lower pass a
trivial named function.

`build` takes the **source text** as well as the node: a `Cst` holds spans, not
text, so without it a lowering function has no way to read what it matched.

Function values carry three restrictions that shape `build`: they must be
**effect-free** (`check.rs:1649`), **non-generic** (`check.rs:1639`), and
**non-capturing** (`check.rs:1825`). So each is a named top-level
`fn lower_<prod>(src: str, cst: Cst, kids: A[]) -> A!ParseError`. They also cannot be
compared with `==` (`check.rs:3059`), so a `.test` block asserts on parse
*output*, never on the grammar value — the same constraint `lexer.aipl` lives
with.

The driver (`parse.aipl`) is **two passes, not one**: `parse_cst` builds the
tree, `lower` walks it calling `build`, and `parse` is the two in sequence.
Lowering *during* the parse would run `build` inside speculative alternatives
that then backtrack — wasted allocation at best, and at worst a `build` that
legitimately fails aborting a parse that should simply have tried the next
alternative. `parse_cst` is also the entry point for consumers that only want
the tree. Three things the driver must get right: **`link(grammar)`** runs once, rewriting `Named(str)` →
`NamedIdx(u64)` and erroring on unresolved references; **a progress guard on
`Many`**, since an inner rule matching empty input loops forever and AIPL has no
`break` (`lexer.aipl:194-196` documents the lexer's version of the same
invariant); and **furthest-failure tracking** for errors.

### 1.1 S-expressions — the smallest grammar that is still recursive

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
in isolation — done in `parse.aipl`, against a three-kind toy language lexed by
`lexer.aipl` itself rather than a hand-built token array, since parsing a string
reads better in a test than hand-counted spans and it exercises the intended
composition at the same time. `Climb` is covered there too (precedence,
both associativities), so it does not have to wait for Stage 4 to be proven.

Two authoring notes from writing them, both compiler limits rather than design:
an integer literal does not flex through a constructor, so `Many(r, 0)` is a type
error and the `many`/`many1` helpers exist to absorb it; and most `Rule`
constructors mention no `K`, so a rule wants an annotated binding
(`let r: Rule<Kind> = ..`) for its type parameter to resolve.

**What 1.1 landed** (`grammar_sexp.aipl`, plus one addition to `cst.aipl`):

- A lowering function needs a token's *spelling*, and `text` is the lossless
  view — it includes the trivia attached in front of the token, so an atom lowered
  through it carries the preceding blank line. `cst.aipl` grew the meaningful
  counterpart, **`token_leaves`/`token_text`**: the same walk with `CTrivia`
  dropped. Every lowering that reads a leaf wants it, so it belongs in the
  library rather than in each grammar.
- **The deep-nesting test is a `.test` block, and that is already a built
  binary.** A library case's `--- performance ---` is measured by AOT-linking the
  synthesized `.test` driver and running it (`tests/cases.rs`, `measured_program`
  → `measure_perf_stats`), so the same block runs twice: under `aipl check` on
  the CLI's 256 MB thread stack, and as an ordinary binary on 8 MB. No separate
  case file is needed — and none is possible, since `tests/cases/` files are
  staged into a temp dir where a relative import into `crates/` would not resolve.
- **Parse depth is stack-bound, not heap-bound.** Recursive descent spends a frame
  per level and none of those calls is in tail position, so tail-call elimination
  does not apply to the parse itself. Measured against the 8 MB a shipped binary
  gets: 1,000 levels of `((( ... )))` parse, render and re-parse fine; 2,000
  segfaults. The test sits at 200, which leaves ~5x headroom while still being
  deep enough that a regression in frame size would show.

### 1.2 JSON

Adds what S-expressions leave out, still without precedence: several terminal
classes (string, number, the three keywords), `ListOf` with a separator and a
trailing-comma rule, `OneOf` across six alternatives, and a heterogeneous AST.
Complete but tiny, and `lexer.aipl`'s own test block already drives a
JSON-flavored rule set to crib the token rules from. `grammar_json.aipl` supplies
those rules, ~10 productions, and lowering to a `variant Json`.

**What 1.2 landed** (`grammar_json.aipl`, eight productions, plus one fix to the
driver):

- **`ListOf` never recorded its separator as an expectation**, so `[1` reported
  "expected `]`" and left out the comma — the commonest way a list actually goes
  wrong. `match_list_of` now calls `far_miss` with the separator when none
  follows an item; since `ListOf` always matches, that only widens the expected
  set and never changes what parses. This is the kind of thing a second grammar
  exists to find: the S-expression toy has no separators at all.
- **`JMember` is where a uniform lowering type shows its seam.** `Build<A>` pins
  one `A` for every production, so `member` — whose natural result is a key/value
  *pair*, not a JSON value — lowers to a `Json` case of its own, meaningful only
  inside a `JObject`. Worth knowing before Stage 4: AIPL's AST has many more
  shapes than its expression type, so either they all become cases of one lowering
  variant or `Grammar` grows a second type parameter.
- **Numbers stay text.** `JNumber` carries the source spelling. JSON's number
  syntax is wider than any one AIPL type, so what a number *means* is the
  consumer's policy — the same call the lexer already makes by handing `Int`/
  `Float` over unparsed.
- **The sign is the grammar's, not the lexer's.** `Exact("-")` ahead of
  `Float`/`Int` makes every `-` its own token and `Int` unsigned, so
  `Maybe(Spelling("-"))` in one production handles the sign — rather than a sign
  the lexer swallowed for integers and one it didn't for floats.

Two more authoring notes, both compiler behavior rather than design (the 1.1 list
has the first two):

- A `char` interpolates as `'x'`, quotes included, so building a string from a
  `str`'s bytes wants the one-byte *slice* `s[i..i + 1]`, not `s[i]`.
- A `"""` block is raw *and* dedented, so a leading space on a single-line block
  is stripped while a trailing one is kept. JSON sources that begin with `"` read
  more predictably as ordinary escaped literals.

### 1.3 A calculator — `Climb`, end to end

Neither earlier toy has an infix operator at all, so precedence climbing was
proven only by `parse.aipl`'s own unit tests, which stop at the concrete tree.
`grammar_calc.aipl` is the four-operator arithmetic grammar this section used to
propose, and it goes one step further than the other two toys: it **evaluates**
the lowered tree, which is the assertion a wrong shape cannot survive.

```
expr  -> Climb(unary, [+ -  |  * / %  |  ^ right])
unary -> "-" unary | atom
atom  -> Num | "(" expr ")"
```

`render_calc` prints fully parenthesized, so a rendering *is* a tree shape and
the precedence assertions read as `"1 + 2 * 3"` → `"(1 + (2 * 3))"`. Evaluation
then pins the same facts as arithmetic: 14 rather than 20, `2 ^ 3 ^ 2` = 512
rather than 64 (right-associative), `10 - 3 - 2` = 5 rather than 9 (left).

**What 1.3 landed:**

- **`cst.aipl` grew `own_tokens`/`own_token_text`** — the tokens a node matched
  *directly*, one level down. It is the mirror of `nodes` (direct branches), and
  it is what a lowering function reads an operator from: `Climb` nests both
  operands under the enclosing production's index and leaves the operator as a
  direct leaf between them, so `token_text` would hand back the whole
  expression's tokens run together. Third such helper the toys have asked for,
  after 1.1's `token_leaves`/`token_text`.
- **`expect_as` earned its keep, visibly.** Unlabelled, an empty input reported
  "expected `-` or an expression" — the `-` being the first alternative of
  `unary` leaking out, when a unary minus *is* an expression. Labelling `unary`
  and `atom` with the same word collapses them, exactly as the LR parser's
  `SYMBOL_DISPLAY_NAMES` collapses `expr`/`term`/`unary`/`atom`. Worth knowing
  for Stage 4: under ordered choice this is not a nicety, it is what keeps a
  message from listing the grammar.
- **Unary minus is not a `Climb` operator.** `Climb` is binary, so negation lives
  in the atom rule — which is also what makes it bind tighter than every infix
  level (`-2 ^ 2` is `(-2) ^ 2`). A prefix-operator table is a thing `Rule` does
  not have and, on this evidence, does not need.
- **The two-pass design paid off concretely.** An integer literal too big for an
  `i64` is a *lowering* failure raised after the parse succeeded, and it
  propagates out of `parse` unchanged. Run during the parse it would have aborted
  alternatives that should simply have been retried.

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

## Stages 2-6

**2 — Introspection, proven.** `ebnf.aipl` dumps the grammar as EBNF; FIRST-set
computation lets `OneOf` pick a branch without backtracking (also why packrat
memoization is not needed while dicts remain linear scans).

**3 — The highlighter generator.** Generate
`editors/vscode/syntaxes/aipl.tmLanguage.json` from the grammar. First because it
**already has a strict oracle**: `tests/highlighting.rs` validates that file
against every token of every case file and example. The target is regex- and
line-based, so the generator emits token classification plus contextual patterns
for the easy declarations — what the hand-written grammar does today.

**4 — AIPL's own grammar**, differential-tested against gazelle across the whole
corpus, in the shape of
`tests/lexer_dogfood.rs::dogfood_lex_hook_matches_fresh_compile_on_corpus`.

**5 — The formatter generator**, targeting the existing `Doc`. `Layout` starts
from `walker.aipl`'s vocabulary (`ListStyle`, `comma_list_docs`, `match_expr`'s
hard-broken block) with a `Custom(str)` escape hatch naming a hand-written layout
function. Honest risk: most of `walker.aipl`'s bulk is *heuristics* — comment
attachment, call hugging, chain breaking, import sorting — and those will not
fall out of a grammar. The realistic win is retiring the mechanical two thirds
and shrinking the "teach two parsers" problem, not deleting the file.

**6 — Retire gazelle.**

Stages 4-6 change the compiler's own parse path and pull these files into
`DOGFOOD_SOURCE_FILES`; that is where IR regeneration, relink cost, and the
formatter bootstrap deadlock start to matter. Stages 1-3 touch none of it.

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
   source byte-for-byte. Stage 5 depends on this; it is the cheapest place to
   catch a regression.
6. **Round-trip on each toy** — parse → lower → render → parse, for
   S-expressions, then JSON, then the calculator (whose rendering is fully
   parenthesized, so the round trip is exact and the text shows the tree). The
   deep-nesting case (~200 levels of
   `((((...))))`) belongs with S-expressions, since that is the first grammar
   that can nest, and it must run as a **built binary**, not just under
   `aipl check` — otherwise it is measured against 256 MB instead of the 8 MB a
   shipped binary gets, and tail-call elimination goes untested.
7. **Stage 3 oracle** — `cargo test --test highlighting` against the generated
   `.tmLanguage.json`, unchanged.
8. **Finish** — `cargo handoff`, which regenerates the `#[test]` list for
   new case files and fills their `--- performance ---` sections. Expect churn: a
   library case's perf section measures its `.test` driver, and for scale
   `walker.aipl` records 3,075,003 instructions.
