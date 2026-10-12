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
// plain (elements need no drop):   [cap][refcount][elem0][elem1]...               16 bytes
// drop (`str[]`, `T[][]`, ...):    [drop_fn][init][cap][refcount][elem0]...       32 bytes
//                                                              ^ base
// ```
//
// `base` — the first element — is the block's address, and the pointer an array
// value carries (tagged: see below). The refcount sits at `base - HEADER_SIZE`,
// shared with every other refcounted block, and `cap` just before it is the
// element region's size in *bytes*, so a block can be freed without knowing its
// element size (which is not stored: codegen knows it at compile time and passes
// it to every runtime entry). The plain block is a `str24` buffer exactly.
//
// **Neither shape has a length.** That lives in the value (`arr24::ArrValue`),
// which is what a window needs, and the block only has to know what it must
// release. An integer array has nothing to release, so its block has no
// `drop_fn` and no `init`. A block whose elements own heap has both: `drop_fn`
// releases the elements and `init` is how many of them the block owns, which is
// the count `release` walks. It is the block's own count, not any value's `len`:
// a window's `len` is the window's, and releasing through one would leak the
// rest.
//
// # The tags
//
// Arrays are 8-byte aligned, so the two low bits of a data pointer are free, and
// they say which shape the block is — a type-erased walk (a nested array's
// release, the FFI) reads it off the value rather than asking for the element
// type:
//
// ```text
// 0b00  Static — somebody else's block: a literal in the data section, or an array
//                the host lent. Its refcount is `STATIC_REFCOUNT`, and the runtime
//                reads nothing else of it: it is never written, grown, walked or
//                freed, only copied from through the value's `data`.
// 0b01  Plain  — the plain block above
// 0b10  Drop   — the drop block above
// 0b11  Empty  — the shared empty array (below)
// ```
//
// # Empty
//
// The **empty** array, as a tagged pointer that owns no allocation: the bare tag
// `0b11` over a shared static block. It is what an empty `filter`, a zero-length
// slice and a `map` over nothing produce, none of which needs a block.
//
// It is not where arrays with elements are held inline. Those carry their
// elements in the 24-byte value's own bytes, `str`'s inline layout
// (`arr24::ArrValue`).
//
// Every place that uses an array pointer as a memory base strips the tag
// first (`arr_untag`), and every place that depends on the shape classifies once
// (`arr_repr`) and matches on the result — the pattern `str_repr`/`StrRepr`
// set, so that a new representation is a compile error at each site that
// does not yet handle it rather than a silent fall-through.

/// Where the first element is, relative to the base. A name rather than a bare
/// zero so the places that step from a block to its elements say so.
pub(crate) const ARR_ELEMS_OFFSET: usize = 0;

/// The representation tag, in the data pointer's two low bits.
pub(crate) const ARR_TAG_MASK: usize = 0b11;
pub(crate) const ARR_STATIC_TAG: usize = 0b00;
pub(crate) const ARR_PLAIN_TAG: usize = 0b01;
pub(crate) const ARR_DROP_TAG: usize = 0b10;
pub(crate) const ARR_EMPTY_TAG: usize = 0b11;

/// Bytes before the base of each shape's block: `[cap][refcount]`, and
/// `[drop_fn][init][cap][refcount]`.
pub(crate) const ARR_PLAIN_HEADER: usize = 16;
pub(crate) const ARR_DROP_HEADER: usize = 32;
/// How far back from the base each header word sits.
pub(crate) const ARR_CAP_BACK: usize = 16;
pub(crate) const ARR_INIT_BACK: usize = 24;
pub(crate) const ARR_DROPFN_BACK: usize = 32;

/// The empty array, as a value: no allocation, no refcount, nothing to free.
///
/// The `Empty` tag sits over [`ARR_EMPTY_BLOCK`] rather than over nothing, so the
/// refcount a release reads is a real word (and `STATIC_REFCOUNT`, which makes
/// every inc/dec a no-op). Nothing else of the block is ever read: its capacity
/// is zero by definition (`block_cap_bytes`), and it has no elements to walk.
pub(crate) fn arr_inline_empty() -> *const u8 {
    (arr_empty_block() as usize | ARR_EMPTY_TAG) as *const u8
}

/// `[refcount]` and slack, for an array that is permanently empty.
///
/// `i64::MAX` is `str24::STATIC_REFCOUNT`, spelled out because this file is shared
/// and cannot reach into its host's modules.
static ARR_EMPTY_BLOCK: [i64; 4] = [i64::MAX, 0, 0, 0];

/// The base of [`ARR_EMPTY_BLOCK`]. The refcount sits one word before it, which
/// is what `header_of` reads, so the base is the *second* word.
pub(crate) fn arr_empty_block() -> *const u8 {
    unsafe { (ARR_EMPTY_BLOCK.as_ptr() as *const u8).add(8) }
}

/// `bool[]` is bit-packed (8 elements per byte, like `std::vector<bool>` but
/// with the ordinary array interface). Codegen signals it with an element size
/// of 0 — the one sentinel that means "bit-packed" rather than a byte stride.
/// `len` still counts elements; `cap` (bytes) holds `ceil(len/8)`. Bits past
/// `len` are never read, so they need not be cleared.
pub(crate) const ELEM_BITPACKED: i64 = 0;

/// The shape an array pointer's tag names — see the tags above.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrRepr {
    /// Somebody else's block: refcount and nothing else.
    Static,
    Plain,
    Drop,
    /// The shared empty array.
    Empty,
}

