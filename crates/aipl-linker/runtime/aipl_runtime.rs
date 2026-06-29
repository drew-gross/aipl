//! AIPL standalone-binary runtime. Compiled to a staticlib by `build.rs`
//! and embedded into the `aipl` driver; the driver writes it to disk and
//! links it against user object files produced by `aipl build`.
//!
//! No-std + libc bindings so the resulting staticlib has no Rust-std
//! dependency: only libc (which the platform linker will pull in via
//! `clang foo.o runtime.a`). The functions and refcount protocol mirror
//! the JIT runtime in `src/codegen.rs` byte-for-byte.

#![no_std]
// `aipl_instrument` is a custom cfg set only by the instrumented build variant
// (see build.rs); declare it allowed so the default build doesn't warn.
#![allow(unexpected_cfgs)]

use core::ffi::{c_char, c_int, c_long, c_void};
use core::panic::PanicInfo;
#[cfg(aipl_instrument)]
use core::sync::atomic::{AtomicU64, Ordering};

// Refcount prefix shared by every refcounted heap block (strings AND arrays):
// the i64 refcount is at `ptr - HEADER_SIZE`, so `header_of`/inc/dec are common.
const HEADER_SIZE: usize = 8;
// A heap *string* also stores its content length, in a word *before* the
// refcount (keeping the refcount at `ptr - HEADER_SIZE`, shared with arrays):
// `[len: i64][refcount: i64][content][NUL]`, value → content. See the JIT
// runtime for the full description.
const STR_HEADER_SIZE: usize = 16;
const STATIC_REFCOUNT: i64 = i64::MAX;

extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn abort() -> !;
    fn strlen(s: *const c_char) -> usize;
    fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

// libc stdio for `read_file_to_string`. Declared unconditionally (the
// instrumented stats reporter also uses `fopen`/`fclose`).
const SEEK_SET: c_int = 0;
const SEEK_END: c_int = 2;
extern "C" {
    fn fopen(path: *const c_char, mode: *const c_char) -> *mut c_void;
    fn fread(ptr: *mut c_void, size: usize, nmemb: usize, stream: *mut c_void) -> usize;
    fn fwrite(ptr: *const c_void, size: usize, nmemb: usize, stream: *mut c_void) -> usize;
    fn fseek(stream: *mut c_void, offset: c_long, whence: c_int) -> c_int;
    fn ftell(stream: *mut c_void) -> c_long;
    fn fclose(stream: *mut c_void) -> c_int;
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    unsafe { abort() }
}

// The macOS static linker requires all undefined symbols to be resolved at
// link time, even if they're unreachable.  With panic=abort the personality
// function is never called, but core's exception tables still reference the
// symbol.  Provide a stub so `ld` is satisfied.
#[cfg(target_os = "macos")]
#[no_mangle]
pub unsafe extern "C" fn rust_eh_personality() -> ! {
    unsafe { abort() }
}

// ---------- Allocation instrumentation ----------
//
// Every heap allocation/free the runtime makes goes through `rt_alloc`/
// `rt_free` rather than `malloc`/`free` directly. In the default build these
// are zero-cost forwarders. In the instrumented variant (`--cfg aipl_instrument`,
// linked by the test harness's `--- performance ---` checks) they tally call
// counts, which `main` reports at exit. Only the runtime's own allocations are
// counted — libc-internal allocations (e.g. inside `fopen`) bypass these.

#[cfg(aipl_instrument)]
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(aipl_instrument)]
static FREE_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(aipl_instrument)]
static REALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
// Total bytes requested via fresh allocations (`rt_alloc`), i.e. the sum of the
// `allocations` count's sizes. Reallocation growth is *not* added here — it's a
// resize of an existing block, tallied separately as `reallocations`.
#[cfg(aipl_instrument)]
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
// Total CLIF instructions executed across all compiled AIPL functions. Codegen
// instruments each basic block to call `aipl_count_insns` with that block's
// (compile-time-fixed) instruction count, so this is the sum over executed
// blocks of their instruction counts — deterministic for a given program
// (it depends only on control flow, not on timing or addresses). Runtime/library
// helpers (`aipl_concat`, `__aipl_str_eq`, …) are native, not counted.
#[cfg(aipl_instrument)]
static INSN_COUNT: AtomicU64 = AtomicU64::new(0);

#[inline]
unsafe fn rt_alloc(size: usize) -> *mut c_void {
    #[cfg(aipl_instrument)]
    {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    }
    unsafe { malloc(size) }
}

#[inline]
unsafe fn rt_free(ptr: *mut c_void) {
    #[cfg(aipl_instrument)]
    FREE_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe { free(ptr) }
}

#[inline]
unsafe fn rt_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    // Tallied separately from alloc/free: an in-place grow reuses an existing
    // block rather than a fresh malloc/free pair, so it's neither an allocation
    // nor a deallocation, but it is worth tracking on its own.
    #[cfg(aipl_instrument)]
    REALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe { realloc(ptr, size) }
}

/// Add `n` to the executed-instruction tally. Codegen emits one call per basic
/// block (with `n` = that block's instruction count), so the running total
/// counts CLIF instructions executed. In the default (non-instrumented) build
/// this is a no-op forwarder, exactly like `rt_alloc` and friends.
#[no_mangle]
pub extern "C" fn aipl_count_insns(n: i64) {
    #[cfg(aipl_instrument)]
    INSN_COUNT.fetch_add(n as u64, Ordering::Relaxed);
    #[cfg(not(aipl_instrument))]
    let _ = n;
}

#[inline]
unsafe fn header_of(ptr: *const u8) -> *mut i64 {
    unsafe { ptr.sub(HEADER_SIZE) as *mut i64 }
}

/// The stored content length of a heap string (the word at `-16`, before refcount).
#[inline]
unsafe fn heap_len(ptr: *const u8) -> usize {
    unsafe { *(ptr.sub(STR_HEADER_SIZE) as *const i64) as usize }
}

// ---------- Small-string optimization (SSO) ----------
//
// Mirror of the JIT runtime (see crates/aipl-codegen/src/lib.rs for the full
// description). A `str` value is either a heap/static pointer (8-byte aligned, so
// low two bits 0) or an inline small string tagged `0b01`: byte0 = (len<<2)|1
// with len in 0..=7, bytes 1..=7 = content. The low two bits are the repr tag:
// 00 = heap/static, 01 = inline, 10 = view, 11 = concat. inc/dec no-op on inline;
// consumers materialize, so correctness never depends on the "<=7 is inline"
// invariant.

// The representation discriminant lives in the low two bits of every `str`
// value; `str_repr` decodes it into a [`StrRepr`]. Branch on a value's
// representation by `match`ing `str_repr(..)` (NOT ad-hoc `is_*` checks), so
// adding a representation forces every dispatch site to handle it.
const TAG_MASK: usize = 0b11;
const HEAP_TAG: usize = 0b00;
const INLINE_TAG: usize = 0b01;

#[inline]
fn inline_len(v: *const u8) -> usize {
    ((v as usize) >> 2) & 0x7
}

// ---------- String views (slices that share a backing buffer) ----------
//
// Mirror of the JIT runtime (see crates/aipl-codegen/src/lib.rs). A *view* is the
// third `str` representation, tagged by low bit 1 (bit 0 = 0): the value is
// `view_obj_ptr | 0b10`, pointing at a heap struct:
//   [0] refcount: i64 | [8] data_ptr: *const u8 | [16] len: i64 | [24] owner.
// `data_ptr` points into the owner's content; `owner` (the parent str value) is
// inc'd on create and dec'd on free, so the shared buffer outlives the view.
const VIEW_TAG: usize = 0b10;
const VIEW_SIZE: usize = 32;
const VIEW_DATA_OFFSET: usize = 8;
const VIEW_LEN_OFFSET: usize = 16;
const VIEW_OWNER_OFFSET: usize = 24;

#[inline]
fn view_obj(v: *const u8) -> *mut u8 {
    ((v as usize) & !0b111) as *mut u8
}

// ---------- Concatenated strings (lazy ropes) ----------
//
// Mirror of the JIT runtime (see crates/aipl-codegen/src/lib.rs). The fourth
// `str` representation, tagged `0b11`: value `node_ptr | 0b11`, node is a heap
// struct `[0] refcount | [8] left:str | [16] right:str | [24] cache:ptr`. Built
// by `aipl_concat_lazy` for every `str + str`; materialized (memoized into
// `cache`) on first byte-access. inc/dec count the node and, at zero, release
// both children and the cache.
const CONCAT_TAG: usize = 0b11;
const CONCAT_SIZE: usize = 40;
const CONCAT_LEFT_OFFSET: usize = 8;
const CONCAT_RIGHT_OFFSET: usize = 16;
const CONCAT_CACHE_OFFSET: usize = 24;
const CONCAT_LEN_OFFSET: usize = 32; // total content length, summed once at build

#[inline]
fn concat_obj(v: *const u8) -> *mut u8 {
    ((v as usize) & !0b111) as *mut u8
}

// ---------- Representation dispatch ----------
//
// Mirror of the JIT runtime. Classify a `str` value with `str_repr`, then
// `match` — prefer that over scattered `is_*` checks so adding a `StrRepr`
// variant makes the compiler flag every site that doesn't handle it. Variants
// that genuinely share handling may share an arm (e.g. `Null | Heap`), but spell
// them out rather than using a bare `_`.

/// The active runtime representation of a (non-poisoned) `str` value.
enum StrRepr {
    /// Null pointer — the empty string, no storage.
    Null,
    /// Small-string-optimized: <= 7 content bytes packed in the value itself.
    Inline,
    /// Owned or static heap string; the value is its NUL-terminated content ptr.
    Heap,
    /// A slice sharing another string's buffer; carries the view object.
    View(*mut u8),
    /// A lazy concatenation — a *rope*; carries the concat node.
    Rope(*mut u8),
}

/// Classify a `str` value into its active [`StrRepr`] (the discriminant is the
/// low two bits; see the per-representation sections above).
#[inline]
fn str_repr(v: *const u8) -> StrRepr {
    if v.is_null() {
        return StrRepr::Null;
    }
    match (v as usize) & TAG_MASK {
        HEAP_TAG => StrRepr::Heap,
        INLINE_TAG => StrRepr::Inline,
        VIEW_TAG => StrRepr::View(view_obj(v)),
        CONCAT_TAG => StrRepr::Rope(concat_obj(v)),
        _ => unreachable!("two-bit tag is exhaustive"),
    }
}

/// Pack <= 7 content bytes into an inline str value.
fn pack_inline(bytes: &[u8]) -> *const u8 {
    let mut val: u64 = ((bytes.len() as u64) << 2) | 1;
    let mut i = 0;
    while i < bytes.len() {
        val |= (bytes[i] as u64) << (8 * (i + 1));
        i += 1;
    }
    val as usize as *const u8
}

/// Content bytes of any str value: inline → copied into `buf` (which must
/// outlive the slice); heap/static → its NUL-delimited bytes; null → empty.
unsafe fn str_bytes<'a>(v: *const u8, buf: &'a mut [u8; 8]) -> &'a [u8] {
    match str_repr(v) {
        StrRepr::Null => &[],
        StrRepr::Inline => {
            let src = (v as usize as u64).to_le_bytes();
            let len = inline_len(v);
            let mut i = 0;
            while i < 8 {
                buf[i] = src[i];
                i += 1;
            }
            &buf[1..1 + len]
        }
        StrRepr::View(obj) => unsafe {
            let data = *(obj.add(VIEW_DATA_OFFSET) as *const *const u8);
            let len = *(obj.add(VIEW_LEN_OFFSET) as *const i64) as usize;
            core::slice::from_raw_parts(data, len)
        },
        // Materialize (memoized) and read the flattened cache's bytes.
        StrRepr::Rope(_) => unsafe { str_bytes(concat_materialize(v), buf) },
        StrRepr::Heap => unsafe { core::slice::from_raw_parts(v, heap_len(v)) },
    }
}

