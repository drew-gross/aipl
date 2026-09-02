# A 24-byte `str`

## Status

Design, not started. The largest change in the repo to date: it moves `str` from
a scalar to a composite, rewrites both runtimes, and invalidates every checked-in
`.clif`. Stages 5–7 are follow-on programs it unlocks rather than parts of the
flip.

- [x] **Spike** — the ABI questions, answered against cranelift 0.134 on both target families (`crates/aipl-codegen/src/abi_spike.rs`)
- [ ] 0 Preparation — funnel every "a `str` is one `i64`" assumption through helpers, green at each step
  - [x] the runtime ABI table names kinds (`Abi`/`Ret`) instead of counting arity
  - [x] every runtime call goes through `Builtins::call`/`call_void` (all 94 sites)
  - [x] `value_slot` — a spilled value is sized by its type, not assumed to be a word
  - [ ] audit the remaining by-value spills and `mut` binding slots
- [ ] 1 The flip — new layout in both runtimes + codegen + IR bootstrap
  - [x] the layout itself, proven on its own (`crates/aipl-codegen/src/str24.rs`, staged dead code)
  - [x] the `str` surface: streaming, compare/hash/search, trim, builder, split/join
  - [x] iteration (`Iter`) and I/O (`print`, file read/write), streaming throughout
  - [x] shared with the AOT runtime instead of mirrored — one file, both compile it
  - [x] the `aipl2_*` entry points — the second ABI the switch is tested against
  - [ ] the switch: `str` joins `is_composite`/`elem_size_of`/`sret_size`, literals, the rc arm, FFI
  - [ ] regenerate the artifacts against `aipl2_*`, refill the corpus, delete the old half
- [ ] 2 Storage fallout — `str[]`, dict/set keys, struct fields, optionals at 24-byte slots
- [ ] 3 Free slicing, and `SpanStr` falling out of it
- [ ] 4 In-place mutation and growth — the ownership rule plus the capacity word
- [ ] 5 Rope-native operations — keep concat O(1) and do more work without materializing
- [ ] 6 Converge arrays onto the same model — inline and view arrays, one block header, one body of code
- [ ] 7 Niche-filled optionals — `T?` in a spare bit of `T`, no separate tag word

Stage 0 is most of the work and can land incrementally. **The switch inside
Stage 1 is atomic**, but the layout it switches to need not be: `str24.rs`
implements and tests it as staged dead code, with no compiler in the loop, so the
irreversible commit is only the wiring. Each of 3–7 is separately justifiable
once 1–2 are green.

**A standing principle for all of it:** anywhere strings and arrays can share a
representation, a header, or a body of code, they should. They are the same
thing — a refcounted buffer, a window into it, and a length — and Stage 6 is
where that stops being a coincidence.

## The shape

Today a `str` is one 8-byte word: a tagged pointer, four representations, tag in
the low two bits (`crates/aipl-codegen/src/lib.rs:120-259`). Every payload
beyond the tag lives behind an indirection.

Proposed: **three words, no indirection for the common cases.**

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

- **Slicing stops allocating and stops copying.** `s[lo..hi]` becomes
  `{base, data + lo, hi - lo}` plus one `inc` on the base — a pure value
  computation. Today it is a 32-byte view allocation, or a byte copy when the
  result is ≤7 bytes (`aipl_str_slice`, `lib.rs:1201`).
- **It deletes the "a view is an optimization, not a guarantee" wart** that
  `SPAN_STR.md` is built around. Every buffer-backed `str` carries its own
  provenance, so `SpanStr` collapses to a type marker plus `.span() = data - base`.
- Three live representations instead of four leaves a **spare tag**, where the
  current design is at capacity — and Stage 7 has a use for it.

Dropping the block's length word is what completes the unification. With a
length in the header there are two candidate answers to "how long is this
string", and every operation has to know which one it means. With only capacity
in the header, the value's `len` is the only length there is, and the header
answers a different question entirely: **how much room is there?**

```
spare = (base + cap) - (data + len)
```

That is the whole test for appending in place — no "is this the whole
allocation?" comparison, because Stage 4's ownership rule already establishes
that nothing else can see the bytes past `data + len`.

One thing this gives up: a buffer no longer implies a trailing NUL, so a
`str` handed to C must either have a spare byte to write one into or be
materialized. Every consumer today is already length-delimited (`str_bytes`,
`str_for_each_chunk`, `read_file_impl` at `lib.rs:574`), so this costs a
documented rule rather than working code.

