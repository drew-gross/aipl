//! The 24-byte `str` value — `STR_REPR.md`'s layout, implemented and tested on
//! its own before anything is switched over to it.
//!
//! **Not wired up yet.** Stage 1 of that plan is an atomic change: the runtime,
//! codegen, and every checked-in `.clif` have to agree on the layout, so there
//! is no way to land it a piece at a time. What *can* be de-risked first is
//! this: the layout itself, its invariants, and the operations that read and
//! build it, proven by ordinary Rust tests with no compiler in the loop. When
//! the switch happens, this module stops being dead code and the equivalent is
//! mirrored into `crates/aipl-linker/runtime/aipl_runtime.rs`.
//!
//! # The value
//!
//! ```text
//!               w0                  w1                  w2
//! buffer   00   base: *const u8     data: *const u8     [ len : 56 ][ tag : 8 ]
//! inline   01   content[0..8]       content[8..16]      [ content[16..22] ][ len:8 ][ tag:8 ]
//! rope     10   node: *const u8     0                   [ len : 56 ][ tag : 8 ]
//!          11   — spare —
//! ```
//!
//! The tag is the **top byte** of `w2` in every representation, so classifying a
//! value is one shift on a register the length already needed, and `base`/`data`
//! are dereferenced without masking. See `STR_REPR.md` for why that beats
//! low-bit tagging.
//!
//! # The buffer
//!
//! ```text
//! [cap: i64][refcount: i64][content bytes ...]
//!                           ^ base; `data` points anywhere in [base, base + cap]
//! ```
//!
//! `refcount` stays at `base - 8`, shared with arrays exactly as today. The word
//! at `base - 16` is **capacity**, not length: the value carries the only length
//! there is, which is what lets one representation serve both "the whole string"
//! and "a window into it". A buffer therefore does *not* imply a trailing NUL —
//! every consumer is length-delimited.
#![allow(dead_code)] // staged: wired up by the Stage 1 switch

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};

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
            std::ptr::null()
        }
    }

    /// The refcounted allocation behind this value — a buffer's base or a rope's
    /// node — or null when there is none. This is the only word refcounting ever
    /// touches, which is why `retain`/`release` need `w0` and the tag, never
    /// `data`.
    pub(crate) fn owner(self) -> *const u8 {
        match self.tag() {
            TAG_BUFFER | TAG_ROPE => self.w0 as *const u8,
            _ => std::ptr::null(),
        }
    }

    /// A buffer's content bytes. Panics on any other representation — callers
    /// classify first (`STR_REPR.md`'s "classify + match" rule).
    fn buffer_bytes(self) -> &'static [u8] {
        debug_assert_eq!(self.tag(), TAG_BUFFER);
        if self.w1 == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.w1 as *const u8, self.len()) }
    }

    /// The value's bytes, using `scratch` for representations that have no
    /// contiguous buffer of their own (inline, and a rope that must flatten).
    pub(crate) fn bytes<'a>(&'a self, scratch: &'a mut Vec<u8>) -> &'a [u8] {
        match self.tag() {
            TAG_BUFFER => self.buffer_bytes(),
            TAG_INLINE => {
                let n = self.len();
                scratch.clear();
                scratch.extend_from_slice(&inline_bytes(*self)[..n]);
                &scratch[..n]
            }
            TAG_ROPE => {
                scratch.clear();
                rope_flatten(*self, scratch);
                &scratch[..]
            }
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
            // A rope has no contiguous window to point at; flattening the span is
            // the honest answer until Stage 5 teaches slicing to descend to a
            // leaf.
            _ => {
                let mut scratch = Vec::new();
                let bytes = self.bytes(&mut scratch).to_vec();
                from_bytes(&bytes[lo..hi])
            }
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
        std::ptr::copy_nonoverlapping(init.as_ptr(), base as *mut u8, init.len());
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

fn buffer_layout(cap: usize) -> Layout {
    Layout::from_size_align(BUF_HEADER + cap, std::mem::align_of::<i64>()).expect("buffer layout")
}

/// Allocate `[cap][refcount=1][content: cap bytes]`, returning the content
/// pointer (the `base` a value stores).
fn alloc_buffer(cap: usize) -> *const u8 {
    let layout = buffer_layout(cap);
    let raw = unsafe { alloc(layout) };
    if raw.is_null() {
        handle_alloc_error(layout);
    }
    unsafe {
        std::ptr::write(raw as *mut i64, cap as i64);
        std::ptr::write(raw.add(8) as *mut i64, 1);
        raw.add(BUF_HEADER)
    }
}

unsafe fn free_buffer(base: *const u8) {
    let cap = buffer_cap(base);
    unsafe { dealloc(base.sub(BUF_HEADER) as *mut u8, buffer_layout(cap)) }
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

fn rope_layout() -> Layout {
    Layout::from_size_align(ROPE_SIZE, std::mem::align_of::<i64>()).expect("rope layout")
}

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
    let layout = rope_layout();
    let raw = unsafe { alloc(layout) };
    if raw.is_null() {
        handle_alloc_error(layout);
    }
    unsafe {
        std::ptr::write(raw as *mut i64, 1);
        std::ptr::write(raw.add(ROPE_LEN) as *mut i64, len as i64);
        std::ptr::write(raw.add(ROPE_CACHE) as *mut Str, Str::empty());
        std::ptr::write(raw.add(ROPE_LEFT) as *mut Str, a);
        std::ptr::write(raw.add(ROPE_RIGHT) as *mut Str, b);
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
        std::ptr::read(node.add(ROPE_CACHE) as *const Str).release();
        std::ptr::read(node.add(ROPE_LEFT) as *const Str).release();
        std::ptr::read(node.add(ROPE_RIGHT) as *const Str).release();
        dealloc(node as *mut u8, rope_layout());
    }
}

/// Append every leaf's bytes to `out`, left to right, without building
/// intermediate strings.
fn rope_flatten(s: Str, out: &mut Vec<u8>) {
    match s.tag() {
        TAG_BUFFER => out.extend_from_slice(s.buffer_bytes()),
        TAG_INLINE => out.extend_from_slice(&inline_bytes(s)[..s.len()]),
        TAG_ROPE => {
            let node = rope_node(s.owner());
            unsafe {
                rope_flatten(std::ptr::read(node.add(ROPE_LEFT) as *const Str), out);
                rope_flatten(std::ptr::read(node.add(ROPE_RIGHT) as *const Str), out);
            }
        }
        _ => unreachable!("tag {} is spare", s.tag()),
    }
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
                    std::ptr::read(node.add(ROPE_LEFT) as *const Str),
                    std::ptr::read(node.add(ROPE_RIGHT) as *const Str),
                )
            };
            for_each_chunk(left, f) && for_each_chunk(right, f)
        }
        _ => unreachable!("tag {} is spare", s.tag()),
    }
}

