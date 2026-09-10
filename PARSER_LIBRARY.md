# A parser library in AIPL

## Status

Long-running, interleaved with other work. Update the checkboxes as items land;
each is sized to be finishable in one session.

**The library is built and stages 1-4 are done.** `grammar.aipl`, `cst.aipl`,
`parse.aipl` and `format.aipl`, with three end-to-end toy grammars —
`grammar_sexp.aipl` (recursion and depth), `grammar_json.aipl` (several terminal
classes, a separated `Many`, a heterogeneous AST) and `grammar_calc.aipl`
(`Climb`, lowered and then *evaluated*, which is the assertion a wrong tree
cannot survive). Every `Rule` arm is covered. On top of them: an EBNF dump and
FIRST sets (`ebnf.aipl`), a highlighter generator whose oracle is
`tests/highlighting.rs`, AIPL's own grammar (`grammar_aipl.aipl`) checked against
gazelle over the corpus, and a formatter generator. Those files and their `.test`
blocks are the record of how the library works; what is left is below.

Everything remaining is one goal — **run the compiler on the AIPL parser and
delete gazelle** — split into the pieces that can each be finished and judged on
their own:

- [x] 5a Decide the bridge: how an AIPL parse becomes a Rust `Program`
- [x] 5b Wire FIRST-set pruning into the driver, and re-measure
- [x] 5c Extend the differential test from acceptance to shape (operator grouping)
- [ ] 5d Lower AIPL's grammar to the AST
- [ ] 5e Error-message parity against the corpus fixtures
- [ ] 5f Side-channels: `#[allow]` spans, trailing whitespace, doc attachment
- [ ] 5g The bootstrap procedure, and the `DOGFOOD_SOURCE_FILES` switch

**5a is decided: lower in AIPL and marshal the result**, and 5b is measured. 5c
is next: it is the oracle 5d needs, and worth having before the lowering rather
than after it.

## Context

AIPL's syntax started out described **three times, by hand, in three unrelated
formalisms**. Two of the three are still hand-written, and they are what this
project exists to remove:

| Description | Where | Size | Produces | Status |
|---|---|---|---|---|
| gazelle LR(1) grammar | `crates/aipl-parser/src/lib.rs:18-504` | ~254 non-comment lines, 81 rules, 238 alternatives | the compiler's AST | by hand |
| formatter token walker | `crates/aipl-codegen/src/walker.aipl` | 2920 code lines, 144 functions | a `Doc` layout tree | by hand |
| TextMate grammar | `editors/vscode/syntaxes/aipl.tmLanguage.json` | 176 lines | editor scopes | **generated** (stage 2) |
| the grammar as data | `crates/aipl-codegen/src/grammar_aipl.aipl` | 1144 code lines, 54 productions | a `Cst`, and nothing else yet | the replacement |

CLAUDE.md still names the cost of the first two: *"Adding a syntax form means
teaching two parsers: the gazelle grammar in `aipl-parser` **and** the
formatter's own token walker"* — a pairing with a documented bootstrap deadlock,
since handoff formats the corpus before it regenerates IR, so a formatter that
doesn't yet know the new syntax is the one asked to format sources written in
it.

Note the fourth row makes it *four* descriptions today, not two. That is the
expected shape mid-project and the reason stage 5 is worth finishing rather than
leaving: `grammar_aipl.aipl` only pays for itself once the two hand-written rows
go away.

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
meant to outlive them, so the language was fixed first. **That work is done and
is no longer part of this project**; the library is written against the fixed
compiler and depends on all of it: matching a case without its payload
(`same_case`, plus a `variant` bound), inferring a generic variant's type
parameter from the expected type, recursion through an array, `match` arms as
statement blocks, tail-call elimination, and four shapes that had no test
anywhere in the repo (a fn value returning a boxed recursive variant, an `A[]`
parameter where `A` is boxed, an array of structs each holding a boxed field, a
two-parameter generic variant) — each now locked in by a case.

