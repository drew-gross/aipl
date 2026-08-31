# `SpanStr`: text that knows where it came from

## Status

Planned, not started. Sequenced behind the 24-byte `str` change (`STR_REPR.md`),
which changes the answer here: if every `str` carries base + data + len, most of
Stages 1-3 below evaporate and `SpanStr` becomes a type marker plus
`data - base`. Revisit this plan once that lands. What follows is the design
against *today's* runtime, and the record of what that runtime provides.

- [ ] 1 The type — `SpanStr` in the type system, with `str`'s runtime shape
- [ ] 2 The forced-view runtime entry point, in both runtimes
- [ ] 3 `span_str` (create) and `span` (recover) builtins
- [ ] 4 Cases: creation, printing, `.span()` round-trip, lifetime, edge shapes
- [ ] 5 One real site — `tok_text` in `parse.aipl`

Stages 1–3 are one indivisible change (the type is unusable without the runtime
entry and the builtins); 4 and 5 are what prove it.

**Decisions, settled up front** — the four forks that would each have sent the
work in a different direction:

| Question | Answer |
|---|---|
| Runtime representation | **Guaranteed view.** A `SpanStr` value *is* a `str` view — same 8-byte tagged value, same 32-byte view object. |
| Creation surface | **Explicit builtin.** `src.span_str(tok.span)`; ordinary slicing keeps returning `str`. |
| Name and visibility | **`SpanStr`, importable** — capitalized, alongside `Span` and `ExecResult` in `IMPORTABLE_BUILTIN_TYPES`. |
| Scope of the first change | **Feature + tests + one real site** (`tok_text`), not the full parser migration. |

## Context

The parser library stores a span and looks it up in the source later, which
means every consumer needs both halves in hand:

| Where | What it holds |
|---|---|
| `crates/aipl-codegen/src/lexer.aipl:70` | `struct Token<K> { kind: K, span: Span }` |
| `crates/aipl-codegen/src/lexer.aipl:105` | `struct LexError { message: str, span: Span }` |
| `crates/aipl-codegen/src/cst.aipl:33` | `variant Cst = CLeaf(Span) \| CTrivia(Span) \| CTree(u64, Cst[])` |
| `crates/aipl-codegen/src/parse.aipl:109-114` | `struct Parser<K, A> { src: str, toks, trivia, prods }` |

`Parser.src` exists *only* so a span can be resolved, and the resolution itself
is one line — `crates/aipl-codegen/src/parse.aipl:140-145`:

```aipl
fn tok_text<K: variant, A: any>(p: Parser<K, A>, i: u64) -> str {
    match (p.toks[i]) {
        some(t) => p.src[t.span],
        none => "",
    }
}
```

A `SpanStr` is the pair carried in one value: printable directly, with the span
recoverable when a diagnostic needs it.

## What the runtime already gives us

A `str` value is 8 bytes with a two-bit representation tag
(`crates/aipl-codegen/src/lib.rs:120-166`, mirrored in
`crates/aipl-linker/runtime/aipl_runtime.rs`):

| Tag | Representation |
|---|---|
| `0b00` | heap or static — the value is the NUL-terminated content pointer |
| `0b01` | inline — ≤7 content bytes packed into the value itself |
| `0b10` | **view** — `view_obj_ptr \| 0b10` |
| `0b11` | rope — a lazy concat node |

The view object (`lib.rs:150-166`) is already exactly the thing this feature
wants:

```
[0]  refcount
[8]  data_ptr   -> into the owner's content bytes
[16] len        (views are NOT NUL-terminated)
[24] owner      -> the parent str value; inc'd on create, dec'd on free
```

So a view is a `str` that points into another `str` and keeps it alive. Nothing
about the value shape has to change to add `SpanStr` — which is what makes
"guaranteed view" the cheap answer rather than the clever one.

## The two facts that shape the design

**1. A view is an optimization, not a guarantee.** `aipl_str_slice`
(`lib.rs:1201`) copies into an inline value when the slice is ≤7 bytes *or* the
source is inline:

```rust
if n <= 7 || matches!(str_repr(s), StrRepr::Inline) {
    return make_str(if lo < hi { &bytes[lo..hi] } else { &[] });
}
```

Those are precisely the slices a lexer makes — `(`, `+`, `let`. So `SpanStr`
cannot be "whatever slicing happened to produce"; creation needs its own entry
point that *always* builds a view. And an inline source has no stable buffer to
point into at all, so a `SpanStr` over one must first materialize the source to
an owned heap `str` and view *that*. Same for a rope source. `aipl_trim`
(`lib.rs:1018-1050`) takes the identical shortcut and is the model for the code.

**2. The offset is derivable, so nothing needs storing.** `start` is
`data_ptr − root_data_ptr`, and `end` is `start + len`. Views of views chain
through `owner`, so **creation canonicalizes `owner` to the root** — retain the
root, not the intermediate — and then `.span()` is O(1) arithmetic with no chain
walk and no change to a layout that two runtimes must keep byte-for-byte
identical. Sub-slicing a `SpanStr` stays root-relative for free.

This is the whole reason the feature is small: the alternative designs (widening
the view object with explicit root/start fields, or an inline
`{src, start, end}` triple) each pay for something the arithmetic already gives.

## The type

`SpanStr` is a **named type with `str`'s runtime representation** — the pattern
`Error` already establishes (`crates/aipl-syntax/src/lib.rs:1732-1740`, and
`is_str_repr` at `:2071` / `concrete::is_str_repr` at `:2007`, which is what
makes `Error` share every piece of `str` codegen: refcounting, equality,
hashing, printing, concat). `SpanStr` joins that predicate and inherits all of
it. Unlike `Error` it is *importable*, so it also joins
`IMPORTABLE_BUILTIN_TYPES` (`:2301`) and canonicalizes to `__builtin_SpanStr`
through `builtin_type_canonical` (`:2307`), the way `Span` becomes
`__builtin_Span` — the loader rewrite at `crates/aipl-loader/src/lib.rs:529` and
the checker's two acceptance sites (`crates/aipl-mono/src/check.rs:1365,2708`)
already handle that shape.

Assignability is **one-way**: a `SpanStr` is usable everywhere a `str` is
(printing, `==`, `len`, `+`, dict keys, `to_str`), and a `str` is not a
`SpanStr`. That is what keeps `.span()` total — every `SpanStr` came from the
one builtin that guarantees the representation.

Surface:

```aipl
import { Span, SpanStr, span, span_str } from builtins;

fn tok_text<K: variant, A: any>(p: Parser<K, A>, i: u64) -> SpanStr {
    match (p.toks[i]) {
        some(t) => p.src.span_str(t.span),
        none => "".span_str(0..0),
    }
}

print(text);                 // prints the slice; no source in hand
let sp: Span = text.span();  // offsets into the root, for a caret block
```

Two builtins, both declared in `BUILTIN_SIGNATURES` next to
`struct __builtin_Span` (`aipl-syntax/src/lib.rs:1765`) and implemented in
codegen (not in AIPL — they are representation surgery):

```
fn __builtin_span_str(self: str, at: __builtin_Span) -> __builtin_SpanStr
fn __builtin_span(self: __builtin_SpanStr) -> __builtin_Span
```

`span_str` taking a `Span` (rather than two `u64`s) is deliberate: the parser
already has `Span` values, and `Span`-typed indexing is already sugar for
slicing (`crates/aipl-mono/src/lib.rs:5021`), so the two forms stay
recognizably the same operation.

## Semantics to pin down in cases