### 3. Ropes stay — short-term port now, investment later

Stage 1 ports the rope mechanically; its children are 24 bytes each:

```
node: [refcount: i64][len: i64][cache: 24][left: 24][right: 24]   // 88 bytes
value: { node, 0, len | ROPE<<56 }
```

The long-term plan is **not** to delete it. O(1) concat is a real result, most
ropes in a pipeline are consumed by something that streams (printing already
does, via `str_for_each_chunk`), and the operations that currently force
materialization mostly don't have to. That is Stage 5.

The capacity word does **not** replace ropes; the two answer different questions
and coexist:

| Situation | Answer |
|---|---|
| Appending to a string you own, room to spare | write into the spare capacity, bump `len` (Stage 4) |
| Appending to a string you own, out of room | reallocate with growth, copy once — amortized O(1) |
| Concatenating values someone else may hold | build a rope node — O(1), no copy, no ownership question |

So a builder loop gets the array-style growth path, and general `a + b` keeps the
lazy one. Monomorphization's `ConcatStr` pseudo-type and its `$c{i}` instances
(`crates/aipl-syntax/src/lib.rs:1951-1965`) stay as they are.

### 4. ABI: three scalar arguments, and results through an out pointer

`cl_type_of` returns `I64` for every type today (`lib.rs:7933`) — `str` is one
SSA value everywhere. The spike (`crates/aipl-codegen/src/abi_spike.rs`, run
against cranelift 0.134) settles how it widens, and it is **not** what the plan
first assumed:

| Question | Answer |
|---|---|
| Three `i64` returns? | **Refused on x86-64.** `Unsupported feature: Too many return values to fit in registers. Use a StructReturn argument instead.` It *works* on the aarch64 host — exactly the trap a host-only spike would have walked into. |
| Three scalar params? | Works everywhere, including eight-param signatures (two `str`s plus extras) on x86-64, where cranelift spills past the register budget for us. |
| Result through a leading out pointer? | Works everywhere, and is already how the compiler returns composites (`lib.rs:6034-6100`: "a normal *leading* i64 param — and returns nothing"). |
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

The spike stays in the tree until Stage 1 lands: it is the executable form of
this decision, and re-running it is how a cranelift bump gets re-checked. It
costs a `cranelift-codegen` dev-dependency with `all-arch` (the shipped build
carries only the host backend), which is why it goes away afterwards.

## The flip is smaller than it looks: `str` becomes a composite

Stage 0's audit turned up the single most useful fact about this change. Codegen
already has two classes of value — scalars, which are one `i64` register, and
**composites** (structs, optionals, results, variants), which live in memory and
travel as *addresses* — and the fork between them is decided in three small
places:

| Predicate | Where | Today | After |
|---|---|---|---|
| `is_composite` | `lib.rs:9583`, 13 call sites | optional/result/unboxed struct | …plus `is_str_repr` |
| `elem_size_of` | `lib.rs:11685`, 29 call sites | `_ => 8` | a `str` arm returning 24 |
| `sret_size` | `lib.rs:19490`, feeds `build_signature` | `_ => None` | `Some(24)` for `str` |

Everything else follows from those three, because the composite machinery is
already written: `component` hands back an address instead of loading a word,
`store_array_elem` routes to `copy_composite`, `build_signature` prepends an
sret pointer for the return and **keeps passing one `i64` per parameter** —
which for a composite is its address. So the AIPL-level ABI needs *no change at
all* for `str` parameters.

That also settles the runtime ABI more cheaply than the spike's framing
suggested. If a `str` value lives in memory as a composite, its **address** is
what a call site has in hand, so `Abi::Str` should lower to one pointer word
rather than three scalars, and `Ret::Str` to a leading out pointer — exactly the
shape `abi_spike::q3` verified round-trips. The three-scalar form stays
available (`q4`) for a later by-value fast path, but it is not the default.

**What stays genuinely `str`-specific**, and is the real work of Stage 1:

- **Literals.** A `str` constant is a pointer to a data symbol today; it becomes
  a 24-byte constant (base, data, `len | tag`) whose *address* is the value —
  which is to say, exactly what a struct constant already is.
- **Refcounting.** `emit_rc_w`'s `is_str_repr` arm (`lib.rs:10120`) passes the
  value straight to `aipl_inc`/`aipl_dec`; it must instead load the base word
  out of the value and pass that. One arm, and the only place it lives.
- **The runtime's own internals** — every `aipl_str_*` function, rewritten
  against the new layout, twice (JIT and AOT).