Building the library found four more gaps, all in the same unexplored corner:
nobody had ever written a *generic recursive* variant, which `Rule<K>` is.
Declaring `Rule<K>` at all crashed the compiler. All four are fixed and locked in
by `tests/cases/generics/recursive_variant.aipl`.

Two things worth knowing that came out of it:

- **Parse depth is bounded by the stack, and TCE does not help** — a
  recursive-descent call is never in tail position. Measured against the 8 MB an
  ordinary binary gets, `((( ... )))` parses, renders and re-parses fine at 1,000
  levels and segfaults before 2,000.
- **Hash-backed dicts and sets** are on TODO.txt rather than here. They are the
  largest asymptotic win in the repo and would make packrat memoization
  affordable, but `link` already turns rule references into array indices and
  FIRST sets remove most of the need to memoize — so nothing above waits on them.

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

**Unclosed brackets** are the one diagnostic a PEG cannot reconstruct after the
fact, and `NestedIn` is what supplies it. A flat `Then([Lit("("), .., Lit(")")])`
fails at the close having forgotten an open ever matched, so the best it can say
is "expected `)`, found end of input" — with no hint that the `(` responsible is
two screens up. A group knows, and records the opener's span on the furthest
failure, giving ``expected `}`, found end of input (`{` at line 1 is never
closed)``.

It is deliberately narrow. The hint is attached **only when the close was wanted
past the last token**: anywhere else there is a real token sitting where the
bracket should be, and `[1, 2,]` — which fails at the trailing comma with the `]`
in plain sight — must not be told its `[` is unclosed. When several brackets are
hanging, the innermost wins, that being the one to close first. Under ordered
choice a group failing to close is still an ordinary backtracking non-match, so
the hint only ever reaches a reader when no alternative worked at all.

## Remaining work

All of it changes the compiler's own parse path and pulls these files into
`DOGFOOD_SOURCE_FILES` / `FMT_SOURCE_FILES`
(`crates/aipl-codegen/src/lib.rs:3352,3391`); that is where IR regeneration,
relink cost, and the formatter bootstrap deadlock start to matter. Nothing done
so far has touched any of it — the library files are ordinary cases, picked up by
the harness and by `aipl check crates`, with no `.clif` involved.

**Where it actually stands.** `tests/suites/parser_dogfood.rs` proves less than
its name suggests, and says so in its own module doc: agreement with gazelle is
*acceptance* plus losslessness, file by file, over the corpus — never a tree,
never an AST, never a message. Rejection is half of it, and the ~100 deliberate
syntax-error fixtures are what make that half mean something. So the language is
recognized; everything the production parser does beyond recognizing is unbuilt.

### 5a — The bridge. Done: lower in AIPL, marshal the result

`grammar_aipl.aipl` ends with `variant Ast = Ignored` and all 54 productions pass
`Mk(keep)`. Gazelle's `Build` has **86 action methods** producing
`aipl_syntax::Program` — `Item`, `Signature`, `Function`, the `Type` enum, `Expr`
with 32 `ExprKind` variants, `Pattern`. The chosen bridge is to declare the AST
in AIPL, lower to it in the `build` functions, and marshal the result across the
FFI as an `FfiValue` the Rust side rebuilds into a `Program`.

The alternative — return the `Cst` and build `Program` in Rust by walking it —
needs no AST re-declaration, but leaves `Build`/`Mk` unused by the grammar that
matters most and puts the lowering back in Rust, which is the code this project
is trying to delete. Re-declaring the AST is a real cost and is honestly the
weaker half of this decision; it is accepted because the lowering, not the type
declaration, is where the duplication actually hurts.

**The plan was not viable as written.** The FFI could not marshal a recursive
type in either direction, which is what every syntax tree is. Three things were
wrong and are now fixed, with tests in `tests/suites/ffi.rs`:

- **`check_ffi_return` recursed forever.** It walks a variant's case payloads,
  and a case mentioning its own type never bottoms out — so `call_values` on a
  tree-returning function overflowed the stack rather than failing. It now
  carries the set of named types already on the stack; marshalability is a
  property of the type graph, not of a path through it.