| Question | Answer |
|---|---|
| What is `.span()` relative to? | The root the view chain bottoms out at — the source passed to `span_str`, not an intermediate slice. |
| Equality | Text equality, inherited from `str`. Two `SpanStr`s with different spans and the same bytes are `==`. |
| Sub-slicing a `SpanStr` | Yields a `str` by default; `span_str` on it yields a `SpanStr` whose span is still root-relative. |
| Out-of-range / inverted spans | Clamped exactly as `aipl_str_slice` clamps today; `start >= end` is the empty `SpanStr`, still a view. |
| Empty and ≤7-byte slices | Still views — that is the guarantee, and the deviation from `str` slicing. |
| Inline or rope source | Materialized to an owned heap `str` first; the `SpanStr` owns that copy, and offsets are into it (identical bytes, so spans still line up with the original source text). |
| Lifetime | A `SpanStr` keeps its whole root alive. Intended — it is the point — but it means one live token pins the file. |
| NUL-termination | Views are not NUL-terminated, and never were: every consumer goes through `str_bytes`/`str_for_each_chunk` and is length-delimited (`aipl_print` at `lib.rs:488`, `read_file_impl` at `:574`). Nothing new to fix; worth one case that passes a `SpanStr` to a file builtin. |

## Costs and honest risks

- **One 32-byte heap object per value**, where `Token { kind, span }` is fully
  inline today. A lexer emitting N tokens goes from zero allocations to N, plus
  refcount traffic on the root. This repo asserts allocation counts per case, so
  a full parser migration would move `--- performance ---` everywhere and is
  exactly why the first change stops at one site.
- **The tag space is full** (four representations, two bits). `SpanStr` needs no
  new tag — it reuses the view — but a *fifth* representation would need a
  different encoding, so this feature quietly spends the last easy slot's worth
  of design room.
- **Two runtimes, byte-for-byte.** The forced-view entry point lands in both
  `crates/aipl-codegen/src/lib.rs` (JIT) and
  `crates/aipl-linker/runtime/aipl_runtime.rs` (no-std AOT staticlib, 4234
  lines, mirror stated at its line 8). Write it twice, identically, or the AOT
  binaries diverge from the JIT.
- **40 `is_str_repr` call sites** (plus 13 for `is_error`, 7 for
  `is_concat_str`) are the blast radius of a new str-shaped type. Most should
  need nothing, because they ask "is this str-shaped?" — but each is a place the
  answer might have been "…and therefore it is exactly `str`".
- **Not a `str` in the checker's eyes**, which will surface as type errors at
  sites that annotate `str` and now receive `SpanStr`. That is the assignability
  rule doing its job; the fix is at the annotation, and the fanout is only
  knowable by running the suite (CLAUDE.md's "implement, run, read the failure
  list").

## Verification

1. **Per-file inner loop** — `cargo run -q -- check crates/aipl-codegen/src/parse.aipl`
   for the converted site.
2. **Representation guarantee** — a case that builds a 1-byte and a 0-byte
   `SpanStr` and asserts `.span()` still round-trips; these are the shapes plain
   slicing would have collapsed to inline.
3. **Nesting** — `span_str` of a `SpanStr`'s sub-slice, asserting the span is
   root-relative rather than parent-relative.
4. **Lifetime** — a `SpanStr` outliving the binding its root came from, with the
   text still correct; the `--- performance ---` section's balanced
   allocations/deallocations is what catches a leak or a double free.
5. **Both runtimes** — the same case run under `aipl run` (JIT) *and* built with
   `aipl build` and executed, since the forced-view code is written twice.
6. **Corpus** — anything touching codegen moves metrics widely; confirm from the
   diff that only metrics moved, no `--- stdout ---`/`--- errors ---` body.
7. **Finish** — `cargo handoff`. `parse.aipl` is in `DOGFOOD_SOURCE_FILES`
   (`crates/aipl-codegen/src/lib.rs:3710`), so Stage 5 pulls in the staged-IR
   flow; Stages 1–4 do not.

## Open questions

- **Does `SpanStr` need its own `source()`?** `.span()` plus the source the
  caller already had covers the parser's needs, and adding `source()` would hand
  back a `str` that pins the root — useful for a standalone diagnostic, easy to
  add later, and impossible to remove.
- **Should `Token` carry a `SpanStr` instead of a `Span`?** That is the payoff
  (it deletes `Parser.src` threading), but it is also the allocation-per-token
  cost above. Decide it against the measured number after Stage 5, not before.
- **Does the formatter need to know the type name?** `aipl fmt` works off the
  token stream, so a type name is just an identifier — but the highlighter's
  builtin-type list is worth a grep before assuming nothing changes.
