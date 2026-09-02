# A 24-byte `str`

## Status

**Done through Stage 2.** `str` is a 24-byte composite in both runtimes, the
8-byte tagged representation is gone, and the artifacts speak the one ABI. There
is no switch and no second runtime; 932 tests pass.

Stages 1–5 below are follow-on programs the new layout unlocks, each separately
justifiable rather than part of a single push.

- [ ] 1 `SpanStr` — expose the window a slice already is (`SPAN_STR.md`). Slicing
      itself is free now; what is missing is the language-level type.
- [ ] 2 In-place mutation and growth — the ownership rule plus the capacity word.
      Today `push`/`extend` on a `str` rebuild the buffer every time.
- [ ] 3 Rope-native operations — concat is already O(1); slicing a rope still
      materializes it, and `map`/`filter` over one could stream.
- [ ] 4 Converge arrays onto the same model — inline and view arrays, one block
      header, one body of code.
- [ ] 5 Niche-filled optionals — `T?` in a spare bit of `T`, no separate tag
      word. `str?` is 32 bytes today.

**A standing principle for all of it:** anywhere strings and arrays can share a
representation, a header, or a body of code, they should. They are the same
thing — a refcounted buffer, a window into it, and a length — and Stage 4 is
where that stops being a coincidence.

### What the change cost, measured

The metric trade, on the canaries the plan named (instructions executed, before
→ after):

| case | before | after |
|---|---|---|
| `grammar_calc.aipl` | 1,169,584 | 1,254,004 |
| `walker.aipl` | 3,075,003 | 3,024,877 |

Roughly a wash, and both numbers still carry known debt the follow-on stages
collect: `for (let c : s)` takes a runtime call per byte (the tagged iterator's
inlined fast path read its own cursor layout and has no counterpart yet), and
`to_str` allocates a buffer for short results instead of packing them inline.

## The shape

A `str` used to be one 8-byte word: a tagged pointer, four representations, tag
in the low two bits, every payload beyond the tag behind an indirection. It is
now **three words, with no indirection for the common cases**
(`crates/aipl-codegen/src/str24.rs`).

```
              w0 (bytes 0-7)      w1 (bytes 8-15)     w2 (bytes 16-23)
buffer   00   base: *const u8     data: *const u8     [ len : 56 ][ tag : 8 ]
inline   01   content[0..8]       content[8..16]      [ content[16..22] ][ len:8 ][ tag:8 ]
rope     10   node: *const u8     0                   [ len : 56 ][ tag : 8 ]
         11   — spare —
```

And the block a buffer points into carries **no length at all**:

```
[cap: i64][refcount: i64][content bytes ...]
                          ^ base; `data` points anywhere in [base, base + cap]
```

`refcount` stays at `base - 8`, shared with arrays exactly as today, so
`header_of`/`inc`/`dec` are untouched. The word at `base - 16` changes meaning
from "how many content bytes this allocation holds" to **capacity** — how many
it *can* hold. The prefix stays 16 bytes (`STR_HEADER_SIZE`), so this is a
change of meaning, not of size.

Length is the value's business, not the allocation's. That is what makes the
next two sections work.

## The calls

### 1. The tag goes in the *top* byte, not on a pointer's low bits

Your sketch had both "a pointer last so the tag bits thing still works" and
"22 bytes of data, 1 byte of length, 1 byte of tag" — those two can't both hold:
a pointer's spare bits are its *low* bits (byte 16), and the inline tag byte is
byte 23.

Putting the tag in **byte 23 for every representation** buys more than
consistency:

- **Pointers stay untagged.** `base` and `data` are dereferenced without masking
  — today every view/rope access pays a `& !0b111` first.
- **Classification is one shift-and-compare** on `w2`, a register the code has
  usually loaded anyway for the length.
- **Inline content stays contiguous** (bytes 0..21), so materializing it is one
  `memcpy`, not two around a hole.
- Nothing depends on pointer alignment or on high pointer bits being free — the
  latter is exactly the assumption that breaks on a platform with tagged
  pointers.