- **The reader read the pointer as the payload.** A boxed value is an 8-byte
  pointer to a heap payload that has the type's ordinary inline layout, so the
  fix is one deref and then the existing inline read — `read_ffi_boxed_payload`.
  The payload is read *borrowing* whatever the caller's ownership: releasing per
  constituent would double-release what the block still owns, so one
  `dec_strong` after the read balances the callee's returned reference and the
  drop cascade reclaims the children.
- **A boxed return took the sret path.** It has a struct/variant layout like any
  declared type, so the composite-return arms claimed it and read the returned
  pointer word as a payload's first field. It is a register return, and is now
  taken out of the running before those arms.

Proven on the shape an AST actually is: a struct and a variant that reach each
other (so *both* are boxed), carrying a non-boxed struct inline, an optional
boxed field both ways, and an array of boxed values. Plus 10,000 iterations
against a tree holding heap strings, since a refcount error is silent in one
call.

**Building a boxed value from the host is still refused**, and deliberately: the
host cannot construct the refcounted block. That is fine for this bridge, which
only ever reads one out. The refusal now says so on the top-level path too
(previously reachable only for a boxed value nested inside a composite, so a
boxed parameter got a generic shape-mismatch message instead).

### 5b — FIRST-set pruning. Done, and the answer was not the expected one

**Baseline: 72.8s** for the AIPL parser to accept the corpus
(`cargo test --test dogfood -- aipl_grammar_matches_gazelle`). Run-to-run
variance on that measurement is about +/-3s, which is worth knowing before
reading small differences into it.

Pruning is wired at `match_prod` — the point a reference is followed, so skipping
removes a whole subtree rather than one token compare, and it costs a table
lookup and a membership test with nothing allocated. `atom` is twenty
alternatives behind one production reference each, and one token of lookahead
rules out nearly all of them.

Three findings, two of them negative:

- **Pruning silently degrades error messages, and the driver's own tests caught
  it** — seven assertions, immediately. `match_prod` already collapses a
  *labelled* production to its label, so pruning one is exactly what attempting
  it would have done. A *transparent* production has no label and what it would
  have recorded is its insides' expectations at that position, which pruning
  skips straight past. `label_sets` (in `parse.aipl`) puts them back: the FIRST
  set computed over labels rather than terminals, a labelled sub-production
  contributing its label and a transparent one its insides, as a fixed point for
  the same reason `first_sets` is one. Pruning is now asserted invisible —
  against the toys in `parse.aipl` and against the real 54-production grammar in
  `grammar_aipl.aipl`.
- **`can_start` cost about what it saved.** The natural spelling —
  `syms.contains(SKind(k)) || syms.contains(SText(text))` — *builds* a `Sym` per
  call and walks the set twice, and it is asked once per production reference.
  Rewritten as one allocation-free pass, pruning went from a small loss to ~11%.
- **The analysis costs more than pruning saves, for a single parse.** Computing
  the FIRST and label sets is two fixed points over the grammar, and doing it per
  source made the corpus run *slower* (79s against 72.8s). It depends only on the
  grammar, so it should be paid once.

Hence `Prepared` / `prepare` / `parse_prepared`. `prepare` links and analyses;
`parse_prepared` parses against that. `parse_cst` deliberately links and stops
there — an unanalysed grammar has empty sets, the driver finds nothing to prune
on, and a one-shot parse pays nothing for a lookahead table it would use once.

**Measured, 300 parses of a small source, fixed overhead subtracted:**

| | per parse |
|---|---|
| `parse_cst` (link only, no pruning) | ~12.0 ms |
| `parse_prepared`, no pruning | 5.97 ms |
| `parse_prepared`, pruning | 5.33 ms |

So the win is **~2.2x, and almost all of it is not redoing the link and the
analysis**; pruning is a further ~11%. That reorders what matters: the driver was
not slow because it backtracked, it was slow because every parse rebuilt
everything the grammar already knew about itself.

