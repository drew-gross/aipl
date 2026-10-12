// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// Operations on an array *block* (`array_layout`), written once for both
// runtimes. A block has no length — the value does (`arr24::ArrValue`) — so every
// function here takes the length of the array it is given, and says in its doc
// what the result's length is.
//
// The reference counting, the capacity and the allocation of a block are
// `buffer.rs`, which a `str`'s bytes share — a plain block is that buffer as it
// stands, and a drop block is one with two words of prefix. What differs between
// the two hosts comes in from the parent module:
//
// - `elem_rc(fn_ptr, at, count)`: run an element retain or release helper.
// - `aipl_array_with_cap`, which the set operations size their result with (the
//   AOT runtime counts calls to it).
//
// # Ownership
//
// The same protocol the runtimes have always had: a function that returns a block
// *consumes* its array argument's reference ("consumes `a`") and hands back one it
// owns. Elements the new block copies are retained through `retain_fn`.

use super::array_layout::*;
use super::buffer;

type DropFn = extern "C" fn(*const u8, i64);

fn is_static(a: *const u8) -> bool {
    unsafe { buffer::is_static(arr_untag(a)) }
}

/// The empty array standing in for a null block, which a few callers still hand
/// over for "nothing yet".
fn or_empty(a: *const u8) -> *const u8 {
    if a.is_null() {
        arr_inline_empty()
    } else {
        a
    }
}

/// The address of element `i` of the block `a`, `stride` bytes apart.
pub(crate) unsafe fn elem_ptr(a: *const u8, i: usize, stride: usize) -> *const u8 {
    unsafe { arr_untag(a).add(ARR_ELEMS_OFFSET + i * stride) }
}

/// Bit `i` of a bit-packed block.
pub(crate) unsafe fn bit_of(a: *const u8, i: usize) -> bool {
    unsafe { (*arr_untag(a).add(ARR_ELEMS_OFFSET + (i >> 3)) >> (i & 7)) & 1 != 0 }
}

