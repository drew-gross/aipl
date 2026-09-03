# `SpanStr`: text that knows where it came from

## Status

**Planned, not started — but much smaller than it was.** This plan was written
against the 8-byte tagged `str`, and sequenced behind the 24-byte one
(`STR_REPR.md`) precisely because that change would move the answer. It has
landed, and it moved the answer a long way: three of the four things this
feature was going to build now exist for other reasons.

What `STR_REPR.md` settled, and what each settlement deleted here:

| Then | Now | What it deletes |
|---|---|---|
| Four representations, heap ≠ view, tag in a pointer's low bits | Three — `buffer` / `inline` / `rope` — with `buffer` covering "the whole string" and "a window into it" alike | The whole "guaranteed view" apparatus. A window *is* a `str`. |
| A view is a 32-byte heap object with `{data, len, owner}` | A window is a value: `{base, data, len}`, no object, no allocation | The per-`SpanStr` allocation, and with it the reason this plan stopped at one call site |
| Slicing copies at ≤7 bytes, so a lexer's slices are never views | `Str::slice` on a buffer is a pure value computation at any length (`str24.rs:168`) | The forced-view runtime entry point — slicing already does the right thing |
| The offset is `data_ptr − root_data_ptr`, found by walking the `owner` chain and canonicalizing `owner` to the root at creation | The offset is `data − base`, two words of one value | The chain walk, the canonicalization rule, and the ordering constraint it put on creation |
| Two runtimes to write the entry point in, byte-for-byte identical by hand | `crates/aipl-codegen/src/str24.rs` is one file, `include!`d by the AOT runtime | "Write it twice, identically, or the AOT binaries diverge" |
| Four representations in two bits — the tag space is full | Three tag *values* of 256; `0b11` and six bits spare | The claim that this feature spends the last of the design room |

What is left is genuinely small: a named type, one anchoring helper, two
builtins, and the cases that pin the semantics.

- [x] 1 The type — `SpanStr` in the type system, str-shaped, importable
- [ ] 2 `anchor` in `str24.rs` — the one runtime primitive still missing
- [ ] 3 `span_str` (create) and `span` (recover) builtins
- [ ] 4 Cases: creation, printing, `.span()` round-trip, anchoring, lifetime, edges
- [ ] 5 One real site — `tok_text` in `parse.aipl`

Stages 1–3 remain one indivisible change (the type is unusable without the
builtins); 4 and 5 are what prove it. Stage 2 is now perhaps thirty lines in a
file both runtimes share, where it used to be the bulk of the work.

**Stage 1 landed, and what it left behind.** `SpanStr` is `SPAN_STR` /
`is_span_str` / `is_str_named` in `aipl-syntax` — a named type joining
`is_str_repr` beside `Error`, so it inherits `str`'s refcounting, sizing,
equality, hashing and printing without a layout of its own — plus its entry in
`IMPORTABLE_BUILTIN_TYPES` and the one-way `SpanStr → str` arm in
`check::coerce`. Two things follow from stage 3 not existing yet:

- **Nothing can construct a value**, so no `SpanStr` is reachable from `main`
  and monomorphization (demand-driven from the seeds) never hands one to
  codegen. The four cases in `tests/cases/span_str/` are therefore
  checker-level: they pin the import gate, the one-way rule, and the two
  positions the type is *not* valid in. The runtime behavior is stage 4's to
  prove.
- **`SpanStr` sits exactly where `Error` sits** — valid as a parameter, return
  or local, and not as a struct field, array/optional element, or dict value,
  because neither is in `is_set_elem` or `build_struct_layout`'s allowed set.
  Widening that is what the `Token`-carries-a-`SpanStr` open question below
  actually costs; it is not free, and it is not stage 1. Three diagnostics were
  taught to name a builtin as the user wrote it rather than as
  `__builtin_SpanStr`, and the element-type refusal now says the type is
  str-shaped rather than "unknown".

**Decisions, settled up front:**

