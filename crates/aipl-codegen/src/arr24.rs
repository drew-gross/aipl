// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The 24-byte array value — the array half of unifying `str` and array into one
// sequence representation. Deliberately `str24.rs`'s shape, word for word, so the
// two can eventually be one type rather than two that resemble each other.
//
// **Partly wired up.** The value below (`Arr`) is the target, and compiled code
// does not hold it yet: switching a type's representation is atomic — the
// runtime, codegen and the checked-in dogfood artifact have to agree on the
// layout — so arrays are moving one piece at a time, as `str24` did. What
// compiled code holds today is the 24-byte value over the *old* block, described
// under "Windows over the 8-byte block" further down, along with the entry points
// that read and build it. The `Arr` layout itself, its invariants and the
// operations on it are proven by ordinary Rust tests with no compiler in the
// loop (`arr24_todo.txt` tracks the rest).
//
// # The value
//
// ```text
//               w0                  w1                  w2
// block    00   base: *const u8     data: *const u8     [ len : 56 ][ tag : 8 ]
// inline   01   elems[0..8]         elems[8..16]        [ elems[16..22] ][ len:8 ][ tag:8 ]
// reversed 10   base: *const u8     data: *const u8     [ len : 56 ][ tag : 8 ]
//          11   — spare —
// ```
//
// Same three words, same top-byte tag, same reasons as `str24`: classifying is one
// shift on a register the length already needed, and `base`/`data` are
// dereferenced without masking.
//
// # What this buys over the 8-byte tagged pointer
//
// **A slice stops being an allocation.** An 8-byte array value is one tagged
// pointer, so a window into an array had nowhere to put its bounds and needed a
// *view block* — 48 bytes on the heap, holding a reference on the array it
// windowed. Here a window is just a value: `data` moved forward and `len` cut
// short, tag still `block`. (The bridge value below already does this over the
// old block, which is why that view block and its tag are gone.)
//
// `reversed` cannot go the same way — there is no pointer arithmetic that reads a
// run backwards — so it keeps a tag. But it too loses its block: it is the same
// `{base, data, len}` walked from the far end, which is why it carries no extra
// word.
//
// The element *stride* is deliberately not in the value. Codegen knows it at every
// call site and already passes it to every runtime entry point, exactly as it does
// today, and a value that carried it would have one more thing to keep true.
//
// # The block
//
// ```text
// [drop_fn: ptr][init: i64][cap: i64][refcount: i64][elem0][elem1]...
//                                                    ^ base; `data` points
//                                                      anywhere in [base, base+cap]
// ```
//
// `refcount` at `base - 8` and `cap` at `base - 16` are `str24`'s buffer exactly,
// which is the point: the shared half of the two layouts is byte-identical, and an
// array block is a `str` buffer with two more words in front of it.
//
// Those two words are where the unification stops, and the reason is destructors.
// A `str`'s content is bytes, so freeing a buffer is freeing memory; an array's
// elements may own heap of their own, so reaching refcount zero has to release
// each of them — which needs the element drop helper (`drop_fn`) and *how many
// elements there are*. The value's `len` cannot answer the second: a window's `len`
// is the window's, and dropping only the windowed elements would leak the rest. So
// `init` records the block's own initialized element count, which is what
// `release` walks, and it is a different number from the `len` any particular value
// carries.

/// Bytes of an array value — `str24::STR_SIZE`, and for the same reasons.
pub(crate) const ARR_SIZE: usize = 24;
/// The most element *bytes* that fit inline: `w0`, `w1`, and six bytes of `w2`.
pub(crate) const ARR_INLINE_CAP: usize = 22;

pub(crate) const ATAG_BLOCK: u8 = 0b00;
pub(crate) const ATAG_INLINE: u8 = 0b01;
pub(crate) const ATAG_REVERSED: u8 = 0b10;
/// A block value whose run is not (known to be) the whole of its block: what a
/// slice makes. Cleared by nothing — a window stays one, and is copied before it
/// is written through. (`11` was spare in `Arr`'s layout.)
pub(crate) const ATAG_WINDOW: u8 = 0b11;

/// `[drop_fn][init][cap][refcount]` — the prefix before a block's elements.
pub(crate) const ARR_BLOCK_HEADER: usize = 32;
/// How far back from `base` each header word sits.
pub(crate) const ARR_REFCOUNT_BACK: usize = 8;
pub(crate) const ARR_CAP_BACK: usize = 16;
pub(crate) const ARR_INIT_BACK: usize = 24;
pub(crate) const ARR_DROPFN_BACK: usize = 32;

pub(crate) const ARR_LEN_MASK: u64 = (1 << 56) - 1;
pub(crate) const ARR_TAG_SHIFT: u32 = 56;
/// An inline value's element count lives in byte 22, as `str24`'s does.
pub(crate) const ARR_INLINE_LEN_SHIFT: u32 = 48;

/// An array value: three words, no indirection for a block, a window, or inline.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Arr {
    pub w0: u64,
    pub w1: u64,
    pub w2: u64,
}

fn ameta(len: usize, tag: u8) -> u64 {
    debug_assert!(len as u64 <= ARR_LEN_MASK);
    (len as u64) | ((tag as u64) << ARR_TAG_SHIFT)
}

impl Arr {
    /// The empty array. Also what zeroed memory reads as — a block with a null
    /// base and zero length — which matters because sret buffers and fresh array
    /// slots arrive zero-filled, the same property `str24::Str::empty` relies on.
    pub(crate) fn empty() -> Arr {
        Arr {
            w0: 0,
            w1: 0,
            w2: 0,
        }
    }

    pub(crate) fn tag(self) -> u8 {
        (self.w2 >> ARR_TAG_SHIFT) as u8
    }