**The corpus number is unchanged (76.0s, inside the noise band)** and cannot
improve, because the differential test makes one FFI call per file and nothing
can hold a `Prepared` across calls — it contains function values, which do not
marshal. **That is an architectural item for 5g**: the production path must
prepare once and reuse, and there is currently no way for it to do so. Until
that is answered, the 2.2x is available in principle and unreachable in practice.

### 5c — Differential test: from acceptance to equality — **done**

`aipl_grammar_groups_expressions_like_gazelle` in `tests/suites/parser_dogfood.rs`,
against `aipl_node_spans` in `grammar_aipl.aipl`.

**What it compares.** Every *operator application* gazelle records must have a
node in the AIPL grammar's CST with the same token span. Precedence and
associativity are what two grammars can disagree about while accepting the same
text, and an operator call is where they are decided.

**Why only operator applications.** Gazelle's spans are kept for diagnostics
rather than as a record of extent, and every other shape carries one that is
short of its text: a bracketed form's span stops before its closing bracket
(`[1, 2, 3]` spans `[1, 2, 3`), a call's before its parens — the same thing the
lint driver's `spans_its_text` documents from the other side — and unary
`-`/`!` leave their own operator out. An operator's span runs operand to operand
and is exact, *provided* both operands record their own extent, which a
parenthesized or unary operand does not. Those are skipped by looking at the
neighbouring byte; it over-skips, which only costs coverage, and the
discriminating assertion survives either way — in `(a + b) * c` the `*` is
skipped while the `+` inside it is compared, and it is the `+` that says which
way the grouping went.

**Two details that cost time on the first attempt, both now handled:**

- **Token spans, not `Cst::span`.** Trivia attaches in front of the token that
  follows it, so a node's plain span reaches back over the whitespace before it.
  `cst.aipl`'s `token_span` is the tight one, and it finds the leftmost and
  rightmost token by descending rather than by collecting every token under the
  node — `token_leaves` walks the whole subtree, and asking it at every node is
  quadratic in the tree, which is what read as a hang.
- **A fixture, not a sweep.** 35 sources, ~52 operator applications, a fixed
  cost. The corpus-wide version was abandoned twice; its notes are below.

**Verified to have teeth by mutation**, which is the check worth repeating after
any change here: swapping the `*` and `+` levels in the grammar's precedence
table produces 8 disagreements, and making `-` right-associative produces 9. A
differential that cannot fail is worth nothing, and this one is easy to make
vacuous by widening a skip rule — hence the floor on how many applications it
compared.

**Why not the whole corpus** (the two abandoned attempts):

- A second FFI call per file re-parses it, doubling a differential that already
  takes ~76s, and a file's spans cross as a `Vec<FfiValue::Int>` with an element
  per number. The remaining cost was never localized.
- **Measuring a prefix of the corpus does not predict the whole.** `files.sort()`
  puts the small `tests/cases/` files first and the large compiler sources last,
  so a timing taken over the first several dozen files is drawn entirely from the
  cheap end. 40 files at ~75 ms each projected to under two minutes; the real run
  passed thirty and was killed.

It only becomes worth revisiting if the tree can be got across the FFI once and
cheaply — the same "no way to hold prepared state across calls" problem 5g has
to answer.

### 5d — Lower AIPL's grammar to the AST

The bulk of the work. Declare `aipl_syntax`'s AST as AIPL types, replace
`variant Ast = Ignored` with it, and write the 54 `build` functions against the
86 gazelle actions as the reference. The marshalling underneath is done and
tested (5a); what is left is the AST declaration, the lowering, and the Rust-side
`FfiValue` → `Program` reconstruction.

Keeping the two AST declarations in step is the standing cost of this bridge. A
test that walks both and compares shape — not just the differential in 5c — is
worth having early.

### 5e — Error-message parity