| Question | Answer |
|---|---|
| Runtime representation | **A buffer-backed `str`, anchored** — `data − base` is the span. No new tag, no new object, no new layout. |
| Creation surface | **Explicit builtin.** `src.span_str(tok.span)`; ordinary slicing keeps returning `str`. |
| What `.span()` is relative to | **The allocation the text lives in** — which is the source itself, because `span_str` anchors (see "The one new fact"). |
| Name and visibility | **`SpanStr`, importable** — capitalized, alongside `Span` and `ExecResult` in `IMPORTABLE_BUILTIN_TYPES`. |
| Scope of the first change | **Feature + tests + one real site** (`tok_text`). Not because a wider migration is expensive any more — it isn't — but because the measurement that would justify it doesn't exist yet. |

## Context

Unchanged, and still the reason to want this. The parser library stores a span
and looks it up in the source later, so every consumer needs both halves in
hand:

| Where | What it holds |
|---|---|
| `crates/aipl-codegen/src/lexer.aipl:70` | `struct Token<K> { kind: K, span: Span }` |
| `crates/aipl-codegen/src/lexer.aipl:105` | `struct LexError { message: str, span: Span }` |
| `crates/aipl-codegen/src/cst.aipl:33` | `variant Cst = CLeaf(Span) \| CTrivia(Span) \| CTree(u64, Cst[])` |
| `crates/aipl-codegen/src/parse.aipl:109-114` | `struct Parser<K, A> { src: str, toks, trivia, prods }` |

`Parser.src` exists *only* so a span can be resolved, and the resolution is one
line — `crates/aipl-codegen/src/parse.aipl:140-145`:

```aipl
fn tok_text<K: variant, A: any>(p: Parser<K, A>, i: u64) -> str {
    match (p.toks[i]) {
        some(t) => p.src[t.span],
        none => "",
    }
}
```

A `SpanStr` is that pair carried in one value: printable directly, with the span
recoverable when a diagnostic needs it.

## What the runtime already gives us

A `str` is three words (`crates/aipl-codegen/src/str24.rs`, shared verbatim by
the JIT and the AOT runtime):

```
              w0 (bytes 0-7)      w1 (bytes 8-15)     w2 (bytes 16-23)
buffer   00   base: *const u8     data: *const u8     [ len : 56 ][ tag : 8 ]
inline   01   content[0..8]       content[8..16]      [ content[16..22] ][ len:8 ][ tag:8 ]
rope     10   node: *const u8     0                   [ len : 56 ][ tag : 8 ]
         11   — spare —
```

A buffer-backed `str` **is already the thing this feature wanted**: a pointer to
an allocation, a pointer into it, and a length, with the allocation kept alive by
the refcount at `base − 8`. `Str::slice` (`str24.rs:168`) keeps `w0` and offsets
`w1`:

```rust
TAG_BUFFER => Str { w0: self.w0, w1: self.w1 + lo, w2: meta(hi - lo, TAG_BUFFER) },
```

so every slice of a buffer already carries its own provenance, at any length,
with no allocation and no copy. `SpanStr` does not need a representation. It
needs a *type* that promises one.

## The one new fact: `data − base` is allocation-relative

This is the whole remaining design question, and it is the one thing
`STR_REPR.md` asserted a little too breezily when it said `SpanStr` "collapses to
a type marker plus `.span() = data − base`". That arithmetic is right, but what
it is relative to needs pinning down.

`base` is the start of the *allocation*, not of the string value you handed to
`span_str`. For a string that has never been sliced they coincide, and both the
literal path and every fresh buffer make them coincide:

- `emit_const_str` (`aipl-codegen/src/lib.rs`, the static-literal arm) stores the
  same pointer into `base` and `data`.
- `str24::with_capacity` — behind `from_bytes`, `Builder`, `owned_copy`,
  `rope_materialize` — does the same.

So for the case that matters (a source read from a file, a literal of 23 bytes
or more) `data − base` is exactly the offset into the source, and `.span()`
round-trips with the span the lexer produced.

Three sources break that, and they break it *silently*, which is why anchoring
has to be part of creation rather than a documented caveat:

| Source | Why `data − base` is wrong | Fix |
|---|---|---|
| **Inline** (≤22 bytes — most test inputs!) | No allocation at all; `base` is content bytes, not a pointer | Copy once into a buffer |
| **Rope** | `w0` is a node, not a buffer; slicing materializes into the node's cache, so `base` is the cache | Materialize once, then anchor |
| **A window** (`file[a..b]` handed in as the source) | `data − base` is `a + start`, not `start` | Copy once, so `data == base` again |

Hence **`anchor`**, the one runtime primitive this feature still has to add:

```rust
/// A value whose `data` is its allocation's `base`, so `data - base` is an
/// offset into the string itself. Free for a source that was never sliced —
/// which is every string read from a file or built by the runtime — and one
/// copy otherwise.
fn anchor(s: Str) -> Str {
    if s.tag() == TAG_BUFFER && s.w0 == s.w1 { s } else { owned_copy(s) }
}
```

`owned_copy` (`str24.rs:794`) already exists and already returns a buffer via
`Builder::into_buffer`, so this is a predicate and a call. Note the empty string
— the zeroed value, `w0 == w1 == 0`, tag `buffer` — is anchored by that test and
spans `0..0`, which is what "zeroed memory is a valid empty string" requires.

`span_str` then anchors its source, slices, and retains:

```rust
pub(crate) extern "C" fn aipl_span_str(out: *mut Str, s: *const Str, lo: i64, hi: i64)
```

and `span` is arithmetic on the value, with no chain to walk:

```rust
// -> Span { start: data - base, end: start + len }
pub(crate) extern "C" fn aipl_span(out: *mut Span, s: *const Str)
```

**The anchoring copy is hoistable, and the parser should hoist it.** A `Parser`
built over an inline source would otherwise pay a copy per `tok_text` call. Two
options, to decide in Stage 5 against a measured case: expose `anchor` as a
builtin so `Parser` anchors `src` once at construction, or anchor inside the
parser's own constructor. Either way `span_str` keeps the check — it is two
loads and a compare — and only the copy is avoided.

## The type

`SpanStr` is a **named type with `str`'s runtime representation** — the pattern
`Error` already establishes (`crates/aipl-syntax/src/lib.rs:1899` for `is_error`,
`:2244` for the abstract `is_str_repr` and `:2179` for the concrete one, which is
what makes `Error` share every piece of `str` codegen: refcounting, equality,
hashing, printing, concat). `SpanStr` joins that predicate and inherits all of
it. Unlike `Error` it is *importable*, so it also joins
`IMPORTABLE_BUILTIN_TYPES` (`:2478`) and canonicalizes to `__builtin_SpanStr`
through `builtin_type_canonical` (`:2484`), the way `Span` becomes
`__builtin_Span` — the loader rewrite at `crates/aipl-loader/src/lib.rs:525` and
the checker's two acceptance sites (`crates/aipl-mono/src/check.rs:1398,2762`)
already handle that shape.

