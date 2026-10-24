// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The 24-byte array value — the array half of unifying `str` and array into one
// sequence representation. Deliberately `str24.rs`'s shape, word for word, so the
// two can eventually be one type rather than two that resemble each other.
//
// What compiled code holds is [`ArrValue`], below, with the entry points that read
// and build it; the sections after the value describe each shape. The layout, and
// the block under it, are documented here and in `array_layout.rs`; the block's
// reference counting and allocation are `buffer.rs`, which a `str` shares, and its
// operations are `array_ops.rs`.
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
// # What this buys
//
// **A slice is not an allocation.** A window into an array is just a value:
// `data` moved forward and `len` cut short. (The one-word tagged pointer this
// replaced had nowhere to put a window's bounds, and needed a *view block* — 48
// bytes on the heap, holding a reference on the array it windowed.)
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
// plain (elements need no drop):   [cap][refcount][elem0][elem1]...               16 bytes
// drop (`str[]`, `T[][]`, ...):    [drop_fn][init][cap][refcount][elem0]...       32 bytes
//                                                              ^ base; `data` points
//                                                                anywhere in [base, base+cap]
// ```
//
// `refcount` at `base - 8` and `cap` at `base - 16` are `str24`'s buffer exactly,
// which is the point: the shared half of the two layouts is byte-identical, and a
// plain array block *is* a `str` buffer (`array_layout` has the tags that tell the
// shapes apart).
//
// The two extra words of a drop block are where the unification stops, and the
// reason is destructors. A `str`'s content is bytes, so freeing a buffer is
// freeing memory; an array's elements may own heap of their own, so reaching
// refcount zero has to release each of them — which needs the element drop helper
// (`drop_fn`) and *how many elements there are*. The value's `len` cannot answer
// the second: a window's `len` is the window's, and dropping only the windowed
// elements would leak the rest. So `init` records the block's own initialized
// element count, which is what `release` walks, and it is a different number from
// the `len` any particular value carries.

/// Bytes of an array value — `str24::STR_SIZE`, and for the same reasons.
pub(crate) const ARR_SIZE: usize = 24;
/// The most element *bytes* that fit inline: `w0`, `w1`, and six bytes of `w2`.
pub(crate) const ARR_INLINE_CAP: usize = 22;

pub(crate) const ATAG_INLINE: u8 = 0b01;
pub(crate) const ATAG_REVERSED: u8 = 0b10;
/// A block value whose run is not (known to be) the whole of its block: what a
/// slice makes. Cleared by nothing — a window stays one, and is copied before it
/// is written through.
pub(crate) const ATAG_WINDOW: u8 = 0b11;

pub(crate) const ARR_LEN_MASK: u64 = (1 << 56) - 1;
pub(crate) const ARR_TAG_SHIFT: u32 = 56;
/// An inline value's element count lives in byte 22, as `str24`'s does.
pub(crate) const ARR_INLINE_LEN_SHIFT: u32 = 48;

// ---------- Element helpers for an array *of arrays* ----------
//
// An outer array's elements are themselves array values, and at refcount zero
// (or when the outer array's elements are copied) each one has to be released
// (or retained). An array, set or dict value is 24 bytes, its block in the first
// word, so these walk 24-byte slots; codegen chooses them in `array_drop_fn_addr`
// / `array_retain_fn_addr`.
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

