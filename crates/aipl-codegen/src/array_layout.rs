// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The array value's layout — the block and the representation tags in the
// pointer — **shared verbatim** by the JIT runtime
// (`aipl-codegen`, which has it as an ordinary module) and the AOT runtime
// (`aipl-linker/runtime/aipl_runtime.rs`, which `include!`s it inside a
// `mod array_layout`), exactly as `str24.rs` shares the string layout. A
// divergence in a layout constant between the two runtimes is silent memory
// corruption rather than a failing test, which is why the constants live in
// one file rather than being mirrored by hand — the two copies this replaced
// had already drifted in their comments, if not yet in their values.
//
// Everything here is a pure reading of a pointer or a size: no allocation,
// no `std`. What allocates (the block) and what walks elements stays
// in each runtime, where the allocator and the accounting are.
//
// # The block
//
// ```text
// [refcount: i64][len: i64][cap: i64][drop_fn: ptr][elem0][elem1]...
//                 ^ data pointer (the array value, once tagged)
// ```
//
// The refcount sits at `ptr - HEADER_SIZE`, shared with every other
// refcounted block. `cap` is the element region's size in *bytes*, so a
// block can be freed without knowing its element size — which is not stored:
// codegen knows it at compile time and passes it to every runtime entry.
// `drop_fn` is null for scalar elements; for heap elements (`str`, a nested
// array) it releases each element at refcount zero, and its being non-null
// is also what tells `push` to retain the copies it makes.
//
// # The tags
//
// Arrays are 8-byte aligned, so the two low bits of a data pointer are free,
// and they encode the representation — the array's counterpart of the tag
// byte a `str` value carries (`str24::TAG_*`):
//
// ```text
// 0b00  Heap   — the block above
// 0b01  —        unused: a reversed view lived here, and
// 0b10  —        unused: a slice view here, before a reversal and a slice became
//                shapes of the 24-byte value (`arr24::ArrValue`)
// 0b11  Inline — the shared empty array (below)
// ```
//
// # Inline
//
// The **empty** array, as a tagged pointer that owns no allocation: the bare tag
// `0b11` over a shared static block. It is what an empty `filter`, a zero-length
// slice and a `map` over nothing produce, none of which needs a block.
//
// It is not where arrays with elements are held inline. Those carry their
// elements in the 24-byte value's own bytes, `str`'s inline layout
// (`arr24::ArrValue`), and this tag is a stand-in that stage 7 retires along
// with the rest of the 8-byte layout.
//
// Every place that uses an array pointer as a memory base strips the tag
// first (`arr_untag`), and every place that reads elements classifies once
// (`arr_repr`) and matches on the result — the pattern `str_repr`/`StrRepr`
// set, so that a new representation is a compile error at each site that
// does not yet handle it rather than a silent fall-through.

/// The heap block, relative to the data pointer.
pub(crate) const ARR_LEN_OFFSET: usize = 0; // length, in elements
pub(crate) const ARR_CAP_OFFSET: usize = 8; // capacity of the element region, in *bytes*
pub(crate) const ARR_DROPFN_OFFSET: usize = 16; // element drop-fn pointer (null = scalars)
pub(crate) const ARR_ELEMS_OFFSET: usize = 24; // first element

/// The representation tag, in the data pointer's two low bits.
pub(crate) const ARR_TAG_MASK: usize = 0b11;
pub(crate) const ARR_HEAP_TAG: usize = 0b00;
pub(crate) const ARR_INLINE_TAG: usize = 0b11;

/// The empty array, as a value: no allocation, no refcount, nothing to free.
///
/// The `Inline` tag sits over [`ARR_EMPTY_BLOCK`] rather than over nothing, and
/// that is deliberate: *every* reader that untags an array pointer and reads the
/// header — and there are dozens, for the length, the capacity and the element
/// drop-fn — then finds zeroes instead of needing to learn a new case. So this
/// representation costs generated code nothing, and the tag is what the few places
/// that *own* an array (inc, dec, free) dispatch on to do nothing at all.
///
/// A later inline array that genuinely carries its elements in the value cannot
/// borrow that trick: it has no block to point at, so those readers would have to
/// be taught the case. [`arr_inline_len`] is where that starts, and the branch it
/// would need in codegen's `load_arr_len` is the cost to weigh then.
pub(crate) fn arr_inline_empty() -> *const u8 {
    (arr_empty_block() as usize | ARR_INLINE_TAG) as *const u8
}

/// How many elements an inline value holds. Always zero for now — the empty array
/// is the only inline value constructed (see the header).
pub(crate) fn arr_inline_len(_ptr: *const u8) -> usize {
    0
}

