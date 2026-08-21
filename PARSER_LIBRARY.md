# A parser library in AIPL

## Status

Long-running, interleaved with other work. Update the checkboxes as items land;
each is sized to be finishable in one session.

**Stage 1 — language work** (blocks Stage 2)
- [ ] 1.0 Reproducers: fn value returning a boxed variant; `A[]` param where `A`
      is boxed; array of structs holding a boxed field; two-param generic variant
- [ ] 1.1 Generic-variant inference from expected type (`check.rs:605-635`)
- [ ] 1.2 `case_index` / `case_name` builtins — match a case, not a payload
- [x] 1.3 Box types recursing through an array (`lib.rs:6837`) — also TODO.txt:237
- [x] 1.4 Fix `{ boxed field, array field }` — TODO.txt:320 (already fixed; case added)
- [x] 1.5 Tail-call elimination — TODO.txt:467
- [x] 1.6 `match` arms as statement blocks (arm grouping still open)
- [ ] 1.7 *(deferred)* hash-backed dicts/sets — TODO.txt D1

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

Each Stage 1 item can land independently and is useful on its own — 1.3, 1.4 and
1.5 fix bugs already on TODO.txt regardless of this library. Stage 2 onward is
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

## Why the language work comes first

Pressure-testing the natural design against the compiler turned up four blockers.
Each has a workaround, but each workaround is a distortion of the library —
and in every case the underlying defect is already on TODO.txt. Building the
library around them would bake today's compiler limits into the shape of a file
meant to outlive them, so **Stage 1 fixes the language and Stage 2 starts the
library.** The design below is written against the *fixed* compiler, with the
fallback noted where one exists.

Two of these (variant-payload matching, tail calls) are not merely inconvenient:
without them the library is materially worse, not just uglier.

## Stage 1 — Language work

### 1.0 Reproducers first

Each item below gets a ~20-line failing case under `tests/cases/` before it gets
a fix. Four of the shapes the design rests on have **no test anywhere in the
repo** and may already work: a fn value *returning* a boxed recursive variant; an
`A[]` parameter where `A` is boxed; an array of structs that each hold a boxed
field; and a two-parameter generic *variant* (only two-parameter generic structs
are attested, at `tests/cases/structs/generic_pair.aipl:5`). Confirm before
fixing.

### 1.1 Infer a generic variant's type parameter from the expected type — *required*

`infer_generic_variant_ctor` (`crates/aipl-mono/src/check.rs:605-635`) resolves a
generic variant's parameter only from the constructor's own arguments, or via
`sole_instance` (`check.rs:383-405`) when exactly one instance exists
program-wide. So in `variant Rule<K>`, the arms `Spelling(str)` and `Named(str)`
pin nothing and compile only while a single grammar exists in the program —
adding a second grammar breaks the first. `Then([])` fails the same way, since
`bind_field` skips empty array literals (`check.rs:307`).

This is closing a documented asymmetry, not new machinery: **generic structs
already do this**, via `ret_generic_args` (`check.rs:518-522`), and the variant
path's own doc comment (`check.rs:568-570`) already claims the behaviour it
doesn't implement. The fix threads the expected type into the variant path.
Requirement: it must flow *through* array literals and nested constructor
arguments, so `Then([Spelling("("), Named("expr")])` types from the outside in.

*Lifts:* the rule tree stays generic — `variant Rule<K>` as written.
*Fallback:* make `Rule` non-generic and have consumers pre-classify tokens into
`PTok { class: u64, span: Span }` (the projection `walker.aipl` already does with
`FmtTok`). Workable, but it pushes a mapping table onto every consumer.

### 1.2 Match a variant's case without matching its payload — *required*

`==` on variants is structural (`check.rs:3053`), and a generic driver cannot
`match` on a type parameter. So with `Term(K)` holding a token kind,
`Term(Name(""))` matches only the *empty* identifier — **"any identifier" is
inexpressible**, which is fatal for a grammar over a token type whose kinds carry
payloads (`lex_aipl.aipl:59`: `Name(str)`, `IntLit(i64)`, `StrLit(str, StrStyle)`).