// # The shapes of the value
//
// ```text
//            w0                          w1                 w2
// block      tagged pointer (any repr)   data: *const u8    [ len : 56 ][ tag:8 ]
// reversed   untagged heap block         data: *const u8    [ len : 56 ][ tag:8 ]
// inline     elems[0..8]                 elems[8..16]       [ elems[16..22] ][ len:8 ][ tag:8 ]
// ```
//
// A *block* value carries its run — `data` and `len` — whether it is the whole
// block or a window of it, so a reader never has to ask which: the
// elements are `data` and `len`, always. A whole value is the window that
// covers the block (`data` at the first element, `len` the block's), and the
// two differ by a tag: a window has `ATAG_WINDOW` above its length,
// a whole value has none. Something that is about to write through a value asks
// [`ArrValue::is_window`] — a window must be copied first, and a whole value
// need not — without reading the block, which keeps no length of its own to
// compare against.
//
// Zeroed memory is the empty array: no block, no run, a length of zero.
//
// A *reversed* value (`ATAG_REVERSED`) is a window walked from its far end:
// `data` and the length name a forward run of the block, exactly as a window's
// do, and element `i` of the value is element `len - 1 - i` of the run. That is
// why reversing allocates nothing — the view block a reversal used to build is
// gone. Reversing one again is the forward window it came from. A bit-packed `bool[]` has no byte run to walk,
// so reversing one copies.
//
// An *inline* value carries its elements in its own 22 bytes and owns no block
// at all — a short `str`'s layout exactly: the tag
// (`ATAG_INLINE`) in the top byte of `w2` says so, and the byte below it is the
// length. Only an array of integers is ever inline, at the packed stride, so 22
// bytes is 22 `u8`s or 2 `i64`s.
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
// Windows are only ever made over a block of byte-addressable elements. A
// bit-packed `bool[]` has no byte to point at (its whole value still carries a
// `data` and a length, but the run is bits, which only the readers that know it is
// a `bool[]` walk), so slicing one *copies* the window into a block of its own and
// yields a whole value around it. (A reversed value is sliced into a reversed
// value — see below.)
//
// Every entry takes the value and reads its run, so a slice costs nothing until
// something has to write through it, and then only the copy an in-place write
// would have made on its first write anyway (`aipl_arr_settle`).

