// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The 24-byte `str` value — `STR_REPR.md`'s layout, implemented and tested on
// its own before anything is switched over to it.
//
// **Not wired up yet.** Stage 1 of that plan is an atomic change: the runtime,
// codegen, and every checked-in `.clif` have to agree on the layout, so there
// is no way to land it a piece at a time. What *can* be de-risked first is
// this: the layout itself, its invariants, and the operations that read and
// build it, proven by ordinary Rust tests with no compiler in the loop.
//
// # One copy, two runtimes
//
// The JIT runtime (`aipl-codegen`) and the AOT runtime
// (`aipl-linker/runtime/aipl_runtime.rs`) are kept byte-for-byte identical **by
// hand** today, and CLAUDE.md names the hazard: "write it twice, identically, or
// the AOT binaries diverge from the JIT". For the layout — the one thing where a
// divergence is silent memory corruption rather than a failing test — this file
// is shared instead: `aipl-codegen` has it as an ordinary module, and the AOT
// runtime `include!`s it inside a `mod str24`. So it compiles under `no_std` and
// must not reach for `std`, `Vec`, or `std::alloc`.
//
// Two things the host provides, and the reason the file needs no `cfg` of its
// own: `super::rt_alloc` / `super::rt_free` (so each runtime keeps its own
// allocation accounting — the AOT's instrumented build tallies them for
// `--- performance ---`), and the I/O wrappers, which differ genuinely (`std::io`
// against `libc`) and live in each host.
//
// # The value
//
// ```text
//               w0                  w1                  w2
// buffer   00   base: *const u8     data: *const u8     [ len : 56 ][ tag : 8 ]
// inline   01   content[0..8]       content[8..16]      [ content[16..22] ][ len:8 ][ tag:8 ]
// rope     10   node: *const u8     0                   [ len : 56 ][ tag : 8 ]
//          11   — spare —
// ```
//
// The tag is the **top byte** of `w2` in every representation, so classifying a
// value is one shift on a register the length already needed, and `base`/`data`
// are dereferenced without masking. See `STR_REPR.md` for why that beats
// low-bit tagging.
//
// # The buffer
//
// ```text
// [cap: i64][refcount: i64][content bytes ...]
//                           ^ base; `data` points anywhere in [base, base + cap]
// ```
//
// `refcount` stays at `base - 8`, shared with arrays exactly as today. The word
// at `base - 16` is **capacity**, not length: the value carries the only length
// there is, which is what lets one representation serve both "the whole string"
// and "a window into it". A buffer therefore does *not* imply a trailing NUL —
// every consumer is length-delimited.

/// Bytes of a `str` value.
pub(crate) const STR_SIZE: usize = 24;
/// The largest string that fits inline: `w0`, `w1`, and six bytes of `w2`.
pub(crate) const INLINE_CAP: usize = 22;

pub(crate) const TAG_BUFFER: u8 = 0b00;
pub(crate) const TAG_INLINE: u8 = 0b01;
pub(crate) const TAG_ROPE: u8 = 0b10;

/// `[cap][refcount]` — the prefix before a buffer's content. Same 16 bytes as
/// today's `[len][refcount]`, with the first word's meaning changed.
pub(crate) const BUF_HEADER: usize = 16;
/// A static buffer's refcount, which `retain`/`release` never touch.
pub(crate) const STATIC_REFCOUNT: i64 = i64::MAX;

const LEN_MASK: u64 = (1 << 56) - 1;
const TAG_SHIFT: u32 = 56;
/// Inline length lives in byte 22 — `w2`'s sixth byte.
const INLINE_LEN_SHIFT: u32 = 48;

/// A `str` value: three words, no indirection for buffer or inline.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Str {
    pub w0: u64,
    pub w1: u64,
    pub w2: u64,
}

impl Str {
    /// The empty string. Also what zeroed memory reads as — a buffer with a null
    /// base and zero length — which matters because sret buffers and fresh array
    /// slots arrive zero-filled.
    pub(crate) fn empty() -> Str {
        Str {
            w0: 0,
            w1: 0,
            w2: 0,
        }
    }

    pub(crate) fn tag(self) -> u8 {
        (self.w2 >> TAG_SHIFT) as u8
    }