    /// Element count.
    pub(crate) fn len(self) -> usize {
        if self.tag() == ATAG_INLINE {
            ((self.w2 >> ARR_INLINE_LEN_SHIFT) & 0xFF) as usize
        } else {
            (self.w2 & ARR_LEN_MASK) as usize
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The refcounted allocation behind this value, or null when there is none
    /// (an inline value, or the zeroed empty). The only word refcounting touches.
    pub(crate) fn owner(self) -> *const u8 {
        match self.tag() {
            ATAG_BLOCK | ATAG_REVERSED => self.w0 as *const u8,
            _ => core::ptr::null(),
        }
    }

    /// Address of element `i`, for a value that addresses a block. An inline
    /// value's elements live in the value's own words, so it has no address to
    /// give and is handled by [`Arr::inline_bytes`] instead.
    ///
    /// `reversed` counts from the far end: element 0 is the last one in memory.
    pub(crate) fn elem_ptr(self, i: usize, elem_size: usize) -> *const u8 {
        debug_assert!(i < self.len());
        let from_start = match self.tag() {
            ATAG_REVERSED => self.len() - 1 - i,
            _ => i,
        };
        unsafe { (self.w1 as *const u8).add(from_start * elem_size) }
    }

    /// `self[lo..hi]` — **free**: a window into the same allocation, no copy and no
    /// allocation at any length. This is the whole reason for the wide value; the
    /// 8-byte representation had to allocate a view block here.
    /// (`aipl_arr_window` is this over the old block.)
    ///
    /// Bounds clamp like `aipl_arr_slice`'s. Borrows `self`; the caller retains the
    /// result if it outlives the source.
    pub(crate) fn slice(self, lo: usize, hi: usize, elem_size: usize) -> Arr {
        let len = self.len();
        let lo = lo.min(len);
        let hi = hi.clamp(lo, len);
        let n = hi - lo;
        match self.tag() {
            ATAG_INLINE => {
                let all = self.inline_bytes();
                from_elems_inline(&all[lo * elem_size..hi * elem_size], n)
            }
            // A reversed window's element `lo` sits `len - hi` from the end, so the
            // data pointer does not move for a reversed slice — only the length and
            // the far end do. Taking the sub-window of a reversal is the one case
            // the two tags read differently.
            ATAG_REVERSED => Arr {
                w0: self.w0,
                w1: (unsafe { (self.w1 as *const u8).add((len - hi) * elem_size) }) as u64,
                w2: ameta(n, ATAG_REVERSED),
            },
            _ => Arr {
                w0: self.w0,
                w1: if self.w1 == 0 {
                    0
                } else {
                    (unsafe { (self.w1 as *const u8).add(lo * elem_size) }) as u64
                },
                w2: ameta(n, ATAG_BLOCK),
            },
        }
    }

    /// The same elements, read back to front — also free, and also a value rather
    /// than the block the 8-byte representation needed.
    pub(crate) fn reversed(self) -> Arr {
        match self.tag() {
            ATAG_BLOCK => Arr {
                w2: ameta(self.len(), ATAG_REVERSED),
                ..self
            },
            ATAG_REVERSED => Arr {
                w2: ameta(self.len(), ATAG_BLOCK),
                ..self
            },
            // An inline value is reversed by reversing its bytes, which needs the
            // element size; `reverse_inline` is that, and this arm is why
            // `reversed()` alone is only defined for the two block tags.
            _ => self,
        }
    }

    /// The inline element bytes, as the value's own 22 bytes.
    pub(crate) fn inline_bytes(self) -> [u8; ARR_INLINE_CAP] {
        debug_assert_eq!(self.tag(), ATAG_INLINE);
        let mut out = [0u8; ARR_INLINE_CAP];
        out[0..8].copy_from_slice(&self.w0.to_le_bytes());
        out[8..16].copy_from_slice(&self.w1.to_le_bytes());
        out[16..22].copy_from_slice(&self.w2.to_le_bytes()[0..6]);
        out
    }

    /// How many elements could be appended in place, if this value owns its block
    /// exclusively — the capacity half of the question only, exactly as
    /// `str24::Str::spare_capacity` is.
    pub(crate) fn spare_capacity(self, elem_size: usize) -> usize {
        if self.tag() != ATAG_BLOCK || self.w0 == 0 {
            return 0;
        }
        let end = self.w1 as usize + self.len() * elem_size;
        let cap_end = self.w0 as usize + block_cap(self.w0 as *const u8);
        cap_end.saturating_sub(end) / elem_size.max(1)
    }

    pub(crate) fn retain(self) {
        let owner = self.owner();
        if owner.is_null() {
            return;
        }
        unsafe {
            let rc = arefcount_of(owner);
            if *rc != ARR_STATIC_REFCOUNT {
                *rc += 1;
            }
        }
    }

    /// Drop one reference. At zero, release the block's elements through its
    /// stored `drop_fn` — over `init` elements, *not* `self.len()`, since this
    /// value may be a window into a longer block — and free it.
    pub(crate) fn release(self) {
        let owner = self.owner();
        if owner.is_null() {
            return;
        }
        unsafe {
            let rc = arefcount_of(owner);
            if *rc == ARR_STATIC_REFCOUNT {
                return;
            }
            *rc -= 1;
            if *rc == 0 {
                let drop_fn = *(owner.sub(ARR_DROPFN_BACK) as *const i64);
                if drop_fn != 0 {
                    let f: ArrElemDrop = core::mem::transmute(drop_fn);
                    f(owner, block_init(owner) as i64);
                }
                free_block(owner);
            }
        }
    }

    pub(crate) fn is_unique(self) -> bool {
        let owner = self.owner();
        !owner.is_null() && unsafe { *arefcount_of(owner) } == 1
    }
}

/// `str24::STATIC_REFCOUNT`, spelled out because this file is shared and cannot
/// reach into its host's modules.
pub(crate) const ARR_STATIC_REFCOUNT: i64 = i64::MAX;

/// A block's per-element drop helper: `(base, init_count)`.
type ArrElemDrop = unsafe extern "C" fn(*const u8, i64);

unsafe fn arefcount_of(owner: *const u8) -> *mut i64 {
    unsafe { owner.sub(ARR_REFCOUNT_BACK) as *mut i64 }
}

fn block_cap(base: *const u8) -> usize {
    unsafe { *(base.sub(ARR_CAP_BACK) as *const i64) as usize }
}

fn block_init(base: *const u8) -> usize {
    unsafe { *(base.sub(ARR_INIT_BACK) as *const i64) as usize }
}

/// Record that the block behind `base` now has `init` initialized elements —
/// what a push does once it has written one.
pub(crate) unsafe fn set_block_init(base: *const u8, init: usize) {
    unsafe { *(base.sub(ARR_INIT_BACK) as *mut i64) = init as i64 }
}

unsafe fn free_block(base: *const u8) {
    unsafe { super::rt_free(base.sub(ARR_BLOCK_HEADER) as *mut _) }
}

/// Allocate `[drop_fn][init=0][cap][refcount=1][cap bytes]`, returning the `base`
/// a value stores.
fn alloc_block(cap_bytes: usize, drop_fn: i64) -> *const u8 {
    let raw = unsafe { super::rt_alloc(ARR_BLOCK_HEADER + cap_bytes) } as *mut u8;
    assert!(!raw.is_null(), "out of memory");
    unsafe {
        core::ptr::write(raw as *mut i64, drop_fn);
        core::ptr::write(raw.add(8) as *mut i64, 0);
        core::ptr::write(raw.add(16) as *mut i64, cap_bytes as i64);
        core::ptr::write(raw.add(24) as *mut i64, 1);
        raw.add(ARR_BLOCK_HEADER)
    }
}

/// A block with room for `cap` elements and no elements in it yet.
pub(crate) fn with_capacity(cap: usize, elem_size: usize, drop_fn: i64) -> Arr {
    let base = alloc_block(cap * elem_size, drop_fn);
    Arr {
        w0: base as u64,
        w1: base as u64,
        w2: ameta(0, ATAG_BLOCK),
    }
}

/// An inline value holding `count` elements whose bytes are `bytes`.
pub(crate) fn from_elems_inline(bytes: &[u8], count: usize) -> Arr {
    debug_assert!(bytes.len() <= ARR_INLINE_CAP);
    let mut buf = [0u8; 24];
    buf[0..bytes.len()].copy_from_slice(bytes);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let mut w2 = 0u64;
    for (i, b) in buf[16..22].iter().enumerate() {
        w2 |= (*b as u64) << (8 * i);
    }
    w2 |= (count as u64) << ARR_INLINE_LEN_SHIFT;
    w2 |= (ATAG_INLINE as u64) << ARR_TAG_SHIFT;
    Arr { w0, w1, w2 }
}

/// Build a value holding `count` elements from `bytes`: inline when the bytes fit,
/// otherwise a fresh block.
pub(crate) fn from_elems(bytes: &[u8], count: usize, elem_size: usize, drop_fn: i64) -> Arr {
    if bytes.len() <= ARR_INLINE_CAP && drop_fn == 0 {
        return from_elems_inline(bytes, count);
    }
    let a = with_capacity(count, elem_size, drop_fn);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), a.w1 as *mut u8, bytes.len());
        set_block_init(a.w0 as *const u8, count);
    }
    Arr {
        w2: ameta(count, ATAG_BLOCK),
        ..a
    }
}

/// The element bytes of any value, in *element order* (so a reversed value reads
/// back to front), copied into `out`. Returns the initialized prefix.
pub(crate) fn elems_in_order<'a>(a: Arr, elem_size: usize, out: &'a mut [u8]) -> &'a [u8] {
    let n = a.len() * elem_size;
    if a.tag() == ATAG_INLINE {
        let all = a.inline_bytes();
        out[..n].copy_from_slice(&all[..n]);
        return &out[..n];
    }
    for i in 0..a.len() {
        let src = a.elem_ptr(i, elem_size);
        unsafe {
            core::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(i * elem_size), elem_size)
        };
    }
    &out[..n]
}

/// How many elements of stride `elem_size` fit inline. A stride of
/// [`ELEM_BITPACKED`](super::array_layout::ELEM_BITPACKED) — zero, for `bool[]` —
/// means one bit each, so 22 bytes holds 176.
///
/// NOTE: the rest of this file does not handle the bit-packed stride yet;
/// `elem_ptr` multiplies by it and `spare_capacity` divides by it. See
/// `arr24_todo.txt`, stage 3e.
pub(crate) fn inline_cap(elem_size: usize) -> usize {
    if elem_size == 0 {
        ARR_INLINE_CAP * 8
    } else {
        ARR_INLINE_CAP / elem_size
    }
}

// ---------- The wide-array entry points ----------
//
// The ABI codegen will call once the array value widens: an array argument is a
// `*const Arr`, and an array *result* is written through a leading `*mut Arr`,
// exactly as `str24`'s entry points take and return a `str`. They carry an
// `aipl_arrw_` prefix so they can be registered beside the 8-byte `aipl_arr_*` /
// `aipl_array_*` set and moved onto one at a time — the dual-ABI period
// `arr24_todo.txt` stage 2f ends by deleting the old half.
//
// **Nothing calls these yet.** They are here so that each later step is a change
// of caller rather than a change of caller *and* callee.
//
// Every one of them **borrows** its arguments, the rule `str24`'s entry points
// follow: an entry point reads what it is given and takes its own reference if the
// result needs to outlive the call. That is what makes a host-built array safe to
// hand over whatever ownership protocol the callee was compiled for — the lesson
// the `str` FFI double-free taught (stage 2d).

#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_len(a: *const Arr) -> i64 {
    unsafe { (*a).len() as i64 }
}

#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_inline_cap(elem_size: i64) -> i64 {
    inline_cap(elem_size.max(0) as usize) as i64
}

/// Address of element `i`. Only meaningful for a value that addresses a block —
/// an inline value's elements live in the value's own words, so codegen reads
/// those directly rather than through a pointer.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_elem_ptr(a: *const Arr, i: i64, elem_size: i64) -> *const u8 {
    let a = unsafe { *a };
    a.elem_ptr(i.max(0) as usize, elem_size.max(0) as usize)
}

/// `a[lo..hi]`, as a window. Allocates nothing — the whole point of the wide
/// value. Retained, like `aipl_str_slice`, because the result shares `a`'s block
/// and this entry point only borrows `a`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_slice(
    out: *mut Arr,
    a: *const Arr,
    lo: i64,
    hi: i64,
    elem_size: i64,
) {
    let a = unsafe { *a };
    let sliced = a.slice(
        lo.max(0) as usize,
        hi.max(0) as usize,
        elem_size.max(0) as usize,
    );
    sliced.retain();
    unsafe { *out = sliced };
}

/// The same elements back to front. Also allocation-free, and retained for the
/// same reason as a slice.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_reversed(out: *mut Arr, a: *const Arr) {
    let reversed = unsafe { *a }.reversed();
    reversed.retain();
    unsafe { *out = reversed };
}

/// A block with room for `cap` elements and none in it — the accumulator a push
/// loop fills. Deliberately *not* inline even at `cap == 0`: what it returns is
/// about to be written into, and an inline value has no block to write to.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_with_cap(out: *mut Arr, cap: i64, elem_size: i64, drop_fn: i64) {
    unsafe {
        *out = with_capacity(cap.max(0) as usize, elem_size.max(0) as usize, drop_fn);
    }
}

// ---------- Element helpers for an array *of arrays* ----------
//
// An outer array's elements are themselves array values, and at refcount zero
// (or when the outer array's elements are copied) each one has to be released
// (or retained). The 8-byte helpers (`aipl_arr_drop_arr`, `aipl_arr_retain_ptr`)
// walk 8-byte slots; an array, set or dict value is 24 bytes, its block in the
// first word — so those get their own pair, chosen by codegen's
// `array_drop_fn_addr` / `array_retain_fn_addr`. (The 8-byte pair stays for an
// artifact compiled before the switch.)
//
// Each reads only the first word — the block a value holds its reference on —
// and passes over an inline value, which holds none. They call the host's `aipl_array_dec` / `aipl_arr_inc` through
// `super::`, the way `str24` reaches its host's allocator.