/// `[refcount][len][cap][drop_fn][-]` for an array that is permanently empty.
///
/// A handful of entry points need a *block* — they hand a pointer to something
/// that reads the header — and the inline empty value has none. Rather than teach each of them the inline case, they reach for
/// this: a real heap-shaped block that happens to be static and empty, so the
/// reads find `len = 0` and the refcount makes every inc/dec a no-op. The fifth
/// word is slack so that an element address computed past the header (never read,
/// since the length is zero) stays inside the object.
///
/// `i64::MAX` is `str24::STATIC_REFCOUNT`, spelled out because this file is shared
/// and cannot reach into its host's modules.
static ARR_EMPTY_BLOCK: [i64; 5] = [i64::MAX, 0, 0, 0, 0];

/// The data pointer of [`ARR_EMPTY_BLOCK`] — a `Heap`-tagged empty array.
pub(crate) fn arr_empty_block() -> *const u8 {
    // The refcount sits one word before the data pointer, which is what
    // `header_of` reads, so the data pointer is the *second* word.
    unsafe { (ARR_EMPTY_BLOCK.as_ptr() as *const u8).add(8) }
}

/// `bool[]` is bit-packed (8 elements per byte, like `std::vector<bool>` but
/// with the ordinary array interface). Codegen signals it with an element size
/// of 0 — the one sentinel that means "bit-packed" rather than a byte stride.
/// `len` still counts elements; `cap` (bytes) holds `ceil(len/8)`. Bits past
/// `len` are never read, so they need not be cleared.
pub(crate) const ELEM_BITPACKED: i64 = 0;

/// The representation an array pointer carries — see the tags above.
#[derive(Clone, Copy)]
pub(crate) enum ArrRepr {
    Heap,
    /// The value is the array; it addresses no block. See the header.
    Inline,
}

/// Classify an array pointer by its tag.
pub(crate) fn arr_repr(ptr: *const u8) -> ArrRepr {
    match ptr as usize & ARR_TAG_MASK {
        ARR_HEAP_TAG => ArrRepr::Heap,
        ARR_INLINE_TAG => ArrRepr::Inline,
        tag => unreachable!("unknown array repr tag {tag}"),
    }
}

/// Strip the representation tag from an array pointer, returning the actual
/// block base address — the only form a pointer may be dereferenced in.
pub(crate) fn arr_untag(ptr: *const u8) -> *const u8 {
    (ptr as usize & !ARR_TAG_MASK) as *const u8
}

/// The byte stride of an array element, from the `elem_size` codegen passes:
/// exactly that — a narrow integer is its own width, 1, 2 or 4 — except for the
/// bit-packed sentinel, which a byte-stride path treats as the word it always
/// did. (Those paths are not reached with a `bool[]`; the floor only keeps the
/// sentinel from reading as a zero stride.)
pub(crate) fn elem_stride(elem_size: i64) -> usize {
    if elem_size == ELEM_BITPACKED {
        8
    } else {
        elem_size as usize
    }
}

/// Sort `len` narrow integer elements (`width` 1, 2 or 4 bytes) in place,
/// ascending — the `sort` of an array whose elements are packed at their own
/// width, which `sort_words` cannot read as words. `unsigned` is the element
/// type's signedness, which decides where a value with its top bit set sorts.
///
/// SAFETY: `base` must address `len` initialized elements of `width` bytes,
/// aligned for that width (an array's elements start 8-aligned).
pub(crate) unsafe fn sort_narrow(base: *mut u8, len: usize, width: usize, unsigned: bool) {
    unsafe {
        match (width, unsigned) {
            (1, true) => core::slice::from_raw_parts_mut(base, len).sort_unstable(),
            (1, false) => core::slice::from_raw_parts_mut(base as *mut i8, len).sort_unstable(),
            (2, true) => core::slice::from_raw_parts_mut(base as *mut u16, len).sort_unstable(),
            (2, false) => core::slice::from_raw_parts_mut(base as *mut i16, len).sort_unstable(),
            (4, true) => core::slice::from_raw_parts_mut(base as *mut u32, len).sort_unstable(),
            (4, false) => core::slice::from_raw_parts_mut(base as *mut i32, len).sort_unstable(),
            _ => unreachable!("a narrow element is 1, 2 or 4 bytes, not {width}"),
        }
    }
}

/// Bytes needed to hold `count` elements: `ceil(count/8)` when bit-packed
/// (`elem_size == 0`), else `count * elem_size` — which for an array of a narrow
/// integer is its own width, not a word.
pub(crate) fn cap_bytes_for(elem_size: i64, count: usize) -> usize {
    if elem_size == ELEM_BITPACKED {
        count.div_ceil(8)
    } else {
        count * elem_size as usize
    }
}
