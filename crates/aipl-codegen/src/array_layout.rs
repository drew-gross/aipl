// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The array value's layout — the block, the representation tags in the
// pointer, and the view blocks — **shared verbatim** by the JIT runtime
// (`aipl-codegen`, which has it as an ordinary module) and the AOT runtime
// (`aipl-linker/runtime/aipl_runtime.rs`, which `include!`s it inside a
// `mod array_layout`), exactly as `str24.rs` shares the string layout. A
// divergence in a layout constant between the two runtimes is silent memory
// corruption rather than a failing test, which is why the constants live in
// one file rather than being mirrored by hand — the two copies this replaced
// had already drifted in their comments, if not yet in their values.
//
// Everything here is a pure reading of a pointer or a size: no allocation,
// no `std`. What allocates (the block, a view) and what walks elements stays
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
// 0b01  Rev    — a reversed view: a thin wrapper reading an inner array backwards
// 0b10  Slice  — a slice view: a window `[start, start + len)` into an inner array
// ```
//
// Every place that uses an array pointer as a memory base strips the tag
// first (`arr_untag`), and every place that reads elements classifies once
// (`arr_repr`) and matches on the result — the pattern `str_repr`/`StrRepr`
// set, so that a new representation is a compile error at each site that
// does not yet handle it rather than a silent fall-through.
//
// # The view blocks
//
// ```text
// [refcount][len][inner: ptr][drop_fn][retain_fn][elem_size]          (Rev)
// [refcount][len][inner: ptr][drop_fn][retain_fn][elem_size][start]   (Slice)
//             ^ data pointer, tagged
// ```
//
// `len` sits where a heap block's does, so a length read never dispatches.
// A view holds one reference on `inner` (any representation — views
// compose) and records the drop/retain helpers and element size it needs to
// materialize itself into a heap block when something wants to write.

/// The heap block, relative to the data pointer.
pub(crate) const ARR_LEN_OFFSET: usize = 0; // length, in elements
pub(crate) const ARR_CAP_OFFSET: usize = 8; // capacity of the element region, in *bytes*
pub(crate) const ARR_DROPFN_OFFSET: usize = 16; // element drop-fn pointer (null = scalars)
pub(crate) const ARR_ELEMS_OFFSET: usize = 24; // first element

/// The representation tag, in the data pointer's two low bits.
pub(crate) const ARR_TAG_MASK: usize = 0b11;
pub(crate) const ARR_HEAP_TAG: usize = 0b00;
pub(crate) const ARR_REV_TAG: usize = 0b01;
pub(crate) const ARR_SLICE_TAG: usize = 0b10;

/// A view block, relative to its data pointer. `len` is at [`ARR_LEN_OFFSET`].
pub(crate) const REV_INNER_OFFSET: usize = 8; // tagged pointer to the wrapped array
pub(crate) const REV_DROP_OFFSET: usize = 16; // element drop fn, for materializing
pub(crate) const REV_RETAIN_OFFSET: usize = 24; // element retain fn, likewise
pub(crate) const REV_ELEMSIZE_OFFSET: usize = 32; // element stride; `ELEM_BITPACKED` for `bool[]`
pub(crate) const SLICE_START_OFFSET: usize = 40; // slice view only: the window's first element
/// Bytes after the refcount header, per view.
pub(crate) const REV_BLOCK_DATA_SIZE: usize = 40;
pub(crate) const SLICE_BLOCK_DATA_SIZE: usize = 48;

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
    Reversed,
    Sliced,
}

/// Classify an array pointer by its tag.
pub(crate) fn arr_repr(ptr: *const u8) -> ArrRepr {
    match ptr as usize & ARR_TAG_MASK {
        ARR_HEAP_TAG => ArrRepr::Heap,
        ARR_REV_TAG => ArrRepr::Reversed,
        ARR_SLICE_TAG => ArrRepr::Sliced,
        tag => unreachable!("unknown array repr tag {tag}"),
    }
}

/// Strip the representation tag from an array pointer, returning the actual
/// block base address — the only form a pointer may be dereferenced in.
pub(crate) fn arr_untag(ptr: *const u8) -> *const u8 {
    (ptr as usize & !ARR_TAG_MASK) as *const u8
}

/// Bytes needed to hold `count` elements: `ceil(count/8)` when bit-packed
/// (`elem_size == 0`), else `count * elem_size` (with the historic 8-byte
/// floor).
pub(crate) fn cap_bytes_for(elem_size: i64, count: usize) -> usize {
    if elem_size == ELEM_BITPACKED {
        count.div_ceil(8)
    } else {
        count * (elem_size.max(8) as usize)
    }
}