Proposed: a builtin `case_index(v: T) -> u64` returning the constructor's
discriminant, so the driver compares tags rather than values. The tag already
exists in the runtime representation — `to_str` renders `Cons(1, Nil)`, so it is
reading the same thing. A companion `case_name(v: T) -> str` is worth adding for
diagnostics and for the EBNF/highlighter generators, but the parser hot path
wants the integer.

*Alternative worth considering:* first-class payload-free case references (`Name`
as a value of a "case tag" type). Cleaner at the use site, considerably larger to
implement.

### 1.3 Box types that recurse through an array — *required*

`contained_named_types` (`crates/aipl-codegen/src/lib.rs:6837-6847`) walks
`Named`/`Optional`/`Result` and drops arrays into `_ => {}`. So
`variant Cst = Leaf(Span) | Tree(str, Cst[])` forms no containment cycle, is not
boxed, stays an inline composite — and lands exactly on the segfault recorded at
TODO.txt:237 (*"composites are copied byte-wise into the slot rather than stored
as a refcounted pointer"*). Note `Doc` is **not** the precedent it appears to be:
it is boxed because of `Indent(Doc)` and `Group(bool, Doc)`, *direct*
self-references; `Concat(Doc[])` merely rides along.

Two routes: add `Type::Array(inner) => contained_named_types(inner, out)` so
these types become boxed (small change, plausibly fixes TODO.txt:237 outright
since boxing supplies the refcounted pointer the TODO says is missing), or fix
the inline-composite copy path so the current representation is correct
(no layout change, deeper work). Recommend the first, verified against the
Stage 1.0 reproducer.

*Lifts:* the CST is the obvious `Tree(u64, Cst[])`.
*Fallback:* a mutual cons-list (`CstList = CNil | CCons(Cst, CstList)`) to force
the cycle — correct today, but it makes every child access a list walk.

### 1.4 Fix `{ boxed recursive field, array field }` — *required* — **already fixed**

TODO.txt:320, already marked *"`<--- fix this`"*: a struct holding both a
recursive-variant field and an array field breaks the compiler.

**Resolved without a code change.** That TODO line was an intermediate
*narrowing note* for the composite `mut`-binding corruption, written in
`f5ecdec` and fixed by the very next commit, `36b3301` ("Fix assignment in loop
bug"), which shipped `tests/cases/structs/mut_struct_reassigned_in_loop.aipl`
and narrowed further — the real trigger is one hidden-sret buffer per call site
aliasing a composite `mut` binding across `set b = step(b);`, and it "needs no
variant, no matched payload, no helper fn and no empty initial array". The line
was simply never deleted. Verified by building the compiler at `f5ecdec`: the
shape corrupts memory there and is clean on `main`.
`tests/cases/structs/boxed_field_with_array.aipl` now locks the *type* shape in
(it segfaults on that historical build); nothing in the library needs a
fallback. `walker.aipl`
threads around it and never violates it — `Step { col, work: Work, ... }` has a
direct boxed field and no array; `WDocs { w: W, docs: Doc[] }` has an array of
boxed values and no direct boxed field. This lands **with or before 1.3**, since
boxing more types makes the broken shape more common.

*Lifts:* `Climb(Rule, Prec[])` and natural carrier structs like
`{ cst: Cst, kids: A[] }`.
*Fallback:* move all side tables onto the `Grammar` and reference them by index.

### 1.5 Tail-call elimination — **done** (2026-08-20)

Implemented in `aipl-codegen`. A call in tail position lowers to cranelift's
`return_call`, so a recursive walk reuses one frame and depth stops being bounded
by the native stack — the 8 MB an `aipl build` binary gets, against the 256 MB
the CLI and the cases harness raise themselves to. `doc.aipl` and `walker.aipl`
can now drop their work-stack rewrites (not yet done — see *Lifts* below).

Three things had to line up, and all three are settled before any function is
declared, by `tail_call_plan`:

**Calling convention.** `return_call` requires `CallConv::Tail` on the callee
*and* the identical convention on the caller. So a participant's body moves to a
`Linkage::Local` `<name>$tail` carrying that convention, and the exported
`<name>` becomes a C-convention trampoline forwarding to it. **This contradicts
the earlier "no wrappers needed" finding**, which counted the FFI surface as
`main` + the `; entry` list + address-taken functions. It is wider than that:
`Engine::call_values` can name *any* function, and neither it nor a `func_addr`
function value can know whether the function it names happens to tail-recurse.
Without the trampoline, adding a tail call to an AIPL function would silently
make it uncallable from Rust. AIPL call sites skip the trampoline and call
`<name>$tail` directly, so only the `return_call` sites pay for the convention.

**Nothing may point into the caller's frame.** A participant may not take or
return a composite (struct/optional/result) — those are passed and returned by
the address of caller storage, and the frame is gone the moment control
transfers.

**Every refcounted argument must be the callee's own** — the real constraint, and
what the measurement below was about. A tail call runs the caller's scope cleanup
*before* transferring control, which is safe only if each argument still holds a
reference that cleanup cannot release. Refcounting gives exactly that for a
retained argument: the caller's `+1` is the callee's, and releasing any other
reference can never consume it. So the rule is that every argument owning heap is
`ParamInfo::retained` or moved in. Two things had to change to make that true of
real code:

- **Boxed parameters join the borrow protocol.** They were pure borrows — alive
  across a call only by the caller's own reference, with neither side touching
  the count — which fails the rule outright. A participant's boxed parameters are
  marked `tail_owned` and pay the retain/release pair.
- **A participant gives up retain elision.** An `inspect_only` heap parameter is a
  borrow on the caller's reference by construction, so `inspect_only` is forced
  off for participants.

`hand_off_arg` also had to become type-aware: it retained through the bare
`aipl_inc`, which dispatches on `str` tag bits, and boxed values count on their
own block header via `aipl_rec_inc_strong`.

**The measurement that shaped this (2026-08-20).** "Only where no drops are
pending" was scoped as the conservative first cut. It is not a first cut — it is
a no-op. Scanning `dogfood.clif`, `fmt.clif` and `doc.aipl` for calls in tail
position:

| shape | count |
|---|---|
| clean (nothing between call and return) | 76 |
| drops/retains between | 37 |
| other instructions between | 15 |
| **of which self-calls** | **2** (both dirty) |

**0 of the recursive tail-call cycles are fully eliminable under that rule.** A
cycle is bounded only if *every* edge is, and every cycle has at least one
drop-guarded edge. The reason is structural: a function that `match`es an owned
boxed value binds its payload, and those bindings are released on scope exit —
*after* the recursive call returns. `doc.aipl`'s `fits` is the canonical shape.
Drop motion across the tail call is therefore the feature, not an optimization on
top of it. (The same measurement is why self-call-to-loop, which needs no ABI
change at all, was not worth doing: 128 of 130 tail calls are cross-function.)

**Tail position** is computed in codegen rather than by a CLIF pass, because the
soundness test needs types and per-parameter ownership, which the IR has lost. It
propagates through the forms that only sequence or choose a value —
`Seq`/`Let`/`LetMut`/`Assign` bodies, both `if` branches, every `match` arm — and
is *created* by `return e`. It deliberately does not propagate through `Shim`
(which restores its bindings afterwards) or `Try`. `compile_expr` clears the flag
before every recursion, so an expression form added later is non-tail by default
rather than silently inheriting a wrong `true`.

**Coverage**: `tests/cases/tail_calls/` — a scalar accumulator at 300,000 deep, a
boxed cons-list walk at 200,000, and a three-function cycle with no self-call.
All three run as linked binaries on the OS-default 8 MB stack; the same programs
written non-tail segfault at those depths.

**Still open**: composite returns. A tail call returning via sret has to forward
the caller's sret pointer rather than allocate its own; until it does,
composite-returning functions are simply not participants. Same for dict
parameters, which are refcounted but neither `is_heap` nor boxed.

*Lifts:* the parser driver can be written as ordinary recursive descent, which is
the whole readability argument for this design; `doc.aipl` and `walker.aipl` can
revert their work-stack rewrites.

### 1.6 `match` arms as statement blocks — **done** (2026-08-20)

An arm's body may now be a brace-delimited statement block, exactly like an `if`
branch: statements first, and the trailing expression (if any) is the arm's
value. `P => expr` and `P => { .. }` mix freely, and the block form works for
every pattern kind.

The change is small because it needed no new AST: `block` already builds an
`Expr`, which is what `MatchArm.body` is. So it is one grammar nonterminal —

```
arm_body = expr => expr | block => block;
```

— shared by all eight `match_arm` productions, plus its builder. Factoring it
rather than doubling the productions also keeps `expr` reachable from a *single*
place in that subgrammar, which is what the `block_body` comment warns about:
a second `expr` occurrence is what blew up gazelle's LR tables before. The choice
is one token of lookahead, since no expression begins with `{`. Nothing in the
checker, monomorphizer or codegen changed.

**The formatter was the real work**, and this was the case CLAUDE.md flags as the
one `scripts/handoff.sh` cannot bootstrap: `walker.aipl` compiles into
`fmt.clif`, and handoff formats the corpus *before* regenerating IR, so the live
formatter — which does not know the new syntax — is what would format sources
written in it. The documented escape hatch works as written: `fill_staged_ir` →
format the corpus under `AIPL_FMT_IR=…staged` → `fill_staged_ir` again (step 2
shifts spans) → validate → promote.

**Contortions removed** — the two the plan pointed at, both of which existed
*only* because an arm was a single expression:

- `walker.aipl`'s `bump` no longer needs `bump_tok`. The arm now opens its own
  block and steps the cursor in place.
- `doc.aipl`'s printer loop lost the `Step { col, work, emit, emits, done }`
  carrier struct **and three functions** (`go_step`, `go_node`, `go_raw`). Each
  arm now assigns the loop's own `mut` bindings directly, which is what the
  carrier was faking. 89 lines became 66.

That is measurably cheaper, not just shorter — the plan's "~10 extra frames per
rule node" made concrete:

| | instructions executed | change |
|---|---|---|
| `doc.aipl` | 25,074 → 21,531 | **−14%** |
| `walker.aipl` | 3,059,404 → 2,945,436 | **−3.7%** |
| `format_source.aipl` | 330,520 → 328,259 | −0.7% |

**Coverage**: `tests/cases/match_arms/` (block arms across every pattern kind;
statement-only arms driving a `mut` binding declared outside the `match`), plus
block arms added to the `tests/fmt/match_and_variants.aipl` fixture — which pins
that a statement-free block stays flat (`Empty => { 0 }`) while any block
containing a `;` breaks one statement per line.

**Not taken**: arm grouping `A | B =>` (still open on TODO.txt) — it needs a new
`Pattern` variant and exhaustiveness/binding rules, so it is not the "cheap"
add-on this section assumed. Char-literal arms turned out to be **already
implemented** (`Pattern::Char`); that TODO entry is stale.

### 1.7 Deferred: hash-backed dicts and sets

TODO.txt D1 — dicts and sets are linear scans (`lib.rs:2816`, `dict_find`), which
is the largest asymptotic win available in the repo, and would make both rule
lookup by name and packrat memoization affordable. **Not required**: the link
pass (Stage 2) turns rule references into array indices, and FIRST sets (Stage 4)
remove most of the need to memoize. Listed so the dependency is explicit if the
parser later wants memoization.

### Cost note

1.3 and 1.5 move `instructions executed` and `binary size` in *every* binary.
That is CLAUDE.md's "one sanctioned deviation" — run the whole-corpus
`fill_expected` once rather than handoff's per-case loop, then confirm from the
diff that only metric lines moved.

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
    Term(K)                    // any token of this kind (case-tag match, 1.2)
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
since a payload-free arm reintroduces 1.1's inference problem even after the fix
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
itself through `item` is what forces a **recursive CST** (Stage 1.3's boxing) and
**unbounded parse depth** (Stage 1.5's tail calls), and lowering to `Sexp` is
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
packrat being unaffordable pre-1.7).

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

1. **Stage 1 reproducers** — each language fix has a case that failed before it
   and passes after; the four "unproven shape" cases confirm or refute before any
   fix is written.
2. **Corpus after each language change** — 1.3 and 1.5 especially. Whole-corpus
   `fill_expected`, then confirm from the diff that no
   `--- stdout ---`/`--- errors ---` body moved, only metrics.
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
   shipped binary gets, and 1.5 goes untested.
7. **Stage 4 oracle** — `cargo test --test highlighting` against the generated
   `.tmLanguage.json`, unchanged.
8. **Finish** — `scripts/handoff.sh`, which regenerates the `#[test]` list for
   new case files and fills their `--- performance ---` sections. Expect churn: a
   library case's perf section measures its `.test` driver, and for scale
   `walker.aipl` records 3,075,003 instructions.