/// Release each of `len` padded array elements starting at `elems`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_drop_warr(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        if let Some(ptr) = unsafe { owned_block_at(elems.add(i * ARR_SIZE)) } {
            super::aipl_array_dec(ptr);
        }
    }
}

/// The block the padded array value at `at` holds a reference on — its first
/// word — or `None` for an inline value, which holds none (its first word is
/// element bytes).
unsafe fn owned_block_at(at: *const u8) -> Option<*const u8> {
    let v = unsafe { core::ptr::read(at as *const ArrValue) };
    (!v.is_inline()).then_some(v.block)
}

/// Retain each of `len` padded array elements starting at `elems`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_retain_warr(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        if let Some(ptr) = unsafe { owned_block_at(elems.add(i * ARR_SIZE)) } {
            super::aipl_arr_inc(ptr);
        }
    }
}

/// An optional padded array, `{tag, value}`: the tag word, then the 24 bytes.
const OPT_ARR_SIZE: usize = 8 + ARR_SIZE;

/// Release the inner array of each present element of a `T[]?[]`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_drop_opt_warr(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        let e = unsafe { elems.add(i * OPT_ARR_SIZE) };
        if unsafe { core::ptr::read(e as *const i64) } != 0 {
            if let Some(ptr) = unsafe { owned_block_at(e.add(8)) } {
                super::aipl_array_dec(ptr);
            }
        }
    }
}

/// Retain the inner array of each present element of a `T[]?[]`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_retain_opt_warr(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        let e = unsafe { elems.add(i * OPT_ARR_SIZE) };
        if unsafe { core::ptr::read(e as *const i64) } != 0 {
            if let Some(ptr) = unsafe { owned_block_at(e.add(8)) } {
                super::aipl_arr_inc(ptr);
            }
        }
    }
}

// # Windows over the 8-byte block (stage 4b)
//
// Until the block itself changes (stage 6c), an array value in compiled code is
// one of these shapes over the *old* block (`array_layout`):
//
// ```text
//            w0                          w1                 w2
// block      tagged pointer (any repr)   data: *const u8    len
// reversed   untagged heap block         data: *const u8    [ len : 56 ][ tag:8 ]
// inline     elems[0..8]                 elems[8..16]       [ elems[16..22] ][ len:8 ][ tag:8 ]
// ```
//
// A *block* value carries its run — `data` and `len` — whether it is the whole
// block or a window of it (stage 6b), so a reader never has to ask which: the
// elements are `data` and `len`, always. A whole value is the window that
// covers the block (`data` at the first element, `len` the block's), and the
// two differ by a tag (stage 6c): a window has `ATAG_WINDOW` above its length,
// a whole value has none. Something that is about to write through a value asks
// [`ArrValue::is_window`] — a window must be copied first, and a whole value
// need not — without reading the block, which keeps no length of its own to
// compare against.
//
// A value whose `data` is null is the older whole shape, `{block, 0, 0}`, still
// read as the block it names: the checked-in artifact's compiled code builds
// them, and so does zeroed memory (the empty array). Nothing new writes one.
// Reading either shape as a window is correct, which is what lets the artifact
// run beside the code that replaced it.
//
// A *reversed* value (`ATAG_REVERSED`) is a window walked from its far end:
// `data` and the length name a forward run of the block, exactly as a window's
// do, and element `i` of the value is element `len - 1 - i` of the run. That is
// `Arr`'s reversed shape, and why reversing allocates nothing — the view block a
// reversal used to build is gone. Reversing one again is the forward window (or
// the whole array) it came from. A bit-packed `bool[]` has no byte run to walk,
// so reversing one copies.
//
// An *inline* value carries its elements in its own 22 bytes and owns no block
// at all — `Arr`'s inline layout exactly, and a short `str`'s: the tag
// (`ATAG_INLINE`) in the top byte of `w2` says so, and the byte below it is the
// length. Only an array of integers is ever inline (the element types `arr24_todo`
// stage 3 settled on), at the packed stride, so 22 bytes is 22 `u8`s or 2 `i64`s.
// A whole value and a window have a top byte of zero, since a window's length
// is far below 2^56.
//
// Otherwise `w1` points at an element inside the block, and is never null for a
// value that carries its run. (An inline value's `w1` is element bytes, so the
// tag is read first.) A window holds one reference on
// its block, exactly as a whole value does, so retaining or releasing either is
// the first word alone — which is all `aipl_arr_{drop,retain}_warr` read, once
// they have passed over an inline element, which holds none.
//
// Windows are only ever made over a `Heap` block of byte-addressable elements.
// A bit-packed `bool[]` has no byte to point at (its whole value still carries a
// `data` and a length, but the run is bits, which only the block's readers walk),
// so slicing one *copies* the
// window into a block of its own (`aipl_arr_slice`) and yields a whole value
// around it. (A reversed value is sliced into a reversed value — see below.)
//
// Code that reads elements learns the window shape (length, indexing,
// iteration, and the runtime's nested reads); every other runtime entry still
// takes a block, and gets one from [`aipl_arr_window_block`] — a copy of the
// window's elements, made only when something that needs a block is reached.
// The copy is the one thing a slice view saved, and it is paid only where a
// block is wanted: an in-place write copied a view on its first write anyway,
// and the entries that take a block (`sort`, `reverse`, `join`'s separators,
// `execute_program`'s arguments) read every element of it regardless.

/// An array value as compiled code holds it in this stage — see above.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ArrValue {
    pub(crate) block: *const u8,
    pub(crate) data: *const u8,
    pub(crate) len: usize,
}

impl ArrValue {
    /// The value for the whole of `block`, which holds `len` elements: for a
    /// caller that built the block and knows how many, so it does not ask the
    /// block (a block that keeps no length cannot say).
    fn whole_of(block: *const u8, len: usize) -> ArrValue {
        ArrValue {
            block,
            data: unsafe {
                super::array_layout::arr_untag(block).add(super::array_layout::ARR_ELEMS_OFFSET)
            },
            len,
        }
    }

    /// A fresh array of `len` uninitialized elements, as a value whose `data` is
    /// where they go.
    pub(crate) fn new_of(len: usize, drop_fn: i64, elem_size: i64) -> ArrValue {
        ArrValue::whole_of(super::aipl_array_new(len as i64, drop_fn, elem_size), len)
    }

    /// A window: the run `data`/`len` of `block`.
    fn window(block: *const u8, data: *const u8, len: usize) -> ArrValue {
        ArrValue {
            block,
            data,
            len: len | ((ATAG_WINDOW as usize) << ARR_TAG_SHIFT),
        }
    }

    /// The value for the whole of `block`: its run is the block's elements, at
    /// the length the block holds now. (Null: the zeroed empty value.)
    pub(crate) fn whole(block: *const u8) -> ArrValue {
        if block.is_null() {
            return ArrValue {
                block,
                data: core::ptr::null(),
                len: 0,
            };
        }
        ArrValue {
            block,
            data: unsafe {
                super::array_layout::arr_untag(block).add(super::array_layout::ARR_ELEMS_OFFSET)
            },
            len: tagged_len(block),
        }
    }

    /// The value's elements carried in its own bytes (see the header).
    fn inline(bytes: &[u8], len: usize) -> ArrValue {
        let mut raw = [0u8; ARR_SIZE];
        raw[..bytes.len()].copy_from_slice(bytes);
        raw[ARR_INLINE_CAP] = len as u8;
        raw[ARR_SIZE - 1] = ATAG_INLINE;
        unsafe { core::ptr::read(raw.as_ptr() as *const ArrValue) }
    }

    pub(crate) fn is_inline(&self) -> bool {
        (self.len as u64 >> ARR_TAG_SHIFT) as u8 == ATAG_INLINE
    }

    pub(crate) fn is_reversed(&self) -> bool {
        (self.len as u64 >> ARR_TAG_SHIFT) as u8 == ATAG_REVERSED
    }

    /// Whether the value is a *window* of its block — a run that may not be the
    /// whole of it — and so cannot be written through or handed to something
    /// that takes the block as it is. A whole value, either shape, is not.
    pub(crate) fn is_window(&self) -> bool {
        (self.len as u64 >> ARR_TAG_SHIFT) as u8 == ATAG_WINDOW
    }

    /// The run of a value that is not inline: `data` and the length for a value
    /// that carries them (a reversed one's is the forward run it walks), the
    /// block's own elements for one in the older whole shape.
    fn run(&self) -> (*const u8, usize) {
        if self.data.is_null() {
            let first = if self.block.is_null() {
                core::ptr::null()
            } else {
                unsafe {
                    super::array_layout::arr_untag(self.block)
                        .add(super::array_layout::ARR_ELEMS_OFFSET)
                }
            };
            return (first, tagged_len(self.block));
        }
        (self.data, self.run_len())
    }

    /// A window or a reversed value's element count.
    pub(crate) fn run_len(&self) -> usize {
        (self.len as u64 & ARR_LEN_MASK) as usize
    }

    /// A reversed value over the forward run `data`/`len` of `block`.
    fn reversed(block: *const u8, data: *const u8, len: usize) -> ArrValue {
        ArrValue {
            block,
            data,
            len: len | ((ATAG_REVERSED as usize) << ARR_TAG_SHIFT),
        }
    }

    /// The elements of a window or a reversed value, in the value's own order,
    /// copied into a fresh heap block and each retained — the block lent to
    /// something that needs one.
    fn run_to_block(&self, drop_fn: i64, retain_fn: i64, elem_size: i64) -> *const u8 {
        let n = self.run_len();
        let w = elem_size as usize;
        let block = super::aipl_array_new(n as i64, drop_fn, elem_size);
        let at = super::array_layout::ARR_ELEMS_OFFSET;
        unsafe {
            let dst = (block as *mut u8).add(at);
            if self.is_reversed() {
                for i in 0..n {
                    core::ptr::copy_nonoverlapping(
                        self.data.add((n - 1 - i) * w),
                        dst.add(i * w),
                        w,
                    );
                }
            } else {
                core::ptr::copy_nonoverlapping(self.data, dst, n * w);
            }
            super::elem_rc(retain_fn, dst, n);
        }
        block
    }