    /// Content length, in bytes.
    pub(crate) fn len(self) -> usize {
        if self.tag() == TAG_INLINE {
            ((self.w2 >> INLINE_LEN_SHIFT) & 0xFF) as usize
        } else {
            (self.w2 & LEN_MASK) as usize
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The allocation this value refers to, or null when it owns nothing (an
    /// inline value, or the zeroed empty). A rope's node is *not* a buffer, so
    /// this is null for one; [`Str::owner`] is the refcount-bearing pointer.
    pub(crate) fn base(self) -> *const u8 {
        if self.tag() == TAG_BUFFER {
            self.w0 as *const u8
        } else {
            core::ptr::null()
        }
    }

    /// The refcounted allocation behind this value — a buffer's base or a rope's
    /// node — or null when there is none. This is the only word refcounting ever
    /// touches, which is why `retain`/`release` need `w0` and the tag, never
    /// `data`.
    pub(crate) fn owner(self) -> *const u8 {
        match self.tag() {
            TAG_BUFFER | TAG_ROPE => self.w0 as *const u8,
            _ => core::ptr::null(),
        }
    }

    /// A buffer's content bytes. Panics on any other representation — callers
    /// classify first (`STR_REPR.md`'s "classify + match" rule).
    fn buffer_bytes(self) -> &'static [u8] {
        debug_assert_eq!(self.tag(), TAG_BUFFER);
        if self.w1 == 0 {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.w1 as *const u8, self.len()) }
    }

    /// The value's bytes. An inline value is copied into `buf` (22 bytes of
    /// caller stack, no allocation); a rope is materialized **once** into its own
    /// node cache and read from there after, which is how the old runtime avoided
    /// re-flattening too.
    pub(crate) fn bytes<'a>(&'a self, buf: &'a mut [u8; INLINE_CAP]) -> &'a [u8] {
        match self.tag() {
            TAG_BUFFER => self.buffer_bytes(),
            TAG_INLINE => {
                let n = self.len();
                *buf = inline_bytes(*self);
                &buf[..n]
            }
            TAG_ROPE => rope_materialize(*self).buffer_bytes(),
            _ => unreachable!("tag {} is spare", self.tag()),
        }
    }

    /// `self[lo..hi]` — **free**: a window into the same allocation, no copy and
    /// no allocation at any length. Bounds are clamped like `aipl_str_slice`.
    /// Borrows `self`; the caller retains the result if it outlives the source.
    pub(crate) fn slice(self, lo: usize, hi: usize) -> Str {
        let len = self.len();
        let lo = lo.min(len);
        let hi = hi.clamp(lo, len);
        let n = hi - lo;
        match self.tag() {
            TAG_BUFFER => Str {
                w0: self.w0,
                w1: if self.w1 == 0 {
                    0
                } else {
                    (self.w1 as usize + lo) as u64
                },
                w2: meta(n, TAG_BUFFER),
            },
            TAG_INLINE => {
                let all = inline_bytes(self);
                from_bytes_inline(&all[lo..hi])
            }
            // A rope has no contiguous window to point at, so it materializes
            // (once — the node caches it) and the slice is a window into that.
            // Stage 5 is where slicing learns to descend to a leaf instead.
            _ => rope_materialize(self).slice(lo, hi),
        }
    }

    /// How many bytes could be appended in place, if this value owns its buffer
    /// exclusively (the ownership rule in `STR_REPR.md`, which this does *not*
    /// check — it is the capacity half of the question only).
    pub(crate) fn spare_capacity(self) -> usize {
        if self.tag() != TAG_BUFFER || self.w0 == 0 {
            return 0;
        }
        let end = self.w1 as usize + self.len();
        let cap_end = self.w0 as usize + buffer_cap(self.w0 as *const u8);
        cap_end.saturating_sub(end)
    }

    pub(crate) fn retain(self) {
        let owner = self.owner();
        if owner.is_null() {
            return;
        }
        unsafe {
            let rc = refcount_of(owner);
            if *rc != STATIC_REFCOUNT {
                *rc += 1;
            }
        }
    }

    pub(crate) fn release(self) {
        let owner = self.owner();
        if owner.is_null() {
            return;
        }
        unsafe {
            let rc = refcount_of(owner);
            if *rc == STATIC_REFCOUNT {
                return;
            }
            *rc -= 1;
            if *rc == 0 {
                match self.tag() {
                    TAG_BUFFER => free_buffer(owner),
                    TAG_ROPE => free_rope(owner),
                    _ => unreachable!("only owners are freed"),
                }
            }
        }
    }

    /// Whether this value is the sole reference to its allocation. **Necessary
    /// but not sufficient** for mutating in place: retain elision means a
    /// borrowed argument can read `1` while its caller still holds the value, so
    /// this only ever refines static ownership (`STR_REPR.md`).
    pub(crate) fn is_unique(self) -> bool {
        let owner = self.owner();
        !owner.is_null() && unsafe { *refcount_of(owner) } == 1
    }
}

fn meta(len: usize, tag: u8) -> u64 {
    debug_assert!(len as u64 <= LEN_MASK);
    (len as u64) | ((tag as u64) << TAG_SHIFT)
}

fn inline_bytes(s: Str) -> [u8; INLINE_CAP] {
    let mut out = [0u8; INLINE_CAP];
    out[0..8].copy_from_slice(&s.w0.to_le_bytes());
    out[8..16].copy_from_slice(&s.w1.to_le_bytes());
    out[16..22].copy_from_slice(&s.w2.to_le_bytes()[0..6]);
    out
}

fn from_bytes_inline(bytes: &[u8]) -> Str {
    debug_assert!(bytes.len() <= INLINE_CAP);
    let mut buf = [0u8; INLINE_CAP];
    buf[..bytes.len()].copy_from_slice(bytes);
    let mut w2 = [0u8; 8];
    w2[0..6].copy_from_slice(&buf[16..22]);
    w2[6] = bytes.len() as u8;
    w2[7] = TAG_INLINE;
    Str {
        w0: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        w1: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        w2: u64::from_le_bytes(w2),
    }
}

/// The three words of an inline literal — what codegen stores to materialize a
/// short `str` constant with no data object and no allocation.
pub(crate) fn inline_words(bytes: &[u8]) -> [u64; 3] {
    let s = from_bytes_inline(bytes);
    [s.w0, s.w1, s.w2]
}

/// The `meta` word of a buffer value of length `len` — the third word codegen
/// stores for a static literal.
pub(crate) fn buffer_meta(len: usize) -> u64 {
    meta(len, TAG_BUFFER)
}

/// Build a value holding `bytes`: inline when it fits, otherwise a fresh buffer
/// (refcount 1) with no spare capacity.
pub(crate) fn from_bytes(bytes: &[u8]) -> Str {
    if bytes.len() <= INLINE_CAP {
        return from_bytes_inline(bytes);
    }
    with_capacity(bytes.len(), bytes)
}

/// A fresh buffer with room for `cap` bytes, initialized with `init`.
pub(crate) fn with_capacity(cap: usize, init: &[u8]) -> Str {
    debug_assert!(init.len() <= cap);
    let base = alloc_buffer(cap);
    unsafe {
        core::ptr::copy_nonoverlapping(init.as_ptr(), base as *mut u8, init.len());
    }
    Str {
        w0: base as u64,
        w1: base as u64,
        w2: meta(init.len(), TAG_BUFFER),
    }
}

unsafe fn refcount_of(owner: *const u8) -> *mut i64 {
    unsafe { owner.sub(8) as *mut i64 }
}

fn buffer_cap(base: *const u8) -> usize {
    unsafe { *(base.sub(BUF_HEADER) as *const i64) as usize }
}

/// Allocate `[cap][refcount=1][content: cap bytes]`, returning the content
/// pointer (the `base` a value stores).
fn alloc_buffer(cap: usize) -> *const u8 {
    let raw = unsafe { super::rt_alloc(BUF_HEADER + cap) } as *mut u8;
    assert!(!raw.is_null(), "out of memory");
    unsafe {
        core::ptr::write(raw as *mut i64, cap as i64);
        core::ptr::write(raw.add(8) as *mut i64, 1);
        raw.add(BUF_HEADER)
    }
}

unsafe fn free_buffer(base: *const u8) {
    unsafe { super::rt_free(base.sub(BUF_HEADER) as *mut _) }
}

/// Grow a block under its own base, keeping the header — and so the refcount —
/// intact. Only sound for a block this value exclusively owns; the caller
/// establishes that (see [`append_owned`]).
unsafe fn grow_buffer(base: *const u8, cap: usize) -> *const u8 {
    let raw = unsafe { super::rt_realloc(base.sub(BUF_HEADER) as *mut _, BUF_HEADER + cap) };
    assert!(!raw.is_null(), "out of memory");
    unsafe {
        core::ptr::write(raw as *mut i64, cap as i64);
        (raw as *mut u8).add(BUF_HEADER)
    }
}

// ---------- Ropes ----------
//
// `[refcount: i64][len: i64][cache: Str][left: Str][right: Str]`, 88 bytes. The
// node is the value's owner; `w1` is unused. Stage 5 is where ropes learn to do
// more work without flattening; this is the mechanical port.

const ROPE_LEN: usize = 8;
const ROPE_CACHE: usize = 16;
const ROPE_LEFT: usize = ROPE_CACHE + STR_SIZE;
const ROPE_RIGHT: usize = ROPE_LEFT + STR_SIZE;
const ROPE_SIZE: usize = ROPE_RIGHT + STR_SIZE;

/// `a + b` in O(1): a node holding both, flattened only if someone reads it.
/// Takes ownership of the caller's references to `a` and `b`.
pub(crate) fn concat(a: Str, b: Str) -> Str {
    if a.is_empty() {
        a.release();
        return b;
    }
    if b.is_empty() {
        b.release();
        return a;
    }
    let len = a.len() + b.len();
    let raw = unsafe { super::rt_alloc(ROPE_SIZE) } as *mut u8;
    assert!(!raw.is_null(), "out of memory");
    unsafe {
        core::ptr::write(raw as *mut i64, 1);
        core::ptr::write(raw.add(ROPE_LEN) as *mut i64, len as i64);
        core::ptr::write(raw.add(ROPE_CACHE) as *mut Str, Str::empty());
        core::ptr::write(raw.add(ROPE_LEFT) as *mut Str, a);
        core::ptr::write(raw.add(ROPE_RIGHT) as *mut Str, b);
    }
    Str {
        // The node carries the refcount, so it sits where an owner goes.
        w0: unsafe { raw.add(8) } as u64,
        w1: 0,
        w2: meta(len, TAG_ROPE),
    }
}

/// The node's start, from the owner pointer (which points one word in, so the
/// refcount lands at `owner - 8` exactly as a buffer's does).
fn rope_node(owner: *const u8) -> *const u8 {
    unsafe { owner.sub(8) }
}

unsafe fn free_rope(owner: *const u8) {
    let node = rope_node(owner);
    unsafe {
        core::ptr::read(node.add(ROPE_CACHE) as *const Str).release();
        core::ptr::read(node.add(ROPE_LEFT) as *const Str).release();
        core::ptr::read(node.add(ROPE_RIGHT) as *const Str).release();
        super::rt_free(node as *mut _);
    }
}

/// Flatten a rope into one buffer, memoized in the node's `cache` slot so later
/// reads are plain buffer reads. The node owns the cache and releases it when
/// freed.
fn rope_materialize(s: Str) -> Str {
    debug_assert_eq!(s.tag(), TAG_ROPE);
    let node = rope_node(s.owner());
    let cached = unsafe { core::ptr::read(node.add(ROPE_CACHE) as *const Str) };
    if !cached.is_empty() {
        return cached;
    }
    let mut out = Builder::with_capacity(s.len());
    for_each_chunk(s, &mut |chunk| {
        out.push(chunk);
        true
    });
    // Always a buffer, even for a short result: `bytes` reads the cache as one,
    // and an inline value has no address to read from.
    let flat = out.into_buffer();
    unsafe { core::ptr::write(node.add(ROPE_CACHE) as *mut Str, flat) };
    flat
}

// ---------- The str surface ----------
//
// The operations codegen calls, written against the layout above. Everything
// here is length-delimited and representation-agnostic: it classifies once (the
// `STR_REPR.md` rule) and never assumes a contiguous buffer, so a rope is
// streamed rather than flattened wherever the answer allows it.

/// Visit the value's bytes as contiguous chunks, left to right, stopping early
/// if `f` returns false. A rope yields one chunk per leaf — no flattening — and
/// an inline value yields its bytes out of a local copy.
///
/// Returns false if `f` stopped it early.
pub(crate) fn for_each_chunk(s: Str, f: &mut impl FnMut(&[u8]) -> bool) -> bool {
    match s.tag() {
        TAG_BUFFER => {
            let bytes = s.buffer_bytes();
            bytes.is_empty() || f(bytes)
        }
        TAG_INLINE => {
            let all = inline_bytes(s);
            let n = s.len();
            n == 0 || f(&all[..n])
        }
        TAG_ROPE => {
            let node = rope_node(s.owner());
            let (left, right) = unsafe {
                (
                    core::ptr::read(node.add(ROPE_LEFT) as *const Str),
                    core::ptr::read(node.add(ROPE_RIGHT) as *const Str),
                )
            };
            for_each_chunk(left, f) && for_each_chunk(right, f)
        }
        _ => unreachable!("tag {} is spare", s.tag()),
    }
}

/// A chunk cursor over any representation, for walking two values in step.
/// Allocation-free: it descends from the root to the leaf holding the current
/// position (O(rope depth), by stored child lengths) and caches that leaf,
/// exactly like [`Iter`].
struct Cursor {
    root: Str,
    leaf: Str,
    leaf_start: usize,
    pos: usize,
    scratch: [u8; INLINE_CAP],
}

impl Cursor {
    fn new(root: Str) -> Cursor {
        Cursor {
            root,
            leaf: Str::empty(),
            leaf_start: 0,
            pos: 0,
            scratch: [0; INLINE_CAP],
        }
    }