Cost: `len` is capped at 2^56 bytes, and inline is 22 bytes rather than the 23
low-bit tagging could squeeze.

**Sub-decision:** the length field is `w2`'s low 56 bits for buffer and rope, but
inline needs bits 0..47 for content, so its length sits in byte 22 — making
`len()` a two-way select on the tag rather than one mask. The alternative is to
shrink inline content to 16 bytes (`w0`, `w1` only) and put every
representation's length in the same 56-bit field, making `len()` branchless. Keep
22 bytes: the select is on an already-loaded register, and 22 covers
`__builtin_count_if`-length identifiers where 16 does not.

### 2. Heap and view unify completely — there is no "heap" case

Not "same shape, different tag" — **one representation**. A string is a base, a
window into it, and a length; whether the window happens to cover the whole
allocation is not a fact anything needs to branch on:

- **Slicing stops allocating and stops copying.** `s[lo..hi]` is
  `{base, data + lo, hi - lo}` plus one `inc` on the base — a pure value
  computation, where it used to be a 32-byte view allocation or a byte copy for
  a short result.
- **It deletes the "a view is an optimization, not a guarantee" wart** that
  `SPAN_STR.md` is built around. Every buffer-backed `str` carries its own
  provenance, so `SpanStr` collapses to a type marker plus `.span() = data - base`.
- Three live representations instead of four leaves a **spare tag**, where the
  current design is at capacity — and Stage 5 has a use for it.

Dropping the block's length word is what completes the unification. With a
length in the header there are two candidate answers to "how long is this
string", and every operation has to know which one it means. With only capacity
in the header, the value's `len` is the only length there is, and the header
answers a different question entirely: **how much room is there?**

```
spare = (base + cap) - (data + len)
```

That is the whole test for appending in place — no "is this the whole
allocation?" comparison, because Stage 2's ownership rule already establishes
that nothing else can see the bytes past `data + len`.

One thing this gives up: a buffer no longer implies a trailing NUL, so a
`str` handed to C must either have a spare byte to write one into or be
materialized. Every consumer is length-delimited (`Str::bytes`,
`for_each_chunk`, the file paths), so this costs a documented rule rather than
working code — and `c_path_of` in the AOT runtime is where the terminator gets
made when libc needs one.

### 3. Ropes stay — short-term port now, investment later

The rope carries 24-byte children:

```
node: [refcount: i64][len: i64][cache: 24][left: 24][right: 24]   // 88 bytes
value: { node, 0, len | ROPE<<56 }
```

The long-term plan is **not** to delete it. O(1) concat is a real result, most
ropes in a pipeline are consumed by something that streams (printing already
does, via `str_for_each_chunk`), and the operations that currently force
materialization mostly don't have to. That is Stage 3.

The capacity word does **not** replace ropes; the two answer different questions
and coexist:

| Situation | Answer |
|---|---|
| Appending to a string you own, room to spare | write into the spare capacity, bump `len` (Stage 2) |
| Appending to a string you own, out of room | reallocate with growth, copy once — amortized O(1) |
| Concatenating values someone else may hold | build a rope node — O(1), no copy, no ownership question |

So a builder loop gets the array-style growth path, and general `a + b` keeps the
lazy one. Monomorphization's `ConcatStr` pseudo-type and its `$c{i}` instances
(`aipl-syntax`) stay as they are.

### 4. ABI: three scalar arguments, and results through an out pointer

`cl_type_of` returns `I64` for every type — `str` is one
SSA value everywhere. The spike (`crates/aipl-codegen/src/abi_spike.rs`, run
against cranelift 0.134) settles how it widens, and it is **not** what the plan
first assumed:

| Question | Answer |
|---|---|
| Three `i64` returns? | **Refused on x86-64.** `Unsupported feature: Too many return values to fit in registers. Use a StructReturn argument instead.` It *works* on the aarch64 host — exactly the trap a host-only spike would have walked into. |
| Three scalar params? | Works everywhere, including eight-param signatures (two `str`s plus extras) on x86-64, where cranelift spills past the register budget for us. |
| Result through a leading out pointer? | Works everywhere, and is how the compiler already returned composites — "a normal *leading* i64 param, and returns nothing". |
| Cranelift's formal `StructReturn`? | Works **only** as a parameter with no matching return — 0.134 rejects an explicit `StructReturn` return value outright, so the C convention of handing the pointer back is not expressible. In practice identical to the plain out pointer. |
| 24-byte struct across the Rust boundary? | Round-trips correctly by pointer, and three scalar words pass to a Rust `extern "C" fn(i64, i64, i64)` unchanged. |

**Portability is deferred** — this project targets an aarch64 Mac for now — so
the x86-64 refusal is not a blocker today: three-word returns *work* on the host,
and they beat a memory round trip. The reason to write the out-pointer form
anyway is that it costs nothing to choose now and is expensive to retrofit. The
shape lives in `lower_import_sig` alone, but the checked-in `.clif` artifacts
carry their signatures with them, so changing it later means regenerating every
artifact and re-measuring the corpus. Choose multi-value returns only with that
bill in mind.

Those same artifacts are also why a *per-target* signature is not an option when
portability does come back: one `.clif` text is compiled for whichever host loads
it.

**The shape, everywhere, inside and out:** a `str` argument passes as three
scalar words; a `str` result is written through a leading out pointer.

```rust
#[repr(C)]
struct AiplStr { base: *const u8, data: *const u8, meta: u64 }  // meta = len | tag<<56
```