/// An array value as compiled code holds it — see above.
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
    pub(crate) fn whole_of(block: *const u8, len: usize) -> ArrValue {
        ArrValue {
            block,
            data: if block.is_null() {
                core::ptr::null()
            } else {
                unsafe {
                    super::array_layout::arr_untag(block).add(super::array_layout::ARR_ELEMS_OFFSET)
                }
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

    /// The empty array.
    pub(crate) fn empty() -> ArrValue {
        ArrValue::whole_of(super::array_layout::arr_inline_empty(), 0)
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

    /// The run of a value that is not inline: `data` and the length (a reversed
    /// value's is the forward run it walks).
    fn run(&self) -> (*const u8, usize) {
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
        unsafe {
            let dst = super::array_layout::arr_untag(block) as *mut u8;
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
        unsafe {
            core::ptr::copy_nonoverlapping(
                raw.as_ptr(),
                super::array_layout::arr_untag(block) as *mut u8,
                n * elem_size as usize,
            )
        };
        block
    }
}

/// A fresh block of `n` elements copied from the forward run starting at `first`,
/// beginning `lo` elements in, each retained (bits copied one at a time for a
/// bit-packed `bool[]`). The caller owns the one reference.
fn copy_block(
    first: *const u8,
    lo: usize,
    n: usize,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let block = super::aipl_array_new(n as i64, drop_fn, elem_size);
    unsafe {
        let dst = super::array_layout::arr_untag(block) as *mut u8;
        if elem_size == super::array_layout::ELEM_BITPACKED {
            for i in 0..n {
                super::array_ops::write_packed_bit(dst, i, bit_at(first, lo + i));
            }
        } else {
            let w = elem_size as usize;
            core::ptr::copy_nonoverlapping(first.add(lo * w), dst, n * w);
            super::elem_rc(retain_fn, dst, n);
        }
    }
    block
}

/// A fresh block of the `n` elements of the forward run starting at `first`, in
/// reverse order, each retained — what reversing a bit-packed `bool[]` is, which
/// has no byte run for a reversed value to walk.
fn copy_block_reversed(
    first: *const u8,
    n: usize,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let block = super::aipl_array_new(n as i64, drop_fn, elem_size);
    unsafe {
        let dst = super::array_layout::arr_untag(block) as *mut u8;
        if elem_size == super::array_layout::ELEM_BITPACKED {
            for i in 0..n {
                super::array_ops::write_packed_bit(dst, i, bit_at(first, n - 1 - i));
            }
        } else {
            let w = elem_size as usize;
            for i in 0..n {
                core::ptr::copy_nonoverlapping(first.add((n - 1 - i) * w), dst.add(i * w), w);
            }
            super::elem_rc(retain_fn, dst, n);
        }
    }
    block
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
            ArrValue::empty()
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
        ArrValue::empty()
    } else if lo == 0 && n == len {
        // All of it: the same value, a window or not.
        super::aipl_arr_inc(a.block);
        a
    } else if elem_size == super::array_layout::ELEM_BITPACKED {
        // Bits have no byte to point at: copy them.
        ArrValue::whole_of(copy_block(first, lo, n, drop_fn, retain_fn, elem_size), n)
    } else {
        super::aipl_arr_inc(a.block);
        ArrValue::window(a.block, unsafe { first.add(lo * elem_size as usize) }, n)
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
            ArrValue::empty()
        } else if elem_size == super::array_layout::ELEM_BITPACKED {
            ArrValue::whole_of(
                copy_block_reversed(first, len, drop_fn, retain_fn, elem_size),
                len,
            )
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
fn lend_block(a: &ArrValue, drop_fn: i64, retain_fn: i64, elem_size: i64) -> *const u8 {
    if a.is_inline() {
        return a.inline_to_block(elem_size);
    }
    if a.is_reversed() {
        return a.run_to_block(drop_fn, retain_fn, elem_size);
    }
    let (first, len) = a.run();
    copy_block(first, 0, len, drop_fn, retain_fn, elem_size)
}

/// Turn the window — a reversed one, or an inline value — an *owning* slot holds
/// into a whole value over a copy of its elements, in place, so that code which
/// mutates through it finds a block of its own there: uniquely owned, so the
/// write that follows goes in place. A window's reference moves from the source
/// block to the copy; an inline value held none. A whole value is left alone.
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
        unsafe { core::ptr::write(a, ArrValue::whole_of(own, v.inline_len())) };
        return;
    }
    if !v.is_window() && !v.is_reversed() {
        return;
    }
    let own = lend_block(&v, drop_fn, retain_fn, elem_size);
    super::aipl_array_dec(v.block);
    unsafe { core::ptr::write(a, ArrValue::whole_of(own, v.run_len())) };
}

// # Entries that take values
//
// A block keeps no length, and a window, a reversal or an inline value has no
// block of its own to lend, so every entry takes the value and reads `data` and
// the length off it, whatever its shape.

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

/// Append every element of the array value `src` to the block `dst`, which holds
/// `dst_len` elements and which [`array_ops::reserve`] has already made uniquely
/// owned and large enough — a copy plus one retain pass, never a reallocation.
/// *Borrows* `src` (the caller keeps its reference, and pins the block when `src`
/// might be `dst`'s own). Returns `dst`, which then holds `dst_len` plus however
/// many `src` has. A bit-packed `bool[]` has no reserved form and appends bit by
/// bit through the copying push, so the block it returns may be another.
fn extend_into(
    dst: *const u8,
    dst_len: usize,
    src: &ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    if elem_size == super::array_layout::ELEM_BITPACKED {
        let (first, _, add) = elements(src, 0);
        let mut out = dst;
        for i in 0..add {
            let bit = i64::from(bit_at(first, i));
            out = super::array_ops::push(
                out,
                dst_len + i,
                &bit as *const i64 as *const u8,
                drop_fn,
                retain_fn,
                elem_size,
            );
        }
        return out;
    }
    let w = elem_size as usize;
    let (first, step, add) = elements(src, w);
    if add == 0 {
        return dst;
    }
    unsafe {
        let at = (super::array_layout::arr_untag(dst) as *mut u8).add(dst_len * w);
        copy_run(first, step, add, w, at);
        super::elem_rc(retain_fn, at, add);
        super::array_layout::set_block_init(dst, dst_len + add);
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
    let elems = super::array_layout::arr_untag(block) as *mut u8;
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
pub(crate) extern "C" fn aipl_arr_sort(
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
        unsafe { core::ptr::write(out, ArrValue::whole_of(block, 0)) };
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
        core::ptr::write(out, ArrValue::whole_of(block, n));
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
pub(crate) extern "C" fn aipl_arr_join(
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
pub(crate) extern "C" fn aipl_str_join(
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

/// The block and length of the value at `a`, whose reference the call consumes: a
/// whole value's own, or a fresh copy of a window's, a reversal's or an inline
/// value's elements (the window's reference on its source released).
fn consume(a: *const ArrValue, drop_fn: i64, retain_fn: i64, elem_size: i64) -> (*const u8, usize) {
    let mut v = unsafe { *a };
    aipl_arr_settle(&mut v, drop_fn, retain_fn, elem_size);
    (v.block, v.run_len())
}

/// `push` for a value whose reference it consumes: the value at `a`, grown by
/// the element at `x`, written to `out`. In place when `a`'s block is uniquely
/// owned and has room (`array_ops::push_mut`, which copies one that is not); a
/// bit-packed `bool[]` takes the copying push.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_push_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let (block, len) = consume(a, drop_fn, retain_fn, elem_size);
    let grown = if elem_size == super::array_layout::ELEM_BITPACKED {
        super::array_ops::push(block, len, x, drop_fn, retain_fn, elem_size)
    } else {
        super::array_ops::push_mut(block, len, x, drop_fn, retain_fn, elem_size)
    };
    unsafe { core::ptr::write(out, ArrValue::whole_of(grown, len + 1)) };
}

/// `push` onto a `mut` binding's slot. `exclusive` says the binding is
/// statically unaliased, so the push may write into its block in place;
/// otherwise a version of it may be borrowed somewhere, and the push copies.
/// `inline_ok` says the element is an integer, so the value may be (or become)
/// inline, which [`aipl_arr_push_small`] handles.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_push(
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
    let (block, len) = consume(slot, drop_fn, retain_fn, elem_size);
    // In place — a bit-packed `bool[]` included, whose in-place push sets the
    // next bit — unlike [`aipl_arr_push_from`], whose copying `bool[]` push is
    // for a spread's accumulator, which `bool[]` never reserves.
    let grown = if exclusive != 0 {
        super::array_ops::push_mut(block, len, x, drop_fn, retain_fn, elem_size)
    } else {
        super::array_ops::push(block, len, x, drop_fn, retain_fn, elem_size)
    };
    unsafe { core::ptr::write(slot, ArrValue::whole_of(grown, len + 1)) };
}

/// `reserve` for a value whose reference it consumes: room for `extra` more
/// elements, written to `out` (`array_ops::reserve`, which keeps a unique block
/// and copies any other).
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_reserve_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    extra: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let (block, len) = consume(a, drop_fn, retain_fn, elem_size);
    let grown = super::array_ops::reserve(block, len, extra, drop_fn, retain_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(grown, len)) };
}

/// `extend` for a value whose reference it consumes: the value at `a` with every
/// element of the value at `src` (borrowed, read in place) appended, written to
/// `out`. Reserved first, so the append is one copy; a bit-packed `bool[]` has no
/// reserved form and appends bit by bit (see [`extend_into`]). The caller pins
/// `src`'s block across the call when it may be `a`'s own, which is what makes
/// the reserve copy before anything is written.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_extend_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    src: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let (block, len) = consume(a, drop_fn, retain_fn, elem_size);
    let bits = elem_size == super::array_layout::ELEM_BITPACKED;
    let src = unsafe { &*src };
    let add = elements(src, if bits { 0 } else { elem_size as usize }).2;
    let grown = if bits {
        block
    } else {
        super::array_ops::reserve(block, len, add as i64, drop_fn, retain_fn, elem_size)
    };
    let extended = extend_into(grown, len, src, drop_fn, retain_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(extended, len + add)) };
}

/// An array literal's `..src` splice (`__aipl_arr_concat`): the value at `a`,
/// whose reference the call consumes, with every element of the value at `src`
/// (borrowed, read in place) appended, written to `out`. Unlike
/// [`aipl_arr_extend_from`] it does not reserve: `a` is the literal's
/// accumulator, which the literal's own `__aipl_arr_reserve` already made
/// uniquely owned and sized for every element. Its refcount may still read
/// above one (the accumulator binding's bookkeeping), and a reserve here would
/// take that for sharing and copy into a block with no room left for the
/// elements after the splice.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_splice_from(
    out: *mut ArrValue,
    a: *const ArrValue,
    src: *const ArrValue,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) {
    let (block, len) = consume(a, drop_fn, retain_fn, elem_size);
    let src = unsafe { &*src };
    let add = elements(
        src,
        if elem_size == super::array_layout::ELEM_BITPACKED {
            0
        } else {
            elem_size as usize
        },
    )
    .2;
    let extended = extend_into(block, len, src, drop_fn, retain_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(extended, len + add)) };
}