**170 corpus files carry `--- errors ---` blocks**, rendered byte-exact from
gazelle's `friendly_syntax_error`. Every one must match or be refilled and
individually reviewed — and a refill that silently degrades a hundred messages is
exactly what the handoff review guard exists to catch, so this is the step to
diff by hand.

See *Errors* above for why PEG makes this harder than LR rather than easier: the
`expect_as` labels exist but have never been measured against these fixtures.

### 5f — Side-channels

Small individually, each a silent behavior loss if missed:

- `parse_with_allows` collects `#[allow]` spans through a thread-local
  `ALLOW_SINK` for the loader. The AIPL side has no equivalent — the lexer makes
  the markers trivia and `grammar_aipl.aipl` drops them.
- `reject_trailing_whitespace`.
- Doc-comment attachment.

### 5g — The bootstrap, and the switch

The circularity: the parser that parses `grammar_aipl.aipl` would be generated
from `grammar_aipl.aipl`. This is the `walker.aipl` formatter deadlock already
documented in CLAUDE.md, one level worse, and there is no procedure for it yet.
Write the procedure before needing it.

## Constraints still in force

Compiler limits the library is written around rather than blocked by. None is on
the critical path; all are worth knowing before extending these files.

- **A struct field default must be a literal.** `layout: Layout = Inline` is
  "unknown identifier"; `layout: Layout? = none` is refused as a field type
  (`Layout` is not recursive, so not boxed); `layout: Layout = inline()` *is*
  accepted but the call is re-resolved in every file that constructs a
  `Production`, so a parser-only grammar would have to import a function it never
  names — and the error when it forgets points at an unrelated line. Hence
  `layout` has no default and every production states one. A *variant case*
  payload may default to a struct literal (`style: ListStyle = ListStyle {}`),
  which is why `NestedIn` can.
- **A lambda bound to a `let` is not a value.** It cannot be called by name and
  cannot be passed where a function parameter is expected, so test helpers that
  would naturally be local lambdas are named top-level functions. (A lambda in
  *value* position also needs a block body; only an argument-position one may
  have an expression body.)
- **A function value cannot be generic**, and the forwarding lambda that would
  work around it is what `lambda_only_forwards` refuses — the two rule each other
  out, so `group_styles` in `grammar.aipl` carries an `#[allow]` with no other
  spelling available. A lint whose advice does not compile; worth fixing.
- **`# ` opens a doc comment**, so `# { str }` is not a set type. Write `#{str}`.
- **`#{}` does not flex through a generic parameter.** It flexes fine against
  an annotation or a declared field type, but `labels[i].value_or(#{})` is
  "conflicting types for `T`: `#{str}` vs `#{__none__}`" — the receiver pins
  `T` and the literal is matched against it rather than taking it. So the
  label functions each open with a `let empty: #{str} = #{};` and pass that.
  The same is true of `[]`, but an empty array is rarely written in argument
  position, whereas an empty set is the natural default for a fold.

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
   source byte-for-byte. The formatter depends on this; it is the cheapest
   place to catch a regression.
6. **Round-trip on each toy** — parse → lower → render → parse, which each of
   the three asserts, plus the formatter's own pair:
   parse → format → parse gives back the same AST, and formatting an already
   formatted source changes nothing. The calculator renders fully parenthesized, so its round
   trip is exact *and* its text shows the tree; a grammar added later should
   carry the same pair. The deep-nesting case (200 levels of `((((...))))`) lives
   with the S-expressions and runs as a **built binary**, not just under
   `aipl check` — otherwise it is measured against the CLI's 256 MB thread stack
   instead of the 8 MB a shipped binary gets.
7. **Highlighter oracle** — `cargo test --test highlighting` against the generated
   `.tmLanguage.json`, unchanged.
8. **Finish** — `cargo handoff`, which regenerates the `#[test]` list for
   new case files and fills their `--- performance ---` sections. Expect churn: a
   library case's perf section measures its `.test` driver, and for scale
   `walker.aipl` records 21.6M instructions.