- **FFI marshaling** (`lib.rs:6034-6100`), where 24 bytes cross into Rust.

The cost of this shape is that a `str` stops living in registers. That is the
trade three-scalar passing would have bought, and it is worth measuring at Stage
1 rather than assuming: the canaries below are slice- and token-heavy, which is
where memory traffic would show up first.

## One copy of the layout, not two

The plan said "write it twice, identically, or the AOT binaries diverge from the
JIT". Stage 1 does not: `crates/aipl-codegen/src/str24.rs` is an ordinary module
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

## The constraint that shapes Stage 4: `refcount == 1` is not ownership

The compiler **elides retains for borrow-only parameters**
(`crates/aipl-mono/src/lib.rs:8464-8500`): when a callee provably only inspects
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
> `lib.rs:9329-9335`, the analysis behind the existing `$own` instances) — and
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
  optional layout. Stage 7 is the answer to this one and generalizes past `str`.
- **Copying a `str` is three words and a refcount**, not one word and a
  refcount.
- **Codegen churn is enormous** — `str` moves from the scalar class to the
  composite class in a file that mentions `types::I64` 500 times and is 19,514
  lines long, and the runtime pair is 4,234 lines that must stay identical.
- **Both `.clif` artifacts are invalidated**, and the old ones cannot run
  against the new runtime at all (see Bootstrap).
- **Corpus-wide metric churn**: `instructions executed`, `binary size`, and
  `allocations` all move in every case.

## Type-system and storage fallout (Stage 2)

The 8-byte assumption is written into the type rules, not just the codegen:

| Gate | Where | Change |
|---|---|---|
| `is_array_elem` | `crates/aipl-syntax/src/lib.rs:2114` | must admit a 24-byte element; the array runtime is already `elem_size`-general (`alloc_array`, `cap_bytes_for`, `ELEM_BITPACKED`) |
| `is_set_elem` | `:2129` | 24-byte slots; hashing/equality are already content-based |
| `is_dict_key` | `:2141` | same |
| struct fields / variant payloads | same "storable scalar" gate | a `str` field is no longer word-sized |
| `char[]` | `is_char_array`, `lib.rs:9580` | str-shaped, so it becomes 24 bytes too |
| FFI marshaling | `lib.rs:6034-6100` | `str` args and returns cross as 24 bytes; the ≤5-scalar-arg cap needs re-counting |

## Stage 5 — rope-native operations

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

## Stage 6 — converge arrays onto the same model

Arrays today are a single pointer to `[refcount][len][cap][elements]`. After
Stage 1 the two families differ for no good reason: strings carry
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

## Stage 7 — niche-filled optionals

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
through it instead of assuming `{tag, payload}`. Worth doing after Stage 6, when
there is one value model to describe niches against rather than two.

## Bootstrap: two ABIs, decided by measurement

The plan asked whether to bootstrap with hand-ported native hook stubs or by
temporarily supporting both ABIs. **Both, in effect — and the choice is now
forced rather than preferred**, because of what a trial flip showed:

1. **The switch gets no compile-time signal.** `is_composite`, `elem_size_of`
   and `sret_size` are ordinary runtime logic, so widening `str` in all three
   compiles with **zero errors** and then emits code that corrupts memory.
2. **The failure has no diagnostic.** The first case run aborts with `SIGABRT`
   and nothing else: codegen believes a `str` is 24 bytes while the runtime still
   reads 8.
3. **There is no "test a piece of it".** The compiler cannot parse *any* source
   without its dogfooded engines, and those run old-ABI code out of the
   checked-in `.clif`. So until every part of the switch is simultaneously
   correct *and* the artifacts are regenerated, nothing runs at all — and the
   artifacts cannot be regenerated without a compiler that parses.

A half-finished switch is therefore not merely broken, it is **undebuggable**.
The way out is to make the two ABIs coexist:

- Every existing `aipl_*` symbol keeps its 8-byte `str` and its current
  behaviour, so the checked-in artifacts keep running and the compiler keeps
  parsing.
- The new entry points carry an `aipl2_` prefix and the new convention — a `str`
  argument is a `*const Str`, a `str` result is written through a leading
  `*mut Str` (the shape `abi_spike::q2b` pinned as the only portable one).
- An artifact is self-consistent with whichever runtime its symbols name, so the
  two never meet. New codegen emits `aipl2_*`; the old IR keeps calling `aipl_*`.