/// The address of element `idx` of the array value at `a`, whatever its shape
/// (`elem_size` is the stride in bytes). The caller has bounds-checked `idx`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_elem_ptr(
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
/// owned block (`array_ops::reserve` with nothing extra keeps a unique block and
/// copies a shared or static one).
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_writable(
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
    aipl_arr_reserve_from(out, a, 0, drop_fn, retain_fn, elem_size);
}

// ## Sets and dicts
//
// A set or a dict value is always whole — never a window, a reversal or inline —
// so its run is its block's `init` elements, and `array_ops` works on the block
// and the length the value carries. The slot forms consume the reference a `mut`
// binding (or a literal under construction) holds and write the result back;
// `union` consumes both operands.

/// Whether the set value at `s` contains the element at `x`.
#[no_mangle]
pub(crate) extern "C" fn aipl_set_contains(
    s: *const ArrValue,
    x: *const u8,
    elem_size: i64,
    str_cmp: i64,
) -> i64 {
    let s = unsafe { &*s };
    i64::from(super::array_ops::set_contains(
        s.block,
        s.run_len(),
        x,
        elem_size,
        str_cmp,
    ))
}

/// Insert the element at `x` into the set the slot holds (a uniquely owned
/// one), writing the possibly relocated set back.
#[no_mangle]
pub(crate) extern "C" fn aipl_set_insert(
    slot: *mut ArrValue,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
) {
    unsafe {
        let v = *slot;
        let (set, len) = super::array_ops::set_insert(
            v.block,
            v.run_len(),
            x,
            drop_fn,
            retain_fn,
            elem_size,
            str_cmp,
            order,
        );
        core::ptr::write(slot, ArrValue::whole_of(set, len));
    }
}