    /// An inline value's element count.
    pub(crate) fn inline_len(&self) -> usize {
        ((self.len as u64 >> ARR_INLINE_LEN_SHIFT) & 0xFF) as usize
    }

    /// An inline value's 24 bytes, its elements first.
    fn bytes(&self) -> [u8; ARR_SIZE] {
        unsafe { core::ptr::read(self as *const ArrValue as *const [u8; ARR_SIZE]) }
    }

    /// An inline value's elements, copied into a fresh heap block that carries
    /// one reference — for something that needs a block.
    fn inline_to_block(&self, elem_size: i64) -> *const u8 {
        let n = self.inline_len();
        let block = super::aipl_array_new(n as i64, 0, elem_size);
        let raw = self.bytes();
        let at = super::array_layout::ARR_ELEMS_OFFSET;
        unsafe {
            core::ptr::copy_nonoverlapping(
                raw.as_ptr(),
                (block as *mut u8).add(at),
                n * elem_size as usize,
            )
        };
        block
    }
}

/// The element count of the array a tagged pointer names (null: empty). Every
/// representation keeps it at the same offset, so this does not dispatch.
fn tagged_len(block: *const u8) -> usize {
    if block.is_null() {
        return 0;
    }
    let u = super::array_layout::arr_untag(block);
    unsafe { core::ptr::read(u.add(super::array_layout::ARR_LEN_OFFSET) as *const i64) as usize }
}

/// `a[start..end]` as a value written to `out`, carrying one new reference.
/// Bounds clamp to `[0, len]`, as every slice does. Borrows `a`.
///
/// Nothing is allocated for a window over a heap block, a window of a window, an
/// empty result or the whole array; a reversed or bit-packed source is copied
/// (see the header).
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_window(
    out: *mut ArrValue,
    a: *const ArrValue,
    start: i64,
    end: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let a = unsafe { *a };
    if a.is_inline() {
        // A window of an inline value is inline too: fewer of the same bytes.
        let len = a.inline_len();
        let lo = start.clamp(0, len as i64) as usize;
        let hi = end.clamp(0, len as i64) as usize;
        let w = elem_size as usize;
        let raw = a.bytes();
        let n = hi.saturating_sub(lo);
        let result = ArrValue::inline(&raw[lo * w..(lo + n) * w], n);
        unsafe { core::ptr::write(out, result) };
        return;
    }
    if a.is_reversed() {
        // `[lo, hi)` of the value is `[len - hi, len - lo)` of its run, still
        // walked backwards.
        let len = a.run_len();
        let lo = start.clamp(0, len as i64) as usize;
        let hi = end.clamp(0, len as i64) as usize;
        let n = hi.saturating_sub(lo);
        let result = if n == 0 {
            ArrValue::whole(super::array_layout::arr_inline_empty())
        } else {
            super::aipl_arr_inc(a.block);
            let data = unsafe { a.data.add((len - hi) * elem_size as usize) };
            ArrValue::reversed(a.block, data, n)
        };
        unsafe { core::ptr::write(out, result) };
        return;
    }
    let (first, len) = a.run();
    let lo = start.clamp(0, len as i64) as usize;
    let hi = end.clamp(0, len as i64) as usize;
    let n = hi.saturating_sub(lo);
    let result = if n == 0 {
        ArrValue::whole(super::array_layout::arr_inline_empty())
    } else if lo == 0 && n == len {
        // All of it: the same value, a window or not.
        super::aipl_arr_inc(a.block);
        if a.data.is_null() {
            ArrValue::whole_of(a.block, len)
        } else {
            a
        }
    } else {
        match super::array_layout::arr_repr(a.block) {
            super::array_layout::ArrRepr::Heap
                if elem_size != super::array_layout::ELEM_BITPACKED =>
            {
                super::aipl_arr_inc(a.block);
                ArrValue::window(a.block, unsafe { first.add(lo * elem_size as usize) }, n)
            }
            _ => ArrValue::whole(super::aipl_arr_slice(
                a.block, lo as i64, hi as i64, drop_fn, retain_fn, elem_size,
            )),
        }
    };
    unsafe { core::ptr::write(out, result) };
}

/// `a.reverse()` as a value written to `out`, carrying one new reference.
/// Borrows `a`. Nothing is allocated except for a bit-packed `bool[]`, which has
/// no byte run to walk backwards and is copied (see the header).
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_reversed(
    out: *mut ArrValue,
    a: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let a = unsafe { *a };
    let w = elem_size as usize;
    let result = if a.is_inline() {
        let n = a.inline_len();
        let raw = a.bytes();
        let mut rev = [0u8; ARR_INLINE_CAP];
        for i in 0..n {
            rev[i * w..(i + 1) * w].copy_from_slice(&raw[(n - 1 - i) * w..(n - i) * w]);
        }
        ArrValue::inline(&rev[..n * w], n)
    } else if a.is_reversed() {
        // Reversing a reversal is the forward run it walks.
        super::aipl_arr_inc(a.block);
        ArrValue::window(a.block, a.data, a.run_len())
    } else {
        let (first, len) = a.run();
        if len == 0 {
            ArrValue::whole(super::array_layout::arr_inline_empty())
        } else if elem_size == super::array_layout::ELEM_BITPACKED {
            ArrValue::whole(super::arr_reverse_copy(
                a.block, drop_fn, retain_fn, elem_size,
            ))
        } else {
            super::aipl_arr_inc(a.block);
            ArrValue::reversed(a.block, first, len)
        }
    };
    unsafe { core::ptr::write(out, result) };
}

/// A block holding exactly the elements of `a`, in its order — `a` being a
/// window, a reversed value or inline — as a fresh copy, each element retained,
/// carrying the one reference for the caller to release. Borrows `a`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_window_block(
    a: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = unsafe { *a };
    if a.is_inline() {
        return a.inline_to_block(elem_size);
    }
    if a.is_reversed() {
        return a.run_to_block(drop_fn, retain_fn, elem_size);
    }
    let (first, len) = a.run();
    let lo = if elem_size == super::array_layout::ELEM_BITPACKED {
        0
    } else {
        (first as usize
            - (super::array_layout::arr_untag(a.block) as usize
                + super::array_layout::ARR_ELEMS_OFFSET))
            / elem_size as usize
    };
    super::aipl_arr_slice(
        a.block,
        lo as i64,
        (lo + len) as i64,
        drop_fn,
        retain_fn,
        elem_size,
    )
}

/// A reference to a block holding exactly the elements of the array value at
/// `a`, for a runtime walk that reads nested arrays (`join`'s parts): a whole
/// value's own block, retained, or a copy of a window's, a reversed value's or an
/// inline value's elements. The caller releases it.
pub(crate) fn owned_block_of(
    a: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let v = unsafe { *a };
    if v.is_window() || v.is_reversed() || v.is_inline() {
        return aipl_arr_window_block(a, drop_fn, retain_fn, elem_size);
    }
    if !v.block.is_null() {
        super::aipl_arr_inc(v.block);
    }
    v.block
}

/// Turn the window — a reversed one, or an inline value — an *owning* slot holds
/// into a whole
/// value over a copy of its elements, in place, so that code which mutates
/// through the first word finds a block of its own there: uniquely owned, so
/// the write that follows goes in place. A window's reference moves from the
/// source block to the copy; an inline value held none. A whole value is left
/// alone.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_settle(
    a: *mut ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let v = unsafe { *a };
    if v.is_inline() {
        let own = v.inline_to_block(elem_size);
        unsafe { core::ptr::write(a, ArrValue::whole(own)) };
        return;
    }
    if !v.is_window() && !v.is_reversed() {
        return;
    }
    let own = aipl_arr_window_block(a, drop_fn, retain_fn, elem_size);
    super::aipl_array_dec(v.block);
    unsafe { core::ptr::write(a, ArrValue::whole(own)) };
}

// # Entries that take values (stage 6a)
//
// The block-taking entries read an array's length out of its block, which is
// why a window, a reversal or an inline value has to be lent a block of its own
// before reaching one. These take the value and read `data` and the length off
// it, whatever its shape — the step toward taking the length out of the block
// (`arr24_todo.txt`, stage 6). Each replaces one block-taking entry, which stays
// registered until the checked-in artifact no longer calls it.

/// The elements of the array value `v`, in its order, as a run of `width`-byte
/// elements starting at `first` and `step` bytes apart (negative for a reversed
/// value), with their count. A whole value's are its block's.
fn elements(v: &ArrValue, width: usize) -> (*const u8, isize, usize) {
    if v.is_inline() {
        return (
            v as *const ArrValue as *const u8,
            width as isize,
            v.inline_len(),
        );
    }
    if v.is_reversed() {
        let n = v.run_len();
        let last = if n == 0 {
            v.data
        } else {
            unsafe { v.data.add((n - 1) * width) }
        };
        return (last, -(width as isize), n);
    }
    let (first, len) = v.run();
    (first, width as isize, len)
}

/// Append every element of the array value at `src` to the block `dst`, which
/// `aipl_arr_reserve` has already made uniquely owned and large enough — a copy
/// plus one retain pass, never a reallocation. *Borrows* `src` (the caller keeps
/// its reference, and pins the block when `src` might be `dst`'s own); `dst`
/// keeps its identity. A bit-packed `bool[]` source — never a window, a reversal
/// or inline — goes through the bit-at-a-time block entry, as before.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_extend(
    dst: *const u8,
    src: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let v = unsafe { *src };
    if elem_size == super::array_layout::ELEM_BITPACKED {
        super::aipl_arr_inc(v.block);
        return super::aipl_arr_extend(dst, v.block, drop_fn, retain_fn, elem_size);
    }
    let w = elem_size as usize;
    let (first, step, add) = elements(&v, w);
    if add == 0 || dst.is_null() {
        return dst;
    }
    unsafe {
        let len_at = dst.add(super::array_layout::ARR_LEN_OFFSET) as *mut i64;
        let dst_len = core::ptr::read(len_at) as usize;
        let at = (dst as *mut u8).add(super::array_layout::ARR_ELEMS_OFFSET + dst_len * w);
        if step > 0 {
            core::ptr::copy_nonoverlapping(first, at, add * w);
        } else {
            for i in 0..add {
                core::ptr::copy_nonoverlapping(first.offset(step * i as isize), at.add(i * w), w);
            }
        }
        super::elem_rc(retain_fn, at, add);
        core::ptr::write(len_at, (dst_len + add) as i64);
    }
    dst
}