/// A byte cursor over any representation, used where two values are walked in
/// step (comparison) or a window is scanned (`contains`). Chunk-at-a-time under
/// the hood, so a rope costs one step per leaf rather than per byte.
struct Cursor {
    /// Leaves still to visit, deepest-last so `pop` yields them left to right.
    stack: Vec<Str>,
    /// The leaf being read, and how far into it we are.
    cur: Option<(Str, usize)>,
    scratch: [u8; INLINE_CAP],
}

impl Cursor {
    fn new(s: Str) -> Cursor {
        let mut c = Cursor {
            stack: vec![s],
            cur: None,
            scratch: [0; INLINE_CAP],
        };
        c.advance();
        c
    }

    /// Descend to the next leaf with bytes left in it.
    fn advance(&mut self) {
        while let Some(top) = self.stack.pop() {
            match top.tag() {
                TAG_ROPE => {
                    let node = rope_node(top.owner());
                    unsafe {
                        // Right first: `pop` then yields left before right.
                        self.stack
                            .push(std::ptr::read(node.add(ROPE_RIGHT) as *const Str));
                        self.stack
                            .push(std::ptr::read(node.add(ROPE_LEFT) as *const Str));
                    }
                }
                _ if top.len() == 0 => continue,
                _ => {
                    if top.tag() == TAG_INLINE {
                        self.scratch = inline_bytes(top);
                    }
                    self.cur = Some((top, 0));
                    return;
                }
            }
        }
        self.cur = None;
    }

    /// The unread bytes of the current leaf, or `None` at the end.
    fn chunk(&self) -> Option<&[u8]> {
        let (leaf, at) = self.cur?;
        let bytes = match leaf.tag() {
            TAG_BUFFER => leaf.buffer_bytes(),
            TAG_INLINE => &self.scratch[..leaf.len()],
            _ => unreachable!("a cursor only stops on leaves"),
        };
        Some(&bytes[at..])
    }

    /// Consume `n` bytes of the current leaf, moving on when it runs out.
    fn take(&mut self, n: usize) {
        if let Some((leaf, at)) = self.cur {
            let at = at + n;
            if at >= leaf.len() {
                self.cur = None;
                self.advance();
            } else {
                self.cur = Some((leaf, at));
            }
        }
    }
}