/// `a.union(b)` into `out`, consuming both: a fresh set, or — `in_place` — `a`'s
/// own block grown, which the caller has made unique.
#[no_mangle]
pub(crate) extern "C" fn aipl_set_union(
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
        let (a, b) = (*a, *b);
        let (set, len) = if in_place != 0 {
            super::array_ops::set_union_mut(
                a.block,
                a.run_len(),
                b.block,
                b.run_len(),
                drop_fn,
                retain_fn,
                elem_size,
                str_cmp,
                order,
            )
        } else {
            super::array_ops::set_union(
                a.block,
                a.run_len(),
                b.block,
                b.run_len(),
                drop_fn,
                retain_fn,
                elem_size,
                str_cmp,
                order,
            )
        };
        // `b` is consumed; so is `a`, unless it was grown into the result.
        super::aipl_array_dec(b.block);
        if in_place == 0 {
            super::aipl_array_dec(a.block);
        }
        core::ptr::write(out, ArrValue::whole_of(set, len));
    }
}

/// The address of the value bound to the key at `key` in the dict value at `d`,
/// or null.
#[no_mangle]
pub(crate) extern "C" fn aipl_dict_get(
    d: *const ArrValue,
    key: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    let d = unsafe { &*d };
    super::array_ops::dict_get(d.block, d.run_len(), key, pair_size, str_cmp)
}