/// A `str[]` value's elements as `(first, step, len)` — `step` in `Str`s, so
/// `first.offset(step * i)` is element `i` whatever the value's shape. For the
/// runtime entries that walk a `str[]` they are handed (`execute_program`'s
/// arguments).
pub(crate) fn str_run(a: *const ArrValue) -> (*const super::str24::Str, isize, usize) {
    let w = super::str24::STR_SIZE;
    let (first, step, n) = elements(unsafe { &*a }, w);
    (first as *const super::str24::Str, step / w as isize, n)
}

/// Bit `k` of a bit-packed run starting at `bits`.
fn bit_at(bits: *const u8, k: usize) -> bool {
    unsafe { (core::ptr::read(bits.add(k >> 3)) >> (k & 7)) & 1 != 0 }
}

/// Set bit `k` of the bit-packed region at `bits` to `on`.
fn set_bit(bits: *mut u8, k: usize, on: bool) {
    unsafe {
        let byte = bits.add(k >> 3);
        let mask = 1u8 << (k & 7);
        let b = core::ptr::read(byte);
        core::ptr::write(byte, if on { b | mask } else { b & !mask });
    }
}

/// The element region of a fresh block of `n` elements (`aipl_array_new`), and
/// the block.
fn fresh_block(n: usize, drop_fn: i64, elem_size: i64) -> (*const u8, *mut u8) {
    let block = super::aipl_array_new(n as i64, drop_fn, elem_size);
    let elems = unsafe { (block as *mut u8).add(super::array_layout::ARR_ELEMS_OFFSET) };
    (block, elems)
}

/// Copy the run `first`/`step`/`n` (see [`elements`]) to the contiguous region
/// at `dst`, `w` bytes an element.
unsafe fn copy_run(first: *const u8, step: isize, n: usize, w: usize, dst: *mut u8) {
    unsafe {
        if step > 0 {
            core::ptr::copy_nonoverlapping(first, dst, n * w);
        } else {
            for i in 0..n {
                core::ptr::copy_nonoverlapping(first.offset(step * i as isize), dst.add(i * w), w);
            }
        }
    }
}

/// `xs.sort()` as a value written to `out`: a fresh block holding `xs`'s
/// elements ascending, each retained. *Borrows* `xs`, whatever its shape. `kind`
/// is the runtime's `SORT_KIND_*` — how an element is read and compared.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_sort(
    out: *mut ArrValue,
    a: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    kind: i64,
) {
    let v = unsafe { *a };
    let w = super::array_layout::elem_stride(elem_size);
    let (first, step, n) = elements(&v, w);
    let (block, dst) = fresh_block(n, drop_fn, elem_size);
    if n == 0 {
        // The empty array, which has no element region to sort in.
        unsafe { core::ptr::write(out, ArrValue::whole(block)) };
        return;
    }
    unsafe {
        copy_run(first, step, n, w, dst);
        super::elem_rc(retain_fn, dst, n);
        if kind == super::SORT_KIND_STR && w == super::str24::STR_SIZE {
            let vals = core::slice::from_raw_parts_mut(dst as *mut super::str24::Str, n);
            super::str24::sort_values(vals);
        } else if w < 8 {
            let unsigned = kind == super::SORT_KIND_UNSIGNED;
            super::array_layout::sort_narrow(dst, n, w, unsigned);
        } else {
            let words = core::slice::from_raw_parts_mut(dst as *mut i64, n);
            super::sort_words(words, kind);
        }
        core::ptr::write(out, ArrValue::whole(block));
    }
}

/// `parts.join(sep=s, final_sep=f, only_sep=o)` over a `T[][]`: a fresh block of
/// every part's elements with the separators between — which one in which gap
/// is `str24::gap_sep`'s rule — measured first, so it is one exact allocation.
/// *Borrows* the parts and the separators, every one of them read in place
/// whatever its shape (a part is a 24-byte value at the parts' stride). A
/// bit-packed `bool` element is copied bit by bit. `drop_fn`, `retain_fn` and
/// `elem_size` describe `T`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_join(
    out: *mut ArrValue,
    parts: *const ArrValue,
    sep: *const ArrValue,
    final_sep: *const ArrValue,
    only_sep: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let bits = elem_size == super::array_layout::ELEM_BITPACKED;
    let w = if bits { 0 } else { elem_size as usize };
    let (parts_first, parts_step, n) = elements(unsafe { &*parts }, ARR_SIZE);
    let part = |i: usize| unsafe {
        let pv = core::ptr::read(parts_first.offset(parts_step * i as isize) as *const ArrValue);
        elements(&pv, w)
    };
    let gap = |i: usize| {
        let s = if n == 2 {
            only_sep
        } else if i == n - 1 {
            final_sep
        } else {
            sep
        };
        elements(unsafe { &*s }, w)
    };
    let mut total = 0usize;
    for i in 0..n {
        if i > 0 {
            total += gap(i).2;
        }
        total += part(i).2;
    }
    let (block, dst) = fresh_block(total, drop_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(block, total)) };
    if total == 0 {
        return;
    }
    let mut pos = 0usize;
    for i in 0..n {
        let runs = [if i > 0 { Some(gap(i)) } else { None }, Some(part(i))];
        for (first, step, len) in runs.into_iter().flatten() {
            if bits {
                for k in 0..len {
                    set_bit(dst, pos + k, bit_at(first, k));
                }
            } else {
                unsafe { copy_run(first, step, len, w, dst.add(pos * w)) };
            }
            pos += len;
        }
    }
    unsafe { super::elem_rc(retain_fn, dst, total) };
}

/// `parts.join(sep=..)` over a `str[]`, written to `out`: `str24::join_run` over
/// the parts in place, whatever their shape. *Borrows* everything.
#[no_mangle]
pub(crate) extern "C" fn aipl_strv_join(
    out: *mut super::str24::Str,
    parts: *const ArrValue,
    sep: *const super::str24::Str,
    final_sep: *const super::str24::Str,
    only_sep: *const super::str24::Str,
) {
    let w = super::str24::STR_SIZE;
    let (first, step, n) = elements(unsafe { &*parts }, w);
    unsafe {
        let joined = super::str24::join_run(
            first as *const super::str24::Str,
            step / w as isize,
            n,
            *sep,
            *final_sep,
            *only_sep,
        );
        core::ptr::write(out, joined);
    }
}

// ## The mutating entries
//
// Two forms. A *slot* entry takes the address of a `mut` binding's value, which
// owns one reference: it consumes that reference and writes the result's into
// the same slot. A *`_from`* entry takes a value whose reference the call
// consumes (a moved temporary, or one the caller retained for it) and writes the
// result to `out` — which may be the same address. Both copy the value before
// reading it, so a window, a reversal or an inline value is settled into a block
// of its own on a copy, never in memory the caller only lent.

/// The block of the value at `a`, whose reference the call consumes: a whole
/// value's own, or a fresh copy of a window's, a reversal's or an inline value's
/// elements (the window's reference on its source released).
fn consume_to_block(a: *const ArrValue, drop_fn: i64, retain_fn: i64, elem_size: i64) -> *const u8 {
    let mut v = unsafe { *a };
    aipl_arr_settle(&mut v, drop_fn, retain_fn, elem_size);
    v.block
}

/// `push` for a value whose reference it consumes: the value at `a`, grown by
/// the element at `x`, written to `out`. In place when `a`'s block is uniquely
/// owned and has room (`aipl_array_push_mut`, which copies one that is not); a
/// bit-packed `bool[]` takes the copying push.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_push_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let block = consume_to_block(a, drop_fn, retain_fn, elem_size);
    let grown = if elem_size == super::array_layout::ELEM_BITPACKED {
        super::aipl_array_push(block, x, drop_fn, retain_fn, elem_size)
    } else {
        super::aipl_array_push_mut(block, x, drop_fn, retain_fn, elem_size)
    };
    unsafe { core::ptr::write(out, ArrValue::whole(grown)) };
}

/// `push` onto a `mut` binding's slot. `exclusive` says the binding is
/// statically unaliased, so the push may write into its block in place;
/// otherwise a version of it may be borrowed somewhere, and the push copies.
/// `inline_ok` says the element is an integer, so the value may be (or become)
/// inline, which [`aipl_arr_push_small`] handles.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_push(
    slot: *mut ArrValue,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    exclusive: i64,
    inline_ok: i64,
) {
    if inline_ok != 0 && aipl_arr_push_small(slot, x, elem_size) != PUSH_SMALL_NOT_HANDLED {
        return;
    }
    let block = consume_to_block(slot, drop_fn, retain_fn, elem_size);
    // In place — a bit-packed `bool[]` included, whose in-place push sets the
    // next bit — unlike [`aipl_arrv_push_from`], whose copying `bool[]` push is
    // for a spread's accumulator, which `bool[]` never reserves.
    let grown = if exclusive != 0 {
        super::aipl_array_push_mut(block, x, drop_fn, retain_fn, elem_size)
    } else {
        super::aipl_array_push(block, x, drop_fn, retain_fn, elem_size)
    };
    unsafe { core::ptr::write(slot, ArrValue::whole(grown)) };
}