That is what makes the switch testable: user programs move to the new
representation while the compiler's own engines still run the old one, so a
failure is one case failing rather than the world going dark. The artifacts are
regenerated against `aipl2_*` at the end, and the old half is deleted then —
which is also when the hand-ported-stub question disappears, since the real
dogfooded engines do the regeneration.

## Switch: committed, gated off by `AIPL_STR24`

The switch is **in main and inert**. Codegen emits the 24-byte representation
only when `AIPL_STR24` is set; without it the compiler is what it was and the
suite is green (874 passing). That is what lets the work be committed rather than
carried in a stash.

### Why a flag rather than a list of ignored tests

The first attempt was a burn-down list of `#[ignore]`d failures. It cannot work:
two full runs under the switch failed **39 tests and 13 tests with no overlap**.
The breakage is nondeterministic — a value that is 24 bytes to one code path and
8 to another corrupts whatever sits next to it, and what that is depends on
allocation order. There is no stable set to ignore, so ignoring would have
suppressed a moving target. The flag keeps the suite honestly green; the list
survives as `tests/support/str24_migration.txt`, a record of the work rather than
a suppression mechanism.

### The bug that gating exposed

Scoping the switch off turned 874 passing tests into 135 failures, all of the
form *"mismatched argument count: got 3, expected 2"*. The cause is worth
remembering: Stage 0 gave the *old* str-returning symbols `Ret::Str` in the kind
table, and the new out-pointer protocol in `Builtins::call` fired on that — so
every old `str`-returning call gained a leading argument the callee did not take.
`Ret::Str` means "this yields a `str`" to **both** ABIs; only the new one returns
it through a pointer. The protocol now keys off the symbol's convention, not the
return kind alone.

### State of the switched path (`AIPL_STR24=1`)

Programs with no strings work. String literals crashed during codegen of `main`
after the gating refactor, having previously worked end to end (`print("short")`
and a 35-byte static literal both printed). Re-proving those two is the next
step, and `tests/support/str24_migration.txt` records the clusters behind them —
16 tests on the FFI read path, 6 on `emit_render`, 7 on the dogfooded
formatter/lexer/highlighter, 2 on the artifacts, 8 on container shapes.

### What is switched, behind the flag

- the three predicates admitting `str` (via `str24_wide`, which consults the
  gate), so a value travels as an address;
- `emit_const_str` building a literal as three words in a stack slot, with
  `emit_const_str_tagged` beside it for the old path;
- the refcount arm naming `aipl2_inc`/`aipl2_dec` where the type is known to be a
  `str` — **not** through the symbol remap, because `aipl_inc`/`aipl_dec` are
  shared with values that are not `str`s, and remapping them globally sends a
  boxed or array pointer to an entry point that reads 24 bytes off it;
- `active_sym`, the one place a call site's symbol becomes its `aipl2_*`
  counterpart, currently limited to the verified set;
- `build_cli_array` with 24-byte elements when the switch is on;
- **ABI-aware FFI argument marshaling** — a `StrAbi` per `Compilation`, with
  `abi_is_composite`/`abi_elem_size`/`abi_sret_size` answering per callee and
  recursing. Struct layouts need no equivalent: an artifact's come from its
  manifest's explicit sizes and offsets;
- **the out-pointer protocol** in `Builtins::call`, scoped to `aipl2_*`.

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
6. **Canaries for the metric story**, measured before and after so the trade is
   visible rather than assumed: `grammar_calc.aipl` (1,169,584 instructions
   today), `walker.aipl` (3,075,003), `parse.aipl`, and the lexer cases — all
   slice- and token-heavy, which is where the win should show.
7. **Finish** — `cargo handoff`, expecting a corpus-wide refill; then read the
   diff for anything that is not a metric.

## Open questions

- **Is 22-byte inline the right trade against a branchless `len`?** Decidable by
  measuring how many strings in the corpus fall in 17..22 bytes.
- **Does anything depend on 8-byte strings outside the type gates?** The
  bit-packed array mode, dict/set bucket layout, hash seeding, the shim slots,
  and the FFI argument cap are the places to audit before Stage 1.
- **Should `str` remain three SSA values, or become a stack-slot composite?**
  The spike narrows it: arguments are cheap as three scalars, so the question is
  only about *results*, which must go through memory either way. Registers still
  win for values that never cross a call.
- **Does a buffer need a "NUL is present" bit?** The tag byte has six spare bits.
  Only worth it if C handoff turns out to be hot; the capacity test already
  answers "can I write one?".