/// Normalize a possibly-view str (a file path) to a form the C file API can use:
/// a view's bytes aren't NUL-terminated (and its value is tagged), so copy it to
/// a fresh owned string; any other representation is returned unchanged. The
/// caller must `aipl_dec` the result iff it differs from the input.
unsafe fn view_to_owned_path(v: *const u8) -> *const u8 {
    match str_repr(v) {
        // A view's bytes aren't NUL-terminated, and a concat's value is a node
        // pointer, not content — copy either to a fresh owned (NUL-terminated)
        // string. Inline/heap/null are already usable directly.
        StrRepr::View(_) | StrRepr::Rope(_) => {
            let mut b = [0u8; 8];
            make_str(unsafe { str_bytes(v, &mut b) })
        }
        StrRepr::Null | StrRepr::Inline | StrRepr::Heap => v,
    }
}

/// A NUL-terminated C pointer for any (non-null) str value, for `fopen`: inline
/// content is copied + NUL-terminated into `buf` (8 bytes holds <=7 + NUL); a
/// heap str is already NUL-terminated so its pointer is returned directly. A view
/// or concat must be normalized via `view_to_owned_path` first (into an owned
/// heap str), so they reach here only as `Heap`.
unsafe fn str_cptr<'a>(v: *const u8, buf: &'a mut [u8; 8]) -> *const c_char {
    match str_repr(v) {
        StrRepr::Inline => {
            let src = (v as usize as u64).to_le_bytes();
            let len = inline_len(v);
            let mut i = 0;
            while i < len {
                buf[i] = src[i + 1];
                i += 1;
            }
            buf[len] = 0;
            buf.as_ptr() as *const c_char
        }
        // Heap (incl. a normalized view/concat) is already NUL-terminated; null
        // shouldn't reach here, but its pointer is a valid empty C string.
        StrRepr::Null | StrRepr::Heap | StrRepr::View(_) | StrRepr::Rope(_) => {
            v as *const c_char
        }
    }
}

/// Canonicalize freshly-built content into a str value: inline when it fits
/// (<= 7 bytes), else a fresh heap string.
fn make_str(bytes: &[u8]) -> *const u8 {
    if bytes.len() <= 7 {
        pack_inline(bytes)
    } else {
        unsafe {
            let raw = rt_str_buf(bytes.len());
            memcpy(
                raw.add(STR_HEADER_SIZE) as *mut c_void,
                bytes.as_ptr() as *const c_void,
                bytes.len(),
            );
            raw.add(STR_HEADER_SIZE)
        }
    }
}

/// Flatten a concat into a contiguous owned heap str, memoized on the node's
/// `cache` slot. Recurses through nested concats via `str_bytes`.
unsafe fn concat_materialize(v: *const u8) -> *const u8 {
    unsafe {
        let obj = concat_obj(v);
        let cache_slot = obj.add(CONCAT_CACHE_OFFSET) as *mut *const u8;
        let cached = *cache_slot;
        if !cached.is_null() {
            return cached;
        }
        let left = *(obj.add(CONCAT_LEFT_OFFSET) as *const *const u8);
        let right = *(obj.add(CONCAT_RIGHT_OFFSET) as *const *const u8);
        let mut lb = [0u8; 8];
        let mut rb = [0u8; 8];
        let sa = str_bytes(left, &mut lb);
        let sb = str_bytes(right, &mut rb);
        let (la, lbn) = (sa.len(), sb.len());
        let result = if la + lbn <= 7 {
            let mut tmp = [0u8; 7];
            memcpy(
                tmp.as_mut_ptr() as *mut c_void,
                sa.as_ptr() as *const c_void,
                la,
            );
            memcpy(
                tmp.as_mut_ptr().add(la) as *mut c_void,
                sb.as_ptr() as *const c_void,
                lbn,
            );
            pack_inline(&tmp[..la + lbn])
        } else {
            let raw = rt_str_buf(la + lbn);
            memcpy(
                raw.add(STR_HEADER_SIZE) as *mut c_void,
                sa.as_ptr() as *const c_void,
                la,
            );
            memcpy(
                raw.add(STR_HEADER_SIZE + la) as *mut c_void,
                sb.as_ptr() as *const c_void,
                lbn,
            );
            raw.add(STR_HEADER_SIZE)
        };
        *cache_slot = result;
        result
    }
}

/// `str + str`, lazily: build a concat node holding the two operands. Takes
/// ownership of the refs the caller pre-inc'd; the node's `aipl_dec` releases
/// them. The defining producer of the concat representation.
#[no_mangle]
pub extern "C" fn aipl_concat_lazy(a: *const u8, b: *const u8) -> *const u8 {
    // Sum the operands' lengths once (each is O(1)) so the rope's total length is
    // O(1) to read at the root.
    let len = aipl_str_len(a) + aipl_str_len(b);
    unsafe {
        let obj = rt_alloc(CONCAT_SIZE) as *mut u8;
        if obj.is_null() {
            abort();
        }
        *(obj as *mut i64) = 1; // refcount
        *(obj.add(CONCAT_LEFT_OFFSET) as *mut *const u8) = a;
        *(obj.add(CONCAT_RIGHT_OFFSET) as *mut *const u8) = b;
        *(obj.add(CONCAT_CACHE_OFFSET) as *mut *const u8) = core::ptr::null();
        *(obj.add(CONCAT_LEN_OFFSET) as *mut i64) = len;
        (obj as usize | CONCAT_TAG) as *const u8
    }
}

#[no_mangle]
pub extern "C" fn aipl_inc(ptr: *const u8) {
    match str_repr(ptr) {
        // Null and inline values own no heap — nothing to count.
        StrRepr::Null | StrRepr::Inline => {}
        // A view / concat counts its own object (and holds refs on its children).
        StrRepr::View(obj) | StrRepr::Rope(obj) => unsafe { *(obj as *mut i64) += 1 },
        StrRepr::Heap => unsafe {
            let h = header_of(ptr);
            if *h != STATIC_REFCOUNT {
                *h += 1;
            }
        },
    }
}

#[no_mangle]
pub extern "C" fn aipl_dec(ptr: *const u8) {
    match str_repr(ptr) {
        // Null and inline values own no heap — nothing to free.
        StrRepr::Null | StrRepr::Inline => {}
        StrRepr::View(obj) => unsafe {
            let rc = obj as *mut i64;
            *rc -= 1;
            if *rc == 0 {
                let owner = *(obj.add(VIEW_OWNER_OFFSET) as *const *const u8);
                aipl_dec(owner);
                rt_free(obj as *mut c_void);
            }
        },
        StrRepr::Rope(obj) => unsafe {
            let rc = obj as *mut i64;
            *rc -= 1;
            if *rc == 0 {
                aipl_dec(*(obj.add(CONCAT_LEFT_OFFSET) as *const *const u8));
                aipl_dec(*(obj.add(CONCAT_RIGHT_OFFSET) as *const *const u8));
                let cache = *(obj.add(CONCAT_CACHE_OFFSET) as *const *const u8);
                if !cache.is_null() {
                    aipl_dec(cache);
                }
                rt_free(obj as *mut c_void);
            }
        },
        StrRepr::Heap => unsafe {
            let h = header_of(ptr);
            if *h != STATIC_REFCOUNT {
                *h -= 1;
                if *h == 0 {
                    rt_free(ptr.sub(STR_HEADER_SIZE) as *mut c_void);
                }
            }
        },
    }
}

/// Visit each leaf's contiguous bytes in order **without materializing a rope**:
/// a rope recurses into its children (reusing its `cache` if already
/// materialized), every other representation yields its bytes via `str_bytes`.
/// `f` returns `false` to stop early; the return value is whether the whole
/// string was visited. The shared primitive behind the streaming string
/// operations (print, equality, hashing, indexing, prefix/suffix, join). Mirrors
/// the JIT runtime's copy.
fn str_for_each_chunk(ptr: *const u8, f: &mut impl FnMut(&[u8]) -> bool) -> bool {
    match str_repr(ptr) {
        StrRepr::Null => true,
        StrRepr::Rope(obj) => unsafe {
            let cache = *(obj.add(CONCAT_CACHE_OFFSET) as *const *const u8);
            if cache.is_null() {
                let left = *(obj.add(CONCAT_LEFT_OFFSET) as *const *const u8);
                let right = *(obj.add(CONCAT_RIGHT_OFFSET) as *const *const u8);
                str_for_each_chunk(left, f) && str_for_each_chunk(right, f)
            } else {
                str_for_each_chunk(cache, f)
            }
        },
        // A contiguous leaf (inline/view/heap) yields its bytes directly.
        StrRepr::Inline | StrRepr::View(_) | StrRepr::Heap => {
            let mut buf = [0u8; 8];
            let bytes = unsafe { str_bytes(ptr, &mut buf) };
            f(bytes)
        }
    }
}

#[no_mangle]
pub extern "C" fn aipl_print(ptr: *const u8) {
    // A null str prints nothing; an inline empty `""` is non-null and prints a
    // blank line. A rope is streamed leaf-by-leaf (no materialization).
    if !ptr.is_null() {
        str_for_each_chunk(ptr, &mut |chunk| {
            unsafe { write(1, chunk.as_ptr() as *const c_void, chunk.len()) }; // stdout
            true
        });
        unsafe { write(1, b"\n".as_ptr() as *const c_void, 1) };
    }
    aipl_dec(ptr);
}

/// `fn main() -> !Error` failure path: write `error: <msg>\n` to stderr (fd 2).
/// Borrows `msg` (no refcount change) — `main`'s scope drop frees it.
#[no_mangle]
pub extern "C" fn aipl_print_error(msg: *const u8) {
    let prefix = b"error: ";
    unsafe { write(2, prefix.as_ptr() as *const c_void, prefix.len()) };
    str_for_each_chunk(msg, &mut |chunk| {
        unsafe { write(2, chunk.as_ptr() as *const c_void, chunk.len()) };
        true
    });
    unsafe { write(2, b"\n".as_ptr() as *const c_void, 1) };
}

/// The `s[i]` runtime (`aipl_char_at`): returns byte i of s as 0..255, or
/// -1 to signal None (i<0, past null terminator, or null pointer). The
/// codegen wraps the result into a 16-byte Optional slot at the call
/// site. Decrements `s` per the refcount protocol.
#[no_mangle]
pub extern "C" fn aipl_char_at(s: *const u8, i: i64) -> i64 {
    let mut found: i64 = -1;
    if i >= 0 {
        // Walk leaves to the chunk containing index `i` and stop — no materializing.
        let target = i as usize;
        let mut pos = 0usize;
        str_for_each_chunk(s, &mut |chunk| {
            if target < pos + chunk.len() {
                found = i64::from(chunk[target - pos]);
                false // found it — stop early
            } else {
                pos += chunk.len();
                true
            }
        });
    }
    aipl_dec(s);
    found
}