Assignability is **one-way**: a `SpanStr` is usable everywhere a `str` is
(printing, `==`, `len`, `+`, dict keys, `to_str`), and a `str` is not a
`SpanStr`. That is what keeps `.span()` total — every `SpanStr` came from the one
builtin that anchors. It is also what keeps the *derived* operations honest:
`a + b` on two `SpanStr`s is a `str`, because the concatenation is not a window
into anything.

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
let sp: Span = text.span();  // offsets into the source, for a caret block
```

Two builtins, declared in `BUILTIN_SIGNATURES` next to `struct __builtin_Span`
(`aipl-syntax/src/lib.rs:1926`) and implemented in codegen (not in AIPL — they
are representation surgery):

```
fn __builtin_span_str(self: str, at: __builtin_Span) -> __builtin_SpanStr
fn __builtin_span(self: __builtin_SpanStr) -> __builtin_Span
```

`span_str` taking a `Span` (rather than two `u64`s) is deliberate: the parser
already has `Span` values, and `Span`-typed indexing is already sugar for
slicing (`crates/aipl-mono/src/lib.rs:5082`), so the two forms stay recognizably
the same operation.

**Where a runtime entry point has to be registered** — five places, all of them
one line, and only the first is an implementation:

| File | What goes there |
|---|---|
| `crates/aipl-codegen/src/str24.rs` | the `extern "C"` body, shared by both runtimes |
| `crates/aipl-codegen/src/lib.rs` (~`:4165`) | `jit_builder.symbol("aipl_span_str", …)` |
| `crates/aipl-codegen/src/lib.rs` (~`:7392`) | arity and `Ret::` kind for the builtin call table |
| `crates/aipl-linker/runtime/aipl_runtime.rs` (~`:181`) | the symbol name, for the AOT staticlib's export list |
| `crates/aipl-artifact/src/lib.rs` (~`:388`) | the signature, so a checked-in `.clif` can call it |

## Semantics to pin down in cases

| Question | Answer |
|---|---|
| What is `.span()` relative to? | The source passed to `span_str` — guaranteed by anchoring, not by convention. |
| Equality | Text equality, inherited from `str`. Two `SpanStr`s with different spans and the same bytes are `==`. |
| Sub-slicing a `SpanStr` | Yields a `str`. `span_str` on *that* is anchored afresh, so its span is relative to the sub-slice — **the deliberate change from the old plan**, which canonicalized to a root. Root-relative spans need the root passed explicitly, which is what the parser does anyway. |
| Out-of-range / inverted spans | Clamped exactly as `Str::slice` clamps; `start >= end` is the empty `SpanStr`, still buffer-backed. |
| Empty and 1-byte slices | Free windows, like every other slice. No special case any more. |
| Inline source | Copied once by `anchor`, so offsets are into that copy — identical bytes, so spans line up with the original text. Note this is now the *common* case for small inputs: `from_bytes` packs anything ≤22 bytes inline, where the old runtime's threshold was 7. |
| Rope source | Materialized (once, cached in the node) and then anchored. |
| Lifetime | A `SpanStr` keeps its buffer alive through the refcount at `base − 8`. One live token pins the file — intended, and it is the point. |
| Appending to a `SpanStr` binding | `set s = concat(s, x)` yields a `str`, so the in-place append path (`str24::append_owned`) never widens a window and then reports a span for text the source never contained. Worth a case: the one-way assignability rule is what enforces it. |
| NUL-termination | Never assumed anywhere now — a buffer no longer implies a terminator at all (`STR_REPR.md`, "The calls" §2), and `c_path_of` makes one when libc needs it. One case passing a `SpanStr` to a file builtin still earns its keep. |

## Costs and honest risks

The two costs that dominated this plan are gone. What remains:

- **No allocation per value.** A `SpanStr` over an anchored source is 24 bytes of
  value and one refcount bump — the same as any slice. The old "N tokens, N
  32-byte objects" cost, and the whole reason this plan stopped at one site, no
  longer applies.
- **One copy per unanchored source**, paid at `span_str`. Hoistable to once per
  parser; not hoistable if a caller keeps handing in fresh inline strings. This
  is the one place the feature can surprise someone on allocation counts, and
  this repo asserts those per case — so a case that anchors an inline source
  should assert the copy, deliberately, rather than letting it hide.
- **`str?` is 32 bytes**, so `SpanStr?` is too. `STR_REPR.md`'s Stage 4 (niche
  optionals) is the answer and is not this feature's problem — but `tok_text`
  returning `SpanStr?` instead of a sentinel would pay it today.
- **One runtime, one copy of the code.** `str24.rs` is shared, so the
  divergence hazard the old plan headlined is gone. The registration lines above
  are still per-host, but a missing one is a link error, not silent corruption.
- **~60 `is_str_repr` call sites** (plus 16 for `is_error`, 10 for
  `is_concat_str`, 37 for `is_str_shaped` in codegen at `lib.rs:9023`) are the
  blast radius of a new str-shaped type. Most should need nothing, because they
  ask "is this str-shaped?" — but each is a place the answer might have been
  "…and therefore it is exactly `str`". Bigger than the count in the old plan,
  because the 24-byte change added dispatch sites of its own.
- **Not a `str` in the checker's eyes**, which will surface as type errors at
  sites that annotate `str` and now receive `SpanStr`. That is the assignability
  rule doing its job; the fix is at the annotation, and the fanout is only
  knowable by running the suite (CLAUDE.md's "implement, run, read the failure
  list").
- **Interaction with `STR_REPR.md` Stage 2 (rope-native operations).** That stage
  wants a rope slice landing inside one leaf to become a window into *that leaf*
  — whose `base` is the leaf's buffer, not the source's. Anchoring is what keeps
  `SpanStr` correct through that change, and it is worth a comment at the
  `anchor` definition saying so, because a future reader optimizing ropes will
  otherwise see the `owned_copy` on a rope source as pure waste.

## Verification

1. **Per-file inner loop** — `cargo run -q -- check crates/aipl-codegen/src/parse.aipl`
   for the converted site.
2. **Anchoring** — one case per source shape (buffer-anchored, inline, rope,
   window) asserting `.span()` round-trips against the span that was passed in.
   The window case is the one the old plan had no reason to write and the one
   that fails if `anchor` is skipped.
3. **Rust-level unit tests in `str24.rs`** — `anchor` and the two entry points
   are testable with no compiler in the loop, the way the rest of that file is.
   Cheapest place to pin the edges (empty, zeroed, 22/23-byte boundary).
4. **Nesting** — `span_str` of a `SpanStr`'s sub-slice, asserting the span is
   relative to the sub-slice, per the semantics table. This is a *changed*
   answer; the case exists to stop it drifting back.
5. **Lifetime** — a `SpanStr` outliving the binding its source came from, text
   still correct; the `--- performance ---` section's balanced
   allocations/deallocations is what catches a leak or a double free.
6. **Both runtimes** — the same case under `aipl run` (JIT) and as an
   `aipl build` binary. Less load-bearing than it was (the code is shared) but
   the registration lines are not, so this is what catches a missing one.
7. **Corpus** — confirm from the diff that only metrics moved, no
   `--- stdout ---`/`--- errors ---` body.
8. **Finish** — `cargo handoff`. `parse.aipl` is in `DOGFOOD_SOURCE_FILES`, so
   Stage 5 pulls in the staged-IR flow; Stages 1–4 do not.

## Open questions

- **Should `Token` carry a `SpanStr` instead of a `Span`?** The old answer was
  "decide it against the measured allocation cost"; that cost is now zero for an
  anchored source, and the change deletes `Parser.src` threading entirely. It
  costs 8 bytes per token (24 against `Span`'s 16) and pins the source for as
  long as any token lives — which the parser does anyway. This is now the most
  promising follow-on, not a doubtful one.
- **Where does anchoring live — a builtin, or inside the parser's constructor?**
  Exposing `anchor` makes the cost visible and hoistable by any caller; hiding it
  keeps the surface at two builtins. Decide in Stage 5, with the inline-source
  case in front of you.
- **Does `SpanStr` need its own `source()`?** `.span()` plus the source the
  caller already had covers the parser's needs. Now that a `SpanStr` carries
  `base` and the source is anchored, `source()` is genuinely trivial —
  `{base, base, cap}` is not quite it (capacity is not length), so it would need
  a length the value does not carry. Still easy to add later, still impossible to
  remove.
- **Is a type marker the right mechanism at all, given a spare tag?** A
  `SpanStr` could instead be a fourth *representation* (`0b11`) — a buffer that
  promises it is anchored — which would make `.span()` total without the
  one-way-assignability machinery. It would spend the tag `STR_REPR.md`'s Stage 4
  wants for niche optionals, and it would make every existing str operation
  responsible for preserving or clearing the marker. Recorded because it is the
  obvious alternative, not because it looks better.
- **Does the formatter need to know the type name?** `aipl fmt` works off the
  token stream, so a type name is just an identifier — but the highlighter's
  builtin-type list is worth a grep before assuming nothing changes.