/// Write bit `idx` of a bit-packed data region.
pub(crate) unsafe fn write_packed_bit(data: *mut u8, idx: usize, val: bool) {
    unsafe {
        let byte = data.add(idx >> 3);
        let mask = 1u8 << (idx & 7);
        if val {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Allocate a block with room for `cap` elements of `elem_size` bytes, refcount 1,
/// owning `init` elements (the caller fills them in). A nonzero `drop_fn` makes
/// it a drop block; zero, a plain one.
pub(crate) fn alloc_block(init: usize, cap: usize, drop_fn: i64, elem_size: i64) -> *const u8 {
    let cap_bytes = cap_bytes_for(elem_size, cap);
    unsafe {
        if drop_fn == 0 {
            return (buffer::alloc(0, cap_bytes) as usize | ARR_PLAIN_TAG) as *const u8;
        }
        let base = buffer::alloc(ARR_DROP_PREFIX, cap_bytes);
        core::ptr::write(base.sub(ARR_INIT_BACK) as *mut i64, init as i64);
        core::ptr::write(base.sub(ARR_DROPFN_BACK) as *mut i64, drop_fn);
        (base as usize | ARR_DROP_TAG) as *const u8
    }
}

/// Take a reference on the block `a` (a null block, the empty array and a static
/// block carry none to take).
pub(crate) fn retain(a: *const u8) {
    if a.is_null() {
        return;
    }
    unsafe { buffer::retain(arr_untag(a)) };
}

/// Release a reference on the block `a`; at zero, release the elements it owns
/// and free it.
pub(crate) fn release(a: *const u8) {
    if a.is_null() {
        return;
    }
    unsafe {
        if buffer::release(arr_untag(a)) {
            free_block(a);
        }
    }
}

/// Release a dead block's elements and free it.
unsafe fn free_block(a: *const u8) {
    unsafe {
        let repr = arr_repr(a);
        debug_assert!(
            matches!(repr, ArrRepr::Plain | ArrRepr::Drop),
            "freeing a block we do not own"
        );
        let drop_fn = block_drop_fn(a);
        if drop_fn != 0 {
            let f: DropFn = core::mem::transmute(drop_fn);
            f(arr_untag(a), block_init(a) as i64);
        }
        buffer::free(arr_untag(a), block_prefix_bytes(repr));
    }
}

/// `a` with room for `new_cap_bytes` of elements, the same block grown in
/// place when the allocator can and moved with its header and elements when it
/// cannot (no element is re-retained: the block is the one owner). `a` is uniquely
/// owned and not static.
fn grow(a: *const u8, new_cap_bytes: usize) -> *const u8 {
    unsafe {
        let base = buffer::grow(arr_untag(a), block_prefix_bytes(arr_repr(a)), new_cap_bytes);
        (base as usize | (a as usize & ARR_TAG_MASK)) as *const u8
    }
}

/// A unique plain block `a` of `len` elements, as the drop block its elements now
/// need: they were scalars when it was made (an untyped `[]`, a reservation
/// before the element type was known) and are about to be heap values. Moves the
/// elements, which own nothing yet.
fn into_drop_shape(a: *const u8, len: usize, cap_bytes: usize, drop_fn: i64) -> *const u8 {
    unsafe {
        // `cap_bytes` of capacity: byte-sized elements, so the count is the size.
        let moved = alloc_block(len, cap_bytes, drop_fn, 1);
        core::ptr::copy_nonoverlapping(
            arr_untag(a),
            arr_untag(moved) as *mut u8,
            block_cap_bytes(a).min(cap_bytes),
        );
        buffer::free(arr_untag(a), 0);
        moved
    }
}

/// Copy-and-grow push (value semantics): a fresh block of `a`'s `len` elements
/// plus the element at `x`, then release `a`. Used when the array may be aliased.
/// `retain_fn` retains the copied elements (the new block co-owns them). Consumes
/// `a`; the result holds `len + 1`.
pub(crate) fn push(
    a: *const u8,
    len: usize,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = or_empty(a);
    let raw = alloc_block(len + 1, len + 1, drop_fn, elem_size);
    unsafe {
        let dst = arr_untag(raw) as *mut u8;
        if len > 0 {
            core::ptr::copy_nonoverlapping(arr_untag(a), dst, cap_bytes_for(elem_size, len));
            super::elem_rc(retain_fn, dst, len);
        }
        if elem_size == ELEM_BITPACKED {
            write_packed_bit(dst, len, core::ptr::read(x as *const i64) != 0);
        } else {
            let es = elem_stride(elem_size);
            let slot = dst.add(len * es);
            core::ptr::copy_nonoverlapping(x, slot, es);
            super::elem_rc(retain_fn, slot, 1);
        }
    }
    release(a);
    raw
}

/// In-place push for a uniquely owned array (codegen emits this only when its
/// static analysis proves the array is unaliased). Appends without copying when
/// there is spare capacity, else grows to a doubled capacity. A static block (a
/// literal, the empty array) cannot be written into and takes the copying push.
/// Consumes `a`; the result holds `len + 1`.
///
/// The refcount is deliberately *not* consulted beyond that. Static ownership —
/// an exclusive `mut` binding, an owned parameter — is what brought the caller
/// here, and inside a function a uniquely-owned block routinely reads two or
/// three: a `mut` parameter's slot and its entry track, the caller's reference
/// under the `set x = x.f(..)` writeback, all one owner's bookkeeping. Treating
/// those as sharing turned every such push into a copy and broke the accounting
/// around it. The one place a refcount above one *does* mean sharing is a call
/// boundary, and that is where it is checked: an owned parameter is made unique on
/// its way into `mut y = p` (`reserve`), as the in-place `map`/`filter` bodies
/// already do through `__arr_writable`.
pub(crate) fn push_mut(
    a: *const u8,
    len: usize,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = or_empty(a);
    if is_static(a) {
        return push(a, len, x, drop_fn, retain_fn, elem_size);
    }
    unsafe {
        if elem_size == ELEM_BITPACKED {
            // Bit-packed `bool[]`, in place: set bit `len`, growing the byte
            // capacity (doubling) only when the next bit needs a new byte.
            let cap_bytes = block_cap_bytes(a);
            let need = cap_bytes_for(ELEM_BITPACKED, len + 1);
            let a = if need <= cap_bytes {
                a
            } else {
                grow(a, need.max(cap_bytes * 2).max(1))
            };
            write_packed_bit(
                arr_untag(a) as *mut u8,
                len,
                core::ptr::read(x as *const i64) != 0,
            );
            return a;
        }
        let es = elem_stride(elem_size);
        let mut a = a;
        let cap_bytes = block_cap_bytes(a);
        // At capacity: grow to a doubled byte-capacity. The block moves with its
        // header and elements (refcounts unchanged), so no element is re-retained
        // and there is no old block to free — only the new element is retained.
        let grown_cap = if (len + 1) * es > cap_bytes {
            ((len + 1) * es).max(cap_bytes * 2)
        } else {
            cap_bytes
        };
        if drop_fn != 0 && arr_repr(a) == ArrRepr::Plain {
            // Its elements are heap values from here on, so it becomes a drop
            // block — at the grown capacity at once, rather than moved twice.
            a = into_drop_shape(a, len, grown_cap, drop_fn);
        } else if grown_cap != cap_bytes {
            a = grow(a, grown_cap);
        }
        let slot = (arr_untag(a) as *mut u8).add(len * es);
        core::ptr::copy_nonoverlapping(x, slot, es);
        super::elem_rc(retain_fn, slot, 1);
        // Keep the recorded count and drop helper in sync: an array reserved via
        // `aipl_array_with_cap` (or an empty `[]`) learns its element type here.
        set_block_init(a, len + 1);
        set_block_drop_fn(a, drop_fn);
        a
    }
}

/// Make `a` uniquely owned with room for `extra` more elements, so the appends an
/// array-literal spread emits afterwards are plain in-place writes. Two cases:
///
/// - refcount 1 (nothing else can observe the block): keep it. Already-large
///   enough capacity is returned untouched; otherwise grow to exactly what is
///   needed — the header and elements move with the block, so no element is
///   re-retained and there is no old block to free.
/// - shared (or static): allocate one right-sized block, copy the `len` elements
///   in and retain each (the source keeps its own refs), then release the input.
///
/// Either way the result has refcount 1 and capacity for `len + extra`, and still
/// holds `len` elements. Sizing is exact rather than doubling: the spread knows
/// its final length up front. Consumes `a`.
pub(crate) fn reserve(
    a: *const u8,
    len: usize,
    extra: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = or_empty(a);
    let want = len + extra.max(0) as usize;
    let need_bytes = cap_bytes_for(elem_size, want);
    unsafe {
        if buffer::is_unique(arr_untag(a)) {
            let mut a = a;
            let mut cap_bytes = block_cap_bytes(a);
            if drop_fn != 0 && arr_repr(a) == ArrRepr::Plain {
                a = into_drop_shape(a, len, cap_bytes.max(need_bytes), drop_fn);
                cap_bytes = block_cap_bytes(a);
            }
            if need_bytes > cap_bytes {
                a = grow(a, need_bytes);
            }
            set_block_drop_fn(a, drop_fn);
            return a;
        }
    }
    let raw = alloc_block(len, want.max(1), drop_fn, elem_size);
    unsafe {
        if len > 0 {
            let dst = arr_untag(raw) as *mut u8;
            core::ptr::copy_nonoverlapping(arr_untag(a), dst, cap_bytes_for(elem_size, len));
            super::elem_rc(retain_fn, dst, len);
        }
    }
    release(a);
    raw
}

// ---------- Sets ----------
//
// A set reuses the array block verbatim; only construction differs: elements are
// inserted deduplicated. Elements are i64/bool/char or `str`. Scalars compare by
// value (a bit-compare for a packed `bool`) and need no element drop/retain;
// `str` elements are compared by content, with the array's `str` drop/retain
// helpers recorded so the block frees/retains its strings like a `str[]`.

/// Whether the set `a` of `len` elements already contains the element at `x`.
/// `str_cmp != 0` compares `str` elements by content and carries their width (see
/// `str_cmp_width`); otherwise a bit-packed `bool` set (`elem_size == 0`) compares
/// unpacked bits and every other scalar set compares the 8-byte value.
pub(crate) fn set_contains(
    a: *const u8,
    len: usize,
    x: *const u8,
    elem_size: i64,
    str_cmp: i64,
) -> bool {
    if len == 0 {
        return false;
    }
    unsafe {
        if str_cmp == super::str24::STR_SIZE as i64 {
            // Wide `str` elements: the element *is* the 24-byte value, so there
            // is no pointer to load — compare the values in place.
            let target = core::ptr::read(x as *const super::str24::Str);
            (0..len).any(|i| {
                let s = core::ptr::read(
                    elem_ptr(a, i, super::str24::STR_SIZE) as *const super::str24::Str
                );
                super::str24::eq(s, target)
            })
        } else if elem_size == ELEM_BITPACKED {
            let target = core::ptr::read(x as *const i64) != 0;
            (0..len).any(|i| bit_of(a, i) == target)
        } else {
            let stride = elem_size.max(8) as usize;
            let target = core::ptr::read(x as *const i64);
            (0..len).any(|i| core::ptr::read(elem_ptr(a, i, stride) as *const i64) == target)
        }
    }
}

/// The unordered set's `order` argument: append.
pub(crate) const SET_ORDER_NONE: i64 = 0;

/// Where the element at `x` goes in the ordered set `a` of `len` elements, or
/// `None` when an equal element is already there: the index of the first element
/// that should follow it. `order`'s sign is the direction (positive smallest
/// first, negative largest first) and its magnitude the word comparison: `1`
/// signed, `2` unsigned. A `str` element is compared by content regardless (the
/// runtime asks `str_cmp` for that), so it only carries the sign. Linear — an
/// ordered set is small and its insert shifts the tail anyway, so a binary search
/// would not change the cost class.
unsafe fn ordered_set_position(
    a: *const u8,
    len: usize,
    x: *const u8,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
) -> Option<usize> {
    use core::cmp::Ordering;
    let width = elem_size.max(8) as usize;
    // `cmp(existing, new)` in the set's own direction: `Less` means the existing
    // element comes first.
    let cmp = |i: usize| -> Ordering {
        let ep = unsafe { elem_ptr(a, i, width) };
        let natural = if str_cmp == super::str24::STR_SIZE as i64 {
            let e = unsafe { core::ptr::read(ep as *const super::str24::Str) };
            let n = unsafe { core::ptr::read(x as *const super::str24::Str) };
            super::str24::cmp(e, n).cmp(&0)
        } else if order.abs() == 2 {
            let e = unsafe { core::ptr::read(ep as *const u64) };
            let n = unsafe { core::ptr::read(x as *const u64) };
            e.cmp(&n)
        } else {
            let e = unsafe { core::ptr::read(ep as *const i64) };
            let n = unsafe { core::ptr::read(x as *const i64) };
            e.cmp(&n)
        };
        if order < 0 {
            natural.reverse()
        } else {
            natural
        }
    };
    for i in 0..len {
        match cmp(i) {
            Ordering::Less => {}
            Ordering::Equal => return None,
            Ordering::Greater => return Some(i),
        }
    }
    Some(len)
}

/// Move the last of the `len` elements of `a` down to index `at`, shifting
/// `[at, len - 1)` up by one. Elements are plain bytes to this — an ordered set's
/// elements are words or wide `str` values, and moving a value moves its
/// ownership with it.
unsafe fn shift_last_to(a: *const u8, len: usize, at: usize, elem_size: i64) {
    let last = len - 1;
    if at >= last {
        return;
    }
    let width = elem_size.max(8) as usize;
    unsafe {
        let base = elem_ptr(a, at, width) as *mut u8;
        let mut moved = [0u8; 32];
        core::ptr::copy_nonoverlapping(elem_ptr(a, last, width), moved.as_mut_ptr(), width);
        core::ptr::copy(base, base.add(width), (last - at) * width);
        core::ptr::copy_nonoverlapping(moved.as_ptr(), base, width);
    }
}

/// Insert the element at `x` into `a` (a uniquely-owned set of `len` elements),
/// skipping it if already present. Returns the (possibly relocated) block and its
/// new length. For heap elements (`str`), `drop_fn`/`retain_fn` are the element
/// helpers so the block frees/retains its strings; for scalars they are 0.
///
/// `order` (see `SET_ORDER_NONE`) is what makes an ordered set: a non-zero order
/// keeps the block sorted, so the element is placed at its position and the tail
/// shifted up — an append followed by a move of the elements after it, so the
/// block grows exactly as an unordered insert does.
pub(crate) fn set_insert(
    a: *const u8,
    len: usize,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
) -> (*const u8, usize) {
    if order == SET_ORDER_NONE {
        if set_contains(a, len, x, elem_size, str_cmp) {
            return (a, len);
        }
        return (push_mut(a, len, x, drop_fn, retain_fn, elem_size), len + 1);
    }
    let at = match unsafe { ordered_set_position(a, len, x, elem_size, str_cmp, order) } {
        None => return (a, len),
        Some(i) => i,
    };
    let grown = push_mut(a, len, x, drop_fn, retain_fn, elem_size);
    unsafe { shift_last_to(grown, len + 1, at, elem_size) };
    (grown, len + 1)
}

/// The address to hand [`set_insert`] for element `i` of `src`, and a one-word
/// scratch backing it.
///
/// Every element wider than a word — a wide `str` among them — must be passed *in
/// place*: copying it into a local `i64` first keeps only its first word, which is
/// then read back as a whole value. Only the bit-packed `bool` case has no address
/// to give (its element is one bit), so that one is materialized into the scratch
/// and its address returned.
unsafe fn set_elem_ptr(src: *const u8, i: usize, elem_size: i64, scratch: &mut i64) -> *const u8 {
    if elem_size == ELEM_BITPACKED {
        *scratch = i64::from(unsafe { bit_of(src, i) });
        return scratch as *const i64 as *const u8;
    }
    unsafe { elem_ptr(src, i, elem_size.max(8) as usize) }
}

/// `a.union(b)` (copy): a fresh set with every distinct element of `a` then `b`,
/// and its length. Borrows both inputs, which the caller releases. Inserted
/// elements are retained by [`set_insert`], so they outlive the inputs' release.
pub(crate) fn set_union(
    a: *const u8,
    a_len: usize,
    b: *const u8,
    b_len: usize,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
) -> (*const u8, usize) {
    // Pre-size to the upper bound (|a| + |b|) so the insert loop never reallocs.
    let mut dest = super::aipl_array_with_cap((a_len + b_len) as i64, drop_fn, elem_size);
    let mut len = 0usize;
    for (src, n) in [(a, a_len), (b, b_len)] {
        for i in 0..n {
            let mut scratch = 0i64;
            let vp = unsafe { set_elem_ptr(src, i, elem_size, &mut scratch) };
            (dest, len) = set_insert(dest, len, vp, drop_fn, retain_fn, elem_size, str_cmp, order);
        }
    }
    (dest, len)
}

/// `set a = a.union(b)` for an exclusive `a` of `a_len` elements: extend `a` in
/// place with `b`'s distinct elements (reusing `a`'s allocation) and return the
/// (possibly relocated) block and its length. Borrows `b`; `a` is reused, not
/// released.
pub(crate) fn set_union_mut(
    a: *const u8,
    a_len: usize,
    b: *const u8,
    b_len: usize,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
    order: i64,
) -> (*const u8, usize) {
    let (mut a, mut len) = (a, a_len);
    for i in 0..b_len {
        let mut scratch = 0i64;
        let vp = unsafe { set_elem_ptr(b, i, elem_size, &mut scratch) };
        (a, len) = set_insert(a, len, vp, drop_fn, retain_fn, elem_size, str_cmp, order);
    }
    (a, len)
}

// ---------- Dicts ----------
//
// A dict is an array block of `[key][value]` pairs, `pair_size` bytes each.

/// The byte width of a dict key, which is also the offset of its value: a pair
/// is `[key][value]` laid out back to back. `str_cmp` carries the width for a
/// `str` key (see `str_cmp_width`) precisely so this is answerable without a
/// second argument; every other key is one word.
fn dict_key_width(str_cmp: i64) -> usize {
    if str_cmp != 0 {
        str_cmp as usize
    } else {
        8
    }
}

/// Index of the pair in the dict `a` of `len` pairs whose key matches the key at
/// `pair_ptr`, or -1. Keys compare by `str_cmp`: a 24-byte value compare for a
/// `str` key, else the raw 8-byte value.
unsafe fn dict_find(
    a: *const u8,
    len: usize,
    pair_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> i64 {
    let stride = pair_size as usize;
    let wide = str_cmp == super::str24::STR_SIZE as i64;
    let want_wide = if wide {
        unsafe { core::ptr::read(pair_ptr as *const super::str24::Str) }
    } else {
        super::str24::Str::empty()
    };
    let want = unsafe { core::ptr::read(pair_ptr as *const i64) };
    for i in 0..len {
        let ep = unsafe { elem_ptr(a, i, stride) };
        let eq = if wide {
            super::str24::eq(
                unsafe { core::ptr::read(ep as *const super::str24::Str) },
                want_wide,
            )
        } else {
            unsafe { core::ptr::read(ep as *const i64) == want }
        };
        if eq {
            return i as i64;
        }
    }
    -1
}

/// Insert (or, on a duplicate key, replace) the `[key][value]` pair at `pair_ptr`
/// into the uniquely-owned dict `a` of `len` pairs. `drop_fn`/`retain_fn` are the
/// pair helpers (they release/retain a pair's key *and* value). On a key
/// collision the whole existing pair is released and the new one stored in its
/// place (last-binding-wins); otherwise the pair is appended. Returns the
/// (possibly relocated) block and its length. The stored pair is retained, so the
/// caller keeps its originals.
pub(crate) fn dict_insert(
    a: *const u8,
    len: usize,
    pair_ptr: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) -> (*const u8, usize) {
    let idx = unsafe { dict_find(a, len, pair_ptr, pair_size, str_cmp) };
    if idx >= 0 {
        let stride = pair_size as usize;
        unsafe {
            let slot = elem_ptr(a, idx as usize, stride) as *mut u8;
            super::elem_rc(drop_fn, slot, 1); // release the old key+value
            core::ptr::copy_nonoverlapping(pair_ptr, slot, stride);
            super::elem_rc(retain_fn, slot, 1); // co-own the new key+value
        }
        return (a, len);
    }
    (
        push_mut(a, len, pair_ptr, drop_fn, retain_fn, pair_size),
        len + 1,
    )
}

/// Remove `key_ptr`'s pair from the dict `a` of `len` pairs, which `reserve` has
/// already made uniquely owned — so this writes in place.
///
/// The removed pair's key and value are released, and the pairs after it shift
/// down one so the survivors keep the order they were inserted in. Order-
/// preserving rather than swap-with-last: nothing *promises* a dict's order, but
/// it is what iterating one hands back, so a removal that quietly permuted the
/// rest would be a surprise for one saved move.
///
/// Returns whether a pair was removed (the dict then holds `len - 1`); when the
/// key was absent nothing is touched.
pub(crate) fn dict_remove(
    a: *const u8,
    len: usize,
    key_ptr: *const u8,
    drop_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) -> bool {
    unsafe {
        let idx = dict_find(a, len, key_ptr, pair_size, str_cmp);
        if idx < 0 {
            return false;
        }
        let idx = idx as usize;
        let stride = pair_size as usize;
        let slot = elem_ptr(a, idx, stride) as *mut u8;
        super::elem_rc(drop_fn, slot, 1);
        // The regions overlap when anything follows the hole, so `copy`
        // (`memmove`) rather than `copy_nonoverlapping`.
        let moved = len - idx - 1;
        if moved > 0 {
            core::ptr::copy(elem_ptr(a, idx + 1, stride), slot, moved * stride);
        }
        set_block_init(a, len - 1);
        true
    }
}

/// Look up `key_ptr` in the dict `a` of `len` pairs: a pointer to the matching
/// pair's value slot (its bytes are read/retained by the caller), or null if
/// absent.
pub(crate) fn dict_get(
    a: *const u8,
    len: usize,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    let idx = unsafe { dict_find(a, len, key_ptr, pair_size, str_cmp) };
    if idx < 0 {
        return core::ptr::null();
    }
    // The key occupies the first `dict_key_width` bytes of the pair; the value
    // follows immediately.
    unsafe { elem_ptr(a, idx as usize, pair_size as usize).add(dict_key_width(str_cmp)) }
}

/// Whether `key_ptr` is a key of the dict `a` of `len` pairs.
pub(crate) fn dict_contains_key(
    a: *const u8,
    len: usize,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> bool {
    unsafe { dict_find(a, len, key_ptr, pair_size, str_cmp) >= 0 }
}
