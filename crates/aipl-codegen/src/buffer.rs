// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The refcounted buffer — the allocation a `str` value's bytes live in, and the
// one a plain array block's elements do — written once, for both and for both
// runtimes.
//
// ```text
// [cap: i64][refcount: i64][content: cap bytes]
//                           ^ base — what a value points at
// ```
//
// `refcount` is at `base - 8` and `cap`, the content's size in *bytes*, at
// `base - 16`. Codegen's inlined appends read both, which is why the offsets are
// constants other files name. A buffer is freed without knowing what is in it:
// the capacity gives the size, which is why it is stored.
//
// A **drop** array block is this buffer with two more words in front of it,
// `[drop_fn][init]` (`array_layout`). The words are the buffer's *prefix*, and
// every function here that allocates, frees or grows takes how many bytes of it
// there are (zero for a `str`, and for a plain array block), so the part that
// counts references and sizes allocations is the same code whatever sits in front.
//
// The allocator is the host's: `super::rt_alloc`, `rt_realloc` and `rt_free`,
// which each runtime keeps so its allocation accounting (the AOT runtime's
// instrumented build tallies them for `aipl report`) stays its own.

/// `[cap][refcount]` — the prefix before a buffer's content.
pub(crate) const BUF_HEADER: usize = 16;
/// How far back from `base` the refcount word sits; the capacity is the word
/// before it, [`BUF_CAP_BACK`] back.
pub(crate) const BUF_REFCOUNT_BACK: usize = 8;
pub(crate) const BUF_CAP_BACK: usize = 16;
/// A static buffer's refcount, which `retain`/`release` never touch.
pub(crate) const STATIC_REFCOUNT: i64 = i64::MAX;

/// The refcount cell of the buffer (or any refcounted block laid out like one)
/// whose content starts at `base`.
pub(crate) unsafe fn refcount(base: *const u8) -> *mut i64 {
    unsafe { base.sub(BUF_REFCOUNT_BACK) as *mut i64 }
}

/// The capacity of the buffer at `base`, in bytes.
pub(crate) unsafe fn cap(base: *const u8) -> usize {
    unsafe { *(base.sub(BUF_CAP_BACK) as *const i64) as usize }
}

/// Record a new capacity for the buffer at `base`.
pub(crate) unsafe fn set_cap(base: *const u8, cap: usize) {
    unsafe { *(base.sub(BUF_CAP_BACK) as *mut i64) = cap as i64 };
}

/// Whether the buffer at `base` is somebody else's: its refcount is
/// [`STATIC_REFCOUNT`], so it is never written into, grown or freed here.
pub(crate) unsafe fn is_static(base: *const u8) -> bool {
    unsafe { *refcount(base) == STATIC_REFCOUNT }
}

/// Whether the buffer at `base` has exactly one reference. **Necessary but not
/// sufficient** for mutating in place: retain elision means a borrowed argument
/// can read `1` while its caller still holds the value, so this only ever refines
/// static ownership.
pub(crate) unsafe fn is_unique(base: *const u8) -> bool {
    unsafe { *refcount(base) == 1 }
}

/// Take a reference on the buffer at `base` (a static one has none to take).
pub(crate) unsafe fn retain(base: *const u8) {
    unsafe {
        let rc = refcount(base);
        if *rc != STATIC_REFCOUNT {
            *rc += 1;
        }
    }
}

/// Drop a reference on the buffer at `base`; whether it was the last, so the
/// caller must release what the buffer holds and [`free`] it. A static buffer's
/// count is never touched.
pub(crate) unsafe fn release(base: *const u8) -> bool {
    unsafe {
        let rc = refcount(base);
        if *rc == STATIC_REFCOUNT {
            return false;
        }
        *rc -= 1;
        *rc == 0
    }
}

/// Allocate `[prefix bytes][cap][refcount=1][content: cap bytes]`, returning the
/// `base` (the content). The prefix is the caller's to fill in.
pub(crate) fn alloc(prefix: usize, cap: usize) -> *mut u8 {
    let raw = unsafe { super::rt_alloc(prefix + BUF_HEADER + cap) } as *mut u8;
    assert!(!raw.is_null(), "out of memory");
    unsafe {
        let base = raw.add(prefix + BUF_HEADER);
        set_cap(base, cap);
        *refcount(base) = 1;
        base
    }
}

/// Free the buffer at `base`, which was allocated with this `prefix`.
pub(crate) unsafe fn free(base: *const u8, prefix: usize) {
    unsafe { super::rt_free(base.sub(prefix + BUF_HEADER) as *mut _) }
}

/// Grow the buffer at `base` to `cap` bytes under its own address where the
/// allocator can, keeping the prefix and the header — and so the refcount —
/// intact; returns the (possibly moved) `base`. Only sound for a buffer its
/// caller exclusively owns.
pub(crate) unsafe fn grow(base: *const u8, prefix: usize, cap: usize) -> *mut u8 {
    unsafe {
        let raw = super::rt_realloc(
            base.sub(prefix + BUF_HEADER) as *mut _,
            prefix + BUF_HEADER + cap,
        ) as *mut u8;
        assert!(!raw.is_null(), "out of memory");
        let base = raw.add(prefix + BUF_HEADER);
        set_cap(base, cap);
        base
    }
}