    /// The unread bytes of the leaf the cursor is in, or `None` at the end.
    fn chunk(&mut self) -> Option<&[u8]> {
        if self.pos >= self.root.len() {
            return None;
        }
        if self.leaf.is_empty()
            || self.pos < self.leaf_start
            || self.pos >= self.leaf_start + self.leaf.len()
        {
            let (leaf, start) = descend(self.root, self.pos);
            self.leaf = leaf;
            self.leaf_start = start;
            if leaf.tag() == TAG_INLINE {
                self.scratch = inline_bytes(leaf);
            }
        }
        let within = self.pos - self.leaf_start;
        let bytes = match self.leaf.tag() {
            TAG_BUFFER => self.leaf.buffer_bytes(),
            TAG_INLINE => &self.scratch[..self.leaf.len()],
            _ => unreachable!("a cursor only stops on leaves"),
        };
        let rest = &bytes[within..];
        // A value claiming bytes it cannot produce is inconsistent — a rope whose
        // stored length exceeds its leaves, or (during the migration) an 8-byte
        // value read as a 24-byte one. Callers advance by the chunk length, so
        // handing back an empty chunk here would make them loop forever: `cmp`
        // takes `min(len, other)` = 0 and never moves. Stop instead. The assert
        // makes it a loud failure in a debug build rather than a silent one.
        debug_assert!(
            !rest.is_empty(),
            "cursor produced an empty chunk with {} bytes still claimed",
            self.root.len() - self.pos
        );
        if rest.is_empty() {
            return None;
        }
        Some(rest)
    }

    fn take(&mut self, n: usize) {
        self.pos += n;
    }
}

/// Lexicographic byte comparison — `-1`, `0`, or `1`, matching `aipl_str_cmp`.
pub(crate) fn cmp(a: Str, b: Str) -> i64 {
    // Fast path: with no rope on either side both values are already contiguous,
    // so this is one slice comparison rather than a cursor pair walking in step.
    // Every buffer, view and inline value lands here, which is nearly all of
    // them — and `eq` is what a set/dict lookup calls per element, so the cursor
    // setup was being paid per *comparison*, not per string.
    if a.tag() != TAG_ROPE && b.tag() != TAG_ROPE {
        let (mut ab, mut bb) = ([0u8; INLINE_CAP], [0u8; INLINE_CAP]);
        return match a.bytes(&mut ab).cmp(b.bytes(&mut bb)) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        };
    }
    let (mut ca, mut cb) = (Cursor::new(a), Cursor::new(b));
    loop {
        // One borrow at a time: each cursor caches its leaf inside itself, so the
        // two chunk borrows cannot both be live.
        let (xs, xn) = match ca.chunk() {
            Some(x) => (x.as_ptr(), x.len()),
            None => return if cb.chunk().is_some() { -1 } else { 0 },
        };
        let n = match cb.chunk() {
            Some(y) => {
                let n = xn.min(y.len());
                // SAFETY: `xs` points into `ca`'s cached leaf or its own scratch,
                // neither of which `cb.chunk()` can touch.
                let x = unsafe { core::slice::from_raw_parts(xs, n) };
                match x.cmp(&y[..n]) {
                    core::cmp::Ordering::Less => return -1,
                    core::cmp::Ordering::Greater => return 1,
                    core::cmp::Ordering::Equal => n,
                }
            }
            None => return 1,
        };
        ca.take(n);
        cb.take(n);
    }
}

/// Content equality. Lengths are in the values, so a mismatch costs nothing.
pub(crate) fn eq(a: Str, b: Str) -> bool {
    a.len() == b.len() && cmp(a, b) == 0
}

/// FNV-1a over the content, byte for byte identical to `aipl_str_hash` — a left
/// fold, so streaming a rope's leaves in order gives the flattened answer.
pub(crate) fn hash(s: Str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for_each_chunk(s, &mut |chunk| {
        for &c in chunk {
            h ^= c as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        true
    });
    h as i64
}

/// Whether `s`'s bytes from `at` start with `prefix`.
pub(crate) fn starts_with_at(s: Str, prefix: Str, at: usize) -> bool {
    // An offset past the end **clamps** rather than failing: what is left to
    // match is the empty tail, so an empty prefix still matches there and
    // anything else does not. Rejecting `at > len` outright made
    // `"hello".starts_with_at("", 99)` false, where the tagged runtime — and the
    // documented behaviour — say true.
    let at = at.min(s.len());
    if s.len() - at < prefix.len() {
        return false;
    }
    let window = s.slice(at, at + prefix.len());
    eq(window, prefix)
}

pub(crate) fn starts_with(s: Str, prefix: Str) -> bool {
    starts_with_at(s, prefix, 0)
}

pub(crate) fn ends_with(s: Str, suffix: Str) -> bool {
    s.len() >= suffix.len() && starts_with_at(s, suffix, s.len() - suffix.len())
}

// ---------- In-place append (`STR_REPR.md` stage 2) ----------

/// Capacity for an append that has to allocate: enough for what is being
/// written, and otherwise double, so a builder loop pays amortized O(1) rather
/// than reallocating on every byte. The floor keeps a string that has just
/// outgrown its inline form from reallocating again a byte later.
fn grow_cap(need: usize, old_cap: usize) -> usize {
    need.max(old_cap * 2).max(32)
}

/// Whether `add` points into `s`'s own block, in which case growing that block
/// could move the bytes being read from under the copy. `s.extend(s)` is the
/// shape that gets here; it takes the copy path instead.
fn overlaps(s: Str, add: &[u8]) -> bool {
    let base = s.base();
    if base.is_null() {
        return false;
    }
    let (lo, hi) = (base as usize, base as usize + buffer_cap(base));
    let at = add.as_ptr() as usize;
    at < hi && at + add.len() > lo
}

/// Append `add` to `s`, writing into the existing allocation when this value can
/// have it to itself. Takes the caller's reference to `s` and returns the value
/// that replaces it.
///
/// **The caller must have established *static* ownership** — an `exclusive` `mut`
/// binding or an `owned` parameter. The `is_unique` test below only *refines*
/// that: retain elision lets a borrowed value read `refcount == 1` while its
/// caller is still holding it, so on its own it proves nothing (`STR_REPR.md`,
/// "`refcount == 1` is not ownership").
///
/// Three outcomes, cheapest first:
///   - **inline, still fits** — the whole thing is a value computation. An inline
///     value owns no allocation, so there is no ownership question to ask and
///     nothing to free.
///   - **sole owner, room to spare** — write past `data + len` and widen the
///     window. Nothing else can see those bytes, so the refcount is untouched.
///   - **anything else** — shared, static, a rope, or a window that cannot grow
///     under itself: copy once into a buffer sized for the appends after this
///     one too.
pub(crate) fn append_owned(s: Str, add: &[u8]) -> Str {
    if add.is_empty() {
        return s;
    }
    let len = s.len();
    let need = len + add.len();
    if s.tag() == TAG_INLINE && need <= INLINE_CAP {
        let mut buf = inline_bytes(s);
        buf[len..need].copy_from_slice(add);
        return from_bytes_inline(&buf[..need]);
    }
    if s.tag() == TAG_BUFFER && s.is_unique() && !overlaps(s, add) {
        if s.spare_capacity() >= add.len() {
            unsafe {
                core::ptr::copy_nonoverlapping(add.as_ptr(), (s.w1 as *mut u8).add(len), add.len());
            }
            return Str {
                w0: s.w0,
                w1: s.w1,
                w2: meta(need, TAG_BUFFER),
            };
        }
        // Out of room, but the window starts at the base — so the block can grow
        // under it. `realloc` keeps the bytes and the header (refcount included)
        // and often does not move anything. A window that starts *past* the base
        // cannot: its `data` offset would have to be re-derived, and the bytes
        // before it are not ours to keep.
        if s.w0 == s.w1 {
            let cap = grow_cap(need, buffer_cap(s.w0 as *const u8));
            let base = unsafe { grow_buffer(s.w0 as *const u8, cap) };
            unsafe {
                core::ptr::copy_nonoverlapping(add.as_ptr(), (base as *mut u8).add(len), add.len());
            }
            return Str {
                w0: base as u64,
                w1: base as u64,
                w2: meta(need, TAG_BUFFER),
            };
        }
    }
    let base = alloc_buffer(grow_cap(need, len));
    let mut at = 0usize;
    for_each_chunk(s, &mut |chunk| {
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), (base as *mut u8).add(at), chunk.len());
        }
        at += chunk.len();
        true
    });
    unsafe {
        core::ptr::copy_nonoverlapping(add.as_ptr(), (base as *mut u8).add(at), add.len());
    }
    s.release();
    Str {
        w0: base as u64,
        w1: base as u64,
        w2: meta(at + add.len(), TAG_BUFFER),
    }
}

/// Whether `needle` occurs anywhere in `s`. The empty needle is always present.
///
/// Both sides are flattened once and scanned as byte slices. The obvious
/// spelling — `starts_with_at` at every offset — reads better but is
/// pathological: each offset built a fresh slice value and compared it with a
/// pair of cursors that descend from the root, so an O(n·m) search paid a
/// cursor setup per *candidate position*. That made `contains` alone 58% of the
/// compiler's own build time, since the dogfooded lexer leans on it hard.
pub(crate) fn contains(s: Str, needle: Str) -> bool {
    if needle.len() > s.len() {
        return false;
    }
    if needle.is_empty() {
        return true;
    }
    let (mut hb, mut nb) = ([0u8; INLINE_CAP], [0u8; INLINE_CAP]);
    let (hay, pat) = (s.bytes(&mut hb), needle.bytes(&mut nb));
    hay.windows(pat.len()).any(|w| w == pat)
}