/// `reserve` for a value whose reference it consumes: room for `extra` more
/// elements, written to `out` (`aipl_arr_reserve`, which keeps a unique block
/// and copies any other).
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_reserve_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    extra: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let block = consume_to_block(a, drop_fn, retain_fn, elem_size);
    let grown = super::aipl_arr_reserve(block, extra, drop_fn, retain_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole(grown)) };
}

/// `extend` for a value whose reference it consumes: the value at `a` with every
/// element of the value at `src` (borrowed, read in place) appended, written to
/// `out`. Reserved first, so the append is one copy; a bit-packed `bool[]` has no
/// reserved form and appends bit by bit (see [`aipl_arrv_extend`]). The caller
/// pins `src`'s block across the call when it may be `a`'s own, which is what
/// makes the reserve copy before anything is written.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_extend_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    src: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let block = consume_to_block(a, drop_fn, retain_fn, elem_size);
    let grown = if elem_size == super::array_layout::ELEM_BITPACKED {
        block
    } else {
        let add = elements(unsafe { &*src }, elem_size as usize).2;
        super::aipl_arr_reserve(block, add as i64, drop_fn, retain_fn, elem_size)
    };
    let extended = aipl_arrv_extend(grown, src, drop_fn, retain_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole(extended)) };
}

/// An array literal's `..src` splice (`__aipl_arr_concat`): the value at `a`,
/// whose reference the call consumes, with every element of the value at `src`
/// (borrowed, read in place) appended, written to `out`. Unlike
/// [`aipl_arrv_extend_from`] it does not reserve: `a` is the literal's
/// accumulator, which the literal's own `__aipl_arr_reserve` already made
/// uniquely owned and sized for every element. Its refcount may still read
/// above one (the accumulator binding's bookkeeping), and a reserve here would
/// take that for sharing and copy into a block with no room left for the
/// elements after the splice.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_splice_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    src: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let block = consume_to_block(a, drop_fn, retain_fn, elem_size);
    let extended = aipl_arrv_extend(block, src, drop_fn, retain_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole(extended)) };
}

/// The address of element `idx` of the array value at `a`, whatever its shape
/// (`elem_size` is the stride in bytes). The caller has bounds-checked `idx`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_elem_ptr(
    a: *const ArrValue,
    idx: i64,
    elem_size: i64,
) -> *const u8 {
    let (first, step, _) = elements(unsafe { &*a }, elem_size as usize);
    unsafe { first.offset(step * idx as isize) }
}

/// The array the in-place `map`/`filter` bodies write (`__arr_writable`), from a
/// value whose reference it consumes, written to `out`: an inline value as it
/// stands — its elements are its own bytes — and anything else as a uniquely
/// owned block (`aipl_arr_reserve` with nothing extra keeps a unique block and
/// copies a shared or static one).
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_writable(
    out: *mut ArrValue,
    a: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let v = unsafe { *a };
    if v.is_inline() {
        unsafe { core::ptr::write(out, v) };
        return;
    }
    aipl_arrv_reserve_from(out, a, 0, drop_fn, retain_fn, elem_size);
}

// ## Sets and dicts
//
// A set or a dict value is always whole — never a window, a reversal or inline —
// so each of these hands its block to the block entry it replaces. What changes
// is the interface: codegen passes the value, and stage 6c, where the length
// leaves the block, reads it from there. The slot forms consume the reference a
// `mut` binding (or a literal under construction) holds and write the result
// back; `union` consumes both operands.

/// Whether the set value at `s` contains the element at `x`.
#[no_mangle]
pub(crate) extern "C" fn aipl_setv_contains(
    s: *const ArrValue,
    x: *const u8,
    elem_size: i64,
    str_cmp: i64,
) -> i64 {
    super::aipl_set_contains(unsafe { (*s).block }, x, elem_size, str_cmp)
}

/// Insert the element at `x` into the set the slot holds (a uniquely owned
/// one), writing the possibly relocated set back.
#[no_mangle]
pub(crate) extern "C" fn aipl_setv_insert(
    slot: *mut ArrValue,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
) {
    unsafe {
        let set = super::aipl_set_insert(
            (*slot).block,
            x,
            drop_fn,
            retain_fn,
            elem_size,
            str_cmp,
            order,
        );
        core::ptr::write(slot, ArrValue::whole(set));
    }
}

/// `a.union(b)` into `out`, consuming both: a fresh set, or — `in_place` — `a`'s
/// own block grown, which the caller has made unique.
#[no_mangle]
pub(crate) extern "C" fn aipl_setv_union(
    out: *mut ArrValue,
    a: *const ArrValue,
    b: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
    in_place: i64,
) {
    unsafe {
        let (a, b) = ((*a).block, (*b).block);
        let set = if in_place != 0 {
            super::aipl_set_union_mut(a, b, drop_fn, retain_fn, elem_size, str_cmp, order)
        } else {
            super::aipl_set_union(a, b, drop_fn, retain_fn, elem_size, str_cmp, order)
        };
        core::ptr::write(out, ArrValue::whole(set));
    }
}

/// The address of the value bound to the key at `key` in the dict value at `d`,
/// or null.
#[no_mangle]
pub(crate) extern "C" fn aipl_dictv_get(
    d: *const ArrValue,
    key: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    super::aipl_dict_get(unsafe { (*d).block }, key, pair_size, str_cmp)
}

/// Whether the dict value at `d` binds the key at `key`.
#[no_mangle]
pub(crate) extern "C" fn aipl_dictv_contains_key(
    d: *const ArrValue,
    key: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> i64 {
    super::aipl_dict_contains_key(unsafe { (*d).block }, key, pair_size, str_cmp)
}

/// Bind the pair at `pair` in the dict the slot holds (a uniquely owned one),
/// writing the possibly relocated dict back.
#[no_mangle]
pub(crate) extern "C" fn aipl_dictv_insert(
    slot: *mut ArrValue,
    pair: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) {
    unsafe {
        let dict =
            super::aipl_dict_insert((*slot).block, pair, drop_fn, retain_fn, pair_size, str_cmp);
        core::ptr::write(slot, ArrValue::whole(dict));
    }
}

/// `remove_key` on a `mut` binding's slot: the dict made uniquely owned
/// (`aipl_arr_reserve` with nothing extra, which keeps a unique block and copies
/// a shared one), the key's pair removed, and the dict written back.
#[no_mangle]
pub(crate) extern "C" fn aipl_dictv_remove(
    slot: *mut ArrValue,
    key: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) {
    unsafe {
        let dict = super::aipl_arr_reserve((*slot).block, 0, drop_fn, retain_fn, pair_size);
        let _ = super::aipl_dict_remove(dict, key, drop_fn, pair_size, str_cmp);
        core::ptr::write(slot, ArrValue::whole(dict));
    }
}

/// What [`aipl_arr_push_small`] did — see its doc.
pub(crate) const PUSH_SMALL_NOT_HANDLED: i64 = 0;
pub(crate) const PUSH_SMALL_INLINE: i64 = 1;
pub(crate) const PUSH_SMALL_GREW: i64 = 2;

/// The part of `push` onto an integer array's *owning* slot that the block
/// runtime does not do: everything that involves an inline value. `x` addresses
/// the element (its low `width` bytes are it).
///
/// - An inline value with room takes the element in its own bytes:
///   [`PUSH_SMALL_INLINE`].
/// - An inline value that is full moves into a fresh block of twice the room,
///   with the element appended: [`PUSH_SMALL_GREW`]. The slot then holds a whole
///   value over a block it alone owns.
/// - An *empty* block becomes an inline value holding the element, when it was
///   not reserved for more than an inline value holds: [`PUSH_SMALL_INLINE`].
///   That is `mut out = []` and a `map`/`filter` output over a short input
///   (`with_capacity` reserves a word per element, not knowing the element
///   type, so the reservation's count is its bytes over eight). A larger
///   reservation is honoured as a block, since it was made for the pushes that
///   follow.
/// - A window (or a reversed one) is settled into a block of its own, and
///   anything else is left:
///   [`PUSH_SMALL_NOT_HANDLED`], and the caller pushes onto the block as before.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_push_small(slot: *mut ArrValue, x: *const u8, width: i64) -> i64 {
    let v = unsafe { *slot };
    let w = width as usize;
    let room = ARR_INLINE_CAP / w;
    let elem = unsafe { core::slice::from_raw_parts(x, w) };
    if v.is_inline() {
        let n = v.inline_len();
        let mut raw = v.bytes();
        if n < room {
            raw[n * w..(n + 1) * w].copy_from_slice(elem);
            unsafe { core::ptr::write(slot, ArrValue::inline(&raw[..(n + 1) * w], n + 1)) };
            return PUSH_SMALL_INLINE;
        }
        let block = super::aipl_array_with_cap(((n + 1) * 2) as i64, 0, width);
        let at = super::array_layout::ARR_ELEMS_OFFSET;
        unsafe {
            let dst = (block as *mut u8).add(at);
            core::ptr::copy_nonoverlapping(raw.as_ptr(), dst, n * w);
            core::ptr::copy_nonoverlapping(x, dst.add(n * w), w);
            core::ptr::write(
                (block as *mut u8).add(super::array_layout::ARR_LEN_OFFSET) as *mut i64,
                (n + 1) as i64,
            );
            core::ptr::write(slot, ArrValue::whole(block));
        }
        return PUSH_SMALL_GREW;
    }
    if v.is_window() || v.is_reversed() {
        aipl_arr_settle(slot, 0, 0, width);
        return PUSH_SMALL_NOT_HANDLED;
    }
    if tagged_len(v.block) == 0 && reserved_bytes(v.block) <= 8 * room {
        super::aipl_array_dec(v.block);
        unsafe { core::ptr::write(slot, ArrValue::inline(elem, 1)) };
        return PUSH_SMALL_INLINE;
    }
    PUSH_SMALL_NOT_HANDLED
}

/// The element capacity, in bytes, a block was reserved with (null: none).
fn reserved_bytes(block: *const u8) -> usize {
    if block.is_null() {
        return 0;
    }
    let u = super::array_layout::arr_untag(block);
    unsafe { core::ptr::read(u.add(super::array_layout::ARR_CAP_OFFSET) as *const i64) as usize }
}

