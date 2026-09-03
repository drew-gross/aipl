# `SpanStr`: text that knows where it came from

## Status

**Done.** `SpanStr` is an ordinary AIPL struct in the lexer library, every
`Token` carries one, and the parser no longer holds the source at all.

```aipl
struct SpanStr { text: str, span: Span }
struct Token<K> { kind: K, text: SpanStr }
```

This is deliberately *not* what the earlier revisions of this plan proposed. It
is worth recording why, because the earlier design was implemented and then
backed out.

### What was tried, and why it was abandoned

The plan through `f6c6361` made `SpanStr` a **language builtin**: a named type
with `str`'s runtime representation (`is_str_repr`, beside `Error`), importable
from `builtins`, one-way assignable to `str`, with two codegen-implemented
builtins (`span_str`, `span`) and a new runtime primitive (`anchor`) in
`str24.rs`. The appeal was size: a `SpanStr` would be 24 bytes — a buffer-backed
`str` already being `{base, data, len}` — with `.span()` recovered as
`data - base`.

It was abandoned because the complexity was not paying for the 16 bytes:

- **`data - base` is relative to the *allocation*, not to the string.** Three
  ordinary sources break that silently — an inline string (≤22 bytes, so most
  test inputs) has no allocation at all, a rope's `w0` is a node, and a window
  (`file[a..b]` handed in as the source) is off by its own start. Correctness
  therefore required `anchor`, a normalizer establishing `data == base` by
  copying when it does not hold. A whole runtime primitive, and a hidden copy,
  existed only to make one subtraction meaningful.
- **The type could not go where it was wanted.** As a named str-repr type it sat
  exactly where `Error` sits: fine as a parameter, return, or local, and *not* a
  struct field, array element, or dict value — because neither `is_set_elem` nor
  `build_struct_layout` admits one. `Token { text: SpanStr }`, the whole point,
  was a compile error. Fixing that meant widening those rules for `Error` too.
- **The blast radius was the language, not the library** — ~60 `is_str_repr`
  sites, five registration points for each runtime entry, and a spare tag that
  `STR_REPR.md`'s Stage 4 wants for niche optionals.

A struct pays 16 bytes per token over a bare span and needs none of it: no new
representation, no anchoring invariant, no compiler change whatsoever. The
lexer library declares it and every consumer is ordinary AIPL.

## The shape

```aipl
// lexer.aipl
struct SpanStr { text: str, span: Span }

pub fn span_str(src: str, at: Span) -> SpanStr { text: src[at], span: at }

struct Token<K> { kind: K, text: SpanStr }
```

**One invariant, held by construction: `text` is exactly `src[span]`.**
`span_str` is the only thing that builds a `SpanStr`, which is what makes that
true rather than merely intended. Consumers rely on it in both directions — a
diagnostic renders `span` against a source it already has, and a parser reads
`text` without needing the source at all.

Slicing clamps, so an out-of-range or inverted `at` yields empty text; `span` is
kept verbatim either way, since it is what a caret block underlines.

The one place the invariant needed care is `finalize_template`: a template
segment's *finalized* text (post-dedent, say) is not `src[span]`. That text
builds the token's **kind**; the token's `text` is still the source slice. The
function grew a `src` parameter to keep it so.

## What it bought

`Parser` no longer carries the source:

```aipl
struct Parser<K: variant, A: any> {     struct Parser<K: variant, A: any> {
    src: str,                      →        toks: Token<K>[],
    toks: Token<K>[],                       trivia: Token<K>[],
    trivia: Token<K>[],                     prods: Production<K, A>[],
    prods: Production<K, A>[],              end: u64,
}                                       }
```

and `tok_text` stops being a lookup:

```aipl
some(t) => p.src[t.span],   →   some(t) => t.text.text,
```

`end` is all that remains of the source — one byte past its last, for the empty
span `error_at` reports at end of input. Matching a rule spelling now reads the
token and nothing else.

## Cost, measured

| case | before | after |
|---|---|---|
| `lexer.aipl` instructions | 287,920 | 300,378 |
| `lexer.aipl` binary size | 1,912,408 | 1,947,664 |
| `parse.aipl` instructions | 417,952 | 435,950 |
| `parse.aipl` binary size | 303,392 | 305,416 |

Roughly +4% instructions in the lexer, +4% in the parser, and one extra
allocation in `lexer.aipl`'s test — the cost of copying 24 bytes of `str` per
token instead of nothing. Every token now retains the source buffer it slices
from, which is a refcount bump per token, not a copy: `Str::slice` on a buffer
is a pure value computation (`STR_REPR.md`). Corpus-wide only metrics moved; no
`--- stdout ---` or `--- errors ---` body changed.

## The bootstrap this needed, for next time

`lexer.aipl` and `lex_aipl.aipl` are in `DOGFOOD_SOURCE_FILES`, and `lex_aipl`
is an FFI dogfood entry whose result the Rust side unmarshals by field name. So
changing `Token`'s shape breaks a cycle the gate cannot bootstrap on its own:

> Regenerating the `.clif` artifacts requires parsing AIPL source, which goes
> through the dogfooded lexer, whose result the *new* Rust reader can no longer
> unmarshal from the *old* artifact.

This is a second flavor of the ordering problem CLAUDE.md documents for the
formatter's grammar, and the escape is the same staged-IR flow plus one extra
step:

1. Teach the Rust unmarshaler (`marshal_lex`) to read **either** shape — a
   clearly-marked, temporary bootstrap shim.
2. `fill_staged_ir`, `validate_staged_ir`.
3. Full suite under `AIPL_DOGFOOD_IR` / `AIPL_FMT_IR` pointing at the staged
   files.
4. `promote_staged_ir`.
5. **Remove the shim** — the live artifact now speaks the new shape, so the
   compatibility arm is dead code, and leaving it in would hide exactly the
   drift it was added to survive.
6. `cargo handoff`.

Three Rust consumers of the token shape had to move, and only the first is
obvious: `marshal_lex` in `aipl-codegen/src/lib.rs`, `aipl_dump` in
`tests/suites/lexer_dogfood.rs`, and the hard-coded `sanity_check` expectations
in `tests/suites/dogfood_ir.rs`. The last is what makes step 1 necessary:
`sanity_check` serves both the live and the staged artifact, so the moment its
expectation is updated the live artifact fails discovery — and promotion sits
behind discovery.

## Testing

Expectations slice their text from the source rather than spelling it out —
`tok(src, kind, span)` in `lexer.aipl`, `atok(src, kind, span)` in
`lex_aipl.aipl`, and the `tok` closure in `dogfood_ir.rs`. A test therefore
cannot claim text the source does not hold at that span, which is the one thing
a hand-written `SpanStr` could get wrong while still looking right. `span_str`
carries its own `.test` block covering the invariant, both empty ends, and
out-of-range and inverted spans.

## Follow-ons

- **`LexError` could carry a `SpanStr`** instead of `{ message, span }`. It
  currently reports a location without text, and some of its spans (an
  unterminated literal running to end of input) are not text anyone wants to
  print. Not obviously worth it.
- **`Cst` still carries bare `Span`s** (`CLeaf`, `CTrivia`), and `lower` still
  takes `src`. That is the remaining source-threading in the parser, and the
  natural next step if it starts to hurt.
- **The Rust side drops the text** it receives over the FFI (`LexedToken` keeps
  kind and span). It holds the source, so `src[span]` is free there — but that
  means the FFI now marshals a string per token that is immediately discarded.
  Worth measuring if lexing shows up in a profile.