/// The byte at `i`, or `None` past the end — the `char?` `s[i]` yields.
pub(crate) fn char_at(s: Str, i: usize) -> Option<u8> {
    if i >= s.len() {
        return None;
    }
    let one = s.slice(i, i + 1);
    let mut out = None;
    for_each_chunk(one, &mut |chunk| {
        out = chunk.first().copied();
        false
    });
    out
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Leading and trailing ASCII whitespace removed. **Free for a buffer** — the
/// result is a window into the same allocation, where the old runtime allocated
/// a view or copied. Borrows `s`; the result shares its owner, so a caller that
/// keeps the result past `s` retains it.
pub(crate) fn trim(s: Str) -> Str {
    let mut scratch = [0u8; INLINE_CAP];
    let (start, end) = {
        let bytes = s.bytes(&mut scratch);
        let start = bytes
            .iter()
            .position(|&b| !is_space(b))
            .unwrap_or(bytes.len());
        let end = bytes
            .iter()
            .rposition(|&b| !is_space(b))
            .map_or(start, |i| i + 1);
        (start, end)
    };
    s.slice(start, end)
}

/// `s` repeated `n` times.
pub(crate) fn repeat(s: Str, n: usize) -> Str {
    let total = s.len() * n;
    let mut out = Builder::with_capacity(total);
    for _ in 0..n {
        for_each_chunk(s, &mut |chunk| {
            out.push(chunk);
            true
        });
    }
    out.finish()
}

/// A fresh buffer holding `s`'s bytes, writable in place — the working copy
/// `reverse` and `sort` need without a `Vec`.
fn owned_copy(s: Str) -> Str {
    let mut out = Builder::with_capacity(s.len().max(INLINE_CAP + 1));
    for_each_chunk(s, &mut |chunk| {
        out.push(chunk);
        true
    });
    out.into_buffer()
}

/// A buffer's bytes, mutably. Only ever called on a buffer the caller just
/// allocated, so nothing else can be reading them.
fn buffer_bytes_mut(s: Str) -> &'static mut [u8] {
    debug_assert_eq!(s.tag(), TAG_BUFFER);
    unsafe { core::slice::from_raw_parts_mut(s.w1 as *mut u8, s.len()) }
}

pub(crate) fn reverse(s: Str) -> Str {
    let out = owned_copy(s);
    buffer_bytes_mut(out).reverse();
    out
}

pub(crate) fn sort(s: Str) -> Str {
    let out = owned_copy(s);
    buffer_bytes_mut(out).sort_unstable();
    out
}

/// Build a string by appending. Replaces the old
/// `aipl_str_alloc` + `aipl_write_*` cursor dance: the buffer knows its own
/// capacity, so the value being built is a normal `str` the whole time.
pub(crate) struct Builder {
    s: Str,
}

impl Builder {
    pub(crate) fn with_capacity(cap: usize) -> Builder {
        if cap <= INLINE_CAP {
            // Small results still get a buffer: a builder hands back a value
            // whose bytes were written through a pointer, and an inline value
            // has no address to write to.
            return Builder {
                s: with_capacity(INLINE_CAP + 1, &[]),
            };
        }
        Builder {
            s: with_capacity(cap, &[]),
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.s.spare_capacity() < bytes.len() {
            self.grow(bytes.len());
        }
        let end = (self.s.w1 as usize + self.s.len()) as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), end, bytes.len()) };
        self.s.w2 = meta(self.s.len() + bytes.len(), TAG_BUFFER);
    }

    /// Double until `extra` more bytes fit, then copy across — amortized O(1)
    /// per byte, the growth `STR_REPR.md`'s Stage 4 gives `+` for owned values.
    fn grow(&mut self, extra: usize) {
        let need = self.s.len() + extra;
        let cap = (buffer_cap(self.s.w0 as *const u8) * 2).max(need);
        let grown = with_capacity(cap, self.s.buffer_bytes());
        self.s.release();
        self.s = grown;
    }

    /// The finished value. Short results are repacked inline so they stop
    /// pinning a buffer.
    pub(crate) fn finish(self) -> Str {
        if self.s.len() <= INLINE_CAP {
            let packed = from_bytes_inline(self.s.buffer_bytes());
            self.s.release();
            return packed;
        }
        self.s
    }

    /// The finished value, always as a buffer — for callers that need the bytes
    /// at an address (a rope's cache, a working copy for `reverse`/`sort`).
    pub(crate) fn into_buffer(self) -> Str {
        self.s
    }
}

/// `sep`-joined parts.
pub(crate) fn join(parts: &[Str], sep: Str) -> Str {
    let total: usize =
        parts.iter().map(|p| p.len()).sum::<usize>() + sep.len() * parts.len().saturating_sub(1);
    let mut out = Builder::with_capacity(total);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            for_each_chunk(sep, &mut |c| {
                out.push(c);
                true
            });
        }
        for_each_chunk(*part, &mut |c| {
            out.push(c);
            true
        });
    }
    out.finish()
}

/// Split on every occurrence of `sep`; an empty `sep` splits into single bytes.
/// The pieces are **windows** into `s` when it is a buffer — the split of a
/// large string allocates nothing per piece.
pub(crate) fn split_each(s: Str, sep: Str, out: &mut impl FnMut(Str)) {
    if sep.is_empty() {
        for i in 0..s.len() {
            out(s.slice(i, i + 1));
        }
        return;
    }
    let mut at = 0;
    let mut start = 0;
    while at + sep.len() <= s.len() {
        if starts_with_at(s, sep, at) {
            out(s.slice(start, at));
            at += sep.len();
            start = at;
        } else {
            at += 1;
        }
    }
    out(s.slice(start, s.len()));
}

// ---------- Char iteration (`for c in s`) ----------
//
// A fixed-size cursor codegen stack-allocates, so iterating a rope streams its
// bytes in order without materializing and without a heap traversal stack. To
// locate a byte the cursor descends from the root to the containing leaf — O(rope
// depth), using each child's own O(1) length — and caches that leaf, so
// sequential reads inside one leaf are O(1) and a non-rope string is one leaf
// throughout.
//
// The 24-byte value pays off here: an inline leaf is *self-contained*, so the
// cached leaf needs no separate spill buffer. Today's cursor carries an 8-byte
// scratch slot for exactly that (`ITER_SCRATCH`); this one does not.

/// The `for (let c : s)` cursor. Codegen allocates `size_of::<Iter>()` bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Iter {
    root: Str,
    /// The leaf `pos` last landed in — a buffer window or an inline value, never
    /// a rope. Empty until the first `next`.
    leaf: Str,
    /// Absolute position in `root`.
    pos: u64,
    /// Absolute position where `leaf` starts.
    leaf_start: u64,
}

impl Iter {
    pub(crate) fn new(root: Str) -> Iter {
        Iter {
            root,
            leaf: Str::empty(),
            pos: 0,
            leaf_start: 0,
        }
    }

    /// The next byte, or `None` at the end. Idempotent past the end.
    pub(crate) fn next(&mut self) -> Option<u8> {
        let pos = self.pos as usize;
        if pos >= self.root.len() {
            return None;
        }
        let leaf_start = self.leaf_start as usize;
        if self.leaf.is_empty() || pos < leaf_start || pos >= leaf_start + self.leaf.len() {
            let (leaf, start) = descend(self.root, pos);
            self.leaf = leaf;
            self.leaf_start = start as u64;
        }
        let within = pos - self.leaf_start as usize;
        let byte = leaf_byte(self.leaf, within);
        self.pos += 1;
        Some(byte)
    }
}

/// The leaf containing absolute position `pos`, and where that leaf starts.
/// Descends by child length, so nothing is flattened.
fn descend(root: Str, pos: usize) -> (Str, usize) {
    let mut node = root;
    let mut base = 0usize;
    let mut at = pos;
    while node.tag() == TAG_ROPE {
        let n = rope_node(node.owner());
        let (left, right) = unsafe {
            (
                core::ptr::read(n.add(ROPE_LEFT) as *const Str),
                core::ptr::read(n.add(ROPE_RIGHT) as *const Str),
            )
        };
        if at < left.len() {
            node = left;
        } else {
            base += left.len();
            at -= left.len();
            node = right;
        }
    }
    (node, base)
}

/// Byte `i` of a leaf (a buffer window or an inline value).
fn leaf_byte(leaf: Str, i: usize) -> u8 {
    match leaf.tag() {
        TAG_BUFFER => unsafe { *(leaf.w1 as *const u8).add(i) },
        TAG_INLINE => inline_bytes(leaf)[i],
        _ => unreachable!("a cached leaf is never a rope"),
    }
}

/// Sort a run of wide `str` values by content, in place.
///
/// The tagged runtime sorts an array's elements as *words* — reordering
/// pointers. A wide element is the whole 24-byte value, so both the granularity
/// and the comparison change; this keeps that in the file that owns the layout,
/// and both runtimes call it.
pub(crate) fn sort_values(vals: &mut [Str]) {
    vals.sort_unstable_by(|x, y| {
        let c = cmp(*x, *y);
        c.cmp(&0)
    });
}