/// The set `set`'s elements as a fresh array of `width`-byte elements, written to
/// `out` — `to_array` on a set of a narrow integer. A set keeps a word per scalar
/// and an array packs a narrow integer at its own width, so the two stop sharing
/// a block there; each word's low bytes are the element. (Every other element
/// type is laid out alike in both, and `to_array` shares the block.) Borrows the
/// set value; the result carries one reference.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_from_set_words(
    out: *mut ArrValue,
    set: *const ArrValue,
    width: i64,
) {
    let (first, step, len) = elements(unsafe { &*set }, 8);
    let (block, dst) = fresh_block(len, 0, width);
    let w = width as usize;
    for i in 0..len {
        unsafe {
            let from = first.offset(step * i as isize);
            core::ptr::copy_nonoverlapping(from, dst.add(i * w), w);
        }
    }
    unsafe { core::ptr::write(out, ArrValue::whole_of(block, len)) };
}

// # Allocation that hands back a value (stage 6c)
//
// Compiled code that wants a new array asks for a value and writes its elements
// at the value's `data`, rather than taking a block and adding the layout's
// element offset itself. Which is why the layout can change underneath it.

/// A fresh array of `len` elements, uninitialized, written to `out` (the value's
/// `data` is where the elements go).
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_new(out: *mut ArrValue, len: i64, drop_fn: i64, elem_size: i64) {
    let n = len.max(0) as usize;
    let block = super::aipl_array_new(n as i64, drop_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(block, n)) };
}

/// An empty array with room for `cap` elements, written to `out`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_with_cap(
    out: *mut ArrValue,
    cap: i64,
    drop_fn: i64,
    elem_size: i64,
) {
    let block = super::aipl_array_with_cap(cap, drop_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(block, 0)) };
}

/// `s.split(sep)` as a `str[]` written to `out`. Each part is a window into `s`,
/// retained because the array owns it. Borrows both arguments.
#[no_mangle]
pub(crate) extern "C" fn aipl_strv_split(
    out: *mut ArrValue,
    s: *const super::str24::Str,
    sep: *const super::str24::Str,
) {
    let (sv, sepv) = unsafe { (*s, *sep) };
    let mut count = 0usize;
    super::str24::for_each_split(sv, sepv, &mut |_| count += 1);
    let drop_fn = super::str24::aipl_arr_drop_str as *const () as usize as i64;
    let (block, dst) = fresh_block(count, drop_fn, super::str24::STR_SIZE as i64);
    let elems = dst as *mut super::str24::Str;
    let mut k = 0usize;
    super::str24::for_each_split(sv, sepv, &mut |part| {
        part.retain();
        unsafe { core::ptr::write(elems.add(k), part) };
        k += 1;
    });
    unsafe { core::ptr::write(out, ArrValue::whole_of(block, count)) };
}

/// Shorten the whole array value at `a` to its first `len` elements, in place —
/// what the in-place `filter` ends with. The dropped tail was already released
/// or moved, so the block is told it holds `len` (the elements `release` walks).
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_truncate(a: *mut ArrValue, len: i64) {
    let v = unsafe { &mut *a };
    if v.is_inline() {
        let mut raw = v.bytes();
        raw[ARR_INLINE_CAP] = len as u8;
        unsafe { core::ptr::write(a, core::ptr::read(raw.as_ptr() as *const ArrValue)) };
        return;
    }
    v.len = len as usize;
    unsafe {
        core::ptr::write(
            super::array_layout::arr_untag(v.block).add(super::array_layout::ARR_LEN_OFFSET)
                as *mut i64,
            len,
        )
    };
}

/// Make the element drop helper of the block under the array value `a` `drop_fn`:
/// the in-place `map` from `T` to `U`, whose elements the block now holds.
#[no_mangle]
pub(crate) extern "C" fn aipl_arrv_set_drop(a: *const ArrValue, drop_fn: i64) {
    let v = unsafe { &*a };
    if v.is_inline() {
        return;
    }
    unsafe {
        core::ptr::write(
            super::array_layout::arr_untag(v.block).add(super::array_layout::ARR_DROPFN_OFFSET)
                as *mut i64,
            drop_fn,
        )
    };
}

#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_inc(a: *const Arr) {
    unsafe { *a }.retain();
}