/// Classify an array pointer by its tag.
pub(crate) fn arr_repr(ptr: *const u8) -> ArrRepr {
    match ptr as usize & ARR_TAG_MASK {
        ARR_STATIC_TAG => ArrRepr::Static,
        ARR_PLAIN_TAG => ArrRepr::Plain,
        ARR_DROP_TAG => ArrRepr::Drop,
        _ => ArrRepr::Empty,
    }
}

/// Strip the representation tag from an array pointer, returning the actual
/// block base address — the only form a pointer may be dereferenced in.
pub(crate) fn arr_untag(ptr: *const u8) -> *const u8 {
    (ptr as usize & !ARR_TAG_MASK) as *const u8
}

/// Bytes in front of the base of a block of this shape (zero for the shapes that
/// are never allocated).
pub(crate) fn block_header_bytes(repr: ArrRepr) -> usize {
    match repr {
        ArrRepr::Plain => ARR_PLAIN_HEADER,
        ArrRepr::Drop => ARR_DROP_HEADER,
        ArrRepr::Static | ArrRepr::Empty => 0,
    }
}

/// The capacity of the element region, in bytes. Zero for a block the runtime
/// does not own (a static one, the empty array): there is nothing to grow in place.
pub(crate) unsafe fn block_cap_bytes(ptr: *const u8) -> usize {
    match arr_repr(ptr) {
        ArrRepr::Plain | ArrRepr::Drop => unsafe {
            core::ptr::read(arr_untag(ptr).sub(ARR_CAP_BACK) as *const i64) as usize
        },
        ArrRepr::Static | ArrRepr::Empty => 0,
    }
}

/// The element drop helper a block records (zero for a plain block).
pub(crate) unsafe fn block_drop_fn(ptr: *const u8) -> i64 {
    match arr_repr(ptr) {
        ArrRepr::Drop => unsafe {
            core::ptr::read(arr_untag(ptr).sub(ARR_DROPFN_BACK) as *const i64)
        },
        ArrRepr::Plain | ArrRepr::Static | ArrRepr::Empty => 0,
    }
}

/// How many elements a drop block owns — what `release` walks. A plain block owns
/// nothing to release, and says zero.
pub(crate) unsafe fn block_init(ptr: *const u8) -> usize {
    match arr_repr(ptr) {
        ArrRepr::Drop => unsafe {
            core::ptr::read(arr_untag(ptr).sub(ARR_INIT_BACK) as *const i64) as usize
        },
        ArrRepr::Plain | ArrRepr::Static | ArrRepr::Empty => 0,
    }
}

/// Record that a drop block owns `n` elements. A plain block keeps no count.
pub(crate) unsafe fn set_block_init(ptr: *const u8, n: usize) {
    if arr_repr(ptr) == ArrRepr::Drop {
        unsafe { core::ptr::write(arr_untag(ptr).sub(ARR_INIT_BACK) as *mut i64, n as i64) };
    }
}

/// Record the element drop helper of a drop block. A plain block has none to
/// record, and a nonzero one is the caller's mistake to have asked.
pub(crate) unsafe fn set_block_drop_fn(ptr: *const u8, drop_fn: i64) {
    if arr_repr(ptr) == ArrRepr::Drop {
        unsafe { core::ptr::write(arr_untag(ptr).sub(ARR_DROPFN_BACK) as *mut i64, drop_fn) };
    }
}

/// Record the capacity of the element region of a plain or drop block.
pub(crate) unsafe fn set_block_cap(ptr: *const u8, cap_bytes: usize) {
    if matches!(arr_repr(ptr), ArrRepr::Plain | ArrRepr::Drop) {
        unsafe {
            core::ptr::write(
                arr_untag(ptr).sub(ARR_CAP_BACK) as *mut i64,
                cap_bytes as i64,
            )
        };
    }
}

/// The start of the allocation behind a plain or drop block.
pub(crate) fn block_start(ptr: *const u8) -> *mut u8 {
    unsafe { arr_untag(ptr).sub(block_header_bytes(arr_repr(ptr))) as *mut u8 }
}

/// Bytes to allocate for a block holding `cap_bytes` of elements, of the shape
/// `drop_fn` asks for.
pub(crate) fn block_alloc_size(drop_fn: i64, cap_bytes: usize) -> usize {
    cap_bytes
        + if drop_fn != 0 {
            ARR_DROP_HEADER
        } else {
            ARR_PLAIN_HEADER
        }
}

/// Write the header of the freshly allocated `raw` (`block_alloc_size` bytes) and
/// return the tagged base: refcount 1, `cap_bytes` of capacity, and for a drop
/// block (`drop_fn != 0`) its helper and the `init` elements it owns.
pub(crate) unsafe fn init_block(
    raw: *mut u8,
    drop_fn: i64,
    init: usize,
    cap_bytes: usize,
) -> *const u8 {
    unsafe {
        let drop = drop_fn != 0;
        let base = raw.add(if drop {
            ARR_DROP_HEADER
        } else {
            ARR_PLAIN_HEADER
        });
        core::ptr::write(base.sub(8) as *mut i64, 1); // refcount
        core::ptr::write(base.sub(ARR_CAP_BACK) as *mut i64, cap_bytes as i64);
        if drop {
            core::ptr::write(base.sub(ARR_INIT_BACK) as *mut i64, init as i64);
            core::ptr::write(base.sub(ARR_DROPFN_BACK) as *mut i64, drop_fn);
            (base as usize | ARR_DROP_TAG) as *const u8
        } else {
            (base as usize | ARR_PLAIN_TAG) as *const u8
        }
    }
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