// ---------- split / join (the shared halves) ----------
//
// `split` builds an array and `join` reads one, and the array runtime is
// per-runtime — so their entry points cannot live here. What *can* live here is
// all the `str` work, which is the part that would silently diverge: where the
// cuts fall, and how the parts are streamed back together. Each runtime keeps
// only the few lines that allocate an array and find its elements.

/// Call `f` with each `sep`-separated window of `s`, in order. An empty
/// separator never matches, so the whole string is one part; `n` separators
/// yield `n + 1` parts, including empty ones at either end.
///
/// Allocation-free by construction (the caller decides what to do with each
/// part), which is what lets the same code serve the counting pass and the
/// filling pass — and lets it compile in the `no_std` runtime, which has no
/// `Vec` to collect into.
pub(crate) fn for_each_split(s: Str, sep: Str, f: &mut impl FnMut(Str)) {
    let mut sb = [0u8; INLINE_CAP];
    let mut pb = [0u8; INLINE_CAP];
    let hay = s.bytes(&mut sb);
    let hay_len = hay.len();
    let nlen = sep.len();
    if nlen == 0 {
        f(s.slice(0, hay_len));
        return;
    }
    // `needle` has to be read while `hay` is still borrowed, so both scratches
    // are separate buffers.
    let needle = sep.bytes(&mut pb);
    let (mut start, mut i) = (0usize, 0usize);
    while i + nlen <= hay_len {
        if &hay[i..i + nlen] == needle {
            f(s.slice(start, i));
            i += nlen;
            start = i;
        } else {
            i += 1;
        }
    }
    f(s.slice(start, hay_len));
}

/// Concatenate `len` `str` values starting at `elems`, with `sep` between
/// consecutive ones. Streams through a `Builder`, so a rope part copies its
/// leaves without materializing. Borrows everything it reads.
///
/// SAFETY: `elems` addresses `len` initialized, contiguous `Str` values.
pub(crate) unsafe fn join_from(elems: *const Str, len: usize, sep: Str) -> Str {
    let mut total = sep.len() * len.saturating_sub(1);
    for i in 0..len {
        total += unsafe { core::ptr::read(elems.add(i)) }.len();
    }
    let mut b = Builder::with_capacity(total);
    for i in 0..len {
        if i > 0 {
            for_each_chunk(sep, &mut |chunk| {
                b.push(chunk);
                true
            });
        }
        let ev = unsafe { core::ptr::read(elems.add(i)) };
        for_each_chunk(ev, &mut |chunk| {
            b.push(chunk);
            true
        });
    }
    b.finish()
}

// ---------- Entry points (the `aipl_*` ABI) ----------
//
// The switch to a 24-byte `str` has no compile-time signal — the predicates that
// decide a value's shape (`is_composite`, `elem_size_of`, `sret_size`) are
// ordinary runtime logic, so flipping them builds cleanly and then corrupts
// memory. And the compiler cannot parse *anything* without its dogfooded
// engines, which run old-ABI code out of the checked-in `.clif`. Together that
// leaves no way to test a half-finished switch.
//
// So the two ABIs coexist during the transition. These entry points carry a
// distinct `aipl_` prefix and the new calling convention — a `str` argument is
// a `*const Str`, a `str` result is written through a leading `*mut Str` — while
// every old `aipl_*` symbol keeps working unchanged. An artifact is therefore
// self-consistent with whichever runtime its symbols name: the checked-in IR
// keeps running on the old one while new codegen emits calls to these, which is
// what makes the switch testable at all. `STR_REPR.md`'s Stage 1 ends by
// regenerating the artifacts against `aipl_*` and deleting the old half.
//
// Both runtimes define these, since the file is shared; they are separate
// binaries, so the duplicate symbol names never meet.