/// Lexicographic byte comparison — `-1`, `0`, or `1`, matching `aipl_str_cmp`.
pub(crate) fn cmp(a: Str, b: Str) -> i64 {
    let (mut ca, mut cb) = (Cursor::new(a), Cursor::new(b));
    loop {
        match (ca.chunk(), cb.chunk()) {
            (None, None) => return 0,
            (None, Some(_)) => return -1,
            (Some(_), None) => return 1,
            (Some(x), Some(y)) => {
                let n = x.len().min(y.len());
                match x[..n].cmp(&y[..n]) {
                    std::cmp::Ordering::Less => return -1,
                    std::cmp::Ordering::Greater => return 1,
                    std::cmp::Ordering::Equal => {}
                }
                ca.take(n);
                cb.take(n);
            }
        }
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
    if at > s.len() || s.len() - at < prefix.len() {
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

/// Whether `needle` occurs anywhere in `s`. The empty needle is always present.
pub(crate) fn contains(s: Str, needle: Str) -> bool {
    if needle.len() > s.len() {
        return false;
    }
    (0..=s.len() - needle.len()).any(|at| starts_with_at(s, needle, at))
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

pub(crate) fn is_all_whitespace(s: Str) -> bool {
    let mut all = true;
    for_each_chunk(s, &mut |chunk| {
        all = chunk.iter().all(|&b| is_space(b));
        all
    });
    all
}

/// Leading and trailing ASCII whitespace removed. **Free for a buffer** — the
/// result is a window into the same allocation, where the old runtime allocated
/// a view or copied. Borrows `s`; the result shares its owner, so a caller that
/// keeps the result past `s` retains it.
pub(crate) fn trim(s: Str) -> Str {
    let mut scratch = Vec::new();
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

pub(crate) fn reverse(s: Str) -> Str {
    let mut scratch = Vec::new();
    let mut bytes = s.bytes(&mut scratch).to_vec();
    bytes.reverse();
    from_bytes(&bytes)
}

pub(crate) fn sort(s: Str) -> Str {
    let mut scratch = Vec::new();
    let mut bytes = s.bytes(&mut scratch).to_vec();
    bytes.sort_unstable();
    from_bytes(&bytes)
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
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), end, bytes.len()) };
        self.s.w2 = meta(self.s.len() + bytes.len(), TAG_BUFFER);
    }

    /// Double until `extra` more bytes fit, then copy across — amortized O(1)
    /// per byte, the growth `STR_REPR.md`'s Stage 4 gives `+` for owned values.
    fn grow(&mut self, extra: usize) {
        let need = self.s.len() + extra;
        let cap = (buffer_cap(self.s.w0 as *const u8) * 2).max(need);
        let mut scratch = Vec::new();
        let grown = with_capacity(cap, self.s.bytes(&mut scratch));
        self.s.release();
        self.s = grown;
    }

    /// The finished value. Short results are repacked inline so they stop
    /// pinning a buffer.
    pub(crate) fn finish(self) -> Str {
        if self.s.len() <= INLINE_CAP {
            let mut scratch = Vec::new();
            let packed = from_bytes_inline(self.s.bytes(&mut scratch));
            self.s.release();
            return packed;
        }
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
pub(crate) fn split(s: Str, sep: Str) -> Vec<Str> {
    let mut out = Vec::new();
    if sep.is_empty() {
        for i in 0..s.len() {
            out.push(s.slice(i, i + 1));
        }
        return out;
    }
    let mut at = 0;
    let mut start = 0;
    while at + sep.len() <= s.len() {
        if starts_with_at(s, sep, at) {
            out.push(s.slice(start, at));
            at += sep.len();
            start = at;
        } else {
            at += 1;
        }
    }
    out.push(s.slice(start, s.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: Str) -> String {
        let mut scratch = Vec::new();
        String::from_utf8(s.bytes(&mut scratch).to_vec()).unwrap()
    }

    #[test]
    fn value_is_three_words() {
        assert_eq!(std::mem::size_of::<Str>(), STR_SIZE);
        assert_eq!(std::mem::align_of::<Str>(), 8);
    }

    #[test]
    fn zeroed_memory_is_the_empty_string() {
        // sret buffers and fresh array slots arrive zero-filled.
        let zeroed: Str = unsafe { std::mem::zeroed() };
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
            let mut scratch = Vec::new();
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
        assert!(is_all_whitespace(from_bytes(b" \t\n")));
        assert!(!is_all_whitespace(from_bytes(b" x ")));
        assert!(is_all_whitespace(Str::empty()));
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
        let pieces = split(s, from_bytes(b","));
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
        let edges = split(from_bytes(b",a,"), from_bytes(b","));
        assert_eq!(
            edges.iter().map(|p| text(*p)).collect::<Vec<_>>(),
            ["", "a", ""]
        );
        assert_eq!(
            split(from_bytes(b"abc"), from_bytes(b"-"))
                .iter()
                .map(|p| text(*p))
                .collect::<Vec<_>>(),
            ["abc"]
        );
        // An empty separator splits into single bytes.
        assert_eq!(
            split(from_bytes(b"abc"), Str::empty())
                .iter()
                .map(|p| text(*p))
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        s.release();
    }
}