#[no_mangle]
pub(crate) extern "C" fn aipl_arrw_dec(a: *const Arr) {
    unsafe { *a }.release();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `elem_size` for these tests: 4-byte elements, so a window's arithmetic is
    /// visible and 5 fit inline (22 / 4).
    const E: usize = 4;

    fn bytes_of(vals: &[u32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn read_back(a: Arr) -> Vec<u32> {
        let mut buf = vec![0u8; a.len() * E + 8];
        let got = elems_in_order(a, E, &mut buf);
        got.chunks(E)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn value_is_three_words() {
        assert_eq!(core::mem::size_of::<Arr>(), ARR_SIZE);
        assert_eq!(core::mem::align_of::<Arr>(), 8);
        // The shared half of the two layouts is byte-identical, which is the point
        // of writing this against `str24`'s shape.
        assert_eq!(ARR_SIZE, super::super::str24::STR_SIZE);
        assert_eq!(ARR_REFCOUNT_BACK, super::super::str24::BUF_REFCOUNT_BACK);
    }

    #[test]
    fn zeroed_memory_is_the_empty_array() {
        let zeroed: Arr = unsafe { core::mem::zeroed() };
        assert_eq!(zeroed.tag(), ATAG_BLOCK);
        assert_eq!(zeroed.len(), 0);
        assert!(zeroed.is_empty());
        zeroed.retain(); // must no-op on a null owner
        zeroed.release();
    }

    #[test]
    fn round_trips_across_the_inline_boundary() {
        for n in [0usize, 1, 5, 6, 20] {
            let vals: Vec<u32> = (0..n as u32).collect();
            let a = from_elems(&bytes_of(&vals), n, E, 0);
            assert_eq!(a.len(), n, "len for {n}");
            let expect_inline = n * E <= ARR_INLINE_CAP;
            assert_eq!(
                a.tag(),
                if expect_inline {
                    ATAG_INLINE
                } else {
                    ATAG_BLOCK
                },
                "representation for {n}"
            );
            assert_eq!(read_back(a), vals, "elements for {n}");
            a.release();
        }
    }

    /// The whole reason for the wide value: a window is a value, not an
    /// allocation. The 8-byte representation had to build a 48-byte view block
    /// here, holding a reference on the array it windowed.
    #[test]
    fn a_window_allocates_nothing_and_shares_the_block() {
        let vals: Vec<u32> = (0..10).collect();
        let a = from_elems(&bytes_of(&vals), 10, E, 0);
        assert_eq!(a.tag(), ATAG_BLOCK);

        let mid = a.slice(3, 7, E);
        assert_eq!(mid.len(), 4);
        assert_eq!(read_back(mid), vec![3, 4, 5, 6]);
        // Same allocation, different data pointer: that *is* the window.
        assert_eq!(mid.owner(), a.owner());
        assert_ne!(mid.w1, a.w1);

        // Windows compose, and clamp like a slice bound.
        assert_eq!(read_back(mid.slice(1, 3, E)), vec![4, 5]);
        assert_eq!(read_back(a.slice(8, 100, E)), vec![8, 9]);
        assert!(a.slice(7, 2, E).is_empty());

        a.release();
    }

    /// A reversal is also a value rather than a block, and sub-windowing one reads
    /// from the other end — the one case the two block tags differ.
    #[test]
    fn a_reversal_is_a_value_and_windows_from_the_far_end() {
        let vals: Vec<u32> = (0..6).collect();
        let a = from_elems(&bytes_of(&vals), 6, E, 0);
        let rev = a.reversed();
        assert_eq!(rev.tag(), ATAG_REVERSED);
        assert_eq!(rev.owner(), a.owner());
        assert_eq!(read_back(rev), vec![5, 4, 3, 2, 1, 0]);

        // Reversing twice is the original, and costs nothing either way.
        assert_eq!(read_back(rev.reversed()), vals);

        // `rev[1..4]` is the reversal's elements 1..4 — counted from its own front.
        assert_eq!(read_back(rev.slice(1, 4, E)), vec![4, 3, 2]);

        a.release();
    }

    /// Refcounting is on the block, and a window holds its own reference — so the
    /// elements outlive the value they were sliced from.
    #[test]
    fn a_window_keeps_the_block_alive() {
        let vals: Vec<u32> = (0..10).collect();
        let a = from_elems(&bytes_of(&vals), 10, E, 0);
        let mid = a.slice(2, 5, E);
        mid.retain();
        assert!(!a.is_unique());
        a.release(); // the source is gone; the window is not
        assert_eq!(read_back(mid), vec![2, 3, 4]);
        assert!(mid.is_unique());
        mid.release();
    }

    /// `release` walks the block's `init` count, not the value's `len`. A window
    /// that dropped only its own elements would leak the rest — which is why the
    /// block keeps a count of its own and the unification stops there.
    #[test]
    fn a_window_releases_every_element_of_its_block() {
        use core::sync::atomic::{AtomicI64, Ordering};
        static DROPPED: AtomicI64 = AtomicI64::new(0);
        unsafe extern "C" fn count_drops(_base: *const u8, n: i64) {
            DROPPED.fetch_add(n, Ordering::Relaxed);
        }

        DROPPED.store(0, Ordering::Relaxed);
        let vals: Vec<u32> = (0..10).collect();
        let drop_fn = count_drops as unsafe extern "C" fn(*const u8, i64) as i64;
        let a = from_elems(&bytes_of(&vals), 10, E, drop_fn);
        assert_eq!(a.tag(), ATAG_BLOCK, "a drop_fn forces a block");

        let mid = a.slice(2, 5, E);
        mid.retain();
        a.release();
        assert_eq!(DROPPED.load(Ordering::Relaxed), 0, "still one reference");
        mid.release();
        assert_eq!(
            DROPPED.load(Ordering::Relaxed),
            10,
            "all ten, not the window's three"
        );
    }

    /// The inline capacity is in *elements*, so it falls out of the stride — and a
    /// bit-packed `bool[]` (stride 0) gets eight to the byte.
    #[test]
    fn inline_capacity_follows_the_stride() {
        assert_eq!(inline_cap(8), 2); // i64
        assert_eq!(inline_cap(4), 5); // i32
        assert_eq!(inline_cap(1), 22); // i8
        assert_eq!(inline_cap(0), 176); // bit-packed bool
        assert_eq!(aipl_arrw_inline_cap(8), 2);
        assert_eq!(aipl_arrw_inline_cap(-1), inline_cap(0) as i64);
    }

    /// The entry points are the ABI codegen will call: an array in through a
    /// pointer, an array out through a leading out pointer. Each borrows what it
    /// is given and retains what it hands back, so a caller that releases its own
    /// reference does not invalidate the result.
    #[test]
    fn the_wide_entry_points_borrow_and_retain() {
        let vals: Vec<u32> = (0..10).collect();
        let a = from_elems(&bytes_of(&vals), 10, E, 0);
        assert_eq!(aipl_arrw_len(&a), 10);

        let mut window = Arr::empty();
        aipl_arrw_slice(&mut window, &a, 4, 8, E as i64);
        assert_eq!(aipl_arrw_len(&window), 4);
        assert_eq!(read_back(window), vec![4, 5, 6, 7]);

        let mut rev = Arr::empty();
        aipl_arrw_reversed(&mut rev, &window);
        assert_eq!(read_back(rev), vec![7, 6, 5, 4]);

        // Both results took their own reference, so the source can go.
        aipl_arrw_dec(&a);
        assert_eq!(read_back(window), vec![4, 5, 6, 7]);
        assert_eq!(read_back(rev), vec![7, 6, 5, 4]);

        let elem2 = aipl_arrw_elem_ptr(&window, 2, E as i64);
        let got = unsafe { core::ptr::read(elem2 as *const u32) };
        assert_eq!(got, 6);

        aipl_arrw_dec(&window);
        aipl_arrw_dec(&rev);
    }

    /// An accumulator stays a block even when empty: a push has to have somewhere
    /// to write, and an inline value has no block.
    #[test]
    fn with_cap_is_a_block_even_at_zero() {
        let mut a = Arr::empty();
        aipl_arrw_with_cap(&mut a, 0, E as i64, 0);
        assert_eq!(a.tag(), ATAG_BLOCK);
        assert!(!a.owner().is_null());
        assert_eq!(a.len(), 0);
        aipl_arrw_dec(&a);
    }

    #[test]
    fn spare_capacity_is_what_is_left_of_the_block() {
        let a = with_capacity(8, E, 0);
        assert_eq!(a.len(), 0);
        assert_eq!(a.spare_capacity(E), 8);
        // A value that is not a plain block has no room to append into.
        assert_eq!(a.reversed().spare_capacity(E), 0);
        assert_eq!(from_elems(&bytes_of(&[1, 2]), 2, E, 0).spare_capacity(E), 0);
        a.release();
    }

    /// Stage 6b: a whole value carries its run like any window, and the two
    /// differ only in whether the run is the block's. The older whole shape
    /// (`data` null), which the artifact still builds, reads as the same run.
    #[test]
    fn a_whole_value_carries_its_run() {
        let root = super::super::array_layout::ARR_ELEMS_OFFSET;
        let block = super::super::aipl_array_new(4, 0, 8);
        let whole = ArrValue::whole(block);
        assert_eq!(whole.data as usize, block as usize + root);
        assert_eq!(whole.len, 4);
        assert!(!whole.is_window());

        // The same run, in the older shape: nothing carried, still not a window,
        // and read as the block's elements.
        let older = ArrValue {
            block,
            data: core::ptr::null(),
            len: 0,
        };
        assert!(!older.is_window());
        assert_eq!(older.run(), (whole.data, 4));
        assert_eq!(elements(&older, 8), elements(&whole, 8));

        // A slice is tagged a window whatever it covers, so the block need not
        // say how long it is: a prefix and a suffix both are.
        let prefix = ArrValue::window(block, whole.data, 3);
        let suffix = ArrValue::window(block, unsafe { whole.data.add(8) }, 3);
        assert!(prefix.is_window());
        assert!(suffix.is_window());

        // The empty array and the zeroed value are both empty and neither a window.
        let empty = ArrValue::whole(super::super::array_layout::arr_inline_empty());
        assert_eq!(empty.len, 0);
        assert!(!empty.is_window());
        let zeroed = ArrValue::whole(core::ptr::null());
        assert_eq!(elements(&zeroed, 8).2, 0);
        super::super::aipl_array_dec(block);
    }

    /// The stage-4b bridge over the old block: a slice is a window sharing its
    /// source, a window of a window points into the same block, and a block is
    /// lent only on demand. The source's refcount tells the whole story.
    #[test]
    fn a_window_shares_its_block_and_lends_one_on_demand() {
        let root = super::super::array_layout::ARR_ELEMS_OFFSET;
        let refcount = |b: *const u8| unsafe { *super::super::header_of(b) };
        let block = super::super::aipl_array_new(5, 0, 8);
        for i in 0..5 {
            unsafe { core::ptr::write(block.add(root + i * 8) as *mut i64, 10 + i as i64) };
        }
        let whole = ArrValue::whole(block);
        let mut out = ArrValue::whole(core::ptr::null());

        // `[10..15][1..4]` is a window of three, sharing the block.
        aipl_arr_window(&mut out, &whole, 1, 4, 0, 0, 8);
        let mid = out;
        assert_eq!(mid.block, block);
        assert_eq!(mid.run_len(), 3);
        assert_eq!(unsafe { *(mid.data as *const i64) }, 11);
        assert_eq!(refcount(block), 2);

        // A window of the window is still over the same block.
        aipl_arr_window(&mut out, &mid, 1, 99, 0, 0, 8);
        let inner = out;
        assert_eq!(inner.block, block);
        assert_eq!(inner.run_len(), 2);
        assert_eq!(unsafe { *(inner.data as *const i64) }, 12);
        assert_eq!(refcount(block), 3);

        // The whole array, and an empty window, are not windows at all.
        aipl_arr_window(&mut out, &whole, 0, 5, 0, 0, 8);
        assert!(!out.is_window());
        assert_eq!(out.block, block);
        super::super::aipl_array_dec(out.block);
        aipl_arr_window(&mut out, &mid, 2, 2, 0, 0, 8);
        assert!(!out.is_window());
        assert_eq!(tagged_len(out.block), 0);

        // A block is lent on demand: a copy of exactly the window's elements,
        // which leaves the source's count alone.
        let copy = aipl_arr_window_block(&inner, 0, 0, 8);
        assert_ne!(copy, block);
        assert_eq!(tagged_len(copy), 2);
        assert_eq!(unsafe { *(copy.add(root) as *const i64) }, 12);
        assert_eq!(refcount(block), 3);
        super::super::aipl_array_dec(copy);

        // Settling moves the slot's reference from the source to a copy of its
        // own, which nothing else holds.
        let mut slot = mid;
        aipl_arr_settle(&mut slot, 0, 0, 8);
        assert!(!slot.is_window());
        assert_eq!(tagged_len(slot.block), 3);
        assert_eq!(refcount(slot.block), 1);
        assert_eq!(refcount(block), 2);

        super::super::aipl_array_dec(slot.block);
        super::super::aipl_array_dec(inner.block);
        assert_eq!(refcount(block), 1);
        super::super::aipl_array_dec(block);
    }

    /// A reversal is a value over the same block: reading it, slicing it and
    /// reversing it again move references, never elements, and only a block
    /// lent to the runtime copies — in the reversed order.
    #[test]
    fn a_reversal_shares_its_block_and_reverses_back() {
        let root = super::super::array_layout::ARR_ELEMS_OFFSET;
        let refcount = |b: *const u8| unsafe { *super::super::header_of(b) };
        let elem = |b: *const u8, i: usize| unsafe { *(b.add(root + i * 8) as *const i64) };
        let block = super::super::aipl_array_new(4, 0, 8);
        for i in 0..4 {
            unsafe { core::ptr::write(block.add(root + i * 8) as *mut i64, 10 + i as i64) };
        }
        let whole = ArrValue::whole(block);
        let mut out = ArrValue::whole(core::ptr::null());

        aipl_arr_reversed(&mut out, &whole, 0, 0, 8);
        let r = out;
        assert!(r.is_reversed());
        assert_eq!(r.block, block);
        assert_eq!(r.run_len(), 4);
        assert_eq!(refcount(block), 2);

        // `r[1..3]` is 12, 11: a reversed run of the block's elements 1..3.
        aipl_arr_window(&mut out, &r, 1, 3, 0, 0, 8);
        let mid = out;
        assert!(mid.is_reversed());
        assert_eq!(mid.run_len(), 2);
        assert_eq!(unsafe { *(mid.data as *const i64) }, 11);

        // Lent as a block, the elements come out in the value's order.
        let lent = aipl_arr_window_block(&mid, 0, 0, 8);
        assert_eq!((elem(lent, 0), elem(lent, 1)), (12, 11));
        super::super::aipl_array_dec(lent);

        // Reversing the whole reversal is the forward run again — over the same
        // block, but a window, since nothing here says it covers all of it.
        aipl_arr_reversed(&mut out, &r, 0, 0, 8);
        assert!(!out.is_reversed() && out.is_window());
        assert_eq!(out.block, block);
        assert_eq!(out.run_len(), 4);
        assert_eq!(refcount(block), 4);

        for v in [out, mid, r] {
            super::super::aipl_array_dec(v.block);
        }
        assert_eq!(refcount(block), 1);
        super::super::aipl_array_dec(block);
    }
}