/// SAFETY: every entry point takes initialized `*const Str` arguments and, where
/// it produces a `str`, a writable `*mut Str` out pointer.
unsafe fn read(s: *const Str) -> Str {
    unsafe { *s }
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_len(s: *const Str) -> i64 {
    unsafe { read(s) }.len() as i64
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_eq(a: *const Str, b: *const Str) -> i64 {
    i64::from(eq(unsafe { read(a) }, unsafe { read(b) }))
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_cmp(a: *const Str, b: *const Str) -> i64 {
    cmp(unsafe { read(a) }, unsafe { read(b) })
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_hash(s: *const Str) -> i64 {
    hash(unsafe { read(s) })
}

#[no_mangle]
pub(crate) extern "C" fn aipl_char_at(s: *const Str, i: i64) -> i64 {
    match char_at(unsafe { read(s) }, i.max(0) as usize) {
        Some(b) => b as i64,
        None => -1,
    }
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_starts_with(s: *const Str, prefix: *const Str) -> i64 {
    i64::from(starts_with(unsafe { read(s) }, unsafe { read(prefix) }))
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_starts_with_at(
    s: *const Str,
    prefix: *const Str,
    at: i64,
) -> i64 {
    i64::from(starts_with_at(
        unsafe { read(s) },
        unsafe { read(prefix) },
        at.max(0) as usize,
    ))
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_ends_with(s: *const Str, suffix: *const Str) -> i64 {
    i64::from(ends_with(unsafe { read(s) }, unsafe { read(suffix) }))
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_contains(s: *const Str, needle: *const Str) -> i64 {
    i64::from(contains(unsafe { read(s) }, unsafe { read(needle) }))
}

#[no_mangle]
pub(crate) extern "C" fn aipl_inc(s: *const Str) {
    unsafe { read(s) }.retain();
}

#[no_mangle]
pub(crate) extern "C" fn aipl_dec(s: *const Str) {
    unsafe { read(s) }.release();
}

/// `s[lo..hi]` — allocation-free, so this is a pure value computation. The
/// result borrows `s`'s allocation; the caller retains if it outlives the source.
#[no_mangle]
/// `s[lo..hi]`. The result is a *window*: for a buffer-backed value it shares
/// the input's owner rather than copying, which is the representation's whole
/// point — so it needs its own reference on that owner. `slice` itself does not
/// take one (it is the internal, ownership-transferring form), and this entry
/// point only borrows `s`, so the retain belongs here. `retain` is a no-op for
/// an inline result, which owns nothing.
pub(crate) extern "C" fn aipl_str_slice(out: *mut Str, s: *const Str, lo: i64, hi: i64) {
    let s = unsafe { read(s) };
    let sliced = s.slice(lo.max(0) as usize, hi.max(0) as usize);
    sliced.retain();
    unsafe { *out = sliced };
}

/// Concatenation, always lazy: `concat` builds a rope node in O(1), so this one
/// entry point serves all three tagged-ABI spellings (`aipl_concat`'s eager
/// copy, `aipl_concat_lazy`, and `aipl_concat_mut`'s in-place append).
///
/// `concat` *takes ownership* of both operands — it stores them into the node
/// without retaining — but an `aipl_*` entry point **borrows**, so the retains
/// happen here. That is what lets the call sites drop the pre-inc pair the old
/// convention required, and it keeps the borrow rule true of every entry point
/// rather than true of most of them.
#[no_mangle]
pub(crate) extern "C" fn aipl_concat(out: *mut Str, a: *const Str, b: *const Str) {
    let (a, b) = unsafe { (read(a), read(b)) };
    a.retain();
    b.retain();
    unsafe { *out = concat(a, b) };
}

#[no_mangle]
/// `trim(s)` — a window like `aipl_str_slice`, and retained for the same
/// reason: `trim` is `slice` with the bounds computed.
pub(crate) extern "C" fn aipl_trim(out: *mut Str, s: *const Str) {
    let trimmed = trim(unsafe { read(s) });
    trimmed.retain();
    unsafe { *out = trimmed };
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_reverse(out: *mut Str, s: *const Str) {
    unsafe { *out = reverse(read(s)) };
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_sort(out: *mut Str, s: *const Str) {
    unsafe { *out = sort(read(s)) };
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_repeat(out: *mut Str, s: *const Str, n: i64) {
    unsafe { *out = repeat(read(s), n.max(0) as usize) };
}

/// Build a value over `len` bytes the caller then fills — the allocate-then-write
/// idiom `to_str` uses, with the value (not a bare cursor) as the unit.
/// A one-character `str` from a codepoint's byte — the wide counterpart of
/// codegen's `emit_char_to_str`, which bit-packs a tagged inline value directly
/// in IR.
///
/// A wide inline value is spread across all three words, so packing it in IR
/// would mean open-coding the layout at every call site; one entry point keeps
/// the layout in the file that owns it. Allocates nothing — a single byte is
/// always inline.
#[no_mangle]
pub(crate) extern "C" fn aipl_char_to_str(out: *mut Str, c: i64) {
    let byte = [c as u8];
    unsafe { *out = from_bytes_inline(&byte) };
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_alloc(out: *mut Str, len: i64) {
    unsafe { *out = with_capacity(len.max(0) as usize, &[]) };
}

/// The writable end of a buffer being filled: `base + len`.
#[no_mangle]
pub(crate) extern "C" fn aipl_str_write_ptr(s: *const Str) -> *mut u8 {
    let s = unsafe { read(s) };
    (s.w1 as usize + s.len()) as *mut u8
}

/// Record that `n` more bytes were written into the buffer behind `s`.
#[no_mangle]
pub(crate) extern "C" fn aipl_str_grew(s: *mut Str, n: i64) {
    unsafe {
        let v = *s;
        *s = Str {
            w0: v.w0,
            w1: v.w1,
            w2: meta(v.len() + n.max(0) as usize, TAG_BUFFER),
        };
    }
}

/// Append one byte to the value in `s`, in place where it can be — the `push`
/// half of [`append_owned`], whose ownership contract this inherits.
#[no_mangle]
pub(crate) extern "C" fn aipl_str_push_byte(s: *mut Str, b: i64) {
    unsafe { *s = append_owned(*s, &[b as u8]) };
}

/// Append every byte of `add` to the value in `s`, in place where it can be.
/// Borrows `add`; takes and replaces the reference in `s` ([`append_owned`]).
#[no_mangle]
pub(crate) extern "C" fn aipl_str_append(s: *mut Str, add: *const Str) {
    let add = unsafe { read(add) };
    let mut scratch = [0u8; INLINE_CAP];
    // `bytes` flattens a rope source into its own cache, which keeps the append
    // itself a single copy; `append_owned` guards the case where those bytes
    // live in the destination's own block.
    let bytes = add.bytes(&mut scratch);
    unsafe { *s = append_owned(*s, bytes) };
}

/// A contiguous read pointer for the value's bytes, materializing a rope into
/// its own cache if needed. Length comes from `aipl_str_len`.
#[no_mangle]
pub(crate) extern "C" fn aipl_str_data(s: *const Str, scratch: *mut u8) -> *const u8 {
    let s = unsafe { read(s) };
    match s.tag() {
        TAG_BUFFER => s.w1 as *const u8,
        TAG_INLINE => unsafe {
            let bytes = inline_bytes(s);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), scratch, INLINE_CAP);
            scratch as *const u8
        },
        TAG_ROPE => rope_materialize(s).w1 as *const u8,
        _ => unreachable!("tag {} is spare", s.tag()),
    }
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_iter_init(cur: *mut Iter, s: *const Str) {
    unsafe { *cur = Iter::new(read(s)) };
}

#[no_mangle]
pub(crate) extern "C" fn aipl_str_iter_next(cur: *mut Iter) -> i64 {
    match unsafe { &mut *cur }.next() {
        Some(b) => b as i64,
        None => -1,
    }
}

/// Release each of `len` `str` elements in a run — the array header's per-element
/// drop helper for a 24-byte element. The `aipl_arr_drop_str` of the new ABI.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_drop_str(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        unsafe { core::ptr::read(elems.add(i * STR_SIZE) as *const Str) }.release();
    }
}

/// Element drop-fn for `str?[]` under the wide ABI: each element is a flattened
/// `{tag, value}` optional, so the stride is one word plus a whole `Str` and the
/// value sits after the tag. The tagged version strides 16 and reads a pointer;
/// there is no pointer here, so the value is released in place.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_drop_opt_str(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        let e = unsafe { elems.add(i * OPT_STR_SIZE) };
        if unsafe { core::ptr::read(e as *const i64) } != 0 {
            unsafe { core::ptr::read(e.add(OPT_VALUE_OFFSET) as *const Str) }.release();
        }
    }
}

/// Element retain-fn for `str?[]` under the wide ABI. Unlike the tagged
/// `aipl_arr_retain_opt`, this cannot also serve `T[]?[]`: an array element is
/// still an 8-byte pointer, so only the `str?` case changes shape.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_retain_opt_str(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        let e = unsafe { elems.add(i * OPT_STR_SIZE) };
        if unsafe { core::ptr::read(e as *const i64) } != 0 {
            unsafe { core::ptr::read(e.add(OPT_VALUE_OFFSET) as *const Str) }.retain();
        }
    }
}

/// Retain each of `len` `str` elements in a run — the retain half of
/// `aipl_arr_drop_str`.
#[no_mangle]
pub(crate) extern "C" fn aipl_arr_retain_str(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        unsafe { core::ptr::read(elems.add(i * STR_SIZE) as *const Str) }.retain();
    }
}

/// Bytes of the cursor state codegen must reserve for `for (let c : s)`.
pub(crate) const ITER_SIZE: usize = core::mem::size_of::<Iter>();

/// A flattened optional is `{tag: i64, value}`, so its value starts one word in
/// and a `str?` is that word plus a whole `Str`. Mirrors codegen's
/// `OPT_VALUE_OFFSET` / `elem_size_of`; both runtimes read the same layout.
pub(crate) const OPT_VALUE_OFFSET: usize = 8;
pub(crate) const OPT_STR_SIZE: usize = OPT_VALUE_OFFSET + STR_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: Str) -> String {
        let mut scratch = [0u8; INLINE_CAP];
        String::from_utf8(s.bytes(&mut scratch).to_vec()).unwrap()
    }

    #[test]
    fn value_is_three_words() {
        assert_eq!(core::mem::size_of::<Str>(), STR_SIZE);
        assert_eq!(core::mem::align_of::<Str>(), 8);
    }

    #[test]
    fn zeroed_memory_is_the_empty_string() {
        // sret buffers and fresh array slots arrive zero-filled.
        let zeroed: Str = unsafe { core::mem::zeroed() };
        assert_eq!(zeroed.tag(), TAG_BUFFER);
        assert_eq!(zeroed.len(), 0);
        assert!(zeroed.is_empty());
        assert_eq!(text(zeroed), "");
        zeroed.retain(); // must no-op on a null owner
        zeroed.release();
    }

    #[test]
    fn round_trips_across_the_inline_boundary() {
        for n in [0usize, 1, 7, 8, 21, INLINE_CAP, INLINE_CAP + 1, 100] {
            let bytes: Vec<u8> = (0..n).map(|i| b'a' + (i % 26) as u8).collect();
            let s = from_bytes(&bytes);
            assert_eq!(s.len(), n, "len for {n}");
            let expect_inline = n <= INLINE_CAP;
            assert_eq!(
                s.tag(),
                if expect_inline {
                    TAG_INLINE
                } else {
                    TAG_BUFFER
                },
                "representation for {n}"
            );
            let mut scratch = [0u8; INLINE_CAP];
            assert_eq!(s.bytes(&mut scratch), &bytes[..], "bytes for {n}");
            s.release();
        }
    }

    #[test]
    fn slicing_a_buffer_allocates_nothing_and_shares_the_base() {
        let src = from_bytes(b"the quick brown fox jumps over it");
        assert_eq!(src.tag(), TAG_BUFFER);
        // A one-byte slice is still a view — this is the guarantee today's
        // `aipl_str_slice` cannot make, since it copies at <= 7 bytes.
        let one = src.slice(4, 5);
        assert_eq!(one.tag(), TAG_BUFFER);
        assert_eq!(one.base(), src.base());
        assert_eq!(text(one), "q");
        let mid = src.slice(4, 9);
        assert_eq!(text(mid), "quick");
        assert_eq!(mid.base(), src.base());
        // A slice of a slice stays a window into the same allocation.
        let inner = mid.slice(1, 3);
        assert_eq!(inner.base(), src.base());
        assert_eq!(text(inner), "ui");
        // Out-of-range and inverted ranges clamp, exactly like the old runtime.
        assert_eq!(text(src.slice(30, 999)), " it");
        assert_eq!(text(src.slice(9, 4)), "");
        src.release();
    }

    #[test]
    fn slicing_an_inline_value_stays_inline() {
        let s = from_bytes(b"hello");
        let mid = s.slice(1, 4);
        assert_eq!(mid.tag(), TAG_INLINE);
        assert_eq!(text(mid), "ell");
    }

    #[test]
    fn refcounts_balance_and_a_window_keeps_its_buffer_alive() {
        let src = from_bytes(b"a string long enough to need a buffer");
        let base = src.base();
        let rc = || unsafe { *refcount_of(base) };
        assert_eq!(rc(), 1);
        let window = src.slice(2, 8);
        window.retain(); // a window that outlives the source
        assert_eq!(rc(), 2);
        src.release();
        assert_eq!(rc(), 1, "the buffer outlives the value it came from");
        assert_eq!(text(window), "string");
        window.release();
    }

    #[test]
    fn inline_values_own_nothing() {
        let s = from_bytes(b"short");
        assert!(s.owner().is_null());
        assert!(!s.is_unique(), "an inline value has no allocation to own");
        s.retain();
        s.release();
        assert_eq!(text(s), "short");
    }

    #[test]
    fn spare_capacity_is_what_is_left_after_the_window() {
        let s = with_capacity(64, b"seed");
        assert_eq!(s.len(), 4);
        assert_eq!(s.spare_capacity(), 60);
        // A window that does not reach the buffer's end has the rest behind it.
        let head = s.slice(0, 2);
        assert_eq!(head.spare_capacity(), 62);
        assert!(s.is_unique());
        s.release();
    }

    #[test]
    fn concat_is_a_rope_and_reads_flat() {
        let a = from_bytes(b"a string long enough to need its own buffer ");
        let b = from_bytes(b"and another one just as long, also buffered");
        let joined = concat(a, b);
        assert_eq!(joined.tag(), TAG_ROPE);
        assert_eq!(joined.len(), a.len() + b.len());
        assert_eq!(
            text(joined),
            "a string long enough to need its own buffer and another one just as long, also buffered"
        );
        // Nested ropes flatten in order.
        let tail = from_bytes(b" plus a tail that is also fairly long");
        let outer = concat(joined, tail);
        assert!(text(outer).ends_with("plus a tail that is also fairly long"));
        assert_eq!(outer.len(), joined.len() + tail.len());
        outer.release();
    }

    #[test]
    fn concat_with_empty_keeps_the_other_side() {
        let a = from_bytes(b"content");
        let joined = concat(a, Str::empty());
        assert_eq!(joined.tag(), TAG_INLINE);
        assert_eq!(text(joined), "content");
    }

    #[test]
    fn a_length_is_recoverable_from_every_representation() {
        let inline = from_bytes(b"tiny");
        let buffer = from_bytes(b"long enough to live in its own allocation");
        let rope = concat(
            from_bytes(b"long enough to live in its own allocation"),
            from_bytes(b" and then some more of the same"),
        );
        assert_eq!(inline.len(), 4);
        assert_eq!(
            buffer.len(),
            b"long enough to live in its own allocation".len()
        );
        assert_eq!(
            rope.len(),
            b"long enough to live in its own allocation".len()
                + b" and then some more of the same".len()
        );
        for s in [inline, buffer, rope] {
            assert_eq!(s.len(), text(s).len());
        }
        buffer.release();
        rope.release();
    }

    // ---------- the str surface ----------

    /// Every representation, for tests that must hold across all of them.
    fn variants(text: &str) -> Vec<(&'static str, Str)> {
        let mid = text.len() / 2;
        vec![
            ("inline-or-buffer", from_bytes(text.as_bytes())),
            (
                "rope",
                concat(
                    from_bytes(&text.as_bytes()[..mid]),
                    from_bytes(&text.as_bytes()[mid..]),
                ),
            ),
            (
                "window",
                from_bytes(format!("<<{text}>>").as_bytes()).slice(2, 2 + text.len()),
            ),
        ]
    }

    #[test]
    fn equality_and_order_agree_across_representations() {
        for (what, a) in variants("comparable content, long enough to be a buffer") {
            for (other, b) in variants("comparable content, long enough to be a buffer") {
                assert!(eq(a, b), "{what} vs {other}");
                assert_eq!(cmp(a, b), 0, "{what} vs {other}");
            }
            for (other, b) in variants("comparable content, long enough to be a bufferX") {
                assert!(!eq(a, b), "{what} vs {other}");
                assert_eq!(cmp(a, b), -1, "{what} vs {other}");
                assert_eq!(cmp(b, a), 1, "{other} vs {what}");
            }
        }
        assert_eq!(cmp(from_bytes(b"abc"), from_bytes(b"abd")), -1);
        assert_eq!(cmp(Str::empty(), from_bytes(b"a")), -1);
        assert_eq!(cmp(Str::empty(), Str::empty()), 0);
    }

    #[test]
    fn hash_is_content_addressed_across_representations() {
        let text = "a hashable string that is longer than the inline capacity";
        let hashes: Vec<i64> = variants(text).into_iter().map(|(_, s)| hash(s)).collect();
        assert!(
            hashes.windows(2).all(|w| w[0] == w[1]),
            "same content must hash the same: {hashes:?}"
        );
        assert_ne!(hash(from_bytes(b"a")), hash(from_bytes(b"b")));
        // Matches the current runtime's FNV-1a basis for the empty string.
        assert_eq!(hash(Str::empty()), 0xcbf2_9ce4_8422_2325u64 as i64);
    }

    #[test]
    fn prefix_suffix_and_search_cross_leaf_boundaries() {
        // The needle straddles the rope's split, which is the case a per-leaf
        // implementation gets wrong.
        let s = concat(
            from_bytes(b"the quick brown "),
            from_bytes(b"fox jumps over"),
        );
        assert!(starts_with(s, from_bytes(b"the quick")));
        assert!(ends_with(s, from_bytes(b"jumps over")));
        assert!(contains(s, from_bytes(b"brown fox")), "spans the seam");
        assert!(contains(s, from_bytes(b"n f")), "spans the seam");
        assert!(!contains(s, from_bytes(b"brown cat")));
        assert!(starts_with_at(s, from_bytes(b"brown"), 10));
        assert!(!starts_with_at(s, from_bytes(b"brown"), 11));
        assert!(contains(s, Str::empty()), "the empty needle is everywhere");
        assert!(!contains(from_bytes(b"ab"), from_bytes(b"abc")));
        s.release();
    }

    #[test]
    fn char_at_reads_every_representation() {
        // Indices come from the Rust string rather than being counted by hand,
        // so the test cannot disagree with itself about where a byte is.
        let src = "indexable string, long enough for a buffer";
        for (what, s) in variants(src) {
            for (i, want) in src.as_bytes().iter().enumerate() {
                assert_eq!(char_at(s, i), Some(*want), "{what} at {i}");
            }
            assert_eq!(char_at(s, src.len()), None, "{what} past the end");
            assert_eq!(char_at(s, src.len() + 10), None, "{what} well past the end");
        }
        assert_eq!(char_at(Str::empty(), 0), None);
    }

    #[test]
    fn trim_is_a_free_window_on_a_buffer() {
        let s = from_bytes(b"   plenty of surrounding whitespace here   ");
        let t = trim(s);
        assert_eq!(t.base(), s.base(), "no copy: same allocation");
        assert_eq!(text(t), "plenty of surrounding whitespace here");
        assert_eq!(text(trim(from_bytes(b"  x  "))), "x");
        assert_eq!(text(trim(from_bytes(b"    "))), "");
        assert_eq!(text(trim(Str::empty())), "");
        // (`is_all_whitespace` used to be asserted here; it is now the AIPL
        // builtin `builtin_is_all_whitespace.aipl`, tested by its own `.test`
        // block, and its native implementation is gone from both runtimes.)
        s.release();
    }

    #[test]
    fn builder_grows_and_repacks() {
        let mut b = Builder::with_capacity(4);
        b.push(b"one ");
        b.push(b"two ");
        b.push(b"three, and enough more to force at least one growth step");
        let out = b.finish();
        assert_eq!(
            text(out),
            "one two three, and enough more to force at least one growth step"
        );
        assert_eq!(out.tag(), TAG_BUFFER);
        out.release();
        // A short result comes back inline rather than pinning a buffer.
        let mut small = Builder::with_capacity(64);
        small.push(b"tiny");
        let out = small.finish();
        assert_eq!(out.tag(), TAG_INLINE);
        assert_eq!(text(out), "tiny");
    }

    #[test]
    fn repeat_join_reverse_and_sort() {
        assert_eq!(text(repeat(from_bytes(b"ab"), 3)), "ababab");
        assert_eq!(text(repeat(from_bytes(b"ab"), 0)), "");
        let parts = [from_bytes(b"a"), from_bytes(b"b"), from_bytes(b"c")];
        assert_eq!(text(join(&parts, from_bytes(b", "))), "a, b, c");
        assert_eq!(text(join(&parts, Str::empty())), "abc");
        assert_eq!(text(join(&[], from_bytes(b","))), "");
        assert_eq!(text(reverse(from_bytes(b"abcd"))), "dcba");
        assert_eq!(text(sort(from_bytes(b"dbca"))), "abcd");
        // Across a rope, too.
        let rope = concat(from_bytes(b"dcb"), from_bytes(b"a"));
        assert_eq!(text(reverse(rope)), "abcd");
        rope.release();
    }

    #[test]
    fn split_pieces_are_windows_into_the_source() {
        let s = from_bytes(b"alpha,beta,gamma,delta,epsilon,zeta,eta,theta");
        let mut pieces = Vec::new();
        split_each(s, from_bytes(b","), &mut |p| pieces.push(p));
        let joined: Vec<String> = pieces.iter().map(|p| text(*p)).collect();
        assert_eq!(
            joined,
            ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"]
        );
        assert!(
            pieces.iter().all(|p| p.base() == s.base()),
            "no allocation per piece"
        );
        // Edges: leading/trailing separators produce empty pieces, and a missing
        // separator yields the whole string.
        let mut edges = Vec::new();
        split_each(from_bytes(b",a,"), from_bytes(b","), &mut |p| edges.push(p));
        assert_eq!(
            edges.iter().map(|p| text(*p)).collect::<Vec<_>>(),
            ["", "a", ""]
        );
        let mut none = Vec::new();
        split_each(from_bytes(b"abc"), from_bytes(b"-"), &mut |p| none.push(p));
        assert_eq!(none.iter().map(|p| text(*p)).collect::<Vec<_>>(), ["abc"]);
        // An empty separator splits into single bytes.
        let mut bytes = Vec::new();
        split_each(from_bytes(b"abc"), Str::empty(), &mut |p| bytes.push(p));
        assert_eq!(
            bytes.iter().map(|p| text(*p)).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        s.release();
    }

    // ---------- iteration and I/O ----------

    fn collect(s: Str) -> Vec<u8> {
        let mut it = Iter::new(s);
        let mut out = Vec::new();
        while let Some(b) = it.next() {
            out.push(b);
        }
        // Past the end it keeps saying so, rather than wrapping or repeating.
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None);
        out
    }

    #[test]
    fn iteration_yields_the_same_bytes_in_every_representation() {
        let src = "iterable content, comfortably longer than inline";
        for (what, s) in variants(src) {
            assert_eq!(collect(s), src.as_bytes(), "{what}");
        }
        assert_eq!(collect(Str::empty()), b"");
        assert_eq!(collect(from_bytes(b"x")), b"x");
    }

    #[test]
    fn iteration_streams_a_deep_rope_in_order() {
        // 64 leaves, left-nested: the shape that would blow a fixed traversal
        // stack or force a flatten if either were how this worked.
        let mut rope = from_bytes(b"0");
        let mut expect = String::from("0");
        for i in 1..64u32 {
            let piece = format!("{}", i % 10);
            rope = concat(rope, from_bytes(piece.as_bytes()));
            expect.push_str(&piece);
        }
        assert_eq!(rope.len(), expect.len());
        assert_eq!(collect(rope), expect.as_bytes());
        // The cached leaf means a full pass is still one descent per leaf.
        let mut it = Iter::new(rope);
        assert_eq!(it.next(), Some(b'0'));
        assert_eq!(it.next(), Some(b'1'));
        rope.release();
    }

    #[test]
    fn iterating_a_window_stays_inside_it() {
        // Bounds derived from the pieces, not counted by hand.
        let (lead, mid) = ("<<<", "the middle part only");
        let s = from_bytes(format!("{lead}{mid}>>>").as_bytes());
        let window = s.slice(lead.len(), lead.len() + mid.len());
        assert_eq!(text(window), mid);
        assert_eq!(collect(window), mid.as_bytes());
        s.release();
    }

    // ---------- the `aipl_*` entry points, called as codegen will ----------

    /// Call an out-pointer entry point the way emitted code does: reserve a
    /// value-sized slot, hand over its address, read the value back.
    fn out(f: impl FnOnce(*mut Str)) -> Str {
        let mut slot = Str::empty();
        f(&mut slot);
        slot
    }

    #[test]
    fn entry_points_round_trip_through_pointers() {
        let a = from_bytes(b"a buffer-length string for the entry points");
        let b = from_bytes(b" and its continuation, also long");

        assert_eq!(aipl_str_len(&a), a.len() as i64);
        assert_eq!(aipl_str_eq(&a, &a), 1);
        assert_eq!(aipl_str_eq(&a, &b), 0);
        assert_eq!(aipl_str_cmp(&a, &b), 1);
        assert_eq!(aipl_str_hash(&a), hash(a));
        assert_eq!(aipl_char_at(&a, 0), b'a' as i64);
        assert_eq!(aipl_char_at(&a, 9999), -1);
        assert_eq!(aipl_str_starts_with(&a, &from_bytes(b"a buffer")), 1);
        assert_eq!(aipl_str_ends_with(&a, &from_bytes(b"points")), 1);
        assert_eq!(aipl_str_contains(&a, &from_bytes(b"length")), 1);

        let joined = out(|o| aipl_concat(o, &a, &b));
        assert_eq!(text(joined), format!("{}{}", text(a), text(b)));
        let sliced = out(|o| aipl_str_slice(o, &a, 2, 8));
        assert_eq!(text(sliced), "buffer");
        assert_eq!(sliced.base(), a.base(), "slicing stays allocation-free");
        let padded = from_bytes(b"   padded value here   ");
        let trimmed = out(|o| aipl_trim(o, &padded));
        assert_eq!(text(trimmed), "padded value here");
        let rev = out(|o| aipl_str_reverse(o, &from_bytes(b"abcd")));
        assert_eq!(text(rev), "dcba");
        let rep = out(|o| aipl_str_repeat(o, &from_bytes(b"xy"), 3));
        assert_eq!(text(rep), "xyxyxy");

        joined.release();
        rev.release();
        rep.release();
        // The two window-producing entry points retain, so their results are
        // owned references like any other and are released here.
        sliced.release();
        trimmed.release();
        padded.release();
        a.release();
        b.release();
    }

    #[test]
    fn the_builder_entry_points_fill_a_buffer() {
        // `to_str`'s shape: reserve, write through the pointer, record growth.
        let s = out(|o| aipl_str_alloc(o, 16));
        let mut s = s;
        let payload = b"12345678";
        unsafe {
            core::ptr::copy_nonoverlapping(payload.as_ptr(), aipl_str_write_ptr(&s), payload.len());
        }
        aipl_str_grew(&mut s, payload.len() as i64);
        assert_eq!(text(s), "12345678");
        assert_eq!(aipl_str_len(&s), 8);
        s.release();
    }

    #[test]
    fn iteration_entry_points_match_the_bytes() {
        let src = "iterated through the entry points, long enough to buffer";
        for (what, s) in variants(src) {
            let mut cur = Iter::new(Str::empty());
            aipl_str_iter_init(&mut cur, &s);
            let mut got = Vec::new();
            loop {
                let b = aipl_str_iter_next(&mut cur);
                if b < 0 {
                    break;
                }
                got.push(b as u8);
            }
            assert_eq!(got, src.as_bytes(), "{what}");
        }
        assert!(ITER_SIZE >= core::mem::size_of::<Str>(), "cursor is sized");
    }

    #[test]
    fn refcount_entry_points_balance() {
        let s = from_bytes(b"refcounted through the entry points, long enough");
        let base = s.base();
        let rc = || unsafe { *refcount_of(base) };
        assert_eq!(rc(), 1);
        aipl_inc(&s);
        assert_eq!(rc(), 2);
        aipl_dec(&s);
        assert_eq!(rc(), 1);
        // An inline value has no allocation, so both are no-ops.
        let tiny = from_bytes(b"tiny");
        aipl_inc(&tiny);
        aipl_dec(&tiny);
        assert_eq!(text(tiny), "tiny");
        s.release();
    }

    // ---------- in-place append ----------

    #[test]
    fn an_inline_append_that_still_fits_allocates_nothing() {
        let s = append_owned(from_bytes(b"ab"), b"cd");
        assert_eq!(s.tag(), TAG_INLINE);
        assert_eq!(text(s), "abcd");
        // 22 bytes exactly: the last append that stays inline.
        let full = append_owned(from_bytes(b"0123456789012345678901"), b"");
        assert_eq!(full.tag(), TAG_INLINE);
        assert_eq!(text(full), "0123456789012345678901");
    }

    #[test]
    fn outgrowing_the_inline_form_moves_to_a_buffer() {
        let s = append_owned(from_bytes(b"0123456789012345678901"), b"!");
        assert_eq!(s.tag(), TAG_BUFFER);
        assert_eq!(text(s), "0123456789012345678901!");
        s.release();
    }

    #[test]
    fn a_sole_owner_appends_into_its_own_spare_capacity() {
        let s = with_capacity(64, b"start");
        let base = s.base();
        let s = append_owned(s, b" and more");
        // Same allocation, wider window — no copy and no reallocation.
        assert_eq!(s.base(), base);
        assert_eq!(text(s), "start and more");
        s.release();
    }

    #[test]
    fn running_out_of_room_grows_the_block_and_keeps_the_content() {
        let mut s = with_capacity(4, b"ab");
        for _ in 0..50 {
            s = append_owned(s, b"xy");
        }
        assert_eq!(s.len(), 2 + 100);
        assert_eq!(text(s), format!("ab{}", "xy".repeat(50)));
        s.release();
    }

    /// `refcount == 1` is what refines static ownership into "safe to write"
    /// (`STR_REPR.md`), so a second live reference has to force the copy — the
    /// other holder's bytes must not move under it.
    #[test]
    fn a_shared_buffer_is_copied_rather_than_written_into() {
        let a = with_capacity(64, b"shared");
        a.retain();
        let b = a;
        let a = append_owned(a, b"!");
        assert_eq!(text(a), "shared!");
        assert_eq!(text(b), "shared", "the other holder is untouched");
        assert_ne!(a.base(), b.base());
        a.release();
        b.release();
    }

    #[test]
    fn appending_a_value_to_itself_reads_before_it_writes() {
        let s = with_capacity(64, b"ab");
        let mut scratch = [0u8; INLINE_CAP];
        let own = s.bytes(&mut scratch);
        // SAFETY: `own` points into `s`'s own block, which is exactly the
        // aliasing `overlaps` exists to catch; the bytes stay valid because
        // `append_owned` copies out of the old block before releasing it.
        let own: &[u8] = unsafe { core::slice::from_raw_parts(own.as_ptr(), own.len()) };
        let s = append_owned(s, own);
        assert_eq!(text(s), "abab");
        s.release();
    }

    #[test]
    fn a_rope_is_flattened_by_the_append_rather_than_written_into() {
        let long = b"long enough to live in its own allocation";
        let r = concat(
            from_bytes(long),
            from_bytes(b" plus a second half of the same"),
        );
        let before = text(r);
        let s = append_owned(r, b"!");
        assert_eq!(text(s), format!("{before}!"));
        assert_eq!(s.tag(), TAG_BUFFER);
        s.release();
    }

    #[test]
    fn the_entry_points_update_through_the_pointer() {
        let mut s = from_bytes(b"ab");
        aipl_str_push_byte(&mut s, b'c' as i64);
        assert_eq!(text(s), "abc");
        let add = from_bytes(b"defghijklmnopqrstuvwxyz0123456789");
        aipl_str_append(&mut s, &add);
        assert_eq!(text(s), "abcdefghijklmnopqrstuvwxyz0123456789");
        s.release();
        add.release();
    }
}