/// Whether the dict value at `d` binds the key at `key`.
#[no_mangle]
pub(crate) extern "C" fn aipl_dict_contains_key(
    d: *const ArrValue,
    key: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> i64 {
    let d = unsafe { &*d };
    i64::from(super::array_ops::dict_contains_key(
        d.block,
        d.run_len(),
        key,
        pair_size,
        str_cmp,
    ))
}

/// Bind the pair at `pair` in the dict the slot holds (a uniquely owned one),
/// writing the possibly relocated dict back.
#[no_mangle]
pub(crate) extern "C" fn aipl_dict_insert(
    slot: *mut ArrValue,
    pair: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) {
    unsafe {
        let v = *slot;
        let (dict, len) = super::array_ops::dict_insert(
            v.block,
            v.run_len(),
            pair,
            drop_fn,
            retain_fn,
            pair_size,
            str_cmp,
        );
        core::ptr::write(slot, ArrValue::whole_of(dict, len));
    }
}

/// `insert_key` on a `mut` binding's slot: the dict made uniquely owned
/// (`array_ops::reserve` with nothing extra, which keeps a unique block and
/// copies a shared one), the pair bound in it, and the dict written back. The
/// pair replaces an existing binding of its key in place, and is appended
/// otherwise.
#[no_mangle]
pub(crate) extern "C" fn aipl_dict_set(
    slot: *mut ArrValue,
    pair: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) {
    unsafe {
        let v = *slot;
        let len = v.run_len();
        let dict = super::array_ops::reserve(v.block, len, 0, drop_fn, retain_fn, pair_size);
        let (dict, len) =
            super::array_ops::dict_insert(dict, len, pair, drop_fn, retain_fn, pair_size, str_cmp);
        core::ptr::write(slot, ArrValue::whole_of(dict, len));
    }
}

/// `remove_key` on a `mut` binding's slot: the dict made uniquely owned
/// (`array_ops::reserve` with nothing extra, which keeps a unique block and
/// copies a shared one), the key's pair removed, and the dict written back.
#[no_mangle]
pub(crate) extern "C" fn aipl_dict_remove(
    slot: *mut ArrValue,
    key: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) {
    unsafe {
        let v = *slot;
        let len = v.run_len();
        let dict = super::array_ops::reserve(v.block, len, 0, drop_fn, retain_fn, pair_size);
        let removed = super::array_ops::dict_remove(dict, len, key, drop_fn, pair_size, str_cmp);
        core::ptr::write(slot, ArrValue::whole_of(dict, len - usize::from(removed)));
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
        unsafe {
            let dst = super::array_layout::arr_untag(block) as *mut u8;
            core::ptr::copy_nonoverlapping(raw.as_ptr(), dst, n * w);
            core::ptr::copy_nonoverlapping(x, dst.add(n * w), w);
            core::ptr::write(slot, ArrValue::whole_of(block, n + 1));
        }
        return PUSH_SMALL_GREW;
    }
    if v.is_window() || v.is_reversed() {
        aipl_arr_settle(slot, 0, 0, width);
        return PUSH_SMALL_NOT_HANDLED;
    }
    if v.run_len() == 0 && unsafe { super::array_layout::block_cap_bytes(v.block) } <= 8 * room {
        super::aipl_array_dec(v.block);
        unsafe { core::ptr::write(slot, ArrValue::inline(elem, 1)) };
        return PUSH_SMALL_INLINE;
    }
    PUSH_SMALL_NOT_HANDLED
}

/// The set `set`'s elements as a fresh array of `width`-byte elements, written to
/// `out` — `to_array` on a set of a narrow integer. A set keeps a word per scalar
/// and an array packs a narrow integer at its own width, so the two stop sharing
/// a block there; each word's low bytes are the element. (Every other element
/// type is laid out alike in both, and `to_array` shares the block.) Borrows the
/// set value; the result carries one reference.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_from_set_words(
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

// # Allocation that hands back a value
//
// Compiled code that wants a new array asks for a value and writes its elements
// at the value's `data`, rather than taking a block and adding the layout's
// element offset itself. Which is why the layout can change underneath it.

/// A fresh array of `len` elements, uninitialized, written to `out` (the value's
/// `data` is where the elements go).
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_new(out: *mut ArrValue, len: i64, drop_fn: i64, elem_size: i64) {
    let n = len.max(0) as usize;
    let block = super::aipl_array_new(n as i64, drop_fn, elem_size);
    unsafe { core::ptr::write(out, ArrValue::whole_of(block, n)) };
}

/// An empty array with room for `cap` elements, written to `out`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_with_cap(
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
pub(crate) extern "C" fn aipl_str_split(
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
pub(crate) extern "C" fn aipl_arr_truncate(a: *mut ArrValue, len: i64) {
    let v = unsafe { &mut *a };
    if v.is_inline() {
        let mut raw = v.bytes();
        raw[ARR_INLINE_CAP] = len as u8;
        unsafe { core::ptr::write(a, core::ptr::read(raw.as_ptr() as *const ArrValue)) };
        return;
    }
    v.len = len as usize;
    unsafe { super::array_layout::set_block_init(v.block, len as usize) };
}

/// Make the element drop helper of the block under the array value `a` `drop_fn`:
/// the in-place `map` from `T` to `U`, whose elements the block now holds. (A
/// plain block has none to set, and the in-place gate keeps it to element types
/// that agree on whether they need one.)
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_set_drop(a: *const ArrValue, drop_fn: i64) {
    let v = unsafe { &*a };
    if v.is_inline() {
        return;
    }
    unsafe { super::array_layout::set_block_drop_fn(v.block, drop_fn) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The elements `count_drop` was asked to release, across the unit tests that
    /// use it (each uses its own counter).
    fn dropped(counter: &core::sync::atomic::AtomicI64) -> i64 {
        counter.load(core::sync::atomic::Ordering::SeqCst)
    }

    static WINDOW_DROPS: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

    extern "C" fn count_window_drops(_elems: *const u8, n: i64) {
        WINDOW_DROPS.fetch_add(n, core::sync::atomic::Ordering::SeqCst);
    }

    /// A plain array block is a `str` buffer — `[cap][refcount][content]` — and a
    /// drop block is that with two words of prefix, told apart by the tag.
    #[test]
    fn a_plain_block_is_a_buffer_and_a_drop_block_is_one_with_a_prefix() {
        use super::super::array_layout::*;
        let plain = super::super::array_ops::alloc_block(0, 4, 0, 8);
        assert!(arr_repr(plain) == ArrRepr::Plain);
        let base = arr_untag(plain);
        unsafe {
            assert_eq!(super::super::buffer::cap(base), 32);
            assert_eq!(*super::super::buffer::refcount(base), 1);
            assert_eq!(block_drop_fn(plain), 0);
        }
        super::super::array_ops::release(plain);

        let drop_fn = count_window_drops as extern "C" fn(*const u8, i64) as usize as i64;
        let dropping = super::super::array_ops::alloc_block(2, 4, drop_fn, 8);
        assert!(arr_repr(dropping) == ArrRepr::Drop);
        unsafe {
            assert_eq!(super::super::buffer::cap(arr_untag(dropping)), 32);
            assert_eq!(block_init(dropping), 2);
            assert_eq!(block_drop_fn(dropping), drop_fn);
        }
        let before = dropped(&WINDOW_DROPS);
        super::super::array_ops::release(dropping);
        assert_eq!(dropped(&WINDOW_DROPS) - before, 2);
    }

    /// Releasing walks the block's own `init` elements, not the `len` of whichever
    /// value let go last: a window over one element of three must not leak the
    /// other two when it is the last reference.
    #[test]
    fn a_window_releases_every_element_of_its_block() {
        let drop_fn = count_window_drops as extern "C" fn(*const u8, i64) as usize as i64;
        let block = super::super::aipl_array_new(3, drop_fn, 8);
        let whole = ArrValue::whole_of(block, 3);
        let mut out = ArrValue::empty();
        aipl_arr_window(&mut out, &whole, 1, 2, drop_fn, 0, 8);
        assert!(out.is_window());
        assert_eq!(out.run_len(), 1);
        let before = dropped(&WINDOW_DROPS);
        // The whole value lets go first: the window keeps the block alive.
        super::super::aipl_array_dec(whole.block);
        assert_eq!(dropped(&WINDOW_DROPS) - before, 0);
        // The window is the last reference, and the block frees all three.
        super::super::aipl_array_dec(out.block);
        assert_eq!(dropped(&WINDOW_DROPS) - before, 3);
    }

    /// A whole value carries its run like any window; a window is told
    /// by its tag, not by comparing against a block that keeps no length.
    #[test]
    fn a_whole_value_carries_its_run() {
        let root = super::super::array_layout::ARR_ELEMS_OFFSET;
        let block = super::super::aipl_array_new(4, 0, 8);
        let whole = ArrValue::whole_of(block, 4);
        assert_eq!(
            whole.data as usize,
            super::super::array_layout::arr_untag(block) as usize + root
        );
        assert_eq!(whole.len, 4);
        assert!(!whole.is_window());

        // A slice is tagged a window whatever it covers, so the block need not
        // say how long it is: a prefix and a suffix both are.
        let prefix = ArrValue::window(block, whole.data, 3);
        let suffix = ArrValue::window(block, unsafe { whole.data.add(8) }, 3);
        assert!(prefix.is_window());
        assert!(suffix.is_window());

        // The empty array and the zeroed value are both empty and neither a window.
        let empty = ArrValue::empty();
        assert_eq!(empty.len, 0);
        assert!(!empty.is_window());
        let zeroed = ArrValue::whole_of(core::ptr::null(), 0);
        assert_eq!(elements(&zeroed, 8).2, 0);
        super::super::aipl_array_dec(block);
    }

    /// The stage-4b bridge over the old block: a slice is a window sharing its
    /// source, a window of a window points into the same block, and a block is
    /// lent only on demand. The source's refcount tells the whole story.
    #[test]
    fn a_window_shares_its_block_and_lends_one_on_demand() {
        let root = super::super::array_layout::ARR_ELEMS_OFFSET;
        let refcount = |b: *const u8| unsafe {
            *super::super::buffer::refcount(super::super::array_layout::arr_untag(b))
        };
        let block = super::super::aipl_array_new(5, 0, 8);
        for i in 0..5 {
            unsafe {
                core::ptr::write(
                    super::super::array_layout::arr_untag(block).add(root + i * 8) as *mut i64,
                    10 + i as i64,
                )
            };
        }
        let whole = ArrValue::whole_of(block, 5);
        let mut out = ArrValue::empty();

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
        assert_eq!(out.run_len(), 0);

        // A block is lent on demand: a copy of exactly the window's elements,
        // which leaves the source's count alone.
        let copy = lend_block(&inner, 0, 0, 8);
        assert_ne!(copy, block);
        assert_eq!(
            unsafe { *(super::super::array_layout::arr_untag(copy).add(root) as *const i64) },
            12
        );
        assert_eq!(
            unsafe { *(super::super::array_layout::arr_untag(copy).add(root + 8) as *const i64) },
            13
        );
        assert_eq!(refcount(block), 3);
        super::super::aipl_array_dec(copy);

        // Settling moves the slot's reference from the source to a copy of its
        // own, which nothing else holds.
        let mut slot = mid;
        aipl_arr_settle(&mut slot, 0, 0, 8);
        assert!(!slot.is_window());
        assert_eq!(slot.run_len(), 3);
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
        let refcount = |b: *const u8| unsafe {
            *super::super::buffer::refcount(super::super::array_layout::arr_untag(b))
        };
        let elem = |b: *const u8, i: usize| unsafe {
            *(super::super::array_layout::arr_untag(b).add(root + i * 8) as *const i64)
        };
        let block = super::super::aipl_array_new(4, 0, 8);
        for i in 0..4 {
            unsafe {
                core::ptr::write(
                    super::super::array_layout::arr_untag(block).add(root + i * 8) as *mut i64,
                    10 + i as i64,
                )
            };
        }
        let whole = ArrValue::whole_of(block, 4);
        let mut out = ArrValue::empty();

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
        let lent = lend_block(&mid, 0, 0, 8);
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