defined identically in `crates/aipl-codegen/src/lib.rs` and
`crates/aipl-linker/runtime/aipl_runtime.rs`, which are kept byte-for-byte
identical (that file's own header says so, at its line 8).

The spike stays in the tree: it is the executable form of this decision, and
re-running it is how a cranelift bump gets re-checked. It
costs a `cranelift-codegen` dev-dependency with `all-arch` (the shipped build
carries only the host backend), which is why it goes away afterwards.

## One copy of the layout, not two

The plan said "write it twice, identically, or the AOT binaries diverge from the
JIT". It is not written twice: `crates/aipl-codegen/src/str24.rs` is an ordinary module
in the JIT runtime **and** `include!`d by
`crates/aipl-linker/runtime/aipl_runtime.rs`, so both compile the same text.

For the layout specifically this is worth more than it costs. A divergence in the
tag encoding or a field offset is not a failing test — it is silent memory
corruption in AOT binaries only, which is the hardest kind of bug this repo could
have. Sharing makes that class unrepresentable, and it is the standing principle
(strings and arrays, JIT and AOT: one implementation wherever the shape must
agree) applied to the place it matters most.

What made it possible is that the two runtimes already differ in exactly two
ways, both small:

- **Allocation.** The AOT runtime funnels everything through
  `rt_alloc`/`rt_free` so its instrumented build can tally
  `--- performance ---` counts. The shared file calls `super::rt_alloc` /
  `super::rt_free`, and each host supplies them — the JIT's forward to libc, the
  AOT's keep their counters. Same names, same signatures, no `cfg` in the shared
  file.
- **I/O.** `std::io`/`std::fs` on one side, `libc` on the other. That is the only
  thing `str24_host.rs` holds.

Two constraints the shared file therefore carries, both stated at its top:
`no_std` (no `Vec`, no `std::alloc` — a rope materializes into its own node cache
and `Builder` grows through `rt_alloc`), and **no inner attributes** — an
`include!`d file may carry neither `//!` docs nor `#![allow]`, so its prose is
plain `//` comments and the `allow` sits on each `mod` declaration.

The build wiring: `crates/aipl-linker/build.rs` gained a `rerun-if-changed` for
the shared path, so editing it rebuilds the staticlib. Verified by injecting a
type error into the shared file and watching the AOT build fail.

## The constraint that shapes Stage 2: `refcount == 1` is not ownership

The compiler **elides retains for borrow-only parameters**
(`inspect_only_params`, in `aipl-mono`): when a callee provably only inspects
an argument, the caller's retain and the callee's release *both* disappear. So
inside such a callee, a buffer can show `refcount == 1` while the caller is still
holding it. The same is true of the non-retaining borrows the mut-binding model
allows — CLAUDE.md: *"a non-retaining borrow (`let alias = a`) of any version
stays valid until the scope where that version was created exits"*.

A bare runtime `refcount == 1` test therefore does **not** mean "nobody else can
see this". Mutating on that basis would corrupt the caller's value, and only in
the configurations where retain elision fired — the worst possible failure mode
to debug.

The rule that *is* sound, and that the compiler already has the machinery for:

> In-place mutation requires **static ownership** — an `owned` parameter or a
> recognized owned temporary (`move_owned_temp` / `owned_temp_since`,
> the analysis behind the existing `$own` instances) — and
> `refcount == 1` as a *dynamic refinement* of it, never as a substitute.

That is exactly how arrays already do in-place `map`/`filter` (`__filter21$own0`
in the perf sections; `tests/cases/lambdas/filter_in_place.aipl`). Strings get
the same treatment, with one addition the arrays didn't need: a `str` may be a
window into a larger buffer, so writing past `data + len` also needs the capacity
test above.

## What it buys

- **Slices are free** — no allocation, no copy, at any length.
- **Strings ≤22 bytes never allocate**, against ≤7 today. Most identifiers,
  keywords, punctuation, dict keys, and short literals stop touching the heap.
- **`len` needs no load** for buffer and rope strings; today a heap length is a
  load at `base - 16` and a view length is a load from the view object.
- **One less indirection everywhere** — a view read is currently: mask the tag,
  load the object, load `data`, load `len`. It becomes: read `w1`, read `w2`.
- **In-place mutation and growth become expressible**, on the ownership rule
  above.
- **`SpanStr` nearly disappears as a feature** — see `SPAN_STR.md`.

## What it costs

- **Every `str` slot triples** — 24 bytes in arrays, struct fields, dict/set
  entries. An array of a thousand short strings goes from 8 KB plus a thousand
  heap blocks to 24 KB and *zero*; an array of a thousand long strings pays
  16 KB more for nothing.
- **`str?` goes 16 → 32 bytes** under the current uniform `{tag, payload}`
  optional layout. Stage 5 is the answer to this one and generalizes past `str`.
- **Copying a `str` is three words and a refcount**, not one word and a
  refcount.
- **Codegen churn is enormous** — `str` moves from the scalar class to the
  composite class in a file that mentions `types::I64` 500 times and is 19,514
  lines long, and the runtime pair is 4,234 lines that must stay identical.
- **Both `.clif` artifacts are invalidated**, and the old ones cannot run
  against the new runtime at all (see Bootstrap).
- **Corpus-wide metric churn**: `instructions executed`, `binary size`, and
  `allocations` all move in every case.

## Stage 3 — rope-native operations

Ropes earn their keep only if the operations a rope commonly meets don't force
it flat. The materialize path stays as the fallback; the work is to reach it
less often.

- **Streaming already works** (`str_for_each_chunk`) and printing uses it.
  Extend it to equality, comparison, hashing, `contains`/`starts_with`/
  `ends_with`, and iteration — all of which can walk two chunk streams.
- **`map` and `filter` over a rope** produce a rope of transformed leaves, one
  leaf at a time, never building the input flat. Each output leaf is a fresh
  buffer; the structure is preserved.
- **Slicing a rope** that lands inside one leaf is a *view of that leaf* — free,
  and it does not retain the whole tree. A slice spanning leaves either builds a
  smaller rope or materializes just the span.
- **`len` is already O(1)** (stored at the node).
- Worth measuring first: how deep ropes actually get in this corpus, and whether
  a depth cap that materializes beyond it beats the pointer chasing.

The payoff is that `a + b` stays O(1) *and* the result stays cheap to consume,
which is what makes laziness worth its complexity.

## Stage 4 — converge arrays onto the same model

Arrays are a single pointer to `[refcount][len][cap][elements]`, so the two
families now differ for no good reason: strings carry
`{base, data, len}` with inline and view forms; arrays carry a pointer with the
length in the block.

The target is one model:

- The same 24-byte value shape, tag in the same byte, so **view arrays** (a
  window into a shared buffer — free `xs[a..b]`, no copy) and **inline arrays**
  (a few elements packed into 22 bytes, no allocation) fall out the same way.
- The same block header `[cap][refcount][elements]`, so growth, uniqueness, and
  in-place append are one implementation rather than two.
- `char[]`, which is already str-shaped (`is_char_array`), stops being a special
  case and becomes the ordinary intersection of the two.

Element size is the wrinkle: strings have byte elements, arrays are
`elem_size`-general with a bit-packed mode. The shared code has to be
element-size-parameterized where the string version can hardcode 1.

## Stage 5 — niche-filled optionals

`T?` is `{tag: i64, payload}` today, which is what makes `str?` 32 bytes. But any
type with an unused bit can host its own `none`:

| Type | Niche |
|---|---|
| `str` | the spare tag value `0b11`, or any of the 6 unused bits in the tag byte |
| `bool` | 7 unused bits |
| `char` | the high 56 bits of its slot |
| pointer-shaped values | alignment bits |
| narrow ints (`i8`…`i32`) | the unused high bits of the 64-bit slot |

So `str?` is 24 bytes, `bool?` and `char?` are 8, and only types with no niche
(`i64`, `u64`, a fully-packed struct) keep the extra tag word.

The work is that "optional" stops being one layout and becomes a per-type
question: a niche descriptor per type, and every site that builds, tests, or
reads an optional — including `read_ffi_optional` and the sret paths — goes
through it instead of assuming `{tag, payload}`. Worth doing after Stage 4, when
there is one value model to describe niches against rather than two.

## Verification

1. **The spike** — `cargo test -p aipl-codegen --lib abi_spike`, seven checks
   pinning the ABI decision above across x86-64 (both ABIs) and aarch64. Re-run
   it after any cranelift bump.
2. **Representation cases** — one per arm and per boundary: empty, 1 byte, 22
   bytes, 23 bytes (the inline/buffer boundary), a slice of each, a slice of a
   slice, a rope of each, and a rope slice that lands inside one leaf.
3. **Zeroed memory is a valid empty string** — sret buffers and fresh array slots
   are zero-filled, which reads as `buffer` with a null base and zero length;
   `inc`/`dec` must no-op on it.
4. **Both runtimes** — every representation case run under `aipl run` (JIT) and
   again as an `aipl build` binary, since the layout is written twice.
5. **Balanced allocations** — every case's `--- performance ---` asserts
   allocations == deallocations; that is the leak/double-free tripwire for a
   refcount protocol whose shape just changed.
6. **Canaries for the metric story** — `grammar_calc.aipl`, `walker.aipl`,
   `parse.aipl` and the lexer cases, all slice- and token-heavy, so a change
   that claims to help slicing should show there. Current numbers are in the
   status table above; measure before and after, so a trade is visible rather
   than assumed.

## Open questions

- **Is 22-byte inline the right trade against a branchless `len`?** Decidable by
  measuring how many strings in the corpus fall in 17..22 bytes.
- **Should a `str` ever live in registers rather than a stack slot?** Settled
  for now as a composite: it lives in memory and travels as its address, which
  is what let the existing composite machinery carry it. The one place the
  register form won is a tail call, where the callee's frame outlives the
  caller's — `tail_passes_str_by_value` passes the three words there. Whether
  that is worth extending to ordinary calls is unmeasured.
- **Does a buffer need a "NUL is present" bit?** The tag byte has six spare bits.
  Only worth it if C handoff turns out to be hot; the capacity test already
  answers "can I write one?".