/// `s.is_all_whitespace() -> bool` (i64 0/1): true when every byte is ASCII
/// whitespace, or `s` is empty (consistent with `s.trim() == ""`). Decrements
/// `s` per the refcount protocol. Mirrors the JIT runtime.
#[no_mangle]
pub extern "C" fn aipl_str_is_all_whitespace(s: *const u8) -> i64 {
    // Scan leaves, stopping at the first non-whitespace chunk — no materializing.
    // An empty string visits no chunks, so it stays all-whitespace (true).
    let all = str_for_each_chunk(s, &mut |chunk| {
        chunk
            .iter()
            .all(|&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
    });
    aipl_dec(s);
    i64::from(all)
}

/// `read_file_to_string(name) -> str?`: read `name`'s bytes into a fresh
/// refcounted str, or return null (None) on any failure — open/read error or a
/// NUL byte in the contents (which a NUL-terminated str can't represent).
/// Codegen wraps null into None. Decrements `name` per the refcount protocol.
#[no_mangle]
pub extern "C" fn aipl_read_file_to_string(name: *const u8) -> *const u8 {
    let path = unsafe { view_to_owned_path(name) };
    let result = unsafe { read_file_impl(path) };
    if path != name {
        aipl_dec(path); // free the owned copy made for a view path
    }
    aipl_dec(name);
    result
}

unsafe fn read_file_impl(name: *const u8) -> *const u8 {
    if name.is_null() {
        return core::ptr::null();
    }
    unsafe {
        // A heap str is already NUL-terminated; an inline one is copied into a
        // local NUL-terminated buffer for the C path.
        let mut pbuf = [0u8; 8];
        let cpath = str_cptr(name, &mut pbuf);
        let f = fopen(cpath, b"rb\0".as_ptr() as *const c_char);
        if f.is_null() {
            return core::ptr::null();
        }
        // Size via seek/tell, then read in one go.
        if fseek(f, 0, SEEK_END) != 0 {
            fclose(f);
            return core::ptr::null();
        }
        let size = ftell(f);
        if size < 0 || fseek(f, 0, SEEK_SET) != 0 {
            fclose(f);
            return core::ptr::null();
        }
        let size = size as usize;
        let raw = rt_str_buf(size); // [len][refcount=1][size bytes][NUL]
        let data = raw.add(STR_HEADER_SIZE);
        let n = fread(data as *mut c_void, 1, size, f);
        fclose(f);
        if n != size {
            rt_free(raw as *mut c_void);
            return core::ptr::null();
        }
        let mut i = 0usize;
        while i < size {
            if *data.add(i) == 0 {
                rt_free(raw as *mut c_void);
                return core::ptr::null();
            }
            i += 1;
        }
        // SSO: a short file's contents are inline — free the read buffer.
        if size <= 7 {
            let inlined = pack_inline(core::slice::from_raw_parts(data, size));
            rt_free(raw as *mut c_void);
            return inlined;
        }
        data
    }
}

/// `write_string_to_file(path, contents) -> bool`: write `contents`' bytes to
/// `path`, returning 1 on success or 0 on any failure (open/write error).
/// Decrements both `path` and `contents` per the refcount protocol (callers
/// pre-inc, as with any str-taking fn).
#[no_mangle]
pub extern "C" fn aipl_write_string_to_file(path: *const u8, contents: *const u8) -> i64 {
    let cpath = unsafe { view_to_owned_path(path) };
    let ok = unsafe { write_file_impl(cpath, contents) };
    if cpath != path {
        aipl_dec(cpath); // free the owned copy made for a view path
    }
    aipl_dec(path);
    aipl_dec(contents);
    ok
}

unsafe fn write_file_impl(path: *const u8, contents: *const u8) -> i64 {
    if path.is_null() || contents.is_null() {
        return 0;
    }
    unsafe {
        // A heap path is already NUL-terminated; an inline one is copied into a
        // local NUL-terminated buffer for the C path.
        let mut pbuf = [0u8; 8];
        let cpath = str_cptr(path, &mut pbuf);
        let f = fopen(cpath, b"wb\0".as_ptr() as *const c_char);
        if f.is_null() {
            return 0;
        }
        let mut cbuf = [0u8; 8];
        let bytes = str_bytes(contents, &mut cbuf);
        let len = bytes.len();
        let written = if len == 0 {
            0
        } else {
            fwrite(bytes.as_ptr() as *const c_void, 1, len, f)
        };
        fclose(f);
        if written == len {
            1
        } else {
            0
        }
    }
}

/// Format `n` in decimal into the END of `buf` (at least 20 bytes), returning
/// the start index of the written bytes. Digits are built least-significant
/// first; `wrapping_neg` on the bit pattern yields the magnitude even for
/// i64::MIN, which has no positive i64 counterpart.
fn fmt_i64(buf: &mut [u8; 20], n: i64) -> usize {
    let neg = n < 0;
    let mut mag: u64 = if neg {
        (n as u64).wrapping_neg()
    } else {
        n as u64
    };
    let mut i = buf.len();
    if mag == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while mag > 0 {
            i -= 1;
            buf[i] = b'0' + (mag % 10) as u8;
            mag /= 10;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    i
}

/// Allocate a result str buffer for `content` content bytes, refcount 1, with
/// the content length stored. Returns the block start (content at `+STR_HEADER_SIZE`).
unsafe fn rt_str_buf(content: usize) -> *mut u8 {
    unsafe {
        let raw = rt_alloc(STR_HEADER_SIZE + content + 1) as *mut u8;
        if raw.is_null() {
            abort();
        }
        *(raw as *mut i64) = content as i64; // stored length (block word 0)
        *(raw as *mut i64).add(1) = 1; // refcount (block word 1)
        *raw.add(STR_HEADER_SIZE + content) = 0;
        raw
    }
}

// One-allocation `to_str` cursor primitives (mirror the JIT runtime). `to_str`
// measures the total length, allocates once via `aipl_str_alloc`, then fills the
// buffer through a moving cursor with the write helpers below.

/// Allocate one writable `str` buffer of `len` content bytes; return the data ptr.
#[no_mangle]
pub extern "C" fn aipl_str_alloc(len: i64) -> *const u8 {
    let len = if len < 0 { 0 } else { len as usize };
    unsafe { rt_str_buf(len).add(STR_HEADER_SIZE) }
}

/// Decimal byte length of `n` (matching `aipl_write_i64`).
#[no_mangle]
pub extern "C" fn aipl_i64_len(n: i64) -> i64 {
    let mut buf = [0u8; 20];
    (20 - fmt_i64(&mut buf, n)) as i64
}

/// Write `n`'s decimal representation at `dst`; return the advanced cursor.
#[no_mangle]
pub extern "C" fn aipl_write_i64(dst: *const u8, n: i64) -> *const u8 {
    unsafe {
        let mut buf = [0u8; 20];
        let start = fmt_i64(&mut buf, n);
        let len = 20 - start;
        memcpy(
            dst as *mut c_void,
            buf.as_ptr().add(start) as *const c_void,
            len,
        );
        dst.add(len)
    }
}

/// Format `n` (unsigned) into the END of `buf`, returning the start index.
fn fmt_u64(buf: &mut [u8; 20], mut n: u64) -> usize {
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while n > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    i
}

/// Decimal byte length of `n` interpreted as *unsigned* (matching aipl_write_u64).
#[no_mangle]
pub extern "C" fn aipl_u64_len(n: i64) -> i64 {
    let mut buf = [0u8; 20];
    (20 - fmt_u64(&mut buf, n as u64)) as i64
}

/// Write `n` (interpreted as unsigned) in decimal at `dst`; return the cursor.
#[no_mangle]
pub extern "C" fn aipl_write_u64(dst: *const u8, n: i64) -> *const u8 {
    unsafe {
        let mut buf = [0u8; 20];
        let start = fmt_u64(&mut buf, n as u64);
        let len = 20 - start;
        memcpy(
            dst as *mut c_void,
            buf.as_ptr().add(start) as *const c_void,
            len,
        );
        dst.add(len)
    }
}

/// Byte content length of a `str`; 0 for null. O(1) for every representation —
/// each stores or encodes its length, so this never walks the bytes (a rope reads
/// the total cached at its root; a heap string reads its header word).
#[no_mangle]
pub extern "C" fn aipl_str_len(s: *const u8) -> i64 {
    match str_repr(s) {
        StrRepr::Null => 0,
        StrRepr::Inline => inline_len(s) as i64,
        StrRepr::Heap => unsafe { heap_len(s) as i64 },
        StrRepr::View(obj) => unsafe { *(obj.add(VIEW_LEN_OFFSET) as *const i64) },
        StrRepr::Rope(obj) => unsafe { *(obj.add(CONCAT_LEN_OFFSET) as *const i64) },
    }
}

/// Copy `n` bytes `src` → `dst`; return the advanced cursor.
#[no_mangle]
pub extern "C" fn aipl_write_bytes(dst: *const u8, src: *const u8, n: i64) -> *const u8 {
    let n = if n < 0 { 0 } else { n as usize };
    unsafe {
        memcpy(dst as *mut c_void, src as *const c_void, n);
        dst.add(n)
    }
}

#[no_mangle]
pub extern "C" fn aipl_concat(a: *const u8, b: *const u8) -> *const u8 {
    unsafe {
        let mut ba = [0u8; 8];
        let mut bb = [0u8; 8];
        let sa = str_bytes(a, &mut ba);
        let sb = str_bytes(b, &mut bb);
        let (la, lb) = (sa.len(), sb.len());
        let result = if la + lb <= 7 {
            // SSO: a short result is inline — no allocation.
            let mut tmp = [0u8; 7];
            memcpy(
                tmp.as_mut_ptr() as *mut c_void,
                sa.as_ptr() as *const c_void,
                la,
            );
            memcpy(
                tmp.as_mut_ptr().add(la) as *mut c_void,
                sb.as_ptr() as *const c_void,
                lb,
            );
            pack_inline(&tmp[..la + lb])
        } else {
            // rt_str_buf writes the [len][refcount][..][NUL] header for us.
            let raw = rt_str_buf(la + lb);
            if la > 0 {
                memcpy(
                    raw.add(STR_HEADER_SIZE) as *mut c_void,
                    sa.as_ptr() as *const c_void,
                    la,
                );
            }
            if lb > 0 {
                memcpy(
                    raw.add(STR_HEADER_SIZE + la) as *mut c_void,
                    sb.as_ptr() as *const c_void,
                    lb,
                );
            }
            raw.add(STR_HEADER_SIZE)
        };
        aipl_dec(a);
        aipl_dec(b);
        result
    }
}

/// ASCII whitespace: space, tab, newline, carriage return, vertical tab, and
/// form feed.
#[inline]
fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `trim(s) -> str`. Returns a fresh string with leading and trailing ASCII
/// whitespace removed, then drops `s`. An all-whitespace (or empty) string
/// trims to "".
#[no_mangle]
pub extern "C" fn aipl_trim(s: *const u8) -> *const u8 {
    unsafe {
        let mut sbuf = [0u8; 8];
        let bytes = str_bytes(s, &mut sbuf);
        let n = bytes.len();
        let mut start = 0;
        while start < n && is_ascii_ws(bytes[start]) {
            start += 1;
        }
        let mut end = n;
        while end > start && is_ascii_ws(bytes[end - 1]) {
            end -= 1;
        }
        let trimmed_n = end - start;
        // Nothing trimmed: return s as-is, transferring our reference to the caller.
        if start == 0 && end == n {
            return s;
        }
        // Small result (≤ 7 bytes, incl. all-whitespace → trimmed_n == 0): copy
        // inline and release s. Inline sources (≤ 7 bytes total) always land here.
        if trimmed_n <= 7 {
            let result = make_str(&bytes[start..end]);
            aipl_dec(s);
            return result;
        }
        // Large result from a heap/view source: return a view into s's buffer.
        // Transfer our reference of s to the view's owner — no inc, no dec, no copy.
        let data = bytes.as_ptr().add(start);
        let obj = rt_alloc(VIEW_SIZE) as *mut u8;
        *(obj as *mut i64) = 1; // refcount
        *(obj.add(VIEW_DATA_OFFSET) as *mut *const u8) = data;
        *(obj.add(VIEW_LEN_OFFSET) as *mut i64) = trimmed_n as i64;
        *(obj.add(VIEW_OWNER_OFFSET) as *mut *const u8) = s;
        (obj as usize | VIEW_TAG) as *const u8
    }
}

/// `s.reverse() -> str` — new string with the bytes in reverse order.
/// Consumes `s` (callers pre-inc). Mirrors the JIT runtime's `aipl_str_reverse`.
#[no_mangle]
pub extern "C" fn aipl_str_reverse(s: *const u8) -> *const u8 {
    unsafe {
        let mut sbuf = [0u8; 8];
        let bytes = str_bytes(s, &mut sbuf);
        let n = bytes.len();
        // Build reversed copy in a stack-local or heap buffer, then make_str.
        // For short strings (<=7 bytes) make_str packs inline; longer ones heap.
        let result = if n == 0 {
            make_str(&[])
        } else {
            // Allocate a temporary reversed buffer: use a fixed stack buf for
            // short strings (<=128 bytes) to avoid a heap allocation.
            let mut tmp = [0u8; 128];
            if n <= 128 {
                for (i, &b) in bytes.iter().rev().enumerate() {
                    tmp[i] = b;
                }
                make_str(&tmp[..n])
            } else {
                // Allocate a fresh heap string and write the reversed bytes in.
                let raw = rt_str_buf(n);
                let dst = raw.add(STR_HEADER_SIZE) as *mut u8;
                for (i, &b) in bytes.iter().rev().enumerate() {
                    *dst.add(i) = b;
                }
                raw.add(STR_HEADER_SIZE)
            }
        };
        aipl_dec(s);
        result
    }
}

/// `s.repeat(n) -> str` — concatenate `s` with itself `n` times.
/// Returns `""` for `n <= 0`. Consumes `s` (callers pre-inc).
#[no_mangle]
pub extern "C" fn aipl_str_repeat(s: *const u8, n: i64) -> *const u8 {
    unsafe {
        let mut sbuf = [0u8; 8];
        let bytes = str_bytes(s, &mut sbuf);
        let result = if n <= 0 || bytes.is_empty() {
            make_str(&[])
        } else {
            let chunk = bytes.len();
            let total = chunk * n as usize;
            let raw = rt_str_buf(total);
            let dst = raw.add(STR_HEADER_SIZE) as *mut c_void;
            let src = bytes.as_ptr() as *const c_void;
            for i in 0..n as usize {
                memcpy(dst.add(i * chunk), src, chunk);
            }
            raw.add(STR_HEADER_SIZE)
        };
        aipl_dec(s);
        result
    }
}

/// `xs.reverse() -> T[]` — new array with elements in reverse order.
/// O(1): returns a reversed-view repr wrapping `a`.
/// Transfers ownership of `a` into the view (no drop, no retain on `a`).
/// Mirrors the JIT runtime's `aipl_arr_reverse`.
#[no_mangle]
pub extern "C" fn aipl_arr_reverse(
    a: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    if a.is_null() {
        return a;
    }
    let len = unsafe { array_len(a) };
    unsafe { alloc_reversed_view(a, len, drop_fn, retain_fn, elem_size) }
}

/// `s[start..end]` — string slice. Both bounds are clamped to `[0, len]` (an
/// out-of-range end yields a shorter string; `start >= end` yields `""`).
/// *Borrows* `s` (does not drop it) and returns a fresh `str`. Mirrors the JIT
/// runtime's `aipl_str_slice`.
#[no_mangle]
pub extern "C" fn aipl_str_slice(s: *const u8, start: i64, end: i64) -> *const u8 {
    unsafe {
        let mut sbuf = [0u8; 8];
        let bytes = str_bytes(s, &mut sbuf);
        let len = bytes.len() as i64;
        let lo = start.clamp(0, len) as usize;
        let hi = end.clamp(0, len) as usize;
        let n = hi.saturating_sub(lo);
        // Small result or an SSO source: copy (don't pin a parent buffer).
        if n <= 7 || matches!(str_repr(s), StrRepr::Inline) {
            return make_str(if lo < hi { &bytes[lo..hi] } else { &[] });
        }
        // Large slice of a heap (owned or view) source: share its buffer via a
        // view that retains the source.
        let data = bytes.as_ptr().add(lo);
        aipl_inc(s);
        let obj = rt_alloc(VIEW_SIZE) as *mut u8;
        *(obj as *mut i64) = 1; // refcount
        *(obj.add(VIEW_DATA_OFFSET) as *mut *const u8) = data;
        *(obj.add(VIEW_LEN_OFFSET) as *mut i64) = n as i64;
        *(obj.add(VIEW_OWNER_OFFSET) as *mut *const u8) = s;
        (obj as usize | VIEW_TAG) as *const u8
    }
}

/// `split(self, sep) -> str[]` — parts of `self` between non-overlapping
/// occurrences of `sep`, each a slice of `self` (a buffer-sharing view for a long
/// part, else an inline/heap copy). An empty `sep` yields one part: the whole
/// string. Consumes both `self` and `sep`; the view parts hold their own refs on
/// `self`'s buffer. Mirrors the JIT runtime's `aipl_str_split`.
#[no_mangle]
pub extern "C" fn aipl_str_split(s: *const u8, sep: *const u8) -> *const u8 {
    unsafe {
        let mut sbuf = [0u8; 8];
        let mut pbuf = [0u8; 8];
        let hay = str_bytes(s, &mut sbuf);
        let needle = str_bytes(sep, &mut pbuf);
        let nlen = needle.len();
        // Part count = occurrences + 1 (an empty separator never matches → 1).
        let count = if nlen == 0 {
            1
        } else {
            let mut c = 1usize;
            let mut i = 0usize;
            while i + nlen <= hay.len() {
                if &hay[i..i + nlen] == needle {
                    c += 1;
                    i += nlen;
                } else {
                    i += 1;
                }
            }
            c
        };
        let drop_fn = aipl_arr_drop_str as ArrDropFn as usize as i64;
        let arr = aipl_array_new(count as i64, drop_fn, 8);
        let elems = arr.add(ARR_ELEMS_OFFSET) as *mut i64;
        if nlen == 0 {
            *elems = aipl_str_slice(s, 0, hay.len() as i64) as i64;
        } else {
            let (mut start, mut i, mut k) = (0usize, 0usize, 0usize);
            while i + nlen <= hay.len() {
                if &hay[i..i + nlen] == needle {
                    *elems.add(k) = aipl_str_slice(s, start as i64, i as i64) as i64;
                    k += 1;
                    i += nlen;
                    start = i;
                } else {
                    i += 1;
                }
            }
            *elems.add(k) = aipl_str_slice(s, start as i64, hay.len() as i64) as i64;
        }
        aipl_dec(s);
        aipl_dec(sep);
        arr
    }
}

/// `join(parts: str[], sep: str) -> str` — concatenate the parts with `sep`
/// between consecutive elements (`[]` -> `""`, `[x]` -> `x`). Two passes: measure
/// the total length, then fill a single fresh buffer (inline when <= 7 bytes).
/// Consumes both args (the array drop releases its element strings). Mirrors the
/// JIT runtime.
#[no_mangle]
pub extern "C" fn aipl_str_join(arr: *const u8, sep: *const u8) -> *const u8 {
    let result = unsafe {
        let len = *(arr as *const i64) as usize; // length lives at the data pointer
        let elems = arr.add(ARR_ELEMS_OFFSET) as *const i64;
        let mut sb = [0u8; 8];
        // `sep` is read once and reused, so materialize a rope separator just once.
        let sep_bytes = str_bytes(sep, &mut sb);
        // Measure: every part's length (O(1)) plus a separator between each pair.
        let mut total = sep_bytes.len() * len.saturating_sub(1);
        for i in 0..len {
            let ep = *elems.add(i) as *const u8;
            total += aipl_str_len(ep) as usize;
        }
        // Fill, writing a separator before every element but the first.
        let mut scratch = [0u8; 7];
        let dst = if total <= 7 {
            scratch.as_mut_ptr()
        } else {
            rt_str_buf(total).add(STR_HEADER_SIZE)
        };
        let mut pos = 0usize;
        for i in 0..len {
            if i > 0 {
                memcpy(
                    dst.add(pos) as *mut c_void,
                    sep_bytes.as_ptr() as *const c_void,
                    sep_bytes.len(),
                );
                pos += sep_bytes.len();
            }
            let ep = *elems.add(i) as *const u8;
            // Stream the element into the buffer; a rope copies its leaves with
            // nothing materialized.
            str_for_each_chunk(ep, &mut |chunk| {
                memcpy(
                    dst.add(pos) as *mut c_void,
                    chunk.as_ptr() as *const c_void,
                    chunk.len(),
                );
                pos += chunk.len();
                true
            });
        }
        if total <= 7 {
            pack_inline(&scratch[..total])
        } else {
            dst
        }
    };
    aipl_array_dec(arr);
    aipl_dec(sep);
    result
}

/// Pointer to `s`'s contiguous content bytes (length via `aipl_str_len`), for
/// codegen sites that walk a str by index (the `for` loop and `to_str`
/// rendering). Inline content is copied into the caller's 8-byte `scratch`;
/// owned/view content is returned in place. Mirrors the JIT runtime.
#[no_mangle]
pub extern "C" fn aipl_str_data(s: *const u8, scratch: *mut u8) -> *const u8 {
    match str_repr(s) {
        StrRepr::Inline => {
            let val = (s as usize as u64).to_le_bytes();
            let len = inline_len(s);
            let mut i = 0;
            while i < len {
                unsafe { *scratch.add(i) = val[1 + i] };
                i += 1;
            }
            scratch
        }
        StrRepr::View(obj) => unsafe { *(obj.add(VIEW_DATA_OFFSET) as *const *const u8) },
        StrRepr::Rope(_) => unsafe { aipl_str_data(concat_materialize(s), scratch) },
        // A null or heap value's pointer is already its contiguous content.
        StrRepr::Null | StrRepr::Heap => s,
    }
}

// ---------- Char-iteration cursor (`for c in s`) ----------
//
// Mirror of the JIT runtime. A small fixed cursor codegen stack-allocates so
// iterating a rope streams its bytes leaf-by-leaf without materializing and
// without a heap traversal stack. `next` descends from the root to the leaf
// containing the current position (O(rope depth), via the stored child lengths)
// and caches the leaf, so sequential reads within it are O(1). Layout
// (`ITER_SIZE` bytes, 8-aligned): [0] root | [8] pos | [16] total | [24] leaf_ptr
// | [32] leaf_start | [40] leaf_len | [48] scratch (8 bytes).
const ITER_ROOT: usize = 0;
const ITER_POS: usize = 8;
const ITER_TOTAL: usize = 16;
const ITER_LEAF_PTR: usize = 24;
const ITER_LEAF_START: usize = 32;
const ITER_LEAF_LEN: usize = 40;
const ITER_SCRATCH: usize = 48;

#[no_mangle]
pub extern "C" fn aipl_str_iter_init(cur: *mut u8, s: *const u8) {
    unsafe {
        *(cur.add(ITER_ROOT) as *mut *const u8) = s;
        *(cur.add(ITER_POS) as *mut i64) = 0;
        *(cur.add(ITER_TOTAL) as *mut i64) = aipl_str_len(s);
        *(cur.add(ITER_LEAF_PTR) as *mut *const u8) = core::ptr::null();
        *(cur.add(ITER_LEAF_START) as *mut i64) = 0;
        *(cur.add(ITER_LEAF_LEN) as *mut i64) = 0;
    }
}

/// Next byte of the iterated string as `0..=255`, or `-1` at the end.
#[no_mangle]
pub extern "C" fn aipl_str_iter_next(cur: *mut u8) -> i64 {
    unsafe {
        let pos = *(cur.add(ITER_POS) as *const i64);
        if pos >= *(cur.add(ITER_TOTAL) as *const i64) {
            return -1;
        }
        let leaf_start = *(cur.add(ITER_LEAF_START) as *const i64);
        let leaf_len = *(cur.add(ITER_LEAF_LEN) as *const i64);
        if pos < leaf_start || pos >= leaf_start + leaf_len {
            // Descend from the root to the leaf containing `pos`; only non-rope
            // nodes (or an already-materialized rope's cache) become the leaf.
            let mut node = *(cur.add(ITER_ROOT) as *const *const u8);
            let mut base: i64 = 0;
            loop {
                match str_repr(node) {
                    StrRepr::Rope(obj) => {
                        let cache = *(obj.add(CONCAT_CACHE_OFFSET) as *const *const u8);
                        if cache.is_null() {
                            let left = *(obj.add(CONCAT_LEFT_OFFSET) as *const *const u8);
                            let ll = aipl_str_len(left);
                            if pos - base < ll {
                                node = left;
                            } else {
                                base += ll;
                                node = *(obj.add(CONCAT_RIGHT_OFFSET) as *const *const u8);
                            }
                        } else {
                            node = cache;
                            break;
                        }
                    }
                    _ => break,
                }
            }
            let data = aipl_str_data(node, cur.add(ITER_SCRATCH));
            *(cur.add(ITER_LEAF_PTR) as *mut *const u8) = data;
            *(cur.add(ITER_LEAF_START) as *mut i64) = base;
            *(cur.add(ITER_LEAF_LEN) as *mut i64) = aipl_str_len(node);
        }
        let leaf_ptr = *(cur.add(ITER_LEAF_PTR) as *const *const u8);
        let leaf_start = *(cur.add(ITER_LEAF_START) as *const i64);
        let b = *leaf_ptr.add((pos - leaf_start) as usize);
        *(cur.add(ITER_POS) as *mut i64) = pos + 1;
        i64::from(b)
    }
}

/// In-place trim for a *uniquely owned* string (codegen calls this only for
/// `set s = trim(s)` when static analysis proves `s` is unaliased). Shifts the
/// trimmed content to the buffer start and `realloc`s down to fit, reusing the
/// block rather than allocating a fresh string. A static literal can't be
/// mutated, so it falls back to a copy (`aipl_trim`) — which also keeps the
/// alloc/dealloc tally balanced for the first trim off a literal. `s` is reused,
/// not dropped.
#[no_mangle]
pub extern "C" fn aipl_trim_mut(s: *const u8) -> *const u8 {
    unsafe {
        // Only a uniquely-owned (non-static) heap string can be trimmed in place;
        // any other representation copies via `aipl_trim`. Matching (rather than an
        // `is_*` chain) means a new representation must explicitly opt into the
        // in-place path. `header_of` is valid only for the heap case.
        let StrRepr::Heap = str_repr(s) else {
            return aipl_trim(s);
        };
        if *header_of(s) == STATIC_REFCOUNT {
            return aipl_trim(s);
        }
        let n = heap_len(s);
        let mut start = 0;
        while start < n && is_ascii_ws(*s.add(start)) {
            start += 1;
        }
        let mut end = n;
        while end > start && is_ascii_ws(*s.add(end - 1)) {
            end -= 1;
        }
        let len = end - start;
        let data = s as *mut u8;
        if len <= 7 {
            // SSO: the trimmed result is inline — copy it out and free the block.
            let mut tmp = [0u8; 7];
            memcpy(
                tmp.as_mut_ptr() as *mut c_void,
                data.add(start) as *const c_void,
                len,
            );
            rt_free(s.sub(STR_HEADER_SIZE) as *mut c_void);
            return pack_inline(&tmp[..len]);
        }
        if start > 0 {
            // Shift the trimmed content to the front (regions overlap).
            memmove(data as *mut c_void, data.add(start) as *const c_void, len);
        }
        *data.add(len) = 0;
        // Shrink the block to fit; reuses it (never grows).
        let block = s.sub(STR_HEADER_SIZE) as *mut c_void;
        let raw = rt_realloc(block, STR_HEADER_SIZE + len + 1) as *mut u8;
        if raw.is_null() {
            abort();
        }
        *(raw as *mut i64) = len as i64; // updated stored length (block word 0)
        raw.add(STR_HEADER_SIZE)
    }
}

/// In-place concat for a *uniquely owned* string (codegen calls this only for
/// `set s = s + rhs` when static analysis proves `s` is unaliased). Grows `s`'s
/// buffer with `realloc` and appends `b`, then drops `b`. A static string (a
/// literal) can't be grown in place, so it falls back to a copy — which is also
/// why the first append off a literal still shows as a counted allocation,
/// keeping the alloc/dealloc tally balanced. `a` is reused, so it isn't dropped.
#[no_mangle]
pub extern "C" fn aipl_concat_mut(a: *const u8, b: *const u8) -> *const u8 {
    unsafe {
        // Only a uniquely-owned (non-static) heap `a` can be grown in place; any
        // other representation copies via `aipl_concat`. Matching (rather than an
        // `is_*` chain) means a new representation must explicitly opt into the
        // in-place path. `header_of` is valid only for the heap case.
        let StrRepr::Heap = str_repr(a) else {
            return aipl_concat(a, b);
        };
        if *header_of(a) == STATIC_REFCOUNT {
            return aipl_concat(a, b);
        }
        // `a` is a heap str; read its stored length. `b` may be inline, so
        // materialize it.
        let la = heap_len(a);
        let mut bbuf = [0u8; 8];
        let sb = str_bytes(b, &mut bbuf);
        let lb = sb.len();
        let block = a.sub(STR_HEADER_SIZE) as *mut c_void;
        let raw = rt_realloc(block, STR_HEADER_SIZE + la + lb + 1) as *mut u8;
        if raw.is_null() {
            abort();
        }
        *(raw as *mut i64) = (la + lb) as i64; // updated stored length (block word 0)
        let data = raw.add(STR_HEADER_SIZE);
        if lb > 0 {
            memcpy(
                data.add(la) as *mut c_void,
                sb.as_ptr() as *const c_void,
                lb,
            );
        }
        *data.add(la + lb) = 0;
        aipl_dec(b);
        data
    }
}

// ---------- Refcounted array runtime ----------
//
// Layout mirrors the JIT runtime in `src/codegen.rs`:
//   [refcount: i64][len: i64][cap: i64][drop_fn: ptr][elem0: i64]...
// The data pointer points at the `len` field; element i is at
// `ptr + ARR_ELEMS_OFFSET + i*8`. `cap` is the number of element slots the
// block was allocated for (>= len); spare capacity lets `aipl_array_push_mut`
// append without reallocating. `drop_fn` is null for scalar elements; for heap
// elements (`str`, nested arrays) it releases each element at refcount zero and
// marks the elements as heap pointers for `push`.

type ArrDropFn = extern "C" fn(*const u8, i64);

const ARR_CAP_OFFSET: usize = 8; // capacity of the element region, in *bytes*
const ARR_DROPFN_OFFSET: usize = 16; // element drop-fn pointer (null = scalars)
const ARR_ELEMS_OFFSET: usize = 24; // first element, relative to data ptr

// Array representation tag bits (stored in the low 2 bits of the data pointer,
// which are free since arrays are 8-byte aligned). Mirrors the JIT runtime.
const ARR_TAG_MASK: usize = 0b11;
const ARR_HEAP_TAG: usize = 0b00;
const ARR_REV_TAG: usize = 0b01;

// Reversed-view block layout (relative to the data pointer, after HEADER_SIZE).
// The block is `REV_BLOCK_DATA_SIZE` bytes; its pointer is tagged with ARR_REV_TAG.
const REV_LEN_OFFSET: usize = 0; // mirrors ARR_LEN_OFFSET; len of the view
const REV_INNER_OFFSET: usize = 8; // pointer to the inner (heap) array
const REV_DROP_OFFSET: usize = 16; // element drop_fn (i64)
const REV_RETAIN_OFFSET: usize = 24; // element retain_fn (i64)
const REV_ELEMSIZE_OFFSET: usize = 32; // element stride (i64); 0 = bit-packed
const REV_BLOCK_DATA_SIZE: usize = 40;

#[derive(Clone, Copy)]
enum ArrRepr {
    Heap,
    Reversed,
}

fn arr_repr(ptr: *const u8) -> ArrRepr {
    match ptr as usize & ARR_TAG_MASK {
        ARR_HEAP_TAG => ArrRepr::Heap,
        ARR_REV_TAG => ArrRepr::Reversed,
        tag => panic!("unknown array repr tag {tag}"),
    }
}

fn arr_untag(ptr: *const u8) -> *const u8 {
    (ptr as usize & !ARR_TAG_MASK) as *const u8
}

/// Allocate a reversed-view block.  Transfers ownership of `inner` into
/// the view (no drop, no retain on `inner`).  Returns data_ptr | ARR_REV_TAG.
unsafe fn alloc_reversed_view(
    inner: *const u8,
    len: usize,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    unsafe {
        let raw = rt_alloc(HEADER_SIZE + REV_BLOCK_DATA_SIZE) as *mut u8;
        if raw.is_null() {
            abort();
        }
        *(raw as *mut i64) = 1; // refcount
        let data = raw.add(HEADER_SIZE);
        *(data.add(REV_LEN_OFFSET) as *mut i64) = len as i64;
        *(data.add(REV_INNER_OFFSET) as *mut *const u8) = inner;
        *(data.add(REV_DROP_OFFSET) as *mut i64) = drop_fn;
        *(data.add(REV_RETAIN_OFFSET) as *mut i64) = retain_fn;
        *(data.add(REV_ELEMSIZE_OFFSET) as *mut i64) = elem_size;
        (data as usize | ARR_REV_TAG) as *const u8
    }
}

/// Materialize a reversed view into a fresh heap array, consuming the view
/// (dec + free the view block).  The inner array is dec'd too.
unsafe fn do_arr_reverse(a: *const u8, drop_fn: i64, retain_fn: i64, elem_size: i64) -> *const u8 {
    unsafe {
        let u = arr_untag(a);
        let inner = *(u.add(REV_INNER_OFFSET) as *const *const u8);
        let len = *(u as *const i64) as usize;
        if elem_size == ELEM_BITPACKED {
            let raw = array_alloc(len, len, drop_fn, ELEM_BITPACKED) as *const u8;
            let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            for i in 0..len {
                let j = len - 1 - i;
                write_packed_bit(dst, i, arr_load_bit_rt(inner, j));
            }
            raw
        } else {
            let es = (elem_size.max(8)) as usize;
            let raw = array_alloc(len, len, drop_fn, elem_size) as *const u8;
            let dst_base = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            for i in 0..len {
                let j = len - 1 - i;
                let src = arr_elem_ptr_rt(inner, j, es);
                memcpy(dst_base.add(i * es) as *mut c_void, src as *const c_void, es);
            }
            elem_rc(retain_fn, dst_base, len);
            raw
        }
    }
}

/// Ensure `a` is a heap array, materializing it if it's a reversed view.
/// Consumes `a` (it's dec'd / freed if a view was materialized).
fn aipl_arr_ensure_heap(a: *const u8) -> *const u8 {
    if a.is_null() {
        return a;
    }
    match arr_repr(a) {
        ArrRepr::Heap => a,
        ArrRepr::Reversed => {
            let u = arr_untag(a);
            let (inner, drop_fn, retain_fn, elem_size) = unsafe {
                (
                    *(u.add(REV_INNER_OFFSET) as *const *const u8),
                    *(u.add(REV_DROP_OFFSET) as *const i64),
                    *(u.add(REV_RETAIN_OFFSET) as *const i64),
                    *(u.add(REV_ELEMSIZE_OFFSET) as *const i64),
                )
            };
            let heap = unsafe { do_arr_reverse(a, drop_fn, retain_fn, elem_size) };
            aipl_array_dec(a);
            // dec the inner (do_arr_reverse didn't)
            aipl_array_dec(inner);
            heap
        }
    }
}

unsafe fn heap_elem_ptr_rt(base: *const u8, idx: usize, elem_size: usize) -> *const u8 {
    unsafe { base.add(ARR_ELEMS_OFFSET).add(idx * elem_size) }
}

unsafe fn arr_elem_ptr_rt(a: *const u8, idx: usize, elem_size: usize) -> *const u8 {
    match arr_repr(a) {
        ArrRepr::Heap => unsafe { heap_elem_ptr_rt(arr_untag(a), idx, elem_size) },
        ArrRepr::Reversed => {
            let u = arr_untag(a);
            let inner = unsafe { *(u.add(REV_INNER_OFFSET) as *const *const u8) };
            let len = unsafe { *(u as *const i64) as usize };
            unsafe { arr_elem_ptr_rt(inner, len - 1 - idx, elem_size) }
        }
    }
}

unsafe fn arr_load_bit_rt(a: *const u8, idx: usize) -> bool {
    match arr_repr(a) {
        ArrRepr::Heap => {
            let base = unsafe { arr_untag(a).add(ARR_ELEMS_OFFSET) };
            unsafe { (*base.add(idx >> 3) >> (idx & 7)) & 1 != 0 }
        }
        ArrRepr::Reversed => {
            let u = arr_untag(a);
            let inner = unsafe { *(u.add(REV_INNER_OFFSET) as *const *const u8) };
            let len = unsafe { *(u as *const i64) as usize };
            unsafe { arr_load_bit_rt(inner, len - 1 - idx) }
        }
    }
}

// Element size is known at compile time, so codegen passes it to these fns as a
// constant rather than storing it in the header. The header keeps the element-
// region capacity in *bytes* so `aipl_array_dec` can free without it.

unsafe fn array_len(ptr: *const u8) -> usize {
    unsafe { *(arr_untag(ptr) as *const i64) as usize }
}

unsafe fn array_cap_bytes(ptr: *const u8) -> usize {
    unsafe { *(arr_untag(ptr).add(ARR_CAP_OFFSET) as *const i64) as usize }
}

/// Retain/drop `count` elements at `at` via a helper-fn pointer, if non-null.
unsafe fn elem_rc(fn_ptr: i64, at: *const u8, count: usize) {
    if fn_ptr != 0 {
        let f: ArrDropFn = unsafe { core::mem::transmute(fn_ptr) };
        f(at, count as i64);
    }
}

// `bool[]` is bit-packed (8 elements per byte). Codegen signals it with an
// `elem_size` of 0; `len` still counts elements, `cap` (bytes) is `ceil(len/8)`.
const ELEM_BITPACKED: i64 = 0;

/// Bytes to hold `count` elements: `ceil(count/8)` bit-packed, else
/// `count * elem_size` (8-byte floor).
fn cap_bytes_for(elem_size: i64, count: usize) -> usize {
    if elem_size == ELEM_BITPACKED {
        (count + 7) / 8
    } else {
        let es = if elem_size < 8 { 8 } else { elem_size as usize };
        count * es
    }
}

unsafe fn write_packed_bit(data: *mut u8, idx: usize, val: bool) {
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

/// Allocate an array block holding `cap` slots of `elem_size` bytes (or
/// bit-packed bools when `elem_size == 0`), with `len` and `drop_fn` set and the
/// byte-capacity recorded (refcount 1).
unsafe fn array_alloc(len: usize, cap: usize, drop_fn: i64, elem_size: i64) -> *mut u8 {
    unsafe {
        let cap_bytes = cap_bytes_for(elem_size, cap);
        let raw = rt_alloc(HEADER_SIZE + ARR_ELEMS_OFFSET + cap_bytes) as *mut u8;
        if raw.is_null() {
            abort();
        }
        *(raw as *mut i64) = 1; // refcount
        *(raw.add(HEADER_SIZE) as *mut i64) = len as i64;
        *(raw.add(HEADER_SIZE + ARR_CAP_OFFSET) as *mut i64) = cap_bytes as i64;
        *(raw.add(HEADER_SIZE + ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
        raw.add(HEADER_SIZE)
    }
}

#[no_mangle]
pub extern "C" fn aipl_array_new(len: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    let len = if len < 0 { 0 } else { len as usize };
    // A fresh literal is allocated to exactly its length (cap == len).
    unsafe { array_alloc(len, len, drop_fn, elem_size) }
}

/// Allocate an empty array (len 0, refcount 1) reserved to `cap` slots of
/// `elem_size` bytes with the given element `drop_fn`. Used by `map`/`filter` to
/// pre-size their output.
#[no_mangle]
pub extern "C" fn aipl_array_with_cap(cap: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    let cap = if cap < 0 { 0 } else { cap as usize };
    unsafe { array_alloc(0, cap, drop_fn, elem_size) }
}

#[no_mangle]
pub extern "C" fn aipl_array_dec(ptr: *const u8) {
    if ptr.is_null() {
        return;
    }
    let u = arr_untag(ptr);
    unsafe {
        let h = header_of(u);
        if *h == STATIC_REFCOUNT {
            return;
        }
        *h -= 1;
        if *h == 0 {
            match arr_repr(ptr) {
                ArrRepr::Heap => {
                    let len = array_len(u);
                    let drop_fn = *(u.add(ARR_DROPFN_OFFSET) as *const i64);
                    if drop_fn != 0 {
                        let f: ArrDropFn = core::mem::transmute(drop_fn);
                        f(u.add(ARR_ELEMS_OFFSET), len as i64);
                    }
                    rt_free(h as *mut c_void);
                }
                ArrRepr::Reversed => {
                    let inner = *(u.add(REV_INNER_OFFSET) as *const *const u8);
                    aipl_array_dec(inner);
                    rt_free(h as *mut c_void);
                }
            }
        }
    }
}

/// Retain an array value (any representation). Uses `arr_untag` to strip the
/// representation tag before touching the refcount.
#[no_mangle]
pub extern "C" fn aipl_arr_inc(ptr: *const u8) {
    if ptr.is_null() {
        return;
    }
    let u = arr_untag(ptr);
    unsafe {
        let h = header_of(u);
        if *h != STATIC_REFCOUNT {
            *h += 1;
        }
    }
}

/// Copy-and-grow push (value semantics): returns a fresh array holding `a`'s
/// elements followed by the `elem_size`-byte element at `x`, then drops `a`.
/// Repr-aware element pointer for use from AOT-compiled code.
/// Returns a pointer to element `idx`, handling reversed views via recursion.
/// `elem_size` must be > 0 (not bit-packed — use `aipl_arr_load_bit` for bools).
#[no_mangle]
pub extern "C" fn aipl_arr_elem_ptr(a: *const u8, idx: i64, elem_size: i64) -> *const u8 {
    unsafe { arr_elem_ptr_rt(a, idx as usize, elem_size as usize) }
}

/// Repr-aware bit load for AOT-compiled code. Returns 0 or 1 as i64.
#[no_mangle]
pub extern "C" fn aipl_arr_load_bit(a: *const u8, idx: i64) -> i64 {
    i64::from(unsafe { arr_load_bit_rt(a, idx as usize) })
}

/// `retain_fn` retains the copied elements (the new array co-owns them).
#[no_mangle]
pub extern "C" fn aipl_array_push(
    a: *const u8,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    unsafe {
        let old_len = if a.is_null() { 0 } else { array_len(a) };
        if elem_size == ELEM_BITPACKED {
            // Bit-packed `bool[]`: copy the old bits, set the new one, drop `a`.
            let raw = array_alloc(old_len + 1, old_len + 1, drop_fn, ELEM_BITPACKED);
            let dst = raw.add(ARR_ELEMS_OFFSET);
            if old_len > 0 && !a.is_null() {
                memcpy(
                    dst as *mut c_void,
                    a.add(ARR_ELEMS_OFFSET) as *const c_void,
                    cap_bytes_for(ELEM_BITPACKED, old_len),
                );
            }
            write_packed_bit(dst, old_len, *(x as *const i64) != 0);
            aipl_array_dec(a);
            return raw;
        }
        let elem_size = if elem_size < 8 { 8 } else { elem_size as usize };
        let raw = array_alloc(old_len + 1, old_len + 1, drop_fn, elem_size as i64);
        let dst = raw.add(ARR_ELEMS_OFFSET);
        if old_len > 0 && !a.is_null() {
            let src = a.add(ARR_ELEMS_OFFSET);
            memcpy(
                dst as *mut c_void,
                src as *const c_void,
                old_len * elem_size,
            );
            elem_rc(retain_fn, dst, old_len);
        }
        let slot = dst.add(old_len * elem_size);
        memcpy(slot as *mut c_void, x as *const c_void, elem_size);
        elem_rc(retain_fn, slot, 1);
        aipl_array_dec(a);
        raw
    }
}

/// In-place push for a *uniquely owned* array (codegen calls this only when its
/// static analysis proves the array isn't aliased). Appends without copying when
/// there's spare capacity; otherwise grows to a doubled byte-capacity by
/// `realloc`. Returns the (possibly relocated) data pointer.
#[no_mangle]
pub extern "C" fn aipl_array_push_mut(
    a: *const u8,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    unsafe {
        let old_len = if a.is_null() { 0 } else { array_len(a) };
        let cap_bytes = if a.is_null() { 0 } else { array_cap_bytes(a) };
        if elem_size == ELEM_BITPACKED {
            // Bit-packed `bool[]`, in place: set bit `old_len`, growing the byte
            // capacity (doubling) only when the next bit needs a new byte.
            let val = *(x as *const i64) != 0;
            if !a.is_null() && cap_bytes_for(ELEM_BITPACKED, old_len + 1) <= cap_bytes {
                write_packed_bit(a.add(ARR_ELEMS_OFFSET) as *mut u8, old_len, val);
                *(a as *mut i64) = (old_len + 1) as i64;
                *(a.add(ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
                return a;
            }
            let mut new_cap_bytes = cap_bytes_for(ELEM_BITPACKED, old_len + 1);
            if cap_bytes * 2 > new_cap_bytes {
                new_cap_bytes = cap_bytes * 2;
            }
            if new_cap_bytes == 0 {
                new_cap_bytes = 1;
            }
            let data = if a.is_null() {
                array_alloc(old_len + 1, new_cap_bytes * 8, drop_fn, ELEM_BITPACKED)
            } else {
                let block = a.sub(HEADER_SIZE) as *mut c_void;
                let raw =
                    rt_realloc(block, HEADER_SIZE + ARR_ELEMS_OFFSET + new_cap_bytes) as *mut u8;
                if raw.is_null() {
                    abort();
                }
                let data = raw.add(HEADER_SIZE);
                *(data.add(ARR_CAP_OFFSET) as *mut i64) = new_cap_bytes as i64;
                *(data.add(ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
                *(data as *mut i64) = (old_len + 1) as i64;
                data
            };
            write_packed_bit(data.add(ARR_ELEMS_OFFSET) as *mut u8, old_len, val);
            return data;
        }
        let elem_size = if elem_size < 8 { 8 } else { elem_size as usize };
        if !a.is_null() && (old_len + 1) * elem_size <= cap_bytes {
            // Spare capacity: append in place, no allocation.
            let elems = a.add(ARR_ELEMS_OFFSET);
            let slot = elems.add(old_len * elem_size);
            memcpy(slot as *mut c_void, x as *const c_void, elem_size);
            elem_rc(retain_fn, slot, 1);
            *(a as *mut i64) = (old_len + 1) as i64; // len += 1
                                                     // Keep the stored drop-fn in sync: an array reserved via
                                                     // `aipl_array_with_cap` (or an empty `[]`) starts with none (0) and
                                                     // first learns its element type here when it has spare capacity and
                                                     // never hits the realloc path that would otherwise set it.
            *(a.add(ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
            return a;
        }
        // At capacity: grow to a doubled byte-capacity. `realloc` preserves the
        // header and existing elements (refcounts unchanged), so no element is
        // re-retained and there's no old block to free — only the new one.
        let new_cap_bytes = core::cmp::max((old_len + 1) * elem_size, cap_bytes * 2);
        let data = if a.is_null() {
            // No block to grow (defensive; exclusive arrays start non-null).
            array_alloc(
                old_len + 1,
                new_cap_bytes / elem_size,
                drop_fn,
                elem_size as i64,
            )
        } else {
            let block = a.sub(HEADER_SIZE) as *mut c_void;
            let raw = rt_realloc(block, HEADER_SIZE + ARR_ELEMS_OFFSET + new_cap_bytes) as *mut u8;
            if raw.is_null() {
                abort();
            }
            let data = raw.add(HEADER_SIZE);
            *(data.add(ARR_CAP_OFFSET) as *mut i64) = new_cap_bytes as i64;
            // Refresh drop_fn: an empty `[]` is created with none (0) and only
            // learns its element type on the first push.
            *(data.add(ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
            *(data as *mut i64) = (old_len + 1) as i64; // len
            data
        };
        let elems = data.add(ARR_ELEMS_OFFSET);
        let slot = elems.add(old_len * elem_size);
        memcpy(slot as *mut c_void, x as *const c_void, elem_size);
        elem_rc(retain_fn, slot, 1);
        data
    }
}

// ---------- Set runtime ----------
//
// A set reuses the array heap block verbatim; only construction differs
// (deduplicated insert). Elements are i64/bool/char (compared by value, a
// bit-compare for a packed `bool` set, no element drop/retain) or `str` (8-byte
// pointers compared by content, with the array `str` drop/retain helpers stored
// so the block frees/retains its strings). Mirrors codegen.

/// Compare two NUL-terminated runtime strings by content; a null pointer equals
/// only itself.
unsafe fn rt_str_eq(a: *const u8, b: *const u8) -> bool {
    if a == b {
        return true; // same allocation/value (incl. two equal inline strings), or both null
    }
    if a.is_null() || b.is_null() {
        return false; // a null str equals only itself
    }
    if aipl_str_len(a) != aipl_str_len(b) {
        return false; // O(1) length check — unequal lengths can't be equal
    }
    // Equal lengths: stream the rope side (if any) leaf-by-leaf and compare each
    // chunk against the other side made contiguous. Materializes only when BOTH
    // sides are ropes (rare); the common rope-vs-literal case copies nothing.
    let (rope, other) = if matches!(str_repr(a), StrRepr::Rope(_)) {
        (a, b)
    } else {
        (b, a)
    };
    let mut ob = [0u8; 8];
    let obytes = unsafe { str_bytes(other, &mut ob) };
    let mut off = 0usize;
    str_for_each_chunk(rope, &mut |chunk| {
        let end = off + chunk.len();
        let ok = chunk == &obytes[off..end];
        off = end;
        ok
    })
}

/// `str == str`: content-compare, then decrement both inputs (consumes a ref
/// from each; callers pre-inc). Returns 1/0. Mirrors the JIT `aipl_str_eq`.
#[no_mangle]
pub extern "C" fn aipl_str_eq(a: *const u8, b: *const u8) -> i64 {
    let eq = unsafe { rt_str_eq(a, b) };
    aipl_dec(a);
    aipl_dec(b);
    i64::from(eq)
}

/// `s.starts_with(prefix) -> bool` (1/0): whether `s`'s bytes begin with
/// `prefix`'s. Consumes (decs) both inputs; callers pre-inc. The empty prefix
/// always matches. Mirrors the JIT `aipl_str_starts_with`.
#[no_mangle]
pub extern "C" fn aipl_str_starts_with(s: *const u8, prefix: *const u8) -> i64 {
    let pl = aipl_str_len(prefix) as usize;
    let starts = if (aipl_str_len(s) as usize) < pl {
        false
    } else {
        // Stream `s` only until the prefix is matched (or mismatches); `prefix`
        // (usually a short literal) is read contiguously.
        let mut pb = [0u8; 8];
        let pbytes = unsafe { str_bytes(prefix, &mut pb) };
        let mut off = 0usize;
        let mut ok = true;
        str_for_each_chunk(s, &mut |chunk| {
            if off >= pl {
                return false; // whole prefix matched — stop scanning `s`
            }
            let take = core::cmp::min(chunk.len(), pl - off);
            if chunk[..take] != pbytes[off..off + take] {
                ok = false;
                return false;
            }
            off += take;
            true
        });
        ok
    };
    aipl_dec(s);
    aipl_dec(prefix);
    i64::from(starts)
}

/// `s.ends_with(suffix) -> bool` (1/0): whether `s`'s bytes end with `suffix`'s.
/// Consumes (decs) both inputs; callers pre-inc. The empty suffix always matches.
/// Mirrors the JIT `aipl_str_ends_with`.
#[no_mangle]
pub extern "C" fn aipl_str_ends_with(s: *const u8, suffix: *const u8) -> i64 {
    let sl = aipl_str_len(s) as usize;
    let ql = aipl_str_len(suffix) as usize;
    let ends = if sl < ql {
        false
    } else {
        // Stream `s` (reaching the end is unavoidable) and compare only the bytes
        // overlapping the trailing suffix region `[start, sl)` — no materializing.
        let start = sl - ql;
        let mut qb = [0u8; 8];
        let qbytes = unsafe { str_bytes(suffix, &mut qb) };
        let mut pos = 0usize;
        let mut ok = true;
        str_for_each_chunk(s, &mut |chunk| {
            let cstart = pos;
            let cend = pos + chunk.len();
            pos = cend;
            if cend > start {
                let from = if cstart < start { start } else { cstart };
                let cs = from - cstart;
                let qs = from - start;
                let n = cend - from;
                if chunk[cs..cs + n] != qbytes[qs..qs + n] {
                    ok = false;
                    return false;
                }
            }
            true
        });
        ok
    };
    aipl_dec(s);
    aipl_dec(suffix);
    i64::from(ends)
}

/// FNV-1a content hash of a NUL-terminated string (consistent with `rt_str_eq`).
/// Borrows `a` (no refcount change). Mirrors the JIT `aipl_str_hash`.
#[no_mangle]
pub extern "C" fn aipl_str_hash(a: *const u8) -> i64 {
    // FNV-1a is a left fold over bytes, so streaming a rope's leaves in order
    // gives the same result as the flattened bytes — no materialization.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    str_for_each_chunk(a, &mut |chunk| {
        for &c in chunk {
            h ^= c as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        true
    });
    h as i64
}

/// Whether `a` already contains the element at `x` (1/0). `str_cmp != 0` compares
/// `str` elements by content; otherwise by value (bit-packed for a `bool` set).
/// A null/empty set is never a member.
#[no_mangle]
pub extern "C" fn aipl_set_contains(
    a: *const u8,
    x: *const u8,
    elem_size: i64,
    str_cmp: i64,
) -> i64 {
    if a.is_null() {
        return 0;
    }
    unsafe {
        let len = array_len(a);
        if str_cmp != 0 {
            let target = *(x as *const i64) as *const u8;
            for i in 0..len {
                let ep = arr_elem_ptr_rt(a, i, 8);
                let s = *(ep as *const i64) as *const u8;
                if rt_str_eq(s, target) {
                    return 1;
                }
            }
            0
        } else if elem_size == ELEM_BITPACKED {
            let target = *(x as *const i64) != 0;
            for i in 0..len {
                if arr_load_bit_rt(a, i) == target {
                    return 1;
                }
            }
            0
        } else {
            let stride = (elem_size.max(8)) as usize;
            let target = *(x as *const i64);
            for i in 0..len {
                let ep = arr_elem_ptr_rt(a, i, stride);
                if *(ep as *const i64) == target {
                    return 1;
                }
            }
            0
        }
    }
}

/// Dedup-insert the element at `x` into the uniquely-owned array-backed set `a`
/// (membership per `str_cmp`); returns the (possibly relocated) set. `drop_fn`/
/// `retain_fn` are the element helpers for `str`, else 0.
#[no_mangle]
pub extern "C" fn aipl_set_insert(
    a: *const u8,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
) -> *const u8 {
    if aipl_set_contains(a, x, elem_size, str_cmp) != 0 {
        return a;
    }
    aipl_array_push_mut(a, x, drop_fn, retain_fn, elem_size)
}

/// Read element `i` of set/array `src` as an i64 (bit-unpacked `bool` when
/// `elem_size == 0`, else the 8-byte value). Repr-aware. Mirrors codegen's `read_set_elem`.
unsafe fn read_set_elem(src: *const u8, i: usize, elem_size: i64) -> i64 {
    if elem_size == ELEM_BITPACKED {
        i64::from(unsafe { arr_load_bit_rt(src, i) })
    } else {
        let stride = (elem_size.max(8)) as usize;
        let ep = unsafe { arr_elem_ptr_rt(src, i, stride) };
        unsafe { *(ep as *const i64) }
    }
}

/// `a.union(b)` (copy): a fresh set with every distinct element of `a` then `b`;
/// consumes (decs) both inputs. Mirrors codegen.
#[no_mangle]
pub extern "C" fn aipl_set_union(
    a: *const u8,
    b: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
) -> *const u8 {
    unsafe {
        let a_len = if a.is_null() { 0 } else { array_len(a) };
        let b_len = if b.is_null() { 0 } else { array_len(b) };
        let mut dest = aipl_array_with_cap((a_len + b_len) as i64, drop_fn, elem_size);
        for i in 0..a_len {
            let v = read_set_elem(a, i, elem_size);
            dest = aipl_set_insert(
                dest,
                &v as *const i64 as *const u8,
                drop_fn,
                retain_fn,
                elem_size,
                str_cmp,
            );
        }
        for i in 0..b_len {
            let v = read_set_elem(b, i, elem_size);
            dest = aipl_set_insert(
                dest,
                &v as *const i64 as *const u8,
                drop_fn,
                retain_fn,
                elem_size,
                str_cmp,
            );
        }
        aipl_array_dec(a);
        aipl_array_dec(b);
        dest
    }
}

/// `set a = a.union(b)` for an exclusive `a`: extend `a` in place with `b`'s
/// distinct elements and return the (possibly relocated) set; consumes (decs)
/// `b`, reuses `a`. Mirrors codegen.
#[no_mangle]
pub extern "C" fn aipl_set_union_mut(
    a: *const u8,
    b: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
) -> *const u8 {
    unsafe {
        let mut a = a;
        let b_len = if b.is_null() { 0 } else { array_len(b) };
        for i in 0..b_len {
            let v = read_set_elem(b, i, elem_size);
            a = aipl_set_insert(
                a,
                &v as *const i64 as *const u8,
                drop_fn,
                retain_fn,
                elem_size,
                str_cmp,
            );
        }
        aipl_array_dec(b);
        a
    }
}

/// Index of the pair in dict `a` whose key matches the key at `pair_ptr` (its
/// first 8 bytes), or -1. Mirrors codegen's `dict_find`.
unsafe fn dict_find(a: *const u8, pair_ptr: *const u8, pair_size: i64, str_cmp: i64) -> i64 {
    if a.is_null() {
        return -1;
    }
    unsafe {
        let len = array_len(a);
        let stride = pair_size as usize;
        let want = *(pair_ptr as *const i64);
        for i in 0..len {
            let ep = arr_elem_ptr_rt(a, i, stride);
            let k = *(ep as *const i64);
            let eq = if str_cmp != 0 {
                rt_str_eq(k as *const u8, want as *const u8)
            } else {
                k == want
            };
            if eq {
                return i as i64;
            }
        }
        -1
    }
}

/// Insert (or, on a duplicate key, replace) the `[key][value]` pair at `pair_ptr`
/// into the uniquely-owned dict `a`; returns the (possibly relocated) dict. The
/// pair helpers release/retain a pair's key and value. Mirrors codegen.
#[no_mangle]
pub extern "C" fn aipl_dict_insert(
    a: *const u8,
    pair_ptr: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    unsafe {
        let idx = dict_find(a, pair_ptr, pair_size, str_cmp);
        if idx >= 0 {
            let stride = pair_size as usize;
            let slot = arr_elem_ptr_rt(a, idx as usize, stride) as *mut u8;
            elem_rc(drop_fn, slot, 1);
            core::ptr::copy_nonoverlapping(pair_ptr, slot, stride);
            elem_rc(retain_fn, slot, 1);
            return a;
        }
    }
    aipl_array_push_mut(a, pair_ptr, drop_fn, retain_fn, pair_size)
}

/// Look up `key_ptr` in dict `a`: a pointer to the matching pair's value slot, or
/// null if absent. Borrows `a`. Mirrors codegen.
#[no_mangle]
pub extern "C" fn aipl_dict_get(
    a: *const u8,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    unsafe {
        let idx = dict_find(a, key_ptr, pair_size, str_cmp);
        if idx < 0 {
            return core::ptr::null();
        }
        arr_elem_ptr_rt(a, idx as usize, pair_size as usize).add(8)
    }
}

/// `d.contains_key(k)`: whether `key_ptr` is a key of dict `a`. Borrows `a`.
#[no_mangle]
pub extern "C" fn aipl_dict_contains_key(
    a: *const u8,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> i64 {
    (unsafe { dict_find(a, key_ptr, pair_size, str_cmp) } >= 0) as i64
}

/// Element drop-fn for `str[]`: dec each element string.
#[no_mangle]
pub extern "C" fn aipl_arr_drop_str(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_dec(*elems.add(i) as *const u8);
        }
    }
}

/// Element drop-fn for an array of arrays (`T[][]`): release each element
/// array (which recursively releases its own elements).
#[no_mangle]
pub extern "C" fn aipl_arr_drop_arr(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_array_dec(*elems.add(i) as *const u8);
        }
    }
}

/// Element retain-fn for `str[]`/`T[][]`: inc each element pointer.
#[no_mangle]
pub extern "C" fn aipl_arr_retain_ptr(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_inc(*elems.add(i) as *const u8);
        }
    }
}

/// Element drop-fn for `str?[]`: each element is an inline 16-byte `{tag, value}`
/// optional; dec the inner string when present (tag != 0).
#[no_mangle]
pub extern "C" fn aipl_arr_drop_opt_str(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            if *(e as *const i64) != 0 {
                aipl_dec(*(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Element drop-fn for `T[]?[]`: release the inner array of each present element.
#[no_mangle]
pub extern "C" fn aipl_arr_drop_opt_arr(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            if *(e as *const i64) != 0 {
                aipl_array_dec(*(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Element retain-fn for `str?[]`/`T[]?[]`: inc the inner heap pointer of each
/// present element (tag != 0).
#[no_mangle]
pub extern "C" fn aipl_arr_retain_opt(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            if *(e as *const i64) != 0 {
                aipl_inc(*(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Allocate an AIPL `str` (refcounted, NUL-terminated) holding a copy of the
/// C string `cstr` (e.g. a CLI argument). Goes through `make_str`, so the result
/// is inline (<= 7 bytes) or a heap string with its length stored.
unsafe fn aipl_str_from_cstr(cstr: *const c_char) -> *const u8 {
    unsafe {
        let n = strlen(cstr);
        // SSO: a short arg is inline — no allocation.
        make_str(core::slice::from_raw_parts(cstr as *const u8, n))
    }
}

/// Build the CLI arguments as an AIPL `str[]`, excluding `argv[0]` (the program
/// name) so a program sees only the arguments a user passed. The array owns its
/// element strings via the `str[]` drop-fn; `main` releases the whole thing.
unsafe fn build_cli_args(argc: c_int, argv: *const *const c_char) -> *const u8 {
    unsafe {
        let n = if argc > 1 { (argc - 1) as i64 } else { 0 };
        let drop_fn = aipl_arr_drop_str as ArrDropFn as usize as i64;
        let arr = aipl_array_new(n, drop_fn, 8);
        let elems = arr.add(ARR_ELEMS_OFFSET) as *mut i64;
        for i in 0..n as usize {
            let s = aipl_str_from_cstr(*argv.add(i + 1));
            *elems.add(i) = s as i64;
        }
        arr
    }
}

// Entry point. The user's `main` is emitted as `__aipl_user_main` when building
// a binary so we can wrap it with the platform-standard `int main(int, char**)`.
// `main` always takes the CLI args as a `str[]` (codegen injects an ignored
// parameter when the user's `main` declares none), so the ABI is uniform.
//
// `__aipl_main_wants_args` is a 1-byte flag the object emits (see codegen's
// `MAIN_WANTS_ARGS_SYMBOL`): nonzero iff the user's `main` actually declared the
// args parameter. When it's zero we skip building the array and pass null — the
// injected, ignored parameter then drops via `aipl_array_dec(null)` (a no-op),
// so a `main` that ignores args costs no allocation.
extern "C" {
    fn __aipl_user_main(args: *const u8) -> i64;
    static __aipl_main_wants_args: u8;
}

#[no_mangle]
pub extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args = if unsafe { __aipl_main_wants_args } != 0 {
        unsafe { build_cli_args(argc, argv) }
    } else {
        core::ptr::null()
    };
    // Perfmon (env-gated): time only the user `main` so process spawn / static
    // init are excluded. `getenv` is the sole cost when the var is unset.
    let perfmon_path = unsafe { getenv(b"AIPL_PERFMON_STATS\0".as_ptr() as *const c_char) };
    let perfmon = !perfmon_path.is_null();
    let t0 = if perfmon { perfmon_os::now_ns() } else { 0 };
    let code = unsafe { __aipl_user_main(args) as c_int };
    let t1 = if perfmon { perfmon_os::now_ns() } else { 0 };
    // Instrumented build only: after the program (and all its frees) finish,
    // dump the allocation tallies if a destination was requested.
    #[cfg(aipl_instrument)]
    unsafe {
        report_alloc_stats();
    }
    if perfmon {
        let peak = perfmon_os::peak_rss_bytes();
        unsafe { report_perfmon_stats(perfmon_path, t1.wrapping_sub(t0), peak) };
    }
    code
}

// ---------- Allocation reporting (instrumented build only) ----------
//
// When `AIPL_ALLOC_STATS` names a path, write the final tallies to it as
//   allocations: <N>
//   deallocations: <M>
//   reallocations: <K>
//   bytes allocated: <B>
//   instructions executed: <I>
// The test harness reads this back to verify a case's `--- performance ---`
// section. Opened in binary mode so no `\n` -> `\r\n` translation occurs.

// Used by both the instrumented alloc-stats reporter and the (env-gated) perfmon
// reporter, so declared unconditionally.
extern "C" {
    fn getenv(name: *const c_char) -> *const c_char;
    fn fputs(s: *const c_char, stream: *mut c_void) -> c_int;
}

/// Write `n` to `f` as a NUL-terminated decimal string (no separators).
unsafe fn fput_u64(f: *mut c_void, n: u64) {
    unsafe {
        let mut digits = [0u8; 20];
        let start = fmt_i64(&mut digits, n as i64);
        let len = digits.len() - start;
        // +1 for the NUL terminator `fputs` expects.
        let mut out = [0u8; 21];
        memcpy(
            out.as_mut_ptr() as *mut c_void,
            digits.as_ptr().add(start) as *const c_void,
            len,
        );
        out[len] = 0;
        fputs(out.as_ptr() as *const c_char, f);
    }
}

#[cfg(aipl_instrument)]
unsafe fn report_alloc_stats() {
    unsafe {
        let path = getenv(b"AIPL_ALLOC_STATS\0".as_ptr() as *const c_char);
        if path.is_null() {
            return;
        }
        let f = fopen(path, b"wb\0".as_ptr() as *const c_char);
        if f.is_null() {
            return;
        }
        fputs(b"allocations: \0".as_ptr() as *const c_char, f);
        fput_u64(f, ALLOC_COUNT.load(Ordering::Relaxed));
        fputs(b"\ndeallocations: \0".as_ptr() as *const c_char, f);
        fput_u64(f, FREE_COUNT.load(Ordering::Relaxed));
        fputs(b"\nreallocations: \0".as_ptr() as *const c_char, f);
        fput_u64(f, REALLOC_COUNT.load(Ordering::Relaxed));
        fputs(b"\nbytes allocated: \0".as_ptr() as *const c_char, f);
        fput_u64(f, ALLOC_BYTES.load(Ordering::Relaxed));
        fputs(b"\ninstructions executed: \0".as_ptr() as *const c_char, f);
        fput_u64(f, INSN_COUNT.load(Ordering::Relaxed));
        fputs(b"\n\0".as_ptr() as *const c_char, f);
        fclose(f);
    }
}

// ---------- Perfmon: in-process timing + peak memory (env-gated) ----------
//
// When `AIPL_PERFMON_STATS` names a path, the binary times its own *post-startup*
// execution (just the user `main`, so process spawn / static init are excluded)
// and reports peak resident memory, writing
//   wall_clock_ns: <N>
//   peak_rss_bytes: <M>
// The perf-monitor refresh in the test harness reads this back. When the env var
// is unset the whole path is skipped (one `getenv` at startup), so production
// `aipl build` binaries pay nothing and behavior is unchanged. The clock/memory
// queries are OS-specific; unsupported targets report 0.

#[cfg(windows)]
mod perfmon_os {
    use core::ffi::c_void;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    // All in kernel32 (the K32-prefixed memory query avoids a psapi link), which
    // the platform linker always pulls in — no extra link flags needed.
    extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    pub fn now_ns() -> u64 {
        unsafe {
            let (mut count, mut freq) = (0i64, 0i64);
            if QueryPerformanceCounter(&mut count) == 0
                || QueryPerformanceFrequency(&mut freq) == 0
                || freq <= 0
            {
                return 0;
            }
            // ticks / ticks-per-sec * 1e9, in u128 to avoid overflow.
            ((count as u128) * 1_000_000_000u128 / (freq as u128)) as u64
        }
    }

    pub fn peak_rss_bytes() -> u64 {
        unsafe {
            let mut pmc: ProcessMemoryCounters = core::mem::zeroed();
            pmc.cb = core::mem::size_of::<ProcessMemoryCounters>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
                pmc.peak_working_set_size as u64
            } else {
                0
            }
        }
    }
}

#[cfg(unix)]
mod perfmon_os {
    // CLOCK_MONOTONIC: 1 on Linux/BSD, 6 on macOS. RUSAGE_SELF = 0 everywhere.
    #[cfg(target_os = "macos")]
    const CLOCK_MONOTONIC: i32 = 6;
    #[cfg(not(target_os = "macos"))]
    const CLOCK_MONOTONIC: i32 = 1;

    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
        // `rusage` is large and layout-stable only in its first fields; read it as
        // a longword array and pick `ru_maxrss` (index 4: after ru_utime/ru_stime,
        // two 16-byte timevals). On Linux ru_maxrss is KB; on macOS it's bytes.
        fn getrusage(who: i32, usage: *mut [i64; 36]) -> i32;
    }

    pub fn now_ns() -> u64 {
        unsafe {
            let mut ts = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if clock_gettime(CLOCK_MONOTONIC, &mut ts) != 0 {
                return 0;
            }
            (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
        }
    }

    pub fn peak_rss_bytes() -> u64 {
        unsafe {
            let mut ru = [0i64; 36];
            if getrusage(0, &mut ru) != 0 {
                return 0;
            }
            let maxrss = ru[4].max(0) as u64;
            if cfg!(target_os = "macos") {
                maxrss // already bytes
            } else {
                maxrss * 1024 // KB -> bytes
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod perfmon_os {
    pub fn now_ns() -> u64 {
        0
    }
    pub fn peak_rss_bytes() -> u64 {
        0
    }
}

/// Write the perfmon stats to the `AIPL_PERFMON_STATS` path (binary mode, so no
/// `\n` -> `\r\n` translation). Mirrors `report_alloc_stats`'s file format.
unsafe fn report_perfmon_stats(path: *const c_char, wall_ns: u64, peak_bytes: u64) {
    unsafe {
        let f = fopen(path, b"wb\0".as_ptr() as *const c_char);
        if f.is_null() {
            return;
        }
        fputs(b"wall_clock_ns: \0".as_ptr() as *const c_char, f);
        fput_u64(f, wall_ns);
        fputs(b"\npeak_rss_bytes: \0".as_ptr() as *const c_char, f);
        fput_u64(f, peak_bytes);
        fputs(b"\n\0".as_ptr() as *const c_char, f);
        fclose(f);
    }
}

// ---------- Test-runner hooks ----------
//
// The `check` command's JIT runner is the authoritative test reporter (see the
// JIT runtime in `aipl-codegen`). These AOT stubs exist only so a *library* case
// (one with `.test` blocks but no `main`) can be built from its synthesized test
// driver — the cases harness AOT-builds that driver to measure `--- performance
// ---`. They tally pass/fail and yield an exit code; failures are diagnosed by
// `aipl check`, so nothing is printed here.
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering as TestOrd};
static TEST_CUR_FAILED: AtomicBool = AtomicBool::new(false);
static TEST_FAILED: AtomicI64 = AtomicI64::new(0);

#[no_mangle]
pub extern "C" fn aipl_test_begin(_name: *const u8) {
    TEST_CUR_FAILED.store(false, TestOrd::Relaxed);
}

#[no_mangle]
pub extern "C" fn aipl_assert(cond: i64, _loc: *const u8) {
    if cond == 0 {
        TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_end() {
    if TEST_CUR_FAILED.load(TestOrd::Relaxed) {
        TEST_FAILED.fetch_add(1, TestOrd::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_summary() -> i64 {
    i64::from(TEST_FAILED.load(TestOrd::Relaxed) > 0)
}
