//! Cranelift JIT codegen.
//!
//! Types:
//!   - `i64` is the primitive integer.
//!   - `bool` (encoded 0/1 in an i64 at the ABI level).
//!   - Declared struct names — stack-allocated, passed by pointer.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
};

use cranelift::{
    codegen::{
        cursor::{Cursor, FuncCursor},
        ir::{Block, BlockArg, FuncRef, Function, Signature, StackSlot, UserFuncName},
        isa::TargetIsa,
        Context,
    },
    prelude::{
        settings, types, AbiParam, Configurable, FunctionBuilder, FunctionBuilderContext,
        InstBuilder, IntCC, MemFlags, StackSlotData, StackSlotKind, Value,
    },
};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

use aipl_syntax::{
    ast::{
        is_unit, Expr, ExprKind, Function as AstFn, Item, MatchArm, Param, Pattern, Primitive,
        Program, Signature as AstSignature, StructDecl, Type,
    },
    error_ty, is_array_elem, is_dict_key, is_error, is_int_ty, is_none_inner, is_set_elem,
    is_str_repr, type_name, IMPORTABLE_BUILTINS,
};
use aipl_syntax::{DebugOptions, Error, Span};

// ---------- Refcounted string runtime ----------
//
// Every str has an 8-byte header preceding the content:
//   [refcount: i64 LE][bytes..][null]
// The pointer the language uses points at `bytes`. Static literals carry
// `STATIC_REFCOUNT` (i64::MAX) so inc/dec become no-ops on them. Dynamic
// strings (from concat) start at 1 and are freed by `aipl_dec` when the
// count reaches 0.

// `HEADER_SIZE` is the refcount prefix shared by every refcounted heap block
// (strings AND arrays): the `i64` refcount sits at `ptr - HEADER_SIZE`, so
// `header_of`/inc/dec are common to both. Don't change it without auditing the
// array runtime.
const HEADER_SIZE: usize = 8;
const STATIC_REFCOUNT: i64 = i64::MAX;

// A heap *string* additionally stores its content length, in a word placed just
// *before* the refcount (so the refcount stays at `ptr - HEADER_SIZE`, shared
// with arrays): layout `[len: i64][refcount: i64][content bytes][NUL]`, with the
// value pointing at the first content byte. So `len` at `ptr - STR_HEADER_SIZE`
// (`-16`), refcount at `ptr - HEADER_SIZE` (`-8`), block start at `-16`. Storing
// the length means `len`/`str_bytes`/drop never walk to the NUL to count bytes
// (the NUL is kept only so a heap path can be handed to the C file API).
const STR_HEADER_SIZE: usize = 16;

/// The refcount cell of any heap block (string or array): the word at `-8`.
unsafe fn header_of(ptr: *const u8) -> *mut i64 {
    unsafe { ptr.sub(HEADER_SIZE) as *mut i64 }
}

/// The stored content length of a heap string (the word at `-16`, before refcount).
unsafe fn heap_len(ptr: *const u8) -> usize {
    unsafe { *(ptr.sub(STR_HEADER_SIZE) as *const i64) as usize }
}

// ---------- Small-string optimization (SSO) ----------
//
// A `str` value is either a heap/static pointer (8-byte aligned, so its low bits
// are 0) or an *inline* small string tagged `0b01` in the low two bits. Inline
// layout, as the value's bytes in memory (little-endian, like the rest of the
// runtime):
//   byte0 = (len << 2) | 1   with len in 0..=7   (low two bits are always 0b01)
//   bytes 1..=7 = content    (unused trailing bytes are 0)
// Strings of length <= 7 are stored inline — no allocation, no refcount; length
// >= 8 stay heap. The low two bits form the representation tag: 00 = heap/static,
// 01 = inline, 10 = view, 11 = concat. Shifting `len` by two (not one) keeps the
// inline tag exactly `0b01` regardless of the length's parity, which frees the
// `0b11` slot for the concat representation. `aipl_inc`/`aipl_dec` no-op on inline
// values, exactly like a static refcount, so refcounting and array element
// drop/retain need no special-casing. Consumers always *materialize* (read the
// bytes from any representation), so correctness never depends on the invariant;
// the "<=7 is always inline" invariant is purely what makes those strings free.

// The representation discriminant lives in the low two bits of every `str`
// value; `str_repr` decodes it into a [`StrRepr`]. Branch on a value's
// representation by `match`ing `str_repr(..)` (NOT ad-hoc `is_*` checks), so
// adding a representation here forces every dispatch site to handle it.
const TAG_MASK: usize = 0b11;
const HEAP_TAG: usize = 0b00;
const INLINE_TAG: usize = 0b01;

#[inline]
fn inline_len(v: *const u8) -> usize {
    ((v as usize) >> 2) & 0x7
}

// ---------- String views (slices that share a backing buffer) ----------
//
// A *view* is the third `str` representation, tagged by low bit 1 (bit 0 stays 0,
// so it's distinct from both inline `..01` and owned-heap `..00`). The value is
// `view_obj_ptr | 0b10`; the view object is a heap struct:
//   [0]  refcount: i64          (the view's own count; views are never STATIC)
//   [8]  data_ptr: *const u8    (into the owner's content bytes)
//   [16] len:      i64          (slice length — views are NOT NUL-terminated)
//   [24] owner:    *const u8    (the parent str value; inc'd on create, dec'd on
//                                free, so the shared buffer outlives the view)
// Views let a `str` slice share the source's buffer; small (<=7) or SSO slices
// still copy to inline, so they don't pin the parent alive.
const VIEW_TAG: usize = 0b10;
const VIEW_SIZE: usize = 32;
const VIEW_DATA_OFFSET: usize = 8;
const VIEW_LEN_OFFSET: usize = 16;
const VIEW_OWNER_OFFSET: usize = 24;

/// The view object behind a view value (clears the tag bits).
#[inline]
fn view_obj(v: *const u8) -> *mut u8 {
    ((v as usize) & !0b111) as *mut u8
}

fn alloc_view() -> *mut u8 {
    let layout = std::alloc::Layout::from_size_align(VIEW_SIZE, std::mem::align_of::<i64>())
        .expect("view layout");
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    raw
}

unsafe fn free_view(obj: *mut u8) {
    let layout = std::alloc::Layout::from_size_align(VIEW_SIZE, std::mem::align_of::<i64>())
        .expect("view layout");
    unsafe { std::alloc::dealloc(obj, layout) }
}

// ---------- Concatenated strings (lazy ropes) ----------
//
// A *concat* is the fourth `str` representation, tagged `0b11` in the low two
// bits (the slot freed by repacking inline as `(len<<2)|1`). Produced by every
// `str + str` (see `aipl_concat_lazy`), it defers the copy: the value is
// `node_ptr | 0b11`, where the node is a heap struct:
//   [0]  refcount: i64
//   [8]  left:  str   (a held ref — the left operand)
//   [16] right: str   (a held ref — the right operand)
//   [24] cache: ptr   (0 until first byte-access; then a flattened owned heap str
//                      the node owns and frees, so repeated reads materialize once)
//   [32] len:   i64   (total content length of the whole rope, summed once when
//                      the node is built — so `len` is O(1) at the root)
// To the source author a concat is just a `str`: every consumer funnels through
// `str_bytes` / `aipl_str_data` (which materialize on demand) or `aipl_str_len`
// (which reads the stored root length). `aipl_inc`/`aipl_dec` count the node and,
// at zero, release both children and the cache.
const CONCAT_TAG: usize = 0b11;
const CONCAT_SIZE: usize = 40;
const CONCAT_LEFT_OFFSET: usize = 8;
const CONCAT_RIGHT_OFFSET: usize = 16;
const CONCAT_CACHE_OFFSET: usize = 24;
const CONCAT_LEN_OFFSET: usize = 32;

/// The concat node behind a concat value (clears the tag bits).
#[inline]
fn concat_obj(v: *const u8) -> *mut u8 {
    ((v as usize) & !0b111) as *mut u8
}

// ---------- Representation dispatch ----------
//
// The canonical way to branch on a `str` value's active representation: classify
// it once with `str_repr`, then `match`. Prefer this over scattered `is_*`
// boolean checks — a `match` is exhaustive, so adding a `StrRepr` variant (a new
// representation) makes the compiler flag every site that doesn't yet handle it,
// instead of silently falling through to a heap/`else` arm. A representation
// whose handling genuinely coincides with another's may share an arm (e.g.
// `Null | Heap`), but spell the variants out rather than using a bare `_` so the
// next representation still forces a decision.

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

fn alloc_concat() -> *mut u8 {
    let layout = std::alloc::Layout::from_size_align(CONCAT_SIZE, std::mem::align_of::<i64>())
        .expect("concat layout");
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    raw
}

unsafe fn free_concat(obj: *mut u8) {
    let layout = std::alloc::Layout::from_size_align(CONCAT_SIZE, std::mem::align_of::<i64>())
        .expect("concat layout");
    unsafe { std::alloc::dealloc(obj, layout) }
}

/// Flatten a concat into a contiguous owned heap str, memoized on the node's
/// `cache` slot (so subsequent reads reuse it). Returns the cache value (a normal
/// inline/heap str). Recurses through nested concats via `str_bytes`.
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
        let lbytes = str_bytes(left, &mut lb);
        let rbytes = str_bytes(right, &mut rb);
        let mut combined = Vec::with_capacity(lbytes.len() + rbytes.len());
        combined.extend_from_slice(lbytes);
        combined.extend_from_slice(rbytes);
        let result = make_str(&combined);
        *cache_slot = result;
        result
    }
}

/// `str + str`, lazily: build a concat node holding the two operands. *Takes
/// ownership* of the refs the caller pre-inc'd (no copy, no further inc/dec); the
/// node's `aipl_dec` releases them. The defining producer of the concat repr.
extern "C" fn aipl_concat_lazy(a: *const u8, b: *const u8) -> *const u8 {
    // Sum the operands' lengths once (each is O(1) — stored on heap/rope, encoded
    // on inline/view), so the whole rope's length is O(1) to read at the root.
    let len = aipl_str_len(a) + aipl_str_len(b);
    let obj = alloc_concat();
    unsafe {
        std::ptr::write(obj as *mut i64, 1); // refcount
        std::ptr::write(obj.add(CONCAT_LEFT_OFFSET) as *mut *const u8, a);
        std::ptr::write(obj.add(CONCAT_RIGHT_OFFSET) as *mut *const u8, b);
        std::ptr::write(
            obj.add(CONCAT_CACHE_OFFSET) as *mut *const u8,
            std::ptr::null(),
        );
        std::ptr::write(obj.add(CONCAT_LEN_OFFSET) as *mut i64, len);
    }
    (obj as usize | CONCAT_TAG) as *const u8
}

/// Pack <= 7 content bytes into an inline str value (`bytes.len()` must be <= 7).
fn pack_inline(bytes: &[u8]) -> *const u8 {
    debug_assert!(bytes.len() <= 7);
    let mut val: u64 = ((bytes.len() as u64) << 2) | 1;
    for (i, &b) in bytes.iter().enumerate() {
        val |= (b as u64) << (8 * (i + 1));
    }
    val as usize as *const u8
}

/// Content bytes of any str value: inline content is copied into `buf` (which
/// must outlive the returned slice); a heap/static pointer yields its NUL-
/// delimited bytes; a null pointer yields empty.
unsafe fn str_bytes(v: *const u8, buf: &mut [u8; 8]) -> &[u8] {
    match str_repr(v) {
        StrRepr::Null => &[],
        StrRepr::Inline => {
            *buf = (v as usize as u64).to_le_bytes();
            &buf[1..1 + inline_len(v)]
        }
        StrRepr::View(obj) => unsafe {
            let data = *(obj.add(VIEW_DATA_OFFSET) as *const *const u8);
            let len = *(obj.add(VIEW_LEN_OFFSET) as *const i64) as usize;
            std::slice::from_raw_parts(data, len)
        },
        // Materialize (memoized) and read the flattened cache's bytes.
        StrRepr::Rope(_) => unsafe { str_bytes(concat_materialize(v), buf) },
        StrRepr::Heap => unsafe { std::slice::from_raw_parts(v, heap_len(v)) },
    }
}

/// Canonicalize freshly-built content into a str value: inline when it fits
/// (<= 7 bytes), else a fresh heap string. The single place producers funnel
/// their result through to maintain the "short == inline" invariant.
fn make_str(bytes: &[u8]) -> *const u8 {
    if bytes.len() <= 7 {
        pack_inline(bytes)
    } else {
        let raw = alloc_dynamic_string(bytes.len());
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), raw.add(STR_HEADER_SIZE), bytes.len());
            raw.add(STR_HEADER_SIZE)
        }
    }
}

fn alloc_dynamic_string(content_len: usize) -> *mut u8 {
    // Layout: [len: i64][refcount: i64][content: u8 * len][null: u8]
    let total = STR_HEADER_SIZE + content_len + 1;
    let layout = std::alloc::Layout::from_size_align(total, std::mem::align_of::<i64>())
        .expect("string layout");
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    unsafe {
        std::ptr::write(raw as *mut i64, content_len as i64); // stored length
        std::ptr::write((raw as *mut i64).add(1), 1); // refcount
        *raw.add(STR_HEADER_SIZE + content_len) = 0; // null terminator
    }
    raw
}

unsafe fn free_dynamic_string(block: *mut u8, content_len: usize) {
    let total = STR_HEADER_SIZE + content_len + 1;
    let layout = std::alloc::Layout::from_size_align(total, std::mem::align_of::<i64>())
        .expect("string layout");
    unsafe {
        std::alloc::dealloc(block, layout);
    }
}

extern "C" fn aipl_inc(ptr: *const u8) {
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

extern "C" fn aipl_dec(ptr: *const u8) {
    match str_repr(ptr) {
        // Null and inline values own no heap — nothing to free.
        StrRepr::Null | StrRepr::Inline => {}
        // Drop the view object; at zero, release its owner and free the object.
        StrRepr::View(obj) => unsafe {
            let rc = obj as *mut i64;
            *rc -= 1;
            if *rc == 0 {
                let owner = *(obj.add(VIEW_OWNER_OFFSET) as *const *const u8);
                aipl_dec(owner);
                free_view(obj);
            }
        },
        // Drop the concat node; at zero, release both children and the (possibly
        // materialized) cache, then free the node.
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
                free_concat(obj);
            }
        },
        StrRepr::Heap => unsafe {
            let h = header_of(ptr);
            if *h != STATIC_REFCOUNT {
                *h -= 1;
                if *h == 0 {
                    free_dynamic_string(ptr.sub(STR_HEADER_SIZE) as *mut u8, heap_len(ptr));
                }
            }
        },
    }
}

/// Visit each leaf's contiguous bytes in order **without materializing a rope**:
/// a rope recurses into its children (reusing its `cache` if already
/// materialized), every other representation yields its bytes via `str_bytes`
/// (no allocation). `f` returns `false` to stop early; the return value is
/// whether the whole string was visited (i.e. `f` never stopped it). This is the
/// shared primitive behind the streaming string operations — print, equality,
/// hashing, indexing, prefix/suffix tests, join — so each works on a rope without
/// flattening it. Mirrors the linker runtime's copy.
fn str_for_each_chunk(ptr: *const u8, f: &mut impl FnMut(&[u8]) -> bool) -> bool {
    match str_repr(ptr) {
        StrRepr::Null => true,
        StrRepr::Rope(obj) => {
            let cache = unsafe { *(obj.add(CONCAT_CACHE_OFFSET) as *const *const u8) };
            if cache.is_null() {
                let left = unsafe { *(obj.add(CONCAT_LEFT_OFFSET) as *const *const u8) };
                let right = unsafe { *(obj.add(CONCAT_RIGHT_OFFSET) as *const *const u8) };
                str_for_each_chunk(left, f) && str_for_each_chunk(right, f)
            } else {
                str_for_each_chunk(cache, f)
            }
        }
        // A contiguous leaf (inline/view/heap) yields its bytes directly.
        StrRepr::Inline | StrRepr::View(_) | StrRepr::Heap => {
            let mut buf = [0u8; 8];
            let bytes = unsafe { str_bytes(ptr, &mut buf) };
            f(bytes)
        }
    }
}

/// `print(s: str)`. Prints `s` with a trailing newline and drops its
/// refcount to honor the caller's pre-call inc. Returns nothing.
extern "C" fn aipl_print(ptr: *const u8) {
    use std::io::Write;
    // A null str prints nothing (defensive — well-typed code never passes one);
    // an inline empty string `""` is non-null and prints a blank line. A rope is
    // streamed leaf-by-leaf (no materialization).
    if !ptr.is_null() {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        str_for_each_chunk(ptr, &mut |chunk| {
            let _ = out.write_all(chunk);
            true
        });
        let _ = out.write_all(b"\n");
    }
    aipl_dec(ptr);
}

/// `fn main() -> !Error` failure path (JIT): write `error: <msg>` to stderr.
/// Borrows `msg` (no refcount change) — the caller's scope drop frees it.
extern "C" fn aipl_print_error(msg: *const u8) {
    use std::io::Write;
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = out.write_all(b"error: ");
    str_for_each_chunk(msg, &mut |chunk| {
        let _ = out.write_all(chunk);
        true
    });
    let _ = out.write_all(b"\n");
}

/// The `s[i]` runtime (`aipl_char_at`): returns byte i of s as 0..255, or
/// -1 to signal None (i<0, past null terminator, or null pointer). Codegen
/// (`emit_char_at`) wraps the result into an Optional slot. Decrements `s` per
/// the refcount protocol — callers pre-inc before the call as with any
/// str-taking fn.
extern "C" fn aipl_char_at(s: *const u8, i: i64) -> i64 {
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

/// `s.is_all_whitespace() -> bool` (returned as i64 0/1): true when every byte is
/// ASCII whitespace, or `s` is empty (consistent with `s.trim() == ""`). Consumes
/// `s` (decs), like the other str builtins — callers pre-inc.
extern "C" fn aipl_str_is_all_whitespace(s: *const u8) -> i64 {
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

/// `read_file_to_string(name) -> str?` runtime: read `name`'s bytes into a fresh
/// refcounted str, or return null (None) on any failure — open/read error,
/// non-UTF-8 path, or a NUL byte in the contents (which a NUL-terminated str
/// can't represent). Codegen wraps null into None. Decrements `name` per the
/// refcount protocol (callers pre-inc, as with any str-taking fn).
extern "C" fn aipl_read_file_to_string(name: *const u8) -> *const u8 {
    let result = read_file_impl(name);
    aipl_dec(name);
    result
}

fn read_file_impl(name: *const u8) -> *const u8 {
    if name.is_null() {
        return std::ptr::null();
    }
    let mut nbuf = [0u8; 8];
    let Ok(path) = std::str::from_utf8(unsafe { str_bytes(name, &mut nbuf) }) else {
        return std::ptr::null();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return std::ptr::null();
    };
    if bytes.contains(&0) {
        return std::ptr::null(); // a NUL-terminated str can't hold embedded NULs
    }
    make_str(&bytes) // inline if <= 7, else heap
}

/// `write_string_to_file(path, contents) -> bool` runtime: write `contents`'
/// bytes to `path`, returning 1 on success or 0 on any failure (bad UTF-8 path,
/// open/write error). Decrements both `path` and `contents` per the refcount
/// protocol (callers pre-inc, as with any str-taking fn).
extern "C" fn aipl_write_string_to_file(path: *const u8, contents: *const u8) -> i64 {
    let result = write_file_impl(path, contents);
    aipl_dec(path);
    aipl_dec(contents);
    result
}

fn write_file_impl(path: *const u8, contents: *const u8) -> i64 {
    if path.is_null() || contents.is_null() {
        return 0;
    }
    let mut pbuf = [0u8; 8];
    let Ok(path) = std::str::from_utf8(unsafe { str_bytes(path, &mut pbuf) }) else {
        return 0;
    };
    let mut cbuf = [0u8; 8];
    let bytes = unsafe { str_bytes(contents, &mut cbuf) };
    match std::fs::write(path, bytes) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

/// `execute_program(program, args) -> ExecResult!Error` runtime: this is the
/// hidden-sret ABI the generic `compile_call` path already builds for any
/// composite-returning builtin (see its `sret` handling) — `out` is the
/// caller-provided buffer sized for `Result<ExecResult, Error>` (`{tag: i64,
/// payload}`, tag 1 = ok, 0 = err, matching the `ok`/`err` expression
/// codegen), so this fn writes the whole result directly rather than
/// returning through a register. Spawns `program` with `args` (no shell) and
/// waits for it: `ok(ExecResult)` for any run that was actually launched,
/// whatever it then exited with; `err(message)` only if it couldn't be
/// launched at all (bad UTF-8, not found, embedded NUL in captured output).
/// The `ExecResult` fields are written at their declared struct offsets
/// 0/8/16 (`stdout`/`stderr`/`exit_code` — must stay in sync with
/// `__builtin_ExecResult` in `BUILTIN_SIGNATURES`). Decrements `program` and
/// `args` per the refcount protocol.
extern "C" fn aipl_execute_program(out: *mut i64, program: *const u8, args: *const u8) {
    let arr = aipl_arr_ensure_heap(args);
    execute_program_impl(out, program, arr);
    aipl_dec(program);
    aipl_array_dec(arr);
}

fn write_err(out: *mut i64, message: &[u8]) {
    let ptr = make_str(message);
    unsafe {
        *out = 0;
        *out.add(1) = ptr as i64;
    }
}

fn write_ok(out: *mut i64, stdout: &[u8], stderr: &[u8], exit_code: i64) {
    let stdout_ptr = make_str(stdout);
    let stderr_ptr = make_str(stderr);
    unsafe {
        *out = 1;
        *out.add(1) = stdout_ptr as i64;
        *out.add(2) = stderr_ptr as i64;
        *out.add(3) = exit_code;
    }
}

fn execute_program_impl(out: *mut i64, program: *const u8, args: *const u8) {
    if program.is_null() || args.is_null() {
        return write_err(out, b"could not execute program");
    }
    let mut pbuf = [0u8; 8];
    let Ok(program) = std::str::from_utf8(unsafe { str_bytes(program, &mut pbuf) }) else {
        return write_err(out, b"could not execute program");
    };
    let len = unsafe { array_len_of(args) };
    let elems = unsafe { args.add(ARR_ELEMS_OFFSET) as *const i64 };
    let mut arg_strings: Vec<String> = Vec::with_capacity(len);
    for i in 0..len {
        let ep = unsafe { std::ptr::read(elems.add(i)) as *const u8 };
        let mut buf = [0u8; 8];
        let Ok(s) = std::str::from_utf8(unsafe { str_bytes(ep, &mut buf) }) else {
            return write_err(out, b"could not execute program");
        };
        arg_strings.push(s.to_string());
    }
    let output = match std::process::Command::new(program)
        .args(&arg_strings)
        .output()
    {
        Ok(o) => o,
        Err(_) => return write_err(out, b"could not execute program"),
    };
    // A NUL-terminated str can't hold an embedded NUL byte (same constraint as
    // `read_file_to_string`).
    if output.stdout.contains(&0) || output.stderr.contains(&0) {
        return write_err(out, b"could not execute program");
    }
    let exit_code = i64::from(output.status.code().unwrap_or(-1));
    write_ok(out, &output.stdout, &output.stderr, exit_code);
}

// ---------- One-allocation `to_str` primitives ----------
//
// `to_str` renders in two passes: a measure pass sums the total byte length, the
// result buffer is allocated once (`aipl_str_alloc`), then a write pass fills it
// via a moving cursor. These are the cursor primitives; everything structural
// (brackets, separators, labels) is emitted in IR.

/// Decimal byte length of `n` (with a leading `-` for negatives). Must agree,
/// byte-for-byte, with `aipl_write_i64`.
extern "C" fn aipl_i64_len(n: i64) -> i64 {
    let mut buf = [0u8; 24];
    fmt_i64(&mut buf, n) as i64
}

/// Write `n`'s decimal representation at `dst`; return the advanced cursor.
extern "C" fn aipl_write_i64(dst: *const u8, n: i64) -> *const u8 {
    let mut buf = [0u8; 24];
    let len = fmt_i64(&mut buf, n);
    unsafe {
        std::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, len);
        dst.add(len)
    }
}

/// Format `n` in decimal into `buf[0..]`, returning the byte count.
fn fmt_i64(buf: &mut [u8; 24], n: i64) -> usize {
    let mut digits = [0u8; 20];
    let mut m = n.unsigned_abs();
    let mut d = 0;
    if m == 0 {
        digits[0] = b'0';
        d = 1;
    } else {
        while m > 0 {
            digits[d] = b'0' + (m % 10) as u8;
            m /= 10;
            d += 1;
        }
    }
    let mut len = 0;
    if n < 0 {
        buf[0] = b'-';
        len = 1;
    }
    for k in 0..d {
        buf[len + k] = digits[d - 1 - k];
    }
    len + d
}

/// Decimal byte length of `n` interpreted as *unsigned*. Agrees, byte-for-byte,
/// with `aipl_write_u64` (used to render `u8`/`u16`/`u32`/`u64`).
extern "C" fn aipl_u64_len(n: i64) -> i64 {
    let mut buf = [0u8; 24];
    fmt_u64(&mut buf, n as u64) as i64
}

/// Write `n` (interpreted as unsigned) in decimal at `dst`; return the cursor.
extern "C" fn aipl_write_u64(dst: *const u8, n: i64) -> *const u8 {
    let mut buf = [0u8; 24];
    let len = fmt_u64(&mut buf, n as u64);
    unsafe {
        std::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, len);
        dst.add(len)
    }
}

/// Format `n` (unsigned) in decimal into `buf`, returning the byte count.
fn fmt_u64(buf: &mut [u8; 24], n: u64) -> usize {
    let mut digits = [0u8; 20];
    let mut m = n;
    let mut d = 0;
    if m == 0 {
        digits[0] = b'0';
        d = 1;
    } else {
        while m > 0 {
            digits[d] = b'0' + (m % 10) as u8;
            m /= 10;
            d += 1;
        }
    }
    for k in 0..d {
        buf[k] = digits[d - 1 - k];
    }
    d
}

/// Byte content length of a `str`; 0 for null. O(1) for every representation —
/// each stores or encodes its length, so this never walks the bytes (a rope reads
/// the total cached at its root; a heap string reads its header word).
extern "C" fn aipl_str_len(s: *const u8) -> i64 {
    match str_repr(s) {
        StrRepr::Null => 0,
        StrRepr::Inline => inline_len(s) as i64,
        StrRepr::Heap => unsafe { heap_len(s) as i64 },
        StrRepr::View(obj) => unsafe { *(obj.add(VIEW_LEN_OFFSET) as *const i64) },
        StrRepr::Rope(obj) => unsafe { *(obj.add(CONCAT_LEN_OFFSET) as *const i64) },
    }
}

/// Copy `n` bytes `src` → `dst`; return the advanced cursor.
extern "C" fn aipl_write_bytes(dst: *const u8, src: *const u8, n: i64) -> *const u8 {
    let n = n.max(0) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(src, dst as *mut u8, n);
        dst.add(n)
    }
}

/// Allocate one writable `str` buffer of `len` content bytes (refcount 1,
/// NUL-terminated); return the data pointer. The single allocation behind a
/// whole `to_str`.
extern "C" fn aipl_str_alloc(len: i64) -> *const u8 {
    let raw = alloc_dynamic_string(len.max(0) as usize);
    unsafe { raw.add(STR_HEADER_SIZE) }
}

/// `str + str`. Allocates a fresh refcounted buffer holding the two operands
/// concatenated. Decrements both inputs at the end.
extern "C" fn aipl_concat(a: *const u8, b: *const u8) -> *const u8 {
    let mut ba = [0u8; 8];
    let mut bb = [0u8; 8];
    let sa = unsafe { str_bytes(a, &mut ba) };
    let sb = unsafe { str_bytes(b, &mut bb) };
    let total = sa.len() + sb.len();
    let result = if total <= 7 {
        // SSO: a short result is inline — no allocation.
        let mut tmp = [0u8; 7];
        tmp[..sa.len()].copy_from_slice(sa);
        tmp[sa.len()..total].copy_from_slice(sb);
        pack_inline(&tmp[..total])
    } else {
        let raw = alloc_dynamic_string(total);
        unsafe {
            std::ptr::copy_nonoverlapping(sa.as_ptr(), raw.add(STR_HEADER_SIZE), sa.len());
            std::ptr::copy_nonoverlapping(
                sb.as_ptr(),
                raw.add(STR_HEADER_SIZE + sa.len()),
                sb.len(),
            );
            raw.add(STR_HEADER_SIZE)
        }
    };
    aipl_dec(a);
    aipl_dec(b);
    result
}

/// `trim(s) -> str`. Returns a fresh string with leading/trailing ASCII
/// whitespace removed, then drops `s`. Mirrors the linker runtime.
extern "C" fn aipl_trim(s: *const u8) -> *const u8 {
    let mut sb = [0u8; 8];
    let bytes = unsafe { str_bytes(s, &mut sb) };
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = bytes.iter().position(|&b| !is_ws(b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&b| !is_ws(b))
        .map_or(start, |i| i + 1);
    let n = end - start;
    // Nothing trimmed: return s as-is, transferring our reference to the caller.
    if start == 0 && end == bytes.len() {
        return s;
    }
    // Small result (≤ 7 bytes, incl. all-whitespace → n == 0): pack inline, release s.
    // Inline sources are always caught here (inline is ≤ 7 bytes, so the trimmed
    // result is too), so the view path below never sees an inline source.
    if n <= 7 {
        let result = make_str(&bytes[start..end]);
        aipl_dec(s);
        return result;
    }
    // Large result: return a view into s's buffer. Transfer our reference of s to
    // the view's owner field — no inc, no dec, no byte copy.
    let data = unsafe { bytes.as_ptr().add(start) };
    let obj = alloc_view();
    unsafe {
        std::ptr::write(obj as *mut i64, 1); // refcount
        std::ptr::write(obj.add(VIEW_DATA_OFFSET) as *mut *const u8, data);
        std::ptr::write(obj.add(VIEW_LEN_OFFSET) as *mut i64, n as i64);
        std::ptr::write(obj.add(VIEW_OWNER_OFFSET) as *mut *const u8, s);
    }
    (obj as usize | VIEW_TAG) as *const u8
}

/// `s.reverse() -> str` — returns a new string with the bytes in reverse order.
/// Consumes `s` per the refcount protocol (callers pre-inc).
extern "C" fn aipl_str_reverse(s: *const u8) -> *const u8 {
    let mut sb = [0u8; 8];
    let bytes = unsafe { str_bytes(s, &mut sb) };
    let mut reversed: Vec<u8> = bytes.to_vec();
    reversed.reverse();
    let result = make_str(&reversed);
    aipl_dec(s);
    result
}

/// `s.repeat(n) -> str` — concatenate `s` with itself `n` times.
/// Returns `""` for `n <= 0`. Consumes `s` (callers pre-inc). Mirrors the linker runtime.
extern "C" fn aipl_str_repeat(s: *const u8, n: i64) -> *const u8 {
    let mut sb = [0u8; 8];
    let bytes = unsafe { str_bytes(s, &mut sb) };
    let result = if n <= 0 || bytes.is_empty() {
        make_str(&[])
    } else {
        let total = bytes.len() * n as usize;
        let mut buf: Vec<u8> = Vec::with_capacity(total);
        for _ in 0..n {
            buf.extend_from_slice(bytes);
        }
        make_str(&buf)
    };
    aipl_dec(s);
    result
}

/// `xs.reverse() -> T[]` — O(1): returns a reversed-view repr wrapping `xs`.
/// Transfers ownership of `xs` into the view (no drop, no retain).
/// `drop_fn`, `retain_fn`, `elem_size` describe the element type.
extern "C" fn aipl_arr_reverse(
    a: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    if a.is_null() {
        return a;
    }
    let len = unsafe { array_len_of(a) };
    alloc_reversed_view(a, len, drop_fn, retain_fn, elem_size)
}

/// `s[start..end]` — string slice. Both bounds are clamped to `[0, len]` (an
/// out-of-range end yields a shorter string; `start >= end` yields `""`).
/// *Borrows* `s` (does not drop it) and returns a fresh `str`. Stage 1 always
/// copies; Stage 2 returns a buffer-sharing view for large heap sources.
extern "C" fn aipl_str_slice(s: *const u8, start: i64, end: i64) -> *const u8 {
    let mut sb = [0u8; 8];
    let bytes = unsafe { str_bytes(s, &mut sb) };
    let len = bytes.len() as i64;
    let lo = start.clamp(0, len) as usize;
    let hi = end.clamp(0, len) as usize;
    let n = hi.saturating_sub(lo);
    // A small result, or any slice of an SSO source, copies — so it doesn't pin
    // a parent buffer alive (and an inline source has no shared buffer anyway).
    if n <= 7 || matches!(str_repr(s), StrRepr::Inline) {
        return make_str(if lo < hi { &bytes[lo..hi] } else { &[] });
    }
    // Large slice of a heap (owned or view) source: share its buffer via a view
    // that retains the source. `bytes.as_ptr()` is the source's content start
    // (the owned pointer, or a parent view's data), so `+ lo` is the slice start.
    let data = unsafe { bytes.as_ptr().add(lo) };
    aipl_inc(s);
    let obj = alloc_view();
    unsafe {
        std::ptr::write(obj as *mut i64, 1); // refcount
        std::ptr::write(obj.add(VIEW_DATA_OFFSET) as *mut *const u8, data);
        std::ptr::write(obj.add(VIEW_LEN_OFFSET) as *mut i64, n as i64);
        std::ptr::write(obj.add(VIEW_OWNER_OFFSET) as *mut *const u8, s);
    }
    (obj as usize | VIEW_TAG) as *const u8
}

/// `xs[start..end]` — array slice. Both bounds are clamped to `[0, len]` (an
/// out-of-range end yields a shorter array; `start >= end` yields `[]`).
/// *Borrows* `xs` (does not drop it) and returns a fresh heap array holding
/// copies of the elements in `[start, end)`, each retained via `retain_fn`
/// (0 for scalar elements).
extern "C" fn aipl_arr_slice(
    a: *const u8,
    start: i64,
    end: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    if a.is_null() {
        return a;
    }
    let len = unsafe { array_len_of(a) } as i64;
    let lo = start.clamp(0, len) as usize;
    let hi = end.clamp(0, len) as usize;
    let n = hi.saturating_sub(lo);
    if elem_size == ELEM_BITPACKED {
        let raw = alloc_array(n, n, drop_fn, ELEM_BITPACKED);
        unsafe {
            let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            for i in 0..n {
                let bit = arr_load_bit(a, lo + i);
                write_packed_bit(dst, i, bit);
            }
        }
        return raw;
    }
    let es = elem_size.max(8) as usize;
    let raw = alloc_array(n, n, drop_fn, elem_size);
    unsafe {
        let dst_base = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
        for i in 0..n {
            let src = arr_elem_ptr(a, lo + i, es);
            std::ptr::copy_nonoverlapping(src, dst_base.add(i * es), es);
        }
        elem_rc(retain_fn, dst_base, n);
    }
    raw
}

/// `split(self, sep) -> str[]` — the parts of `self` between non-overlapping
/// occurrences of `sep`, each produced by `aipl_str_slice` (a buffer-sharing view
/// for a large part, else an inline/heap copy). An empty `sep` yields one part:
/// the whole string. *Consumes* both `self` and `sep` (drops the handed refs); the
/// view parts hold their own refs on `self`'s buffer, so it outlives them.
extern "C" fn aipl_str_split(s: *const u8, sep: *const u8) -> *const u8 {
    let mut sb = [0u8; 8];
    let mut pb = [0u8; 8];
    let hay = unsafe { str_bytes(s, &mut sb) };
    let needle = unsafe { str_bytes(sep, &mut pb) };
    let nlen = needle.len();
    // Part count = occurrences + 1 (an empty separator never matches → 1 part).
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
    let elems = unsafe { arr.add(ARR_ELEMS_OFFSET) as *mut i64 };
    // Re-scan, writing each segment as a slice of `s` (view or copy).
    if nlen == 0 {
        unsafe { *elems = aipl_str_slice(s, 0, hay.len() as i64) as i64 };
    } else {
        let (mut start, mut i, mut k) = (0usize, 0usize, 0usize);
        while i + nlen <= hay.len() {
            if &hay[i..i + nlen] == needle {
                unsafe { *elems.add(k) = aipl_str_slice(s, start as i64, i as i64) as i64 };
                k += 1;
                i += nlen;
                start = i;
            } else {
                i += 1;
            }
        }
        unsafe { *elems.add(k) = aipl_str_slice(s, start as i64, hay.len() as i64) as i64 };
    }
    aipl_dec(s);
    aipl_dec(sep);
    arr
}

/// `join(parts: str[], sep: str) -> str` — concatenate the parts with `sep`
/// between consecutive elements (`[]` -> `""`, `[x]` -> `x`). Two passes: measure
/// the total length, then fill a single fresh buffer (inline when <= 7 bytes).
/// Consumes both args (the array drop releases its element strings), like the
/// other str builtins.
extern "C" fn aipl_str_join(arr: *const u8, sep: *const u8) -> *const u8 {
    let result = unsafe {
        let len = *(arr.add(ARR_LEN_OFFSET) as *const i64) as usize;
        let elems = arr.add(ARR_ELEMS_OFFSET) as *const i64;
        let mut sb = [0u8; 8];
        // `sep` is read once and reused, so materialize a rope separator just once.
        let sep_bytes = str_bytes(sep, &mut sb);
        // Measure: every part's length (O(1)) plus a separator between each pair.
        let mut total = sep_bytes.len() * len.saturating_sub(1);
        for i in 0..len {
            let ep = std::ptr::read(elems.add(i)) as *const u8;
            total += aipl_str_len(ep) as usize;
        }
        // Fill, writing a separator before every element but the first.
        let mut scratch = [0u8; 7];
        let dst = if total <= 7 {
            scratch.as_mut_ptr()
        } else {
            alloc_dynamic_string(total).add(STR_HEADER_SIZE)
        };
        let mut pos = 0usize;
        for i in 0..len {
            if i > 0 {
                std::ptr::copy_nonoverlapping(sep_bytes.as_ptr(), dst.add(pos), sep_bytes.len());
                pos += sep_bytes.len();
            }
            let ep = std::ptr::read(elems.add(i)) as *const u8;
            // Stream the element into the buffer; a rope copies its leaves with
            // nothing materialized.
            str_for_each_chunk(ep, &mut |chunk| {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), dst.add(pos), chunk.len());
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

/// Pointer to `s`'s contiguous content bytes — its length comes from
/// `aipl_str_len`. Used by codegen sites that walk a str by index (the `for`
/// loop and `to_str` rendering), which can't assume NUL-termination once views
/// exist. Inline content is copied into the caller's 8-byte `scratch`; owned and
/// view content is returned in place (valid while `scratch` and `s` live).
extern "C" fn aipl_str_data(s: *const u8, scratch: *mut u8) -> *const u8 {
    match str_repr(s) {
        StrRepr::Inline => {
            let val = (s as usize as u64).to_le_bytes();
            unsafe {
                for i in 0..inline_len(s) {
                    *scratch.add(i) = val[1 + i];
                }
            }
            scratch
        }
        StrRepr::View(obj) => unsafe { *(obj.add(VIEW_DATA_OFFSET) as *const *const u8) },
        // Materialize (memoized) and return the flattened cache's contiguous data.
        StrRepr::Rope(_) => unsafe { aipl_str_data(concat_materialize(s), scratch) },
        // A null or heap value's pointer is already its contiguous content.
        StrRepr::Null | StrRepr::Heap => s,
    }
}

// ---------- Char-iteration cursor (`for c in s`) ----------
//
// A small fixed-size cursor that codegen stack-allocates, so iterating a rope
// streams its bytes in order **without materializing** and without a heap
// traversal stack. `next` returns the byte at the current position then advances;
// to locate that byte it descends from the root to the containing leaf (each
// descent is O(rope depth) using the O(1) stored child lengths) and caches the
// leaf, so sequential reads within a leaf are O(1). A non-rope string is one leaf
// (O(1) per byte). Layout (codegen allocates `ITER_SIZE` bytes, 8-aligned):
//   [0] root: str | [8] pos | [16] total | [24] leaf_ptr | [32] leaf_start
//   [40] leaf_len | [48] scratch (8 bytes, for an inline leaf's spill)
const ITER_ROOT: usize = 0;
const ITER_POS: usize = 8;
const ITER_TOTAL: usize = 16;
const ITER_LEAF_PTR: usize = 24;
const ITER_LEAF_START: usize = 32;
const ITER_LEAF_LEN: usize = 40;
const ITER_SCRATCH: usize = 48;
const ITER_SIZE: usize = 56;

extern "C" fn aipl_str_iter_init(cur: *mut u8, s: *const u8) {
    unsafe {
        *(cur.add(ITER_ROOT) as *mut *const u8) = s;
        *(cur.add(ITER_POS) as *mut i64) = 0;
        *(cur.add(ITER_TOTAL) as *mut i64) = aipl_str_len(s);
        *(cur.add(ITER_LEAF_PTR) as *mut *const u8) = std::ptr::null();
        *(cur.add(ITER_LEAF_START) as *mut i64) = 0;
        *(cur.add(ITER_LEAF_LEN) as *mut i64) = 0;
    }
}

/// Next byte of the iterated string as `0..=255`, or `-1` at the end.
extern "C" fn aipl_str_iter_next(cur: *mut u8) -> i64 {
    unsafe {
        let pos = *(cur.add(ITER_POS) as *const i64);
        if pos >= *(cur.add(ITER_TOTAL) as *const i64) {
            return -1;
        }
        let leaf_start = *(cur.add(ITER_LEAF_START) as *const i64);
        let leaf_len = *(cur.add(ITER_LEAF_LEN) as *const i64);
        if pos < leaf_start || pos >= leaf_start + leaf_len {
            // Descend from the root to the leaf containing `pos`. Only non-rope
            // nodes (or an already-materialized rope's cache) become the leaf, so
            // nothing is flattened here.
            let mut node = *(cur.add(ITER_ROOT) as *const *const u8);
            let mut base: i64 = 0;
            while let StrRepr::Rope(obj) = str_repr(node) {
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
                    node = cache; // contiguous already — treat as the leaf
                    break;
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

/// In-place trim for a uniquely owned string (mirrors the linker runtime).
/// Shifts the trimmed content to the front and `realloc`s down to fit, reusing
/// the block; a static literal can't be mutated so it copies. `s` is reused.
extern "C" fn aipl_trim_mut(s: *const u8) -> *const u8 {
    // Only a uniquely-owned (non-static) heap string can be trimmed in place; any
    // other representation copies via `aipl_trim`. Matching (rather than an `is_*`
    // chain) means a new representation must explicitly opt into the in-place path
    // instead of silently taking it. `header_of` is valid only for the heap case.
    let StrRepr::Heap = str_repr(s) else {
        return aipl_trim(s);
    };
    if unsafe { *header_of(s) } == STATIC_REFCOUNT {
        return aipl_trim(s);
    }
    unsafe {
        let n = heap_len(s);
        let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
        let data = s as *mut u8;
        let mut start = 0;
        while start < n && is_ws(*data.add(start)) {
            start += 1;
        }
        let mut end = n;
        while end > start && is_ws(*data.add(end - 1)) {
            end -= 1;
        }
        let len = end - start;
        if len <= 7 {
            // SSO: the trimmed result is inline — copy it out and free the block
            // (it can't be reused as an inline value lives in the register).
            let mut tmp = [0u8; 7];
            std::ptr::copy_nonoverlapping(data.add(start), tmp.as_mut_ptr(), len);
            free_dynamic_string(s.sub(STR_HEADER_SIZE) as *mut u8, n);
            return pack_inline(&tmp[..len]);
        }
        if start > 0 {
            std::ptr::copy(data.add(start), data, len); // overlapping move
        }
        *data.add(len) = 0;
        let old_layout = std::alloc::Layout::from_size_align(
            STR_HEADER_SIZE + n + 1,
            std::mem::align_of::<i64>(),
        )
        .expect("string layout");
        let block = s.sub(STR_HEADER_SIZE) as *mut u8;
        let raw = std::alloc::realloc(block, old_layout, STR_HEADER_SIZE + len + 1);
        if raw.is_null() {
            std::alloc::handle_alloc_error(old_layout);
        }
        std::ptr::write(raw as *mut i64, len as i64); // updated stored length (block word 0)
        raw.add(STR_HEADER_SIZE)
    }
}

/// In-place concat for a uniquely owned string (mirrors the linker runtime).
/// Grows `a`'s buffer with `realloc` and appends `b`; a static literal can't be
/// grown so it copies instead. `a` is reused (not dropped); `b` is dropped.
extern "C" fn aipl_concat_mut(a: *const u8, b: *const u8) -> *const u8 {
    // Only a uniquely-owned (non-static) heap `a` can be grown in place; any other
    // representation copies via `aipl_concat`. Matching (rather than an `is_*`
    // chain) means a new representation must explicitly opt into the in-place path
    // instead of silently taking it. `header_of` is valid only for the heap case.
    let StrRepr::Heap = str_repr(a) else {
        return aipl_concat(a, b);
    };
    if unsafe { *header_of(a) } == STATIC_REFCOUNT {
        return aipl_concat(a, b);
    }
    unsafe {
        // `a` is a heap str (the guard bailed for inline/static/null); read its
        // stored length. `b` may be inline, so materialize it.
        let la = heap_len(a);
        let mut bbuf = [0u8; 8];
        let sb = str_bytes(b, &mut bbuf);
        let lb = sb.len();
        let old_layout = std::alloc::Layout::from_size_align(
            STR_HEADER_SIZE + la + 1,
            std::mem::align_of::<i64>(),
        )
        .expect("string layout");
        let block = a.sub(STR_HEADER_SIZE) as *mut u8;
        // `realloc` is the in-place analogue of malloc here; it isn't routed
        // through an instrumented counter, so it doesn't show in alloc tallies.
        let raw = std::alloc::realloc(block, old_layout, STR_HEADER_SIZE + la + lb + 1);
        if raw.is_null() {
            std::alloc::handle_alloc_error(old_layout);
        }
        std::ptr::write(raw as *mut i64, (la + lb) as i64); // updated stored length (block word 0)
        let data = raw.add(STR_HEADER_SIZE);
        std::ptr::copy_nonoverlapping(sb.as_ptr(), data.add(la), lb);
        *data.add(la + lb) = 0;
        aipl_dec(b);
        data
    }
}

// ---------- Refcounted array runtime ----------
//
// An array is a refcounted heap block laid out as:
//   [refcount: i64][len: i64][drop_fn: ptr][elem0: i64][elem1: i64]...
// The pointer the language holds points at the `len` field (so `ptr - 8`
// is the refcount, sharing the inc/dec protocol with strings). Elements are
// 8 bytes each (a scalar, or a heap pointer for `str`/array elements);
// element i lives at `ptr + ARR_ELEMS_OFFSET + i*8`. Arrays are never
// static, so refcounts always start at 1.
//
// `drop_fn` is null for arrays of plain scalars (i64/bool/char). For arrays
// whose elements are themselves heap-managed (`str`, or a nested array) it
// points at a runtime helper (`aipl_arr_drop_str` / `aipl_arr_drop_arr`)
// that releases each element before the block is freed. A non-null `drop_fn`
// also marks the elements as heap pointers, so `push` knows to retain the
// copies it makes.

type ArrDropFn = extern "C" fn(*const u8, i64);

const ARR_LEN_OFFSET: usize = 0; // length, in elements
const ARR_CAP_OFFSET: usize = 8; // capacity of the element region, in *bytes*
const ARR_DROPFN_OFFSET: usize = 16; // element drop-fn pointer (null = scalars)
const ARR_ELEMS_OFFSET: usize = 24; // first element, relative to data pointer

// Element size is *not* stored in the header — it's known at compile time, so
// codegen passes it to the array runtime fns as a constant. The header keeps the
// element-region capacity in *bytes* (not slots) so `aipl_array_dec` can size
// the block for free() without needing the element size — important because it
// also serves as the generic drop-fn for nested arrays, where the inner array's
// element size isn't known to the caller.

fn array_block_size(cap_bytes: usize) -> usize {
    HEADER_SIZE + ARR_ELEMS_OFFSET + cap_bytes
}

// ---------- Array representation tags ----------
//
// Arrays are 8-byte aligned, so the two low bits of every array pointer are
// always 0 for a heap array.  We steal those bits (exactly as the string system
// does) to encode the runtime representation:
//
//   0b00  Heap  — the existing heap-allocated array block
//   0b01  Rev   — a thin reversed-view wrapper around an inner array
//
// Every place that uses an array pointer as a memory base must strip the tag
// first (`arr_untag`).  The classify-once / match-everywhere pattern mirrors
// `str_repr` / `StrRepr` in the string system.
const ARR_TAG_MASK: usize = 0b11;
const ARR_HEAP_TAG: usize = 0b00;
const ARR_REV_TAG: usize = 0b01;

// Reversed-view block layout (data ptr is the block base + HEADER_SIZE, tagged
// with ARR_REV_TAG).  Stores everything needed to iterate and to materialize:
//   [ARR_LEN_OFFSET  = 0] len       — element count (same field as heap array)
//   [REV_INNER_OFFSET= 8] inner_ptr — tagged pointer to the wrapped inner array
//   [REV_DROP_OFFSET =16] drop_fn   — element drop fn (for materialization)
//   [REV_RETAIN_OFFSET=24] retain_fn — element retain fn
//   [REV_ELEMSIZE_OFFSET=32] elem_size — runtime elem size
// Block size: HEADER_SIZE + 40 = 48 bytes.
const REV_INNER_OFFSET: usize = 8;
const REV_DROP_OFFSET: usize = 16;
const REV_RETAIN_OFFSET: usize = 24;
const REV_ELEMSIZE_OFFSET: usize = 32;
const REV_BLOCK_DATA_SIZE: usize = 40; // bytes after the refcount header

#[derive(Clone, Copy)]
enum ArrRepr {
    Heap,
    Reversed,
}

fn arr_repr(ptr: *const u8) -> ArrRepr {
    match ptr as usize & ARR_TAG_MASK {
        ARR_HEAP_TAG => ArrRepr::Heap,
        ARR_REV_TAG => ArrRepr::Reversed,
        tag => unreachable!("unknown array repr tag {tag}"),
    }
}

/// Strip the representation tag from an array pointer, returning the actual
/// block base address.
fn arr_untag(ptr: *const u8) -> *const u8 {
    (ptr as usize & !ARR_TAG_MASK) as *const u8
}

/// Allocate a reversed-view block wrapping `inner` (tagged).  Steals the
/// caller's reference to `inner` (does not retain it separately).
fn alloc_reversed_view(
    inner: *const u8,
    len: usize,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let layout = std::alloc::Layout::from_size_align(
        HEADER_SIZE + REV_BLOCK_DATA_SIZE,
        std::mem::align_of::<i64>(),
    )
    .expect("rev-view layout");
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    unsafe {
        std::ptr::write(raw as *mut i64, 1); // refcount = 1
        let data = raw.add(HEADER_SIZE);
        std::ptr::write(data as *mut i64, len as i64);
        std::ptr::write(data.add(REV_INNER_OFFSET) as *mut *const u8, inner);
        std::ptr::write(data.add(REV_DROP_OFFSET) as *mut i64, drop_fn);
        std::ptr::write(data.add(REV_RETAIN_OFFSET) as *mut i64, retain_fn);
        std::ptr::write(data.add(REV_ELEMSIZE_OFFSET) as *mut i64, elem_size);
        (data as usize | ARR_REV_TAG) as *const u8
    }
}

/// Materialize a reversed view (or return the input unchanged for a heap array).
/// Consumes the input pointer's reference.
fn aipl_arr_ensure_heap(a: *const u8) -> *const u8 {
    match arr_repr(a) {
        ArrRepr::Heap => a,
        ArrRepr::Reversed => {
            let u = arr_untag(a);
            let inner = unsafe { std::ptr::read(u.add(REV_INNER_OFFSET) as *const *const u8) };
            let drop_fn = unsafe { std::ptr::read(u.add(REV_DROP_OFFSET) as *const i64) };
            let retain_fn = unsafe { std::ptr::read(u.add(REV_RETAIN_OFFSET) as *const i64) };
            let elem_size = unsafe { std::ptr::read(u.add(REV_ELEMSIZE_OFFSET) as *const i64) };
            let heap = do_arr_reverse(inner, drop_fn, retain_fn, elem_size);
            aipl_array_dec(a);
            heap
        }
    }
}

/// Core reversal logic: build a new heap array whose elements are those of `a`
/// (heap or reversed) in reverse order.  Does NOT drop `a`.
fn do_arr_reverse(a: *const u8, drop_fn: i64, retain_fn: i64, elem_size: i64) -> *const u8 {
    let len = unsafe { array_len_of(arr_untag(a)) };
    if elem_size == ELEM_BITPACKED {
        let raw = alloc_array(len, len, drop_fn, ELEM_BITPACKED);
        unsafe {
            let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            for i in 0..len {
                let j = len - 1 - i;
                let bit = arr_load_bit(a, j);
                write_packed_bit(dst, i, bit);
            }
        }
        return raw;
    }
    let es = elem_size.max(8) as usize;
    let raw = alloc_array(len, len, drop_fn, elem_size);
    unsafe {
        let dst_base = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
        for i in 0..len {
            let j = len - 1 - i;
            let src = arr_elem_ptr(a, j, es);
            std::ptr::copy_nonoverlapping(src, dst_base.add(i * es), es);
        }
        elem_rc(retain_fn, dst_base, len);
    }
    raw
}

/// Return a pointer to element `idx` in a heap array (assumes tag already stripped
/// by `arr_elem_ptr`).
unsafe fn heap_elem_ptr(base: *const u8, idx: usize, elem_size: usize) -> *const u8 {
    unsafe { base.add(ARR_ELEMS_OFFSET).add(idx * elem_size) }
}

/// Repr-aware element pointer for use from JIT-compiled code (non-heap fast
/// path). Returns a pointer to element `idx` in any array representation.
/// `elem_size` is the stride in bytes (0 = bit-packed, NOT valid here — use
/// `aipl_arr_load_bit` for bit-packed arrays).
extern "C" fn aipl_arr_elem_ptr(a: *const u8, idx: i64, elem_size: i64) -> *const u8 {
    unsafe { arr_elem_ptr(a, idx as usize, elem_size as usize) }
}

/// Repr-aware bit load for JIT-compiled code. Returns 0 or 1.
extern "C" fn aipl_arr_load_bit(a: *const u8, idx: i64) -> i64 {
    i64::from(unsafe { arr_load_bit(a, idx as usize) })
}

/// Compute the address of element `idx`, dispatching on representation.
unsafe fn arr_elem_ptr(a: *const u8, idx: usize, elem_size: usize) -> *const u8 {
    match arr_repr(a) {
        ArrRepr::Heap => unsafe { heap_elem_ptr(arr_untag(a), idx, elem_size) },
        ArrRepr::Reversed => {
            let u = arr_untag(a);
            let inner = unsafe { std::ptr::read(u.add(REV_INNER_OFFSET) as *const *const u8) };
            let len = unsafe { std::ptr::read(u as *const i64) as usize };
            let j = len - 1 - idx;
            unsafe { arr_elem_ptr(inner, j, elem_size) }
        }
    }
}

/// Read bit `idx` from an array (any repr).
unsafe fn arr_load_bit(a: *const u8, idx: usize) -> bool {
    match arr_repr(a) {
        ArrRepr::Heap => {
            let base = arr_untag(a).add(ARR_ELEMS_OFFSET);
            unsafe { (*base.add(idx >> 3) >> (idx & 7)) & 1 != 0 }
        }
        ArrRepr::Reversed => {
            let u = arr_untag(a);
            let inner = unsafe { std::ptr::read(u.add(REV_INNER_OFFSET) as *const *const u8) };
            let len = unsafe { std::ptr::read(u as *const i64) as usize };
            unsafe { arr_load_bit(inner, len - 1 - idx) }
        }
    }
}

// `bool[]` is bit-packed (8 elements per byte, like `std::vector<bool>` but with
// the ordinary array interface). It's signalled by an `elem_size` of 0 passed
// from codegen — the one sentinel that means "bit-packed" rather than a byte
// stride. `len` still counts elements; `cap` (bytes) holds `ceil(len/8)`. Bits
// past `len` are never read, so they need not be cleared.
const ELEM_BITPACKED: i64 = 0;

/// Bytes needed to hold `count` elements: `ceil(count/8)` when bit-packed
/// (`elem_size == 0`), else `count * elem_size` (with the historic 8-byte floor).
fn cap_bytes_for(elem_size: i64, count: usize) -> usize {
    if elem_size == ELEM_BITPACKED {
        count.div_ceil(8)
    } else {
        count * (elem_size.max(8) as usize)
    }
}

/// Write bit `idx` of a bit-packed data region (reads happen in codegen IR).
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

unsafe fn array_len_of(ptr: *const u8) -> usize {
    unsafe { std::ptr::read(arr_untag(ptr).add(ARR_LEN_OFFSET) as *const i64) as usize }
}

unsafe fn array_cap_bytes_of(ptr: *const u8) -> usize {
    // Only valid for heap arrays; callers must ensure the ptr is untagged/Heap.
    unsafe { std::ptr::read(arr_untag(ptr).add(ARR_CAP_OFFSET) as *const i64) as usize }
}

/// Retain (`inc`) or drop one element via a retain/drop helper-fn pointer, if
/// non-null. `at` points at the element; the helper handles `count` elements.
unsafe fn elem_rc(fn_ptr: i64, at: *const u8, count: usize) {
    if fn_ptr != 0 {
        let f: ArrDropFn = unsafe { std::mem::transmute(fn_ptr) };
        f(at, count as i64);
    }
}

/// Allocate an array block holding `cap` element slots of `elem_size` bytes,
/// with `len`/`drop_fn` set and the byte-capacity recorded (refcount 1).
fn alloc_array(len: usize, cap: usize, drop_fn: i64, elem_size: i64) -> *const u8 {
    let cap_bytes = cap_bytes_for(elem_size, cap);
    let layout = std::alloc::Layout::from_size_align(
        array_block_size(cap_bytes),
        std::mem::align_of::<i64>(),
    )
    .expect("array layout");
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    unsafe {
        std::ptr::write(raw as *mut i64, 1); // refcount
        std::ptr::write(
            raw.add(HEADER_SIZE + ARR_LEN_OFFSET) as *mut i64,
            len as i64,
        );
        std::ptr::write(
            raw.add(HEADER_SIZE + ARR_CAP_OFFSET) as *mut i64,
            cap_bytes as i64,
        );
        std::ptr::write(
            raw.add(HEADER_SIZE + ARR_DROPFN_OFFSET) as *mut i64,
            drop_fn,
        );
        raw.add(HEADER_SIZE)
    }
}

/// Allocate an array of `len` uninitialized elements (refcount 1, cap == len)
/// with the given element `drop_fn` (0 for scalar elements) and `elem_size`.
/// Codegen stores each element immediately after.
extern "C" fn aipl_array_new(len: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    let len = len.max(0) as usize;
    alloc_array(len, len, drop_fn, elem_size)
}

/// Allocate an empty array (len 0, refcount 1) reserved to `cap` slots of
/// `elem_size` bytes with the given element `drop_fn`. Used by `map`/`filter` to
/// pre-size their output. Mirrors `aipl_array_with_cap` in the linker runtime.
extern "C" fn aipl_array_with_cap(cap: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    alloc_array(0, cap.max(0) as usize, drop_fn, elem_size)
}

/// Decrement an array's refcount; at zero, release each element via the
/// stored `drop_fn` (if any) and free the block (sized by its byte-capacity).
extern "C" fn aipl_array_dec(ptr: *const u8) {
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
                    let len = array_len_of(u);
                    let cap_bytes = array_cap_bytes_of(u);
                    let drop_fn = std::ptr::read(u.add(ARR_DROPFN_OFFSET) as *const i64);
                    if drop_fn != 0 {
                        let f: ArrDropFn = std::mem::transmute(drop_fn);
                        f(u.add(ARR_ELEMS_OFFSET), len as i64);
                    }
                    let layout = std::alloc::Layout::from_size_align(
                        array_block_size(cap_bytes),
                        std::mem::align_of::<i64>(),
                    )
                    .expect("array layout");
                    std::alloc::dealloc(h as *mut u8, layout);
                }
                ArrRepr::Reversed => {
                    let inner = std::ptr::read(u.add(REV_INNER_OFFSET) as *const *const u8);
                    aipl_array_dec(inner);
                    let layout = std::alloc::Layout::from_size_align(
                        HEADER_SIZE + REV_BLOCK_DATA_SIZE,
                        std::mem::align_of::<i64>(),
                    )
                    .expect("rev-view layout");
                    std::alloc::dealloc(h as *mut u8, layout);
                }
            }
        }
    }
}

/// Retain an array value (any representation).  Arrays use this instead of
/// `aipl_inc` because `aipl_inc` dispatches on the *string* tag scheme, which
/// would misinterpret an array's representation tag.
extern "C" fn aipl_arr_inc(ptr: *const u8) {
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

/// Copy-and-grow push (value semantics): a fresh array of `a`'s elements plus
/// the element at `x` (`elem_size` bytes), then drop `a`. Used when the array
/// may be aliased. `retain_fn` retains the copied elements (the new array
/// co-owns them).
extern "C" fn aipl_array_push(
    a: *const u8,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    if elem_size == ELEM_BITPACKED {
        // Bit-packed `bool[]`: fresh block of old_len+1 bits, copy the old bits,
        // set the new one, drop the input. No element refcounting (bools).
        let old_len = if a.is_null() {
            0
        } else {
            unsafe { array_len_of(a) }
        };
        let raw = alloc_array(old_len + 1, old_len + 1, drop_fn, ELEM_BITPACKED);
        unsafe {
            let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            if old_len > 0 && !a.is_null() {
                let src = a.add(ARR_ELEMS_OFFSET);
                std::ptr::copy_nonoverlapping(src, dst, cap_bytes_for(ELEM_BITPACKED, old_len));
            }
            write_packed_bit(dst, old_len, std::ptr::read(x as *const i64) != 0);
        }
        aipl_array_dec(a);
        return raw;
    }
    let elem_size = elem_size.max(8) as usize;
    let old_len = if a.is_null() {
        0
    } else {
        unsafe { array_len_of(a) }
    };
    let raw = alloc_array(old_len + 1, old_len + 1, drop_fn, elem_size as i64);
    unsafe {
        let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
        if old_len > 0 && !a.is_null() {
            let src = a.add(ARR_ELEMS_OFFSET);
            std::ptr::copy_nonoverlapping(src, dst, old_len * elem_size);
            elem_rc(retain_fn, dst, old_len);
        }
        let slot = dst.add(old_len * elem_size);
        std::ptr::copy_nonoverlapping(x, slot, elem_size);
        elem_rc(retain_fn, slot, 1);
    }
    aipl_array_dec(a);
    raw
}

/// In-place push for a uniquely owned array (codegen emits this only when its
/// static analysis proves the array is unaliased). Appends without copying when
/// there's spare capacity, else grows to a doubled capacity by `realloc`.
/// Mirrors `aipl_array_push_mut` in the linker runtime.
extern "C" fn aipl_array_push_mut(
    a: *const u8,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    if elem_size == ELEM_BITPACKED {
        // Bit-packed `bool[]`, in place: set bit `old_len`, growing the byte
        // capacity (doubling) only when the next bit needs a new byte.
        let (old_len, cap_bytes) = if a.is_null() {
            (0, 0)
        } else {
            unsafe { (array_len_of(a), array_cap_bytes_of(a)) }
        };
        let val = unsafe { std::ptr::read(x as *const i64) != 0 };
        if !a.is_null() && cap_bytes_for(ELEM_BITPACKED, old_len + 1) <= cap_bytes {
            unsafe {
                write_packed_bit(a.add(ARR_ELEMS_OFFSET) as *mut u8, old_len, val);
                std::ptr::write(a as *mut i64, (old_len + 1) as i64);
                std::ptr::write(a.add(ARR_DROPFN_OFFSET) as *mut i64, drop_fn);
            }
            return a;
        }
        let new_cap_bytes = cap_bytes_for(ELEM_BITPACKED, old_len + 1)
            .max(cap_bytes * 2)
            .max(1);
        let data: *const u8 = if a.is_null() {
            alloc_array(
                old_len + 1,
                (new_cap_bytes * 8).max(1),
                drop_fn,
                ELEM_BITPACKED,
            )
        } else {
            unsafe {
                let block = a.sub(HEADER_SIZE) as *mut u8;
                let old_layout = std::alloc::Layout::from_size_align(
                    array_block_size(cap_bytes),
                    std::mem::align_of::<i64>(),
                )
                .expect("array layout");
                let raw = std::alloc::realloc(block, old_layout, array_block_size(new_cap_bytes));
                if raw.is_null() {
                    std::alloc::handle_alloc_error(old_layout);
                }
                let data = raw.add(HEADER_SIZE);
                std::ptr::write(data.add(ARR_CAP_OFFSET) as *mut i64, new_cap_bytes as i64);
                std::ptr::write(data.add(ARR_DROPFN_OFFSET) as *mut i64, drop_fn);
                std::ptr::write(data as *mut i64, (old_len + 1) as i64);
                data as *const u8
            }
        };
        unsafe { write_packed_bit(data.add(ARR_ELEMS_OFFSET) as *mut u8, old_len, val) };
        return data;
    }
    let elem_size = elem_size.max(8) as usize;
    let (old_len, cap_bytes) = if a.is_null() {
        (0, 0)
    } else {
        unsafe { (array_len_of(a), array_cap_bytes_of(a)) }
    };
    if !a.is_null() && (old_len + 1) * elem_size <= cap_bytes {
        unsafe {
            let elems = a.add(ARR_ELEMS_OFFSET) as *mut u8;
            let slot = elems.add(old_len * elem_size);
            std::ptr::copy_nonoverlapping(x, slot, elem_size);
            elem_rc(retain_fn, slot, 1);
            std::ptr::write(a as *mut i64, (old_len + 1) as i64); // len += 1
                                                                  // Keep the stored drop-fn in sync: an array reserved via
                                                                  // `aipl_array_with_cap` (or an empty `[]`) starts with none (0) and
                                                                  // first learns its element type here when it has spare capacity and
                                                                  // never hits the realloc path that would otherwise set it.
            std::ptr::write(a.add(ARR_DROPFN_OFFSET) as *mut i64, drop_fn);
        }
        return a;
    }
    // At capacity: `realloc` to a doubled byte-capacity. It preserves the header
    // and existing elements (refcounts unchanged), so no element is re-retained
    // and there's no old block to free — only the new element is retained.
    let new_cap_bytes = ((old_len + 1) * elem_size).max(cap_bytes * 2);
    let data: *const u8 = if a.is_null() {
        // No block to grow (defensive; exclusive arrays start non-null).
        alloc_array(
            old_len + 1,
            new_cap_bytes / elem_size,
            drop_fn,
            elem_size as i64,
        )
    } else {
        unsafe {
            let block = a.sub(HEADER_SIZE) as *mut u8;
            let old_layout = std::alloc::Layout::from_size_align(
                array_block_size(cap_bytes),
                std::mem::align_of::<i64>(),
            )
            .expect("array layout");
            let raw = std::alloc::realloc(block, old_layout, array_block_size(new_cap_bytes));
            if raw.is_null() {
                std::alloc::handle_alloc_error(old_layout);
            }
            let data = raw.add(HEADER_SIZE);
            std::ptr::write(data.add(ARR_CAP_OFFSET) as *mut i64, new_cap_bytes as i64);
            // Refresh drop_fn: an empty `[]` starts with none and only learns its
            // element type on the first push.
            std::ptr::write(data.add(ARR_DROPFN_OFFSET) as *mut i64, drop_fn);
            std::ptr::write(data as *mut i64, (old_len + 1) as i64); // len
            data as *const u8
        }
    };
    unsafe {
        let elems = data.add(ARR_ELEMS_OFFSET) as *mut u8;
        let slot = elems.add(old_len * elem_size);
        std::ptr::copy_nonoverlapping(x, slot, elem_size);
        elem_rc(retain_fn, slot, 1);
    }
    data
}

/// Executed-instruction counter hook. Codegen emits one call per basic block
/// (arg = the block's instruction count). The JIT path never reports perf
/// counts (those come from the AOT instrumented runtime), so this is a no-op
/// here — it exists only so the symbol resolves for JIT-run programs.
extern "C" fn aipl_count_insns(_n: i64) {}

// ---------- Test-runner runtime ----------
//
// The `check` command JIT-runs a synthesized `__test_main` driver: for each
// function with a `.test({ .. })` body it emits `__test_begin(name)` /
// `__test$<fn>()` / `__test_end()`, then `__test_summary()` as the driver's
// (exit-code) result. `__assert(cond, loc)` records and reports each failure.
// State is process-global (the driver runs single-threaded in-process). Only
// failures print: a failing test gets one `test <name> ... FAIL` header line
// followed by an indented line per failed assertion; passing tests are silent.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering as TestOrd};

static TEST_CUR_FAILED: AtomicBool = AtomicBool::new(false);
static TEST_HEADER_PRINTED: AtomicBool = AtomicBool::new(false);
static TEST_CUR_NAME: AtomicUsize = AtomicUsize::new(0);
static TEST_TOTAL: AtomicI64 = AtomicI64::new(0);
static TEST_PASSED: AtomicI64 = AtomicI64::new(0);
static TEST_FAILED: AtomicI64 = AtomicI64::new(0);

/// Read a runtime `str` value (inline or heap) as a `&str` for the report.
/// `buf` backs an inline string's materialized bytes and must outlive the result.
unsafe fn test_cstr(p: *const u8, buf: &mut [u8; 8]) -> &str {
    if p.is_null() {
        return "<unknown>";
    }
    let bytes = unsafe { str_bytes(p, buf) };
    std::str::from_utf8(bytes).unwrap_or("<invalid utf8>")
}

extern "C" fn aipl_test_begin(name: *const u8) {
    TEST_CUR_NAME.store(name as usize, TestOrd::Relaxed);
    TEST_CUR_FAILED.store(false, TestOrd::Relaxed);
    TEST_HEADER_PRINTED.store(false, TestOrd::Relaxed);
}

extern "C" fn aipl_assert(cond: i64, loc: *const u8) {
    if cond == 0 {
        if !TEST_HEADER_PRINTED.swap(true, TestOrd::Relaxed) {
            let mut nbuf = [0u8; 8];
            let name =
                unsafe { test_cstr(TEST_CUR_NAME.load(TestOrd::Relaxed) as *const u8, &mut nbuf) };
            println!("test {name} ... FAIL");
        }
        let mut lbuf = [0u8; 8];
        println!("  assert failed at {}", unsafe {
            test_cstr(loc, &mut lbuf)
        });
        TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
    }
}

extern "C" fn aipl_test_end() {
    TEST_TOTAL.fetch_add(1, TestOrd::Relaxed);
    if TEST_CUR_FAILED.load(TestOrd::Relaxed) {
        TEST_FAILED.fetch_add(1, TestOrd::Relaxed);
    } else {
        TEST_PASSED.fetch_add(1, TestOrd::Relaxed);
    }
}

extern "C" fn aipl_test_summary() -> i64 {
    let total = TEST_TOTAL.load(TestOrd::Relaxed);
    let passed = TEST_PASSED.load(TestOrd::Relaxed);
    let failed = TEST_FAILED.load(TestOrd::Relaxed);
    println!("{total} tests: {passed} passed, {failed} failed");
    i64::from(failed > 0)
}

// ---------- Set runtime ----------
//
// A set reuses the array heap block verbatim (same `[refcount][len][cap]
// [drop_fn][elems...]` layout, same `aipl_array_*` allocator/refcount/push).
// Only construction differs: elements are inserted deduplicated. Elements are
// i64/bool/char or `str`. Scalars compare by value (a bit-compare for a packed
// `bool` set) and need no element drop/retain; `str` elements are 8-byte
// pointers compared by content, with the array's `str` drop/retain helpers
// stored so the block frees/retains its strings like a `str[]`.

/// Compare two NUL-terminated runtime strings by content. A null pointer equals
/// only itself. Used by `==` (via `aipl_str_eq`) and set-of-`str` membership.
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
    // chunk against the other side made contiguous. This materializes only when
    // BOTH sides are ropes (rare); the common rope-vs-literal case copies nothing.
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

/// `str == str`: content-compare, then decrement both inputs (it consumes a ref
/// from each, like the other str-taking builtins; callers pre-inc). Returns 1/0.
extern "C" fn aipl_str_eq(a: *const u8, b: *const u8) -> i64 {
    let eq = unsafe { rt_str_eq(a, b) };
    aipl_dec(a);
    aipl_dec(b);
    i64::from(eq)
}

/// `s.starts_with(prefix) -> bool` (1/0): whether `s`'s bytes begin with
/// `prefix`'s. Consumes (decs) both inputs, like the other str builtins; callers
/// pre-inc. The empty prefix always matches.
extern "C" fn aipl_str_starts_with(s: *const u8, prefix: *const u8) -> i64 {
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
            let take = std::cmp::min(chunk.len(), pl - off);
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
/// Consumes (decs) both inputs, like the other str builtins; callers pre-inc.
/// The empty suffix always matches.
extern "C" fn aipl_str_ends_with(s: *const u8, suffix: *const u8) -> i64 {
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

/// `s.contains(needle) -> bool` (1/0): whether `needle`'s bytes occur
/// contiguously anywhere in `s`'s. Consumes (decs) both inputs, like the other
/// str builtins; callers pre-inc. The empty needle always matches. Unlike
/// `starts_with`/`ends_with` this reads both strings via `str_bytes` (a rope
/// receiver materializes its memoized cache) — a streaming window search
/// across chunk boundaries isn't worth the complexity here.
extern "C" fn aipl_str_contains(s: *const u8, needle: *const u8) -> i64 {
    let mut sb = [0u8; 8];
    let mut nb = [0u8; 8];
    let found = unsafe {
        let sbytes = str_bytes(s, &mut sb);
        let nbytes = str_bytes(needle, &mut nb);
        nbytes.is_empty() || sbytes.windows(nbytes.len()).any(|w| w == nbytes)
    };
    aipl_dec(s);
    aipl_dec(needle);
    i64::from(found)
}

/// FNV-1a content hash of a NUL-terminated runtime string (consistent with
/// `rt_str_eq`: equal content → equal hash). Borrows `a` — no refcount change.
/// A null pointer hashes to the bare offset basis.
extern "C" fn aipl_str_hash(a: *const u8) -> i64 {
    // FNV-1a is a left fold over bytes, so it streams a rope's leaves in order
    // (same result as the flattened bytes) — no materialization.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64-bit offset basis
    str_for_each_chunk(a, &mut |chunk| {
        for &c in chunk {
            h ^= c as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a 64-bit prime
        }
        true
    });
    h as i64
}

/// Whether `a` already contains the element at `x`. `str_cmp != 0` compares
/// `str` elements (8-byte pointers) by content; otherwise a bit-packed `bool`
/// set (`elem_size == 0`) compares unpacked bits and every other scalar set
/// compares the 8-byte value. Returns 1 (present) or 0 (absent); a null/empty
/// set is never a member.
extern "C" fn aipl_set_contains(a: *const u8, x: *const u8, elem_size: i64, str_cmp: i64) -> i64 {
    if a.is_null() {
        return 0;
    }
    let len = unsafe { array_len_of(a) };
    if str_cmp != 0 {
        let target = unsafe { std::ptr::read(x as *const i64) } as *const u8;
        for i in 0..len {
            let sp = unsafe { arr_elem_ptr(a, i, 8) };
            let s = unsafe { std::ptr::read(sp as *const i64) } as *const u8;
            if unsafe { rt_str_eq(s, target) } {
                return 1;
            }
        }
        0
    } else if elem_size == ELEM_BITPACKED {
        let target = unsafe { std::ptr::read(x as *const i64) } != 0;
        for i in 0..len {
            if unsafe { arr_load_bit(a, i) } == target {
                return 1;
            }
        }
        0
    } else {
        let stride = elem_size.max(8) as usize;
        let target = unsafe { std::ptr::read(x as *const i64) };
        for i in 0..len {
            let ep = unsafe { arr_elem_ptr(a, i, stride) };
            let v = unsafe { std::ptr::read(ep as *const i64) };
            if v == target {
                return 1;
            }
        }
        0
    }
}

/// Insert the element at `x` into `a` (a uniquely-owned, array-backed set),
/// skipping it if already present (membership per `str_cmp`, see
/// `aipl_set_contains`). Returns the (possibly relocated) set pointer. For heap
/// elements (`str`), `drop_fn`/`retain_fn` are the element helpers so the block
/// frees/retains its strings; for scalars they're 0.
extern "C" fn aipl_set_insert(
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

/// Read element `i` of set/array `src` as an i64 — a bit-unpacked `bool`
/// (`elem_size == 0`), else the 8-byte value (a scalar or `str` pointer). The
/// value is passed by-address to `aipl_set_insert`, so this normalizes the
/// bit-packed case into a plain i64 the inserter can spill and read.
unsafe fn read_set_elem(src: *const u8, i: usize, elem_size: i64) -> i64 {
    if elem_size == ELEM_BITPACKED {
        i64::from(unsafe { arr_load_bit(src, i) })
    } else {
        let stride = elem_size.max(8) as usize;
        let ep = unsafe { arr_elem_ptr(src, i, stride) };
        unsafe { std::ptr::read(ep as *const i64) }
    }
}

/// `a.union(b)` (copy): a fresh set with every distinct element of `a` then `b`.
/// Consumes (decs) both inputs, like `aipl_concat`. Inserted elements are
/// retained by `aipl_set_insert`, so they outlive the inputs' release.
extern "C" fn aipl_set_union(
    a: *const u8,
    b: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
) -> *const u8 {
    let a_len = if a.is_null() {
        0
    } else {
        unsafe { array_len_of(a) }
    };
    let b_len = if b.is_null() {
        0
    } else {
        unsafe { array_len_of(b) }
    };
    // Pre-size to the upper bound (|a| + |b|) so the insert loop never reallocs.
    let mut dest = aipl_array_with_cap((a_len + b_len) as i64, drop_fn, elem_size);
    for i in 0..a_len {
        let v = unsafe { read_set_elem(a, i, elem_size) };
        let vp = &v as *const i64 as *const u8;
        dest = aipl_set_insert(dest, vp, drop_fn, retain_fn, elem_size, str_cmp);
    }
    for i in 0..b_len {
        let v = unsafe { read_set_elem(b, i, elem_size) };
        let vp = &v as *const i64 as *const u8;
        dest = aipl_set_insert(dest, vp, drop_fn, retain_fn, elem_size, str_cmp);
    }
    aipl_array_dec(a);
    aipl_array_dec(b);
    dest
}

/// `set a = a.union(b)` for an exclusive `a`: extend `a` in place with `b`'s
/// distinct elements (reusing `a`'s allocation) and return the (possibly
/// relocated) set. Consumes (decs) `b`; `a` is reused, not dec'd.
extern "C" fn aipl_set_union_mut(
    a: *const u8,
    b: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    str_cmp: i64,
) -> *const u8 {
    let mut a = a;
    let b_len = if b.is_null() {
        0
    } else {
        unsafe { array_len_of(b) }
    };
    for i in 0..b_len {
        let v = unsafe { read_set_elem(b, i, elem_size) };
        let vp = &v as *const i64 as *const u8;
        a = aipl_set_insert(a, vp, drop_fn, retain_fn, elem_size, str_cmp);
    }
    aipl_array_dec(b);
    a
}

/// Index of the pair in dict `a` whose key matches the key at `pair_ptr` (its
/// first 8 bytes), or -1. Keys compare by `str_cmp`: content (`rt_str_eq`) for
/// `str`, else the raw 8-byte value.
unsafe fn dict_find(a: *const u8, pair_ptr: *const u8, pair_size: i64, str_cmp: i64) -> i64 {
    if a.is_null() {
        return -1;
    }
    let len = unsafe { array_len_of(a) };
    let stride = pair_size as usize;
    let want = unsafe { std::ptr::read(pair_ptr as *const i64) };
    for i in 0..len {
        let ep = unsafe { arr_elem_ptr(a, i, stride) };
        let k = unsafe { std::ptr::read(ep as *const i64) };
        let eq = if str_cmp != 0 {
            unsafe { rt_str_eq(k as *const u8, want as *const u8) }
        } else {
            k == want
        };
        if eq {
            return i as i64;
        }
    }
    -1
}

/// Insert (or, on a duplicate key, replace) the `[key][value]` pair at `pair_ptr`
/// into dict `a` (a uniquely-owned, array-backed dict of `pair_size`-byte pairs).
/// `drop_fn`/`retain_fn` are the pair helpers (they release/retain a pair's key
/// *and* value). On a key collision the whole existing pair is released and the
/// new one stored in its place (last-binding-wins); otherwise the pair is
/// appended. Returns the (possibly relocated) dict pointer. Like the set/array
/// inserters, the stored pair is retained, so the caller keeps its originals.
extern "C" fn aipl_dict_insert(
    a: *const u8,
    pair_ptr: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    let idx = unsafe { dict_find(a, pair_ptr, pair_size, str_cmp) };
    if idx >= 0 {
        let stride = pair_size as usize;
        unsafe {
            let slot = arr_elem_ptr(a, idx as usize, stride) as *mut u8;
            elem_rc(drop_fn, slot, 1); // release the old key+value
            std::ptr::copy_nonoverlapping(pair_ptr, slot, stride);
            elem_rc(retain_fn, slot, 1); // co-own the new key+value
        }
        return a;
    }
    aipl_array_push_mut(a, pair_ptr, drop_fn, retain_fn, pair_size)
}

/// Look up `key_ptr` in dict `a`: returns a pointer to the matching pair's value
/// slot (its bytes are read/retained by the caller), or null if absent. Borrows
/// `a` (no refcount change).
extern "C" fn aipl_dict_get(
    a: *const u8,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> *const u8 {
    let idx = unsafe { dict_find(a, key_ptr, pair_size, str_cmp) };
    if idx < 0 {
        return std::ptr::null();
    }
    // The key occupies the first 8 bytes of the pair; the value follows.
    unsafe { arr_elem_ptr(a, idx as usize, pair_size as usize).add(8) }
}

/// `d.contains_key(k)`: whether `key_ptr` is a key of dict `a`. Borrows `a`.
extern "C" fn aipl_dict_contains_key(
    a: *const u8,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> i64 {
    i64::from(unsafe { dict_find(a, key_ptr, pair_size, str_cmp) } >= 0)
}

/// Element drop-fn for `str[]`: dec each element string.
extern "C" fn aipl_arr_drop_str(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_dec(std::ptr::read(elems.add(i)) as *const u8);
        }
    }
}

/// Element drop-fn for an array of arrays (`T[][]`): release each element
/// array, which recursively releases its own elements via its own drop-fn.
extern "C" fn aipl_arr_drop_arr(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_array_dec(std::ptr::read(elems.add(i)) as *const u8);
        }
    }
}

/// Element retain-fn for `str[]`/`T[][]`: inc each element pointer (the array's
/// co-ownership). Both strings and arrays share the `inc` protocol.
extern "C" fn aipl_arr_retain_ptr(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_inc(std::ptr::read(elems.add(i)) as *const u8);
        }
    }
}

/// Element drop-fn for `str?[]`: each element is an inline 16-byte `{tag, value}`
/// optional; dec the inner string when present (tag != 0).
extern "C" fn aipl_arr_drop_opt_str(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            let tag = std::ptr::read(e as *const i64);
            if tag != 0 {
                aipl_dec(std::ptr::read(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Element drop-fn for `T[]?[]`: release the inner array of each present element.
extern "C" fn aipl_arr_drop_opt_arr(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            let tag = std::ptr::read(e as *const i64);
            if tag != 0 {
                aipl_array_dec(std::ptr::read(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Element retain-fn for `str?[]`/`T[]?[]`: inc the inner heap pointer of each
/// present element (tag != 0). Both inner kinds share the `inc` protocol.
extern "C" fn aipl_arr_retain_opt(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            let tag = std::ptr::read(e as *const i64);
            if tag != 0 {
                aipl_inc(std::ptr::read(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Build a `str[]` from `args` using the JIT's own allocators (so the JIT'd
/// callee can free it through the matching runtime). Mirrors what the AOT
/// runtime's `build_cli_args` does for a native binary.
fn build_cli_array(args: &[String]) -> *const u8 {
    let drop_fn = aipl_arr_drop_str as *const () as usize as i64;
    let arr = aipl_array_new(args.len() as i64, drop_fn, 8);
    unsafe {
        let elems = arr.add(ARR_ELEMS_OFFSET) as *mut i64;
        for (i, a) in args.iter().enumerate() {
            let raw = alloc_dynamic_string(a.len());
            std::ptr::copy_nonoverlapping(a.as_ptr(), raw.add(STR_HEADER_SIZE), a.len());
            std::ptr::write(elems.add(i), raw.add(STR_HEADER_SIZE) as i64);
        }
    }
    arr
}

/// How a `funcs`-map entry's code is reached at a call site.
#[derive(Clone, Copy)]
enum FuncLink {
    /// A user (or monomorphization-synthesized) function defined in this module;
    /// holds its already-declared `FuncId`.
    User(FuncId),
    /// A runtime builtin, named by its `aipl_*` import symbol. The import (and
    /// thus its object symbol) is declared lazily on first reference, so a
    /// program never carries symbols for builtins it doesn't call.
    Builtin(&'static str),
}

#[derive(Clone)]
struct FuncInfo {
    link: FuncLink,
    params: Vec<Type>,
    return_ty: Type,
    effects: Vec<String>,
    /// `true` for a mutating method (`fn f(mut self: T, ...)`): it returns
    /// nothing to the user, mutates its receiver, and must be called as
    /// `v.f(...)`. At the ABI level it returns the mutated `self` (its
    /// `return_ty`), which the call site stores back into `v`.
    is_mutating: bool,
    /// Indices of parameters this instance takes ownership of (set by
    /// monomorphization). At a call site the corresponding argument — always a
    /// fresh, uniquely-owned heap value — is *moved* in (no retain) rather than
    /// borrowed, and the callee won't drop it on entry-scope exit.
    owned_params: Vec<usize>,
}

struct StructLayout {
    fields: Vec<FieldLayout>,
    size: u32,
}

#[derive(Clone)]
struct FieldLayout {
    name: String,
    ty: Type,
    offset: u32,
}

impl StructLayout {
    fn field(&self, name: &str) -> Option<&FieldLayout> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// Byte offset of a variant's payload region, past its 8-byte tag.
const VARIANT_PAYLOAD_OFFSET: u32 = 8;

/// Layout of a `variant` (sum) type: an inline `{ tag, payload }` composite. The
/// tag (a case's index) sits at offset 0; each case's payload is laid out like a
/// struct starting at `VARIANT_PAYLOAD_OFFSET`. All cases share that region, so
/// `size` = tag + the widest case's payload.
struct VariantLayout {
    cases: Vec<VariantCaseLayout>,
    size: u32,
}

struct VariantCaseLayout {
    name: String,
    /// Payload fields, with offsets relative to the variant's base (so the first
    /// field is at `VARIANT_PAYLOAD_OFFSET`). Empty for a nullary case.
    fields: Vec<FieldLayout>,
}

impl VariantLayout {
    /// The `(tag, case)` for constructor `name`, if it's one of this variant's.
    fn case(&self, name: &str) -> Option<(usize, &VariantCaseLayout)> {
        self.cases.iter().enumerate().find(|(_, c)| c.name == name)
    }
}

/// A declared composite type: a `struct` (named fields) or a `variant` (tagged
/// union). Both are inline, addressed composites, so most layout queries (size,
/// `is_composite`, copying) treat them uniformly; only construction, matching,
/// rendering, and refcounting branch on which it is.
enum TypeDef {
    Struct(StructLayout),
    Variant(VariantLayout),
}

impl TypeDef {
    fn size(&self) -> u32 {
        match self {
            TypeDef::Struct(s) => s.size,
            TypeDef::Variant(v) => v.size,
        }
    }
    fn as_struct(&self) -> Option<&StructLayout> {
        match self {
            TypeDef::Struct(s) => Some(s),
            TypeDef::Variant(_) => None,
        }
    }
    fn as_variant(&self) -> Option<&VariantLayout> {
        match self {
            TypeDef::Variant(v) => Some(v),
            TypeDef::Struct(_) => None,
        }
    }
}

/// A value marshaled across the embedding FFI by [`Compilation::call_values`].
/// Scalars (`i64`/`bool`/`char`) ride their shared `i64` ABI as `Int` (`bool` is
/// `0`/`1`, `char` a codepoint; `Unit` also reads back as `Int(0)`); a `str` (or
/// the builtin `Error`) is `Str`; an optional `T?` over a scalar or `str` core
/// is `Opt`; a `Result` is `Res` (only as a *return* value so far — optionals
/// and results can't be passed as arguments yet). A `struct` of scalar/`str`
/// fields is `Struct`, also return-only. Other composites (arrays, sets, dicts,
/// variants) aren't marshalable yet.
#[derive(Debug, Clone, PartialEq)]
pub enum FfiValue {
    /// A scalar AIPL value at its `i64` ABI.
    Int(i64),
    /// An AIPL `str` (or the builtin `Error`, which shares its representation).
    Str(String),
    /// An AIPL optional: `Opt(None)` is `none`; `Opt(Some(v))` is `some(v)`
    /// (nested for `T??`). The inner core is an `Int` or `Str`.
    Opt(Option<Box<FfiValue>>),
    /// An AIPL `Result<ok, err>`: `Res(Ok(v))` for `ok(v)`, `Res(Err(e))` for
    /// `err(e)`. Each side's payload is an `Int` (also standing in for a
    /// `Unit` side, e.g. `!Error`'s ok case), `Str`, or `Struct` — whatever
    /// [`check_ffi_return`] accepted for that side.
    Res(Result<Box<FfiValue>, Box<FfiValue>>),
    /// An AIPL `struct`: its fields in declaration order, each a `(name, value)`.
    /// Returned through a hidden sret pointer (like an optional). Fields are
    /// scalars (`Int`) or `str` (`Str`) for now — see [`Compilation::call_values`].
    Struct(Vec<(String, FfiValue)>),
}

pub struct Compilation {
    module: JITModule,
    funcs: HashMap<String, FuncInfo>,
    /// Declared struct/variant layouts, retained so [`Compilation::call_values`]
    /// can marshal a struct return value back to the host (read each field at its
    /// offset). Populated from the frontend on [`Compilation::new`], and from the
    /// `; struct` manifest lines on the dogfood [`Compilation::from_artifact`] path.
    structs: HashMap<String, TypeDef>,
    ir: String,
}

/// Parse [`aipl_syntax::BUILTIN_SIGNATURES`] into the checker's builtin
/// declarations. The source is a fixed constant, so a parse failure is a
/// compiler bug. Each is marked `pub` to match the hand-built originals
/// (visibility is irrelevant to the checker, the only consumer). The
/// AIPL-implemented builtins aren't in that constant — their signatures come
/// straight from their `.aipl` source via [`aipl_mono::aipl_builtin_sig_decls`],
/// so a builtin's signature lives in exactly one place.
fn builtin_decls() -> Vec<Item> {
    let program = aipl_parser::parse(aipl_syntax::BUILTIN_SIGNATURES)
        .expect("builtin signatures are valid AIPL");
    program
        .items
        .into_iter()
        .map(|item| match item {
            Item::Fn(mut f) => {
                f.is_pub = true;
                Item::Fn(f)
            }
            other => other,
        })
        .chain(aipl_mono::aipl_builtin_sig_decls())
        .collect()
}

/// The `struct` declarations among [`BUILTIN_SIGNATURES`] (e.g. `__builtin_Span`,
/// `__builtin_ExecResult`) — the builtin *types*, as opposed to the builtin
/// function signatures that make up the rest of that constant.
fn builtin_struct_decls() -> Vec<StructDecl> {
    builtin_decls()
        .into_iter()
        .filter_map(|item| match item {
            Item::Struct(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// Build the program the `check` command JIT-runs. Keeps every original item
/// (with any `.test` body stripped so `run`/`build` semantics are unchanged),
/// adds a `__test$<fn>` function per tested function (body = the test block,
/// all effects allowed since a test isn't production code), and a `__test_main`
/// driver that, for each test, calls `__test_begin(name)` / the test /
/// `__test_end()`, then yields `__test_summary()` as its i64 exit code.
/// `Compilation::new(&that).run_0("__test_main")` runs the suite.
pub fn build_test_program(program: &Program) -> Program {
    let span: Span = 0..0;
    let call = |name: &str, args: Vec<Expr>| {
        Expr::new(ExprKind::Call(name.to_string(), args, false), span.clone())
    };
    let seq = |first: Expr, rest: Expr| {
        Expr::new(ExprKind::Seq(Box::new(first), Box::new(rest)), span.clone())
    };
    // A test body may call anything (incl. `!prints`/`!read_files`/`!write_files`/
    // `!execute_program` functions), so the synthesized test fns / driver declare
    // every known effect.
    let all_effects = || {
        vec![
            "prints".to_string(),
            "read_files".to_string(),
            "write_files".to_string(),
            "execute_program".to_string(),
        ]
    };

    let mut items: Vec<Item> = Vec::new();
    let mut tests: Vec<(String, String)> = Vec::new(); // (reported name, test fn name)
    for item in &program.items {
        match item {
            Item::Fn(f) if f.test_body.is_some() => {
                let test_fn = format!("__test${}", f.name);
                let mut orig = f.clone();
                orig.test_body = None;
                items.push(Item::Fn(orig));
                items.push(Item::Fn(AstFn {
                    name: test_fn.clone(),
                    is_pub: true,
                    sig: AstSignature {
                        type_vars: Vec::new(),
                        params: Vec::new(),
                        effects: all_effects(),
                        return_ty: None,
                    },
                    body: f.test_body.clone().expect("test body present"),
                    test_body: None,
                    doc: None,
                }));
                tests.push((f.name.clone(), test_fn));
            }
            other => items.push(other.clone()),
        }
    }
    // Fold the driver body from the tail: `__test_summary()` is the result, with
    // each test's begin/run/end prepended (so they execute in source order).
    let mut body = call("__test_summary", Vec::new());
    for (name, test_fn) in tests.iter().rev() {
        body = seq(call("__test_end", Vec::new()), body);
        body = seq(call(test_fn, Vec::new()), body);
        let name_lit = Expr::new(ExprKind::Str(name.clone()), span.clone());
        body = seq(call("__test_begin", vec![name_lit]), body);
    }
    items.push(Item::Fn(AstFn {
        name: "__test_main".to_string(),
        is_pub: true,
        sig: AstSignature {
            type_vars: Vec::new(),
            params: Vec::new(),
            effects: all_effects(),
            return_ty: Some(Type::Primitive(Primitive::I64)),
        },
        body,
        test_body: None,
        doc: None,
    }));
    Program { items }
}

// ---------------------------------------------------------------------------
// Dogfooding the embedding FFI
//
// The compiler dogfoods a growing set of AIPL functions through the embedding
// FFI (raw-string processing, test-section parsing, error rendering, ...).
// Rather than one hand-assembled `Compilation` per function — each having to
// separately list its own transitive dependencies as bundled in-memory
// modules — every dogfooded `.aipl` file is gathered into one list
// ([`DOGFOOD_SOURCES`]) and compiled together as a single program with one
// checked-in artifact ([`dogfood.clif`]) and one set of FFI entry points
// ([`DOGFOOD_ENTRIES`]). `aipl_loader::load_program_sources` still designates
// `DOGFOOD_SOURCES`' first file as "root" (only its top-level names stay
// unmangled; every other file's are renamed to `__m<index>__<name>`), but
// [`resolve_dogfood_entry`] resolves an entries name against either form, so
// any dogfooded function can be an entry regardless of which file declares
// it — no aggregator/re-export file required. Adding a newly-dogfooded
// function is then: write the `.aipl` file and add it to `DOGFOOD_SOURCES` /
// `DOGFOOD_ENTRIES` — two lists touched, not a new bundle of duplicated
// dependency sources.
const RAW_STRING_SRC: &str = include_str!("process_raw_string.aipl");
const RAW_STRING_DEDENT_SRC: &str = include_str!("dedent.aipl");
const RAW_STRING_COUNT_WHILE_SRC: &str = include_str!("count_while.aipl");
const RAW_STRING_LINES_SRC: &str = include_str!("lines.aipl");
// `trim_while` is no longer imported by `process_raw_string` (the no-trailing-
// whitespace invariant made it unnecessary), but it's kept as a dogfooded helper
// and still supplied to the engine so it loads alongside the others.
const RAW_STRING_TRIM_WHILE_SRC: &str = include_str!("trim_while.aipl");
const RAW_STRING_TRIM_PREFIX_SRC: &str = include_str!("trim_prefix.aipl");
const RAW_STRING_TRIM_END_WHILE_SRC: &str = include_str!("trim_end_while.aipl");
const RAW_STRING_TRIM_SUFFIX_SRC: &str = include_str!("trim_suffix.aipl");
const PARSE_TEST_SECTION_HEADER_SRC: &str = include_str!("parse_test_section_header.aipl");
const STRIP_TEST_SECTIONS_SRC: &str = include_str!("strip_test_sections.aipl");
const FIND_TRAILING_WHITESPACE_SRC: &str = include_str!("find_trailing_whitespace.aipl");
const ASSERT_LOC_SRC: &str = include_str!("assert_loc.aipl");
const LINE_AT_SRC: &str = include_str!("line_at.aipl");
const CARET_BLOCK_SRC: &str = include_str!("caret_block.aipl");
const FILL_OR_ADD_SECTION_SRC: &str = include_str!("fill_or_add_section.aipl");
const FILL_OR_ADD_SECTION_FILE_SRC: &str = include_str!("fill_or_add_section_file.aipl");
const NORMALIZE_OUTPUT_SRC: &str = include_str!("normalize_output.aipl");
const INT_FITS_SRC: &str = include_str!("int_fits.aipl");
const IS_OPERATOR_NAME_SRC: &str = include_str!("is_operator_name.aipl");

/// Every `.aipl` file the compiler dogfoods, as `(name, source)` in-memory
/// modules — so `from "./..."` imports resolve without disk access. Each file
/// appears exactly once (unlike the old per-function engines, which each
/// separately bundled copies of their shared dependencies — `lines.aipl`,
/// `parse_test_section_header.aipl`, etc.). This is the single source of
/// truth shared by [`DOGFOOD_ENGINE`] (re-linked from the checked-in
/// [`DOGFOOD_CLIF`]), the author helper that regenerates it, and the test that
/// verifies it's current.
pub const DOGFOOD_SOURCES: &[(&str, &str)] = &[
    ("./process_raw_string.aipl", RAW_STRING_SRC),
    ("./dedent.aipl", RAW_STRING_DEDENT_SRC),
    ("./count_while.aipl", RAW_STRING_COUNT_WHILE_SRC),
    ("./lines.aipl", RAW_STRING_LINES_SRC),
    ("./trim_while.aipl", RAW_STRING_TRIM_WHILE_SRC),
    ("./trim_prefix.aipl", RAW_STRING_TRIM_PREFIX_SRC),
    ("./trim_end_while.aipl", RAW_STRING_TRIM_END_WHILE_SRC),
    ("./trim_suffix.aipl", RAW_STRING_TRIM_SUFFIX_SRC),
    (
        "./parse_test_section_header.aipl",
        PARSE_TEST_SECTION_HEADER_SRC,
    ),
    ("./strip_test_sections.aipl", STRIP_TEST_SECTIONS_SRC),
    (
        "./find_trailing_whitespace.aipl",
        FIND_TRAILING_WHITESPACE_SRC,
    ),
    ("./assert_loc.aipl", ASSERT_LOC_SRC),
    ("./line_at.aipl", LINE_AT_SRC),
    ("./caret_block.aipl", CARET_BLOCK_SRC),
    ("./fill_or_add_section.aipl", FILL_OR_ADD_SECTION_SRC),
    (
        "./fill_or_add_section_file.aipl",
        FILL_OR_ADD_SECTION_FILE_SRC,
    ),
    ("./normalize_output.aipl", NORMALIZE_OUTPUT_SRC),
    ("./int_fits.aipl", INT_FITS_SRC),
    ("./is_operator_name.aipl", IS_OPERATOR_NAME_SRC),
];

/// The functions Rust calls via the FFI (need `; entry` metadata in the
/// checked-in artifact) — the real name each is declared with in its own file
/// under [`DOGFOOD_SOURCES`]. Resolved through mangling by
/// [`resolve_dogfood_entry`], so a function need not live in the root file to
/// be listed here.
pub const DOGFOOD_ENTRIES: &[&str] = &[
    "process_raw_string",
    "parse_test_section_header",
    "strip_test_sections",
    "find_trailing_whitespace",
    "assert_loc",
    "line_at",
    "caret_block",
    "fill_or_add_section",
    "fill_or_add_section_file",
    "normalize_output",
    "int_fits",
    "is_operator_name",
];

/// The checked-in dogfood IR for the whole of [`DOGFOOD_SOURCES`]/
/// [`DOGFOOD_ENTRIES`], re-linked at runtime instead of recompiling from
/// source. Kept current by the `dogfood_ir` test (regenerate with
/// `fill_dogfood_ir`).
pub const DOGFOOD_CLIF: &str = include_str!("dogfood.clif");
/// The checked-in artifact's filename, for the `dogfood_ir` test.
pub const DOGFOOD_CLIF_FILE: &str = "dogfood.clif";

thread_local! {
    /// The one dogfood engine, re-linked from the checked-in IR lazily on first
    /// use per thread. A `Compilation` isn't `Sync`, hence one per thread.
    /// Re-linking runs no AIPL frontend, so it works even when the frontend
    /// can't currently compile the dogfooded sources, and never recurses even
    /// though several of these hooks are themselves invoked from the parser.
    static DOGFOOD_ENGINE: Compilation = Compilation::from_artifact(DOGFOOD_CLIF)
        .expect("dogfood engine builds");
}

/// Process a raw string's verbatim contents `s` (trim the surrounding line breaks
/// and de-dent), computed by the dogfooded AIPL `process_raw_string` via the
/// embedding FFI. This is the parser's raw-string hook (see
/// [`install_parser_hooks`]). No native fallback: it panics if the known-good
/// engine can't be built or called, so a regression is loud rather than silently
/// bypassed.
pub fn process_raw_string(s: &str) -> String {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("process_raw_string", &[FfiValue::Str(s.to_string())]) {
            Ok(FfiValue::Str(out)) => out,
            other => panic!("dogfooded process_raw_string() call: {other:?}"),
        }
    })
}

/// The parser's test-section-header hook (see [`install_parser_hooks`]): whether
/// `line` is a `--- name ---` marker, and its trimmed inner name — computed by
/// the dogfooded AIPL `parse_test_section_header` via the FFI. The AIPL returns
/// `str?` (`none` for a non-marker or empty inner), marshaled as [`FfiValue::Opt`]
/// and mapped to `Option<String>`. No native fallback here (the parser keeps one
/// for the no-hook case); once installed, the known-good IR is authoritative, so
/// this panics if it can't be built or called.
fn parse_test_section_header(line: &str) -> Option<String> {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "parse_test_section_header",
            &[FfiValue::Str(line.to_string())],
        ) {
            Ok(FfiValue::Opt(None)) => None,
            Ok(FfiValue::Opt(Some(inner))) => match *inner {
                FfiValue::Str(name) => Some(name),
                other => {
                    panic!("dogfooded parse_test_section_header(): expected str?, got {other:?}")
                }
            },
            other => panic!("dogfooded parse_test_section_header() call: {other:?}"),
        }
    })
}

/// The parser's strip-test-sections hook (see [`install_parser_hooks`]): the
/// portion of `src` to keep — everything before the first `--- name ---` marker
/// line — computed by the dogfooded AIPL `strip_test_sections` via the FFI (which
/// scans lines and classifies each with the bundled `parse_test_section_header`,
/// all inside the engine — one FFI crossing per parse, not one per line). The
/// returned string is always a byte-prefix of `src`, so the parser re-borrows it
/// as `&src[..kept.len()]`. No native fallback; panics if it can't be built or
/// called.
fn strip_test_sections(src: &str) -> String {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("strip_test_sections", &[FfiValue::Str(src.to_string())]) {
            Ok(FfiValue::Str(kept)) => kept,
            other => panic!("dogfooded strip_test_sections() call: {other:?}"),
        }
    })
}

/// The parser's trailing-whitespace hook (see [`install_parser_hooks`]): the
/// [`Span`] of the first line's trailing space/tab run, or `None` if no line has
/// any — computed by the dogfooded AIPL `find_trailing_whitespace`
/// (`str -> Span?`), marshaled back as an [`FfiValue::Opt`] of a struct via the
/// FFI. `none` maps to `None`, `some(span.clone())` to `Some(span.clone())` — no sentinel. No
/// native fallback; panics if it can't be built or called.
fn find_trailing_whitespace(src: &str) -> Option<Span> {
    // A `Span` struct value (its `start`/`end` fields) as a Rust `Span`.
    fn span_of(fields: &[(String, FfiValue)]) -> Span {
        let field = |k: &str| match fields.iter().find(|(n, _)| n == k) {
            Some((_, FfiValue::Int(v))) => *v as usize,
            other => panic!("dogfooded find_trailing_whitespace() Span.{k}: {other:?}"),
        };
        field("start")..field("end")
    }
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "find_trailing_whitespace",
            &[FfiValue::Str(src.to_string())],
        ) {
            Ok(FfiValue::Opt(None)) => None,
            Ok(FfiValue::Opt(Some(inner))) => match *inner {
                FfiValue::Struct(fields) => Some(span_of(&fields)),
                other => panic!("dogfooded find_trailing_whitespace() some(_): {other:?}"),
            },
            other => panic!("dogfooded find_trailing_whitespace() call: {other:?}"),
        }
    })
}

/// The parser's assert-location hook (see [`install_parser_hooks`]): formats an
/// assertion's source location as `input:LINE: TEXT` (1-based line, the
/// condition's trimmed source text) — computed by the dogfooded AIPL
/// `assert_loc` via the FFI, with `span` marshaled as an [`FfiValue::Struct`] of
/// its `start`/`end` fields (mirroring [`caret_block`]). No native fallback;
/// panics if it can't be built or called.
fn assert_loc(source: &str, span: Span) -> String {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "assert_loc",
            &[
                FfiValue::Str(source.to_string()),
                FfiValue::Struct(vec![
                    ("start".to_string(), FfiValue::Int(span.start as i64)),
                    ("end".to_string(), FfiValue::Int(span.end as i64)),
                ]),
            ],
        ) {
            Ok(FfiValue::Str(s)) => s,
            other => panic!("dogfooded assert_loc() call: {other:?}"),
        }
    })
}

/// The error renderer's caret-block hook (see [`install_parser_hooks`]): given
/// `source`, a `span` (half-open byte range), and a `filename`, returns the
/// rustc-style location + underline block — computed by the dogfooded AIPL
/// `caret_block` via the FFI. The AIPL calls `line_at` in-engine. No native
/// fallback; panics if it can't be built or called.
fn caret_block(source: &str, span: Span, filename: &str) -> String {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "caret_block",
            &[
                FfiValue::Str(source.to_string()),
                FfiValue::Struct(vec![
                    ("start".to_string(), FfiValue::Int(span.start as i64)),
                    ("end".to_string(), FfiValue::Int(span.end as i64)),
                ]),
                FfiValue::Str(filename.to_string()),
            ],
        ) {
            Ok(FfiValue::Str(s)) => s,
            other => panic!("dogfooded caret_block() call: {other:?}"),
        }
    })
}

/// Reads the file at `path`, splices `body` into (or appends) its
/// `--- section ---` block via the dogfooded AIPL `fill_or_add_section`, and
/// writes the result back to `path` — computed by the dogfooded AIPL
/// `fill_or_add_section_file` via the FFI (itself doing the file I/O; nothing
/// here touches `std::fs`). Not a parser hook — only the cases test harness
/// calls this. Returns `Ok(())` on success or the builtin `Error`'s message on
/// a read/write failure — the AIPL function returns `!Error` directly, marshaled
/// through `FfiValue::Res`. No native fallback; panics if it can't be built or
/// called.
pub fn fill_or_add_section_file(path: &str, section: &str, body: &str) -> Result<(), String> {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "fill_or_add_section_file",
            &[
                FfiValue::Str(path.to_string()),
                FfiValue::Str(section.to_string()),
                FfiValue::Str(body.to_string()),
            ],
        ) {
            Ok(FfiValue::Res(Ok(_))) => Ok(()),
            Ok(FfiValue::Res(Err(e))) => match *e {
                FfiValue::Str(s) => Err(s),
                other => panic!("dogfooded fill_or_add_section_file() err payload: {other:?}"),
            },
            other => panic!("dogfooded fill_or_add_section_file() call: {other:?}"),
        }
    })
}

/// Normalizes a child program's captured output for the cases test harness:
/// collapses CRLF to LF, then strips the trailing run of `\n`/`\r` — computed by
/// the dogfooded AIPL `normalize_output` via the FFI. Not a parser hook; only the
/// cases harness calls this. No native fallback; panics if it can't be built or
/// called.
pub fn normalize_output(s: &str) -> String {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("normalize_output", &[FfiValue::Str(s.to_string())]) {
            Ok(FfiValue::Str(out)) => out,
            other => panic!("dogfooded normalize_output() call: {other:?}"),
        }
    })
}

/// The checker's flexible-literal range check (see
/// [`aipl_syntax::set_int_fits_hook`]): whether integer literal `v` is
/// representable in the integer type named `name` — computed by the dogfooded
/// AIPL `int_fits` via the FFI. The `bool` result rides back on the shared `i64`
/// ABI as `Int(0|1)`. No native fallback; panics if it can't be built or called.
fn int_fits(v: i64, name: &str) -> bool {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "int_fits",
            &[FfiValue::Int(v), FfiValue::Str(name.to_string())],
        ) {
            Ok(FfiValue::Int(b)) => b != 0,
            other => panic!("dogfooded int_fits() call: {other:?}"),
        }
    })
}

/// The loader's operator-import gate (see
/// [`aipl_syntax::set_is_operator_name_hook`]): whether `s` spells a built-in
/// operator — computed by the dogfooded AIPL `is_operator_name` via the FFI. The
/// `bool` result rides back on the shared `i64` ABI as `Int(0|1)`. No native
/// fallback; panics if it can't be built or called.
fn is_operator_name(s: &str) -> bool {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("is_operator_name", &[FfiValue::Str(s.to_string())]) {
            Ok(FfiValue::Int(b)) => b != 0,
            other => panic!("dogfooded is_operator_name() call: {other:?}"),
        }
    })
}

/// Point the parser's hooks at the dogfooded AIPL implementations: the raw-string
/// processor at [`process_raw_string`], the test-section-header parser at
/// [`parse_test_section_header`], the section stripper at [`strip_test_sections`],
/// the trailing-whitespace finder at [`find_trailing_whitespace`], the
/// assertion-location formatter at [`assert_loc`], the error-renderer's
/// caret-block formatter at [`caret_block`], the checker's flexible-literal
/// range check at [`int_fits`], and the loader's operator-import gate at
/// [`is_operator_name`]. Idempotent (first install wins). The compiler's entry
/// points (the CLI and the embedding [`Compilation`] API's callers) install them;
/// there are **no native fallbacks**, so any in-process parse (or error render,
/// literal range-check, or operator-import resolution) must install them first.
pub fn install_parser_hooks() {
    aipl_parser::set_process_raw_string_hook(process_raw_string);
    aipl_parser::set_test_section_header_hook(parse_test_section_header);
    aipl_parser::set_strip_test_sections_hook(strip_test_sections);
    aipl_parser::set_find_trailing_whitespace_hook(find_trailing_whitespace);
    aipl_parser::set_assert_loc_hook(assert_loc);
    aipl_syntax::set_caret_block_hook(caret_block);
    aipl_syntax::set_int_fits_hook(int_fits);
    aipl_syntax::set_is_operator_name_hook(is_operator_name);
}

/// Compile every function in `program` into `module`. When `main_export_name`
/// is set, the user's `main` function is exported under that name instead
/// (used by the binary-builder path so a C-style `main()` wrapper in the
/// runtime can call it).
fn compile_program<M: Module>(
    module: &mut M,
    program: &Program,
    main_export_name: Option<&str>,
    dbg: DebugOptions,
    // When set, instrument each function to tally executed instructions (the
    // `instructions executed` perf counter). Off for JIT and production binary
    // builds — it adds a per-block call — and on only for the test harness's
    // separate measurement object.
    instrument: bool,
) -> Result<(HashMap<String, FuncInfo>, HashMap<String, TypeDef>, String), Error> {
    // Builtin *types* (e.g. `__builtin_Span`) are real struct declarations,
    // unlike builtin functions: a call to a builtin function is intercepted by
    // reserved name in codegen and never needs to reach monomorphization as a
    // real item, but a builtin-typed value is constructed/laid out/passed
    // around exactly like a user struct, so it must flow through mono and
    // codegen as one. Prepend them to the actual compiled program (not just
    // the checker-only view below) so `build_struct_layouts` and mono's own
    // struct table see them regardless of which file (if any) imports them.
    let program = &Program {
        items: builtin_struct_decls()
            .into_iter()
            .map(Item::Struct)
            .chain(program.items.iter().cloned())
            .collect(),
    };

    // Standalone type-check over the (non-monomorphized) source: validates
    // every function in isolation, so errors are reported independent of which
    // instances get emitted. Runs before monomorphization.
    //
    // Builtins are handed to the checker as ordinary function *declarations*
    // (signatures with a trivial reference body) merged ahead of the user's
    // items: the checker resolves a call to `map`/`filter`/`value_or`/`print`/…
    // through these signatures exactly as it would a user function, with no
    // notion that they're builtin. They're for the checker only — monomorphization
    // and codegen lower the real implementations.
    // Lower tuple type annotations to named synthetic structs before checking
    // and monomorphization so the rest of the pipeline only sees named types.
    let program = &aipl_mono::lower_tuples(program);

    // Rewrite payload-carrying variant constructors used as function values
    // (`xs.map(Circle)`) into equivalent lambdas, so the checker and mono only
    // ever see the ordinary lambda form.
    let program = &aipl_mono::lower_ctor_refs(program);

    // Builtin signatures may contain tuple types (e.g. `enumerate`'s `(i64, T)[]`
    // return), so lower them the same way the user's program was lowered. The
    // resulting synthetic struct definitions are prepended; the checker overwrites
    // on duplicate names, which is fine since identical structs are always produced
    // (this also re-adds the builtin struct decls spliced in above, redundantly but
    // harmlessly — the checker's struct map is a plain overwrite-on-insert, not an
    // error, on a duplicate name).
    let lowered_builtins = aipl_mono::lower_tuples(&Program {
        items: builtin_decls(),
    });
    let check_program = Program {
        items: lowered_builtins
            .items
            .into_iter()
            .chain(program.items.iter().cloned())
            .collect(),
    };
    aipl_mono::check(&check_program)?;

    // Optimization: inline single-use private functions (a no-op unless the
    // program has a `main` — see `inline_single_use`). Runs on the checked source
    // before monomorphization; mono's reachability then drops the inlined-away
    // definitions.
    let inlined = aipl_mono::inline_single_use(program);

    // Optimization: fold constant subexpressions (`2 + 3` → `5`). Runs after
    // `check` so diagnostics always report against the unfolded source, and
    // after inlining so bodies folded here are the ones actually emitted.
    let folded = aipl_mono::fold_constants(&inlined);

    // Resolve generic `any[]` functions into concrete instances first, so the
    // rest of codegen only ever sees concrete types.
    let monomorphized = aipl_mono::monomorphize(&folded, dbg)?;
    // A second inlining pass now that monomorphization has lifted each lambda to
    // its own function and split higher-order callees into per-lambda
    // specializations: those lifted lambdas (and any other now-single-use
    // instance) are each called from exactly one site, so fold them back in. Only
    // possible post-mono — the lambdas don't exist until mono creates them.
    let program = &aipl_mono::inline_single_use_post_mono(&monomorphized);

    let mut ctx = module.make_context();
    let mut fbc = FunctionBuilderContext::new();
    let mut funcs: HashMap<String, FuncInfo> = HashMap::new();
    let mut ir = String::new();

    let structs = build_struct_layouts(program)?;

    // Register builtin signatures/types so user code can `print("...")`. The
    // actual `aipl_*` imports are declared lazily on first use (see `Builtins`).
    let builtins = register_builtins(&mut funcs);

    // Monomorphization has already split each function into the instances the
    // program reaches — borrow and owned forms alike, each its own `ConcreteFn`
    // with `owned_params` set. Codegen just declares and defines each one.
    let mut decls: Vec<(FuncId, &aipl_mono::ConcreteFn)> = Vec::new();
    for f in &program.fns {
        // Signature/effect/mutating validity is checked up front by
        // `aipl_mono::check`; codegen trusts it and goes straight to lowering.

        let mut sig = module.make_signature();
        build_signature(&mut sig, f, &structs);

        let export_name = match main_export_name {
            Some(rename) if f.name == "main" => rename,
            _ => &f.name,
        };
        let id = module
            .declare_function(export_name, Linkage::Export, &sig)
            .map_err(|e| Error::msg(format!("declare {}: {e}", f.name)))?;
        dbg.trace("codegen", format_args!("declare `{}`", f.name));
        let info = FuncInfo {
            link: FuncLink::User(id),
            params: f.params.iter().map(|p| p.ty.clone()).collect(),
            return_ty: abi_return_ty(f),
            effects: f.effects.clone(),
            is_mutating: is_mutating_fn(f),
            // Parameters this instance takes ownership of: a call site moves the
            // (fresh) argument in instead of retaining it.
            owned_params: f
                .params
                .iter()
                .enumerate()
                .filter(|(_, p)| p.owned)
                .map(|(i, _)| i)
                .collect(),
        };
        funcs.insert(f.name.clone(), info);
        decls.push((id, f));
    }

    // One counter across all functions so synthesized literal names are unique.
    let lit_ctr = Cell::new(0u32);
    // Per-element-type array drop/retain helpers, generated on demand while
    // compiling and defined afterward (below).
    let elem_rc = RefCell::new(ElemRc::default());
    for (id, f) in decls {
        dbg.trace("codegen", format_args!("define `{}`", f.name));
        define_fn(
            module, &mut ctx, &mut fbc, id, f, &funcs, &structs, &builtins, &lit_ctr, &elem_rc,
            &mut ir, instrument,
        )?;
    }

    // Define the array element drop/retain helpers requested above (the build
    // context is free now). New ones can't be requested here — element types are
    // only encountered while compiling function bodies — so a single drain.
    let pending = std::mem::take(&mut elem_rc.borrow_mut().pending);
    for (elem, drop_id, retain_id) in pending {
        define_elem_rc_fn(
            module,
            &mut ctx,
            &mut fbc,
            &builtins,
            &structs,
            drop_id,
            &elem,
            RcOp::Drop,
            &mut ir,
        )?;
        define_elem_rc_fn(
            module,
            &mut ctx,
            &mut fbc,
            &builtins,
            &structs,
            retain_id,
            &elem,
            RcOp::Retain,
            &mut ir,
        )?;
    }
    // Dict pair drop/retain helpers. A pair helper's body only inc/decs and
    // recurses structurally (never requesting another generated helper — dicts
    // can't be array/dict elements), so a single drain after the element fns is
    // enough.
    let pair_pending = std::mem::take(&mut elem_rc.borrow_mut().pair_pending);
    for (k, v, drop_id, retain_id) in pair_pending {
        define_pair_rc_fn(
            module,
            &mut ctx,
            &mut fbc,
            &builtins,
            &structs,
            drop_id,
            &k,
            &v,
            RcOp::Drop,
            &mut ir,
        )?;
        define_pair_rc_fn(
            module,
            &mut ctx,
            &mut fbc,
            &builtins,
            &structs,
            retain_id,
            &k,
            &v,
            RcOp::Retain,
            &mut ir,
        )?;
    }

    // Per-type `to_str` rendering helpers. A helper's body renders structurally
    // *inline* (its nested types render in the same function, never calling out
    // to another `to_str` helper) and never inc/decs, so defining one requests no
    // further helpers — a single drain suffices.
    let tostr_pending = std::mem::take(&mut elem_rc.borrow_mut().tostr_pending);
    for (ty, id) in tostr_pending {
        define_tostr_fn(
            module, &mut ctx, &mut fbc, &funcs, &structs, &builtins, &lit_ctr, &elem_rc, id, &ty,
            &mut ir, instrument,
        )?;
    }

    let ir = annotate_ir(&ir, module);
    Ok((funcs, structs, ir))
}

/// Annotate printed CLIF with each function's source-level name: a comment line
/// above every `function u0:<id>` header, and a trailing comment on every
/// `fnN = ... u0:<id> ...` reference line in a preamble. The names come from the
/// module's declarations (so they match the AIPL source — `add`, `dedent`,
/// `aipl_concat`, the synthesized `__to_str_*`, …). Purely cosmetic: cranelift's
/// reader ignores `;` comments, so the dogfood-IR loader still reads the numeric
/// ids — this only makes `aipl ir` and the checked-in `.clif` artifacts legible.
fn annotate_ir<M: Module>(ir: &str, module: &M) -> String {
    let names: HashMap<u32, String> = module
        .declarations()
        .get_functions()
        .filter_map(|(id, d)| d.name.clone().map(|n| (id.as_u32(), n)))
        .collect();
    let name_of = |line: &str| u0_ref_id(line).and_then(|id| names.get(&id)).cloned();

    let mut out = String::with_capacity(ir.len() + ir.len() / 8);
    for line in ir.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("function ") {
            // Header `function u0:<id>(...)` → name it on the line above.
            if let Some(name) = name_of(line) {
                out.push_str("; ");
                out.push_str(&name);
                out.push('\n');
            }
            out.push_str(line);
        } else if trimmed.starts_with("fn") && line.contains(" = ") && line.contains(" u0:") {
            // Preamble ref `fnN = [colocated] u0:<id> sigK` → name it inline.
            out.push_str(line);
            if let Some(name) = name_of(line) {
                out.push_str("  ; ");
                out.push_str(&name);
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The `<id>` of the first `u0:<id>` reference in `line`, if any.
fn u0_ref_id(line: &str) -> Option<u32> {
    let rest = line.split("u0:").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn host_isa() -> Result<std::sync::Arc<dyn TargetIsa>, Error> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("use_colocated_libcalls", "false")
        .map_err(|e| Error::msg(format!("flag: {e}")))?;
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| Error::msg(format!("flag: {e}")))?;
    let isa_builder = cranelift_native::builder()
        .map_err(|msg| Error::msg(format!("host machine not supported: {msg}")))?;
    isa_builder
        .finish(settings::Flags::new(flag_builder))
        .map_err(|e| Error::msg(format!("isa: {e}")))
}

/// Build a fresh `JITModule` with every `aipl_*` runtime builtin registered as
/// a linkable symbol. Shared by [`Compilation::new`] (compiling AIPL source) and
/// [`Compilation::from_artifact`] (re-linking checked-in dogfood IR) so both see
/// the identical symbol table.
fn new_jit_module() -> Result<JITModule, Error> {
    let isa = host_isa()?;
    let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    // Expose builtins to the JIT linker.
    jit_builder.symbol("aipl_print", aipl_print as *const u8);
    jit_builder.symbol("aipl_print_error", aipl_print_error as *const u8);
    jit_builder.symbol("aipl_concat", aipl_concat as *const u8);
    jit_builder.symbol("aipl_concat_lazy", aipl_concat_lazy as *const u8);
    jit_builder.symbol("aipl_concat_mut", aipl_concat_mut as *const u8);
    jit_builder.symbol("aipl_char_at", aipl_char_at as *const u8);
    jit_builder.symbol(
        "aipl_str_is_all_whitespace",
        aipl_str_is_all_whitespace as *const u8,
    );
    jit_builder.symbol("aipl_str_eq", aipl_str_eq as *const u8);
    jit_builder.symbol("aipl_str_starts_with", aipl_str_starts_with as *const u8);
    jit_builder.symbol("aipl_str_contains", aipl_str_contains as *const u8);
    jit_builder.symbol("aipl_str_ends_with", aipl_str_ends_with as *const u8);
    jit_builder.symbol(
        "aipl_read_file_to_string",
        aipl_read_file_to_string as *const u8,
    );
    jit_builder.symbol(
        "aipl_write_string_to_file",
        aipl_write_string_to_file as *const u8,
    );
    jit_builder.symbol("aipl_execute_program", aipl_execute_program as *const u8);
    jit_builder.symbol("aipl_trim", aipl_trim as *const u8);
    jit_builder.symbol("aipl_trim_mut", aipl_trim_mut as *const u8);
    jit_builder.symbol("aipl_str_repeat", aipl_str_repeat as *const u8);
    jit_builder.symbol("aipl_str_reverse", aipl_str_reverse as *const u8);
    jit_builder.symbol("aipl_arr_reverse", aipl_arr_reverse as *const u8);
    jit_builder.symbol("aipl_str_slice", aipl_str_slice as *const u8);
    jit_builder.symbol("aipl_arr_slice", aipl_arr_slice as *const u8);
    jit_builder.symbol("aipl_str_split", aipl_str_split as *const u8);
    jit_builder.symbol("aipl_str_join", aipl_str_join as *const u8);
    jit_builder.symbol("aipl_str_data", aipl_str_data as *const u8);
    jit_builder.symbol("aipl_str_iter_init", aipl_str_iter_init as *const u8);
    jit_builder.symbol("aipl_str_iter_next", aipl_str_iter_next as *const u8);
    jit_builder.symbol("aipl_inc", aipl_inc as *const u8);
    jit_builder.symbol("aipl_dec", aipl_dec as *const u8);
    jit_builder.symbol("aipl_array_new", aipl_array_new as *const u8);
    jit_builder.symbol("aipl_array_with_cap", aipl_array_with_cap as *const u8);
    jit_builder.symbol("aipl_array_push", aipl_array_push as *const u8);
    jit_builder.symbol("aipl_array_push_mut", aipl_array_push_mut as *const u8);
    jit_builder.symbol("aipl_array_dec", aipl_array_dec as *const u8);
    jit_builder.symbol("aipl_arr_inc", aipl_arr_inc as *const u8);
    jit_builder.symbol("aipl_arr_elem_ptr", aipl_arr_elem_ptr as *const u8);
    jit_builder.symbol("aipl_arr_load_bit", aipl_arr_load_bit as *const u8);
    jit_builder.symbol("aipl_set_contains", aipl_set_contains as *const u8);
    jit_builder.symbol("aipl_set_insert", aipl_set_insert as *const u8);
    jit_builder.symbol("aipl_set_union", aipl_set_union as *const u8);
    jit_builder.symbol("aipl_set_union_mut", aipl_set_union_mut as *const u8);
    jit_builder.symbol("aipl_dict_insert", aipl_dict_insert as *const u8);
    jit_builder.symbol("aipl_dict_get", aipl_dict_get as *const u8);
    jit_builder.symbol(
        "aipl_dict_contains_key",
        aipl_dict_contains_key as *const u8,
    );
    jit_builder.symbol("aipl_count_insns", aipl_count_insns as *const u8);
    jit_builder.symbol("aipl_str_hash", aipl_str_hash as *const u8);
    jit_builder.symbol("aipl_assert", aipl_assert as *const u8);
    jit_builder.symbol("aipl_test_begin", aipl_test_begin as *const u8);
    jit_builder.symbol("aipl_test_end", aipl_test_end as *const u8);
    jit_builder.symbol("aipl_test_summary", aipl_test_summary as *const u8);
    jit_builder.symbol("aipl_arr_drop_str", aipl_arr_drop_str as *const u8);
    jit_builder.symbol("aipl_arr_drop_arr", aipl_arr_drop_arr as *const u8);
    jit_builder.symbol("aipl_arr_retain_ptr", aipl_arr_retain_ptr as *const u8);
    jit_builder.symbol("aipl_arr_drop_opt_str", aipl_arr_drop_opt_str as *const u8);
    jit_builder.symbol("aipl_arr_drop_opt_arr", aipl_arr_drop_opt_arr as *const u8);
    jit_builder.symbol("aipl_arr_retain_opt", aipl_arr_retain_opt as *const u8);
    jit_builder.symbol("aipl_str_alloc", aipl_str_alloc as *const u8);
    jit_builder.symbol("aipl_i64_len", aipl_i64_len as *const u8);
    jit_builder.symbol("aipl_write_i64", aipl_write_i64 as *const u8);
    jit_builder.symbol("aipl_u64_len", aipl_u64_len as *const u8);
    jit_builder.symbol("aipl_write_u64", aipl_write_u64 as *const u8);
    jit_builder.symbol("aipl_str_len", aipl_str_len as *const u8);
    jit_builder.symbol("aipl_write_bytes", aipl_write_bytes as *const u8);
    Ok(JITModule::new(jit_builder))
}

/// The dogfood-IR tag for an FFI-marshalable entry type. The FFI marshals
/// scalars, `str`, `Unit` (the empty-payload side of a `Result`), optionals of
/// those (a trailing `?` per `Optional` layer, e.g. `str?`), results of those
/// (`{ok}!{err}`, e.g. `unit!Error`), and structs (the bare type name, e.g.
/// `Span`, whose layout is carried separately on a `; struct` manifest line).
/// Anything else can't cross the FFI and is rejected here.
fn ffi_type_tag(t: &Type) -> Result<String, Error> {
    Ok(match t {
        Type::Primitive(Primitive::I64) => "i64".to_string(),
        Type::Primitive(Primitive::Bool) => "bool".to_string(),
        Type::Primitive(Primitive::Char) => "char".to_string(),
        Type::Primitive(Primitive::Str) => "str".to_string(),
        Type::Unit => "unit".to_string(),
        Type::Optional(inner) => format!("{}?", ffi_type_tag(inner)?),
        Type::Result(ok, err) => format!("{}!{}", ffi_type_tag(ok)?, ffi_type_tag(err)?),
        Type::Named(n) => n.clone(),
        _ => {
            return Err(Error::msg(format!(
                "dogfood entry type {} is not FFI-serializable (only i64/bool/char/str, \
                 optionals/results of those, and structs)",
                type_name(t)
            )))
        }
    })
}

/// Collect the distinct struct type names a type references (itself if
/// `Named` and not the builtin `Error` — which is str-repr, not a struct —,
/// or the core of an `Optional`/either side of a `Result`), appending any not
/// already in `out`. Used to gather the struct layouts a set of dogfood
/// entries needs serialized.
fn collect_named_types(t: &Type, out: &mut Vec<String>) {
    match t {
        Type::Named(n) if !is_error(t) => {
            if !out.iter().any(|s| s == n) {
                out.push(n.clone());
            }
        }
        Type::Optional(inner) => collect_named_types(inner, out),
        Type::Result(ok, err) => {
            collect_named_types(ok, out);
            collect_named_types(err, out);
        }
        _ => {}
    }
}

/// Inverse of [`ffi_type_tag`]: parse a manifest type tag back into a `Type`. A
/// trailing `?` is an `Optional` layer over the rest; an unsuffixed tag
/// containing `!` is a `Result` (`{ok}!{err}`, each side parsed the same way —
/// `!` can't appear in a bare tag otherwise, since identifiers don't carry it);
/// a non-keyword tag is a struct type name ([`Type::Named`]) whose layout the
/// `; struct` lines supply.
fn ffi_type_from_tag(tag: &str) -> Result<Type, Error> {
    if let Some(base) = tag.strip_suffix('?') {
        return Ok(Type::Optional(Box::new(ffi_type_from_tag(base)?)));
    }
    if let Some((ok, err)) = tag.split_once('!') {
        return Ok(Type::Result(
            Box::new(ffi_type_from_tag(ok)?),
            Box::new(ffi_type_from_tag(err)?),
        ));
    }
    Ok(match tag {
        "i64" => Type::Primitive(Primitive::I64),
        "bool" => Type::Primitive(Primitive::Bool),
        "char" => Type::Primitive(Primitive::Char),
        "str" => Type::Primitive(Primitive::Str),
        "unit" => Type::Unit,
        _ => Type::Named(tag.to_string()),
    })
}

/// Resolve a dogfood `entries` name (e.g. `"assert_loc"`) to its compiled
/// [`FuncInfo`] in `funcs`, regardless of which file in `sources` declared it.
/// `aipl_loader::load_program_sources` treats `sources`' first file as root and
/// leaves its top-level names unmangled, but renames every other file's to
/// `__m<index>__<name>` — so a match is either an exact hit (the declaring
/// file was root) or the single compiled name that
/// [`aipl_loader::unmangled_name`] recovers `name` from. This is what lets any
/// dogfooded function serve as an FFI entry no matter which file declares it,
/// with no aggregator/re-export file required. Errors if `name` isn't declared
/// `pub` anywhere, or is declared in more than one file (ambiguous).
fn resolve_dogfood_entry<'a>(
    funcs: &'a HashMap<String, FuncInfo>,
    name: &str,
) -> Result<&'a FuncInfo, Error> {
    if let Some(info) = funcs.get(name) {
        return Ok(info);
    }
    let mut matches = funcs
        .iter()
        .filter(|(compiled, _)| aipl_loader::unmangled_name(compiled) == name);
    let (found_name, info) = matches.next().ok_or_else(|| {
        Error::msg(format!(
            "dogfood entry {name:?} not found in the compilation"
        ))
    })?;
    if let Some((other, _)) = matches.next() {
        return Err(Error::msg(format!(
            "dogfood entry {name:?} is ambiguous: both {found_name:?} and {other:?} compiled to it"
        )));
    }
    Ok(info)
}

/// Rewrite every `u0:<id>` function/import reference in CLIF text `ir` through
/// `remap`, in a single left-to-right pass (so no id is remapped twice). Only the
/// function namespace (`u0:`) is touched — data references (`u1:`) and all other
/// text are copied verbatim. Panics on a referenced id missing from `remap`,
/// which would mean the map is incomplete (a serialize bug).
fn remap_func_ids(ir: &str, remap: &HashMap<u32, u32>) -> String {
    let mut out = String::with_capacity(ir.len());
    let mut rest = ir;
    while let Some(pos) = rest.find("u0:") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 3..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            // "u0:" not followed by digits — copy the marker and move past it.
            out.push_str("u0:");
            rest = after;
            continue;
        }
        let old: u32 = digits.parse().expect("u0: id fits u32");
        let new = remap
            .get(&old)
            .unwrap_or_else(|| panic!("dogfood id remap missing u0:{old}"));
        out.push_str(&format!("u0:{new}"));
        rest = &after[digits.len()..];
    }
    out.push_str(rest);
    out
}

/// Compile the dogfooded AIPL `sources` through the live frontend and serialize
/// the result as a checked-in dogfood-IR artifact: CLIF text with a `;`-comment
/// manifest header recording the builtin-import id↔symbol table and the FFI
/// metadata of each callable `entries` function. The inverse is
/// [`Compilation::from_artifact`]. Used by the `fill_dogfood_ir` author helper to
/// (re)generate `dogfood.clif` after a frontend change, and by the verify test
/// to confirm the checked-in artifact is up to date.
///
/// Data symbols (string literals) aren't round-tripped yet; none of the current
/// dogfooded functions produce any, and generation errors loudly if one ever
/// does, rather than silently emitting an artifact that won't link.
pub fn generate_dogfood_artifact(
    sources: &[(&str, &str)],
    entries: &[&str],
) -> Result<String, Error> {
    let dbg = DebugOptions::new(false);
    let program = aipl_loader::load_program_sources(sources, dbg).unwrap();
    let mut module = new_jit_module().unwrap();
    let (funcs, structs, ir) = compile_program(&mut module, &program, None, dbg, false).unwrap();

    // Collect static data objects (e.g. string literals longer than the 7-byte
    // inline SSO threshold). Finalize the JIT module only when any exist so we
    // can read back the raw bytes; functions without data skip the finalize.
    let mut data_entries: Vec<(u32, String, Vec<u8>)> = {
        let ids: Vec<(u32, String)> = module
            .declarations()
            .get_data_objects()
            .map(|(id, decl)| {
                (
                    id.as_u32(),
                    decl.name
                        .clone()
                        .expect("all dogfood data objects carry their symbol name"),
                )
            })
            .collect();
        if ids.is_empty() {
            Vec::new()
        } else {
            module
                .finalize_definitions()
                .map_err(|e| Error::msg(format!("finalize for data collection: {e}")))?;
            ids.into_iter()
                .map(|(id, name)| {
                    let (ptr, len) = module.get_finalized_data(DataId::from_u32(id));
                    // SAFETY: JIT memory is valid for `len` bytes until `module` is dropped.
                    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
                    (id, name, bytes)
                })
                .collect()
        }
    };
    data_entries.sort_by_key(|(id, _, _)| *id);

    // Builtin imports (id -> symbol), recovered from the module's declarations.
    let mut imports: Vec<(u32, String)> = module
        .declarations()
        .get_functions()
        .filter(|(_, d)| d.linkage == Linkage::Import)
        .map(|(id, d)| {
            (
                id.as_u32(),
                d.name
                    .clone()
                    .expect("an import declaration carries its symbol name"),
            )
        })
        .collect();
    imports.sort_by_key(|(id, _)| *id);

    // Canonical id layout for a stable checked-in artifact: number the builtin
    // imports first (`0..K`), then the user-defined functions (`K..`). Because the
    // two regions no longer interleave, adding a dogfooded function only appends
    // user ids and never renumbers the imports — so a new function is a localized
    // diff instead of shifting every `u0:<import>` reference in the file. Imports
    // are ordered by symbol name (not first-use order, which is itself codegen-
    // order-dependent), so the import region is stable on its own; user functions
    // keep their existing relative order, offset by `K`. No gap between the regions,
    // so ids stay dense (`from_artifact` declares `0..=max_id`) — adding an import
    // still shifts the user block, but that is the rare case.
    let remap: HashMap<u32, u32> = {
        let mut imports_by_name = imports.clone();
        imports_by_name.sort_by(|a, b| a.1.cmp(&b.1));
        let mut user_ids: Vec<u32> = module
            .declarations()
            .get_functions()
            .filter(|(_, d)| d.linkage != Linkage::Import)
            .map(|(id, _)| id.as_u32())
            .collect();
        user_ids.sort_unstable();
        let mut m = HashMap::new();
        for (new_id, (old_id, _)) in imports_by_name.iter().enumerate() {
            m.insert(*old_id, new_id as u32);
        }
        let k = imports_by_name.len() as u32;
        for (i, old_id) in user_ids.iter().enumerate() {
            m.insert(*old_id, k + i as u32);
        }
        m
    };
    // Re-key the import table (now the leading `0..K` block) and the function
    // bodies' `u0:<id>` references into the canonical numbering. Entry ids are
    // mapped at emission below.
    let mut imports: Vec<(u32, String)> = imports
        .iter()
        .map(|(old, sym)| (remap[old], sym.clone()))
        .collect();
    imports.sort_by_key(|(id, _)| *id);
    let ir = remap_func_ids(&ir, &remap);

    let mut out = String::new();
    out.push_str("; dogfood-ir v1\n");
    out.push_str("; Checked-in Cranelift IR for AIPL the compiler dogfoods (see from_artifact).\n");
    out.push_str("; DO NOT EDIT BY HAND. Regenerate:\n");
    out.push_str(";   cargo test --test dogfood_ir -- --ignored fill_dogfood_ir\n");
    // Struct types any entry references (param or return), so the inverse can
    // rebuild their layouts and marshal a struct return — collected here, emitted
    // as `; struct` lines after the entries.
    let mut referenced_structs: Vec<String> = Vec::new();
    for name in entries {
        let info = resolve_dogfood_entry(&funcs, name)?;
        let id = match info.link {
            FuncLink::User(id) => remap[&id.as_u32()],
            FuncLink::Builtin(_) => {
                return Err(Error::msg(format!(
                    "dogfood entry {name:?} is a builtin, not a fn"
                )))
            }
        };
        if info.is_mutating {
            return Err(Error::msg(format!(
                "dogfood entry {name:?} is a mutating method; not FFI-callable"
            )));
        }
        let params = info
            .params
            .iter()
            .map(ffi_type_tag)
            .collect::<Result<Vec<_>, _>>()?
            .join(" ");
        let sep = if params.is_empty() { "" } else { " " };
        let ret = ffi_type_tag(&info.return_ty)?;
        out.push_str(&format!("; entry {name} {id}{sep}{params} -> {ret}\n"));
        for t in info.params.iter().chain(std::iter::once(&info.return_ty)) {
            collect_named_types(t, &mut referenced_structs);
        }
    }
    for sname in &referenced_structs {
        let layout = structs
            .get(sname)
            .and_then(TypeDef::as_struct)
            .ok_or_else(|| {
                Error::msg(format!("dogfood entry references unknown struct {sname:?}"))
            })?;
        let mut fields = String::new();
        for f in &layout.fields {
            if !is_ffi_scalar(&f.ty) && !is_str_repr(&f.ty) {
                return Err(Error::msg(format!(
                    "dogfood struct {sname} field {:?} is {}; only i64/bool/char or str \
                     struct fields can cross the FFI",
                    f.name,
                    type_name(&f.ty)
                )));
            }
            fields.push_str(&format!(
                " {}@{}:{}",
                f.name,
                f.offset,
                ffi_type_tag(&f.ty)?
            ));
        }
        out.push_str(&format!("; struct {sname} {}{fields}\n", layout.size));
    }
    for (id, sym) in &imports {
        out.push_str(&format!("; import {id} {sym}\n"));
    }
    for (id, name, bytes) in &data_entries {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("; data {id} {name} {hex}\n"));
    }
    out.push('\n');
    out.push_str(&ir);
    Ok(out)
}

impl Compilation {
    pub fn new(program: &Program, dbg: DebugOptions) -> Result<Self, Error> {
        let mut module = new_jit_module()?;

        // The JIT never reports perf counters, so it's never instrumented.
        let (funcs, structs, ir) = compile_program(&mut module, program, None, dbg, false)?;

        module
            .finalize_definitions()
            .map_err(|e| Error::msg(format!("finalize: {e}")))?;

        Ok(Self {
            module,
            funcs,
            structs,
            ir,
        })
    }

    /// Build a `Compilation` by re-linking checked-in dogfood IR (the artifact
    /// produced by [`generate_dogfood_artifact`]) into a fresh `JITModule`,
    /// *without* running the AIPL frontend. This is how the compiler dogfoods
    /// AIPL: even when the frontend can't compile the dogfooded `.aipl` sources
    /// (mid-change), the checked-in CLIF still links and runs.
    ///
    /// The artifact is CLIF text with a `;`-comment manifest header. cranelift's
    /// reader ignores the comments, so [`cranelift_reader::parse_functions`]
    /// recovers the functions while we separately scan the comments for the
    /// builtin-import table and the FFI metadata of the callable entries. Every
    /// function carries its real `FuncId` in its `function u0:<id>` header (see
    /// the `ctx.func.name` tagging in the emit sites), so we declare all ids in
    /// ascending order — defined functions from the parsed signatures, builtin
    /// imports by symbol name — reproducing the exact id↔name mapping the CLIF's
    /// `u0:<id>` references were emitted against, then define each function.
    pub fn from_artifact(text: &str) -> Result<Self, Error> {
        // Parse the manifest carried as `;` comment lines.
        let mut imports: HashMap<u32, String> = HashMap::new();
        let mut entries: Vec<(String, u32, Vec<Type>, Type)> = Vec::new();
        let mut structs: HashMap<String, TypeDef> = HashMap::new();
        let mut data_entries: Vec<(u32, String, Vec<u8>)> = Vec::new();
        for line in text.lines() {
            let Some(body) = line.trim_start().strip_prefix(';') else {
                continue;
            };
            let body = body.trim();
            if let Some(rest) = body.strip_prefix("struct ") {
                // `struct <name> <size> <field>@<offset>:<tag> ...`
                let toks: Vec<&str> = rest.split_whitespace().collect();
                let name = toks
                    .first()
                    .ok_or_else(|| Error::msg("`; struct` line missing name"))?;
                let size: u32 = toks
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::msg("`; struct` line missing/invalid size"))?;
                let mut fields = Vec::new();
                for ft in &toks[2..] {
                    let (fname, rest) = ft
                        .split_once('@')
                        .ok_or_else(|| Error::msg(format!("malformed `; struct` field {ft:?}")))?;
                    let (off, tag) = rest
                        .split_once(':')
                        .ok_or_else(|| Error::msg(format!("malformed `; struct` field {ft:?}")))?;
                    let offset: u32 = off
                        .parse()
                        .map_err(|_| Error::msg(format!("bad `; struct` field offset {ft:?}")))?;
                    fields.push(FieldLayout {
                        name: fname.to_string(),
                        ty: ffi_type_from_tag(tag)?,
                        offset,
                    });
                }
                structs.insert(
                    name.to_string(),
                    TypeDef::Struct(StructLayout { fields, size }),
                );
            } else if let Some(rest) = body.strip_prefix("import ") {
                let mut it = rest.split_whitespace();
                let id: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::msg("malformed `; import` line in dogfood IR"))?;
                let sym = it
                    .next()
                    .ok_or_else(|| Error::msg("`; import` line missing symbol"))?;
                imports.insert(id, sym.to_string());
            } else if let Some(rest) = body.strip_prefix("entry ") {
                let toks: Vec<&str> = rest.split_whitespace().collect();
                let name = toks
                    .first()
                    .ok_or_else(|| Error::msg("`; entry` line missing name"))?;
                let id: u32 = toks
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::msg("`; entry` line missing/invalid id"))?;
                let arrow = toks
                    .iter()
                    .position(|t| *t == "->")
                    .ok_or_else(|| Error::msg("`; entry` line missing `->`"))?;
                let params = toks[2..arrow]
                    .iter()
                    .map(|t| ffi_type_from_tag(t))
                    .collect::<Result<Vec<_>, _>>()?;
                let ret = ffi_type_from_tag(
                    toks.get(arrow + 1)
                        .ok_or_else(|| Error::msg("`; entry` line missing return type"))?,
                )?;
                entries.push((name.to_string(), id, params, ret));
            } else if let Some(rest) = body.strip_prefix("data ") {
                // `data <id> <name> <hex-bytes>` — static data object (string literals).
                let mut it = rest.splitn(3, ' ');
                let id: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::msg("malformed `; data` id in dogfood IR"))?;
                let name = it
                    .next()
                    .ok_or_else(|| Error::msg("`; data` line missing name"))?
                    .to_string();
                let hex = it
                    .next()
                    .ok_or_else(|| Error::msg("`; data` line missing bytes"))?;
                if hex.len() % 2 != 0 {
                    return Err(Error::msg("`; data` hex string has odd length"));
                }
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| {
                        u8::from_str_radix(&hex[i..i + 2], 16)
                            .map_err(|_| Error::msg(format!("`; data` invalid hex at byte {i}")))
                    })
                    .collect::<Result<Vec<u8>, _>>()?;
                data_entries.push((id, name, bytes));
            }
        }

        let parsed = cranelift_reader::parse_functions(text)
            .map_err(|e| Error::msg(format!("parse dogfood IR: {e}")))?;

        // Each parsed function's FuncId is encoded in its `function u0:<id>` header.
        let mut defined: HashMap<u32, usize> = HashMap::new();
        for (i, f) in parsed.iter().enumerate() {
            let id = match &f.name {
                UserFuncName::User(u) if u.namespace == 0 => u.index,
                other => {
                    return Err(Error::msg(format!(
                        "dogfood IR function has unexpected name {other:?}"
                    )))
                }
            };
            if defined.insert(id, i).is_some() {
                return Err(Error::msg(format!(
                    "dogfood IR defines function id {id} twice"
                )));
            }
        }

        let max_id = defined
            .keys()
            .chain(imports.keys())
            .copied()
            .max()
            .ok_or_else(|| Error::msg("dogfood IR has no functions"))?;

        let mut module = new_jit_module()?;
        // Re-declare static data objects in id order so `u1:N` symbol references
        // in the CLIF functions resolve to the right data at finalize time.
        data_entries.sort_by_key(|(id, _, _)| *id);
        for (expected_id, name, bytes) in &data_entries {
            let got = module
                .declare_data(name, Linkage::Local, false, false)
                .map_err(|e| Error::msg(format!("declare dogfood data {name}: {e}")))?;
            if got.as_u32() != *expected_id {
                return Err(Error::msg(format!(
                    "dogfood IR data id mismatch: expected {expected_id}, got {} \
                     (declaration order broke)",
                    got.as_u32()
                )));
            }
            let mut desc = DataDescription::new();
            desc.set_align(8);
            desc.define(bytes.clone().into_boxed_slice());
            module
                .define_data(got, &desc)
                .map_err(|e| Error::msg(format!("define dogfood data {name}: {e}")))?;
        }
        // Declare every id in ascending order so the JIT-assigned FuncIds line up
        // with the `u0:<id>` indices baked into the CLIF.
        let mut ids: Vec<FuncId> = Vec::with_capacity(max_id as usize + 1);
        for id in 0..=max_id {
            let got = match (imports.get(&id), defined.get(&id)) {
                (Some(sym), None) => {
                    let sig = builtin_import_sig(&mut module, sym);
                    module
                        .declare_function(sym, Linkage::Import, &sig)
                        .map_err(|e| Error::msg(format!("declare dogfood import {sym}: {e}")))?
                }
                (None, Some(&i)) => {
                    let sig = parsed[i].signature.clone();
                    module
                        .declare_function(&format!("__dogfood_fn{id}"), Linkage::Local, &sig)
                        .map_err(|e| Error::msg(format!("declare dogfood fn {id}: {e}")))?
                }
                (None, None) => {
                    return Err(Error::msg(format!(
                        "dogfood IR has neither a function nor an import for id {id}"
                    )))
                }
                (Some(_), Some(_)) => {
                    return Err(Error::msg(format!(
                        "dogfood IR id {id} is both a defined function and an import"
                    )))
                }
            };
            if got.as_u32() != id {
                return Err(Error::msg(format!(
                    "dogfood IR id mismatch: expected {id}, declaration produced {} \
                     (declaration order broke)",
                    got.as_u32()
                )));
            }
            ids.push(got);
        }

        // Define each parsed function under its own id.
        let mut ctx = module.make_context();
        for f in &parsed {
            let id = match &f.name {
                UserFuncName::User(u) => u.index,
                _ => unreachable!("validated above"),
            };
            ctx.func = f.clone();
            module
                .define_function(ids[id as usize], &mut ctx)
                .map_err(|e| Error::msg(format!("define dogfood fn {id}: {e:?}")))?;
            module.clear_context(&mut ctx);
        }
        module
            .finalize_definitions()
            .map_err(|e| Error::msg(format!("finalize dogfood IR: {e}")))?;

        // FFI-callable metadata for the declared entries, so `call`/`call_values`
        // work exactly as they do for a source-compiled `Compilation`.
        let mut funcs: HashMap<String, FuncInfo> = HashMap::new();
        for (name, id, params, return_ty) in entries {
            funcs.insert(
                name,
                FuncInfo {
                    link: FuncLink::User(ids[id as usize]),
                    params,
                    return_ty,
                    effects: Vec::new(),
                    is_mutating: false,
                    owned_params: Vec::new(),
                },
            );
        }

        Ok(Self {
            module,
            funcs,
            // Struct layouts recovered from the `; struct` manifest lines, so a
            // struct-returning entry marshals back through `call_values`.
            structs,
            ir: text.to_string(),
        })
    }

    pub fn ir(&self) -> &str {
        &self.ir
    }

    pub fn run_0(&self, name: &str) -> Result<i64, Error> {
        let id = self.lookup(name)?;
        let ptr = self.module.get_finalized_function(id);
        let f: extern "C" fn() -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f())
    }

    pub fn run_1(&self, name: &str, a: i64) -> Result<i64, Error> {
        let id = self.lookup(name)?;
        let ptr = self.module.get_finalized_function(id);
        let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f(a))
    }

    pub fn run_2(&self, name: &str, a: i64, b: i64) -> Result<i64, Error> {
        let id = self.lookup(name)?;
        let ptr = self.module.get_finalized_function(id);
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f(a, b))
    }

    /// Whether `name` takes exactly one `str[]` parameter — i.e. it wants the
    /// CLI arguments. Used by the driver to choose [`run_cli`] over the
    /// integer-argument `run_*` forms.
    pub fn takes_cli_args(&self, name: &str) -> bool {
        self.funcs
            .get(name)
            .is_some_and(|i| i.params.len() == 1 && i.params[0] == cli_args_ty())
    }

    /// Run a function taking a single `str[]`, passing `args` as that array.
    /// The callee owns the array (and its strings) and frees it on return.
    pub fn run_cli(&self, name: &str, args: &[String]) -> Result<i64, Error> {
        let id = self.lookup(name)?;
        let ptr = self.module.get_finalized_function(id);
        let f: extern "C" fn(*const u8) -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f(build_cli_array(args)))
    }

    /// Call an AIPL function by name from Rust (the embedding FFI). `args` and
    /// the result are `i64` — the ABI of every scalar AIPL value (`bool` is
    /// 0/1, `char` is a codepoint). Validates that the function exists, is a
    /// plain (non-mutating) user function, has the matching arity, and that all
    /// parameters and the return are scalar (`i64`/`bool`/`char`) — `str`,
    /// arrays, and other composites aren't marshalable across the FFI yet.
    pub fn call(&self, name: &str, args: &[i64]) -> Result<i64, Error> {
        let info = match self.funcs.get(name) {
            Some(i) if matches!(i.link, FuncLink::User(_)) => i,
            _ => return Err(Error::msg(format!("no callable fn {name:?}"))),
        };
        if info.is_mutating {
            return Err(Error::msg(format!(
                "fn {name:?} is a mutating method; call it on a receiver, not via the FFI"
            )));
        }
        if info.params.len() != args.len() {
            return Err(Error::msg(format!(
                "fn {name:?} expects {} argument(s), got {}",
                info.params.len(),
                args.len()
            )));
        }
        for (i, p) in info.params.iter().enumerate() {
            if !is_ffi_scalar(p) {
                return Err(Error::msg(format!(
                    "fn {name:?} parameter {i} is {}; the FFI supports only i64/bool/char",
                    type_name(p)
                )));
            }
        }
        if !is_ffi_scalar(&info.return_ty) {
            return Err(Error::msg(format!(
                "fn {name:?} returns {}; the FFI supports only i64/bool/char",
                type_name(&info.return_ty)
            )));
        }
        if args.len() > 6 {
            return Err(Error::msg(format!(
                "fn {name:?} has {} parameters; the FFI supports up to 6",
                args.len()
            )));
        }
        let id = self.lookup(name)?;
        let ptr = self.module.get_finalized_function(id);
        // SAFETY: every scalar AIPL param/return lowers to a single `i64`
        // (`cl_type_of`), and we verified arity (<= 6) + scalar types above, so
        // the finalized code matches the transmuted signature.
        Ok(unsafe { invoke(ptr, args) })
    }

    /// Call AIPL function `name` from Rust, marshaling `str` as well as scalars
    /// (see [`FfiValue`]). Each argument's [`FfiValue`] variant must match the
    /// parameter type — `Int` for `i64`/`bool`/`char`, `Str` for `str` — and the
    /// return is marshaled by the function's declared return type. Like [`call`],
    /// it rejects a missing/mutating/wrong-arity function or an unmarshalable
    /// (array/struct/…) parameter or return.
    ///
    /// A `str` argument is *borrowed* for the duration of the call: the host owns
    /// the backing buffer (passed with a static refcount so the callee never
    /// frees it) and releases it on return. Don't write an AIPL function that
    /// stashes a `str` argument somewhere outliving the call.
    ///
    /// [`call`]: Compilation::call
    pub fn call_values(&self, name: &str, args: &[FfiValue]) -> Result<FfiValue, Error> {
        let info = match self.funcs.get(name) {
            Some(i) if matches!(i.link, FuncLink::User(_)) => i,
            _ => return Err(Error::msg(format!("no callable fn {name:?}"))),
        };
        if info.is_mutating {
            return Err(Error::msg(format!(
                "fn {name:?} is a mutating method; call it on a receiver, not via the FFI"
            )));
        }
        if info.params.len() != args.len() {
            return Err(Error::msg(format!(
                "fn {name:?} expects {} argument(s), got {}",
                info.params.len(),
                args.len()
            )));
        }
        if args.len() > 6 {
            return Err(Error::msg(format!(
                "fn {name:?} has {} parameters; the FFI supports up to 6",
                args.len()
            )));
        }
        // The return type must be FFI-marshalable; this also validates struct
        // fields (including a struct nested as an optional's core).
        check_ffi_return(name, &info.return_ty, &self.structs)?;
        let ret_is_str = is_str_repr(&info.return_ty);
        // An optional `T?` (possibly nested) — scalar, `str`, or struct core — is
        // returned through a hidden sret pointer (see the sret path below).
        let ret_is_opt = matches!(info.return_ty, Type::Optional(_));
        // A `Result<ok, err>` — each side a scalar/`str`/`Unit`/struct — is also
        // sret-returned, same shape as an optional but tagged/sized differently.
        let ret_is_result = matches!(info.return_ty, Type::Result(_, _));
        // A `struct` returned directly (not under an optional); read back field by
        // field from the sret buffer.
        let ret_struct = ffi_struct_layout(&info.return_ty, &self.structs);

        // Marshal each argument to its `i64` ABI, validating the variant against
        // the parameter type. A heap `str` buffer the host allocates is recorded
        // in `to_free` to release after the call. Struct buffers are kept alive
        // in `struct_bufs` until after `invoke` (the ABI passes a pointer).
        let mut abi: Vec<i64> = Vec::with_capacity(args.len());
        let mut to_free: Vec<(*mut u8, usize)> = Vec::new();
        let mut struct_bufs: Vec<Vec<u8>> = Vec::new();
        for (i, (p, a)) in info.params.iter().zip(args).enumerate() {
            match a {
                FfiValue::Int(v) if is_ffi_scalar(p) => abi.push(*v),
                FfiValue::Str(s) if is_str_repr(p) => {
                    let (val, heap) = build_borrowed_str(s.as_bytes());
                    if let Some(buf) = heap {
                        to_free.push(buf);
                    }
                    abi.push(val);
                }
                FfiValue::Struct(fields) => {
                    // Collect layout info (clone to end the borrow of self.structs).
                    let (buf_size, field_specs) = match ffi_struct_layout(p, &self.structs) {
                        Some(layout) => {
                            let specs = layout
                                .fields
                                .iter()
                                .map(|f| (f.name.clone(), f.offset, f.ty.clone()))
                                .collect::<Vec<_>>();
                            (layout.size as usize, specs)
                        }
                        None => {
                            for (header, content_len) in std::mem::take(&mut to_free) {
                                unsafe { free_dynamic_string(header, content_len) };
                            }
                            return Err(Error::msg(format!(
                                "fn {name:?} parameter {i} is {}; pass it as the matching \
                                 FfiValue (Int for i64/bool/char, Str for str, Struct for struct)",
                                type_name(p)
                            )));
                        }
                    };
                    if fields.len() != field_specs.len() {
                        for (header, content_len) in std::mem::take(&mut to_free) {
                            unsafe { free_dynamic_string(header, content_len) };
                        }
                        return Err(Error::msg(format!(
                            "fn {name:?} parameter {i}: struct {} has {} field(s), got {}",
                            type_name(p),
                            field_specs.len(),
                            fields.len()
                        )));
                    }
                    let mut buf = vec![0u8; buf_size];
                    for ((fname, fval), (fl_name, fl_offset, fl_ty)) in
                        fields.iter().zip(field_specs.iter())
                    {
                        if fname != fl_name {
                            for (header, content_len) in std::mem::take(&mut to_free) {
                                unsafe { free_dynamic_string(header, content_len) };
                            }
                            return Err(Error::msg(format!(
                                "fn {name:?} parameter {i}: expected field {:?}, got {:?}",
                                fl_name, fname
                            )));
                        }
                        match fval {
                            FfiValue::Int(v) if is_ffi_scalar(fl_ty) => {
                                let off = *fl_offset as usize;
                                buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
                            }
                            _ => {
                                for (header, content_len) in std::mem::take(&mut to_free) {
                                    unsafe { free_dynamic_string(header, content_len) };
                                }
                                return Err(Error::msg(format!(
                                    "fn {name:?} parameter {i}, field {:?} is {}; only \
                                     i64/bool/char fields can be passed in a FfiValue::Struct",
                                    fl_name,
                                    type_name(fl_ty)
                                )));
                            }
                        }
                    }
                    struct_bufs.push(buf);
                    abi.push(struct_bufs.last().unwrap().as_ptr() as i64);
                }
                _ => {
                    // Release any buffers earlier `str` args already allocated.
                    for (header, content_len) in std::mem::take(&mut to_free) {
                        unsafe { free_dynamic_string(header, content_len) };
                    }
                    return Err(Error::msg(format!(
                        "fn {name:?} parameter {i} is {}; pass it as the matching FfiValue \
                         (Int for i64/bool/char, Str for str, Struct for struct)",
                        type_name(p)
                    )));
                }
            }
        }

        let id = self.lookup(name)?;
        let ptr = self.module.get_finalized_function(id);

        if ret_is_opt {
            // Composite (optional) return: the callee writes it through a hidden
            // sret pointer — a normal *leading* i64 param — and returns nothing.
            // The flattened layout is `{ i64 tag, core }`; `elem_size_of` gives the
            // total (16 for a scalar/str core, more for a struct core like `Span?`).
            let words = (elem_size_of(&info.return_ty, &self.structs) as usize)
                .div_ceil(8)
                .max(1);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                for (header, content_len) in to_free {
                    unsafe { free_dynamic_string(header, content_len) };
                }
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for an optional return; the FFI \
                     supports up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: the function takes `(sret_ptr, <= 5 scalar/str args)` and
            // returns nothing; we transmute through the i64-returning `invoke` and
            // ignore the (unset) return register.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result =
                unsafe { read_ffi_optional(sret_buf.as_ptr(), &info.return_ty, &self.structs) };
            for (header, content_len) in to_free {
                unsafe { free_dynamic_string(header, content_len) };
            }
            return Ok(result);
        }

        if ret_is_result {
            // Composite (result) return: same sret shape as an optional (a
            // leading pointer, no register return), but the buffer is sized to
            // the wider of the two sides and the tag means `1` = Ok / `0` = Err
            // rather than a nesting depth.
            let words = (elem_size_of(&info.return_ty, &self.structs) as usize)
                .div_ceil(8)
                .max(1);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                for (header, content_len) in to_free {
                    unsafe { free_dynamic_string(header, content_len) };
                }
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for a result return; the FFI \
                     supports up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: as the optional path above.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result =
                unsafe { read_ffi_result(sret_buf.as_ptr(), &info.return_ty, &self.structs) };
            for (header, content_len) in to_free {
                unsafe { free_dynamic_string(header, content_len) };
            }
            return Ok(result);
        }

        if let Some(layout) = ret_struct {
            // Composite (struct) return: like the optional path, the callee writes
            // the struct through a hidden leading sret pointer and returns nothing.
            // Size the buffer to the struct (rounded up to whole `i64` words).
            let words = (layout.size as usize).div_ceil(8).max(1);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                for (header, content_len) in to_free {
                    unsafe { free_dynamic_string(header, content_len) };
                }
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for a struct return; the FFI supports \
                     up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: as the optional path, but the buffer is the struct's size.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result = unsafe { read_ffi_struct(sret_buf.as_ptr() as *const u8, layout) };
            for (header, content_len) in to_free {
                unsafe { free_dynamic_string(header, content_len) };
            }
            return Ok(result);
        }

        // SAFETY: arity (<= 6) and per-argument types are validated above; every
        // scalar and `str` lowers to one `i64`, so the finalized code matches the
        // transmuted signature.
        let r = unsafe { invoke(ptr, &abi) };

        let result = if ret_is_str {
            // The callee handed us a reference on the returned `str` (its body
            // retained it before dropping its scope). Copy the bytes out *before*
            // freeing any argument buffer — an identity `fn(s) -> s` returns one
            // of them — then release our reference and free the borrowed buffers.
            let rv = r as *const u8;
            let mut buf = [0u8; 8];
            let bytes = unsafe { str_bytes(rv, &mut buf) };
            let s = String::from_utf8_lossy(bytes).into_owned();
            aipl_dec(rv);
            FfiValue::Str(s)
        } else {
            FfiValue::Int(r)
        };
        for (header, content_len) in to_free {
            unsafe { free_dynamic_string(header, content_len) };
        }
        Ok(result)
    }

    fn lookup(&self, name: &str) -> Result<FuncId, Error> {
        // Only user-defined functions are run directly (builtins are never
        // entry points), so a `User` link is expected here.
        match self.funcs.get(name).map(|i| i.link) {
            Some(FuncLink::User(id)) => Ok(id),
            _ => Err(Error::msg(format!("no fn {name:?}"))),
        }
    }
}

/// Whether `t` is a scalar the embedding FFI can marshal as a bare `i64`.
fn is_ffi_scalar(t: &Type) -> bool {
    matches!(
        t,
        Type::Primitive(Primitive::I64 | Primitive::Bool | Primitive::Char)
    )
}

/// The [`StructLayout`] for `t` if it names a `struct` (not a variant), else
/// `None` — the gate the FFI uses to decide whether to marshal a value field by
/// field.
fn ffi_struct_layout<'a>(
    t: &Type,
    structs: &'a HashMap<String, TypeDef>,
) -> Option<&'a StructLayout> {
    match t {
        Type::Named(n) => structs.get(n).and_then(TypeDef::as_struct),
        _ => None,
    }
}

/// Validate that `ty` can be marshaled back across the embedding FFI as a return
/// value: a scalar, `str`, `Unit`, a `struct` whose fields are all scalar/`str`,
/// an optional (possibly nested) whose core is one of those, or a `Result`
/// whose `ok`/`err` sides are each independently one of those (so `!Error`,
/// i.e. `Result<Unit, Error>`, is fine: `Unit` on the ok side, `Error` — a
/// `str`-repr type — on the err side). Errors name the offending type/field.
/// (`call_values` then dispatches on the type's shape.)
fn check_ffi_return(
    name: &str,
    ty: &Type,
    structs: &HashMap<String, TypeDef>,
) -> Result<(), Error> {
    if is_ffi_scalar(ty) || is_str_repr(ty) || is_unit(ty) {
        return Ok(());
    }
    match ty {
        // Peel optional layers down to the (shared, flattened) core.
        Type::Optional(inner) => check_ffi_return(name, inner, structs),
        // Each side independently: same rules as a bare return.
        Type::Result(ok, err) => {
            check_ffi_return(name, ok, structs)?;
            check_ffi_return(name, err, structs)
        }
        Type::Named(_) => match ffi_struct_layout(ty, structs) {
            Some(layout) => match layout
                .fields
                .iter()
                .find(|f| !is_ffi_scalar(&f.ty) && !is_str_repr(&f.ty))
            {
                Some(f) => Err(Error::msg(format!(
                    "fn {name:?} returns struct {} whose field {:?} is {}; the FFI can return \
                     only structs whose fields are all i64/bool/char or str",
                    type_name(ty),
                    f.name,
                    type_name(&f.ty)
                ))),
                None => Ok(()),
            },
            // A named non-struct (a variant) isn't marshalable yet.
            None => Err(Error::msg(format!(
                "fn {name:?} returns {}; the FFI supports only i64/bool/char, str, structs of \
                 those, and optionals/results of those",
                type_name(ty)
            ))),
        },
        _ => Err(Error::msg(format!(
            "fn {name:?} returns {}; the FFI supports only i64/bool/char, str, structs of those, \
             and optionals/results of those",
            type_name(ty)
        ))),
    }
}

/// Invoke a finalized function pointer with up to six `i64` arguments,
/// transmuting to the matching C-ABI arity.
///
/// SAFETY: the caller must have validated that `args.len() <= 6` and that every
/// parameter and the return lower to a single `i64` (every scalar AIPL value,
/// and `str` as a tagged pointer), so the finalized code matches the transmuted
/// signature.
unsafe fn invoke(ptr: *const u8, args: &[i64]) -> i64 {
    use std::mem::transmute;
    unsafe {
        match args.len() {
            0 => (transmute::<_, extern "C" fn() -> i64>(ptr))(),
            1 => (transmute::<_, extern "C" fn(i64) -> i64>(ptr))(args[0]),
            2 => (transmute::<_, extern "C" fn(i64, i64) -> i64>(ptr))(args[0], args[1]),
            3 => (transmute::<_, extern "C" fn(i64, i64, i64) -> i64>(ptr))(
                args[0], args[1], args[2],
            ),
            4 => (transmute::<_, extern "C" fn(i64, i64, i64, i64) -> i64>(ptr))(
                args[0], args[1], args[2], args[3],
            ),
            5 => (transmute::<_, extern "C" fn(i64, i64, i64, i64, i64) -> i64>(ptr))(
                args[0], args[1], args[2], args[3], args[4],
            ),
            6 => (transmute::<_, extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64>(ptr))(
                args[0], args[1], args[2], args[3], args[4], args[5],
            ),
            n => unreachable!("invoke called with arity {n} > 6 — validate before calling"),
        }
    }
}

/// Build a `str` value for an FFI argument the host owns. A short string (<= 7
/// bytes) is packed inline (no allocation); a longer one gets a fresh heap buffer
/// tagged `STATIC_REFCOUNT`, so the callee's refcount inc/dec all no-op on it —
/// exactly like a string literal — and never frees it. The host keeps ownership
/// and frees the heap buffer after the call. Returns the value and, for the heap
/// case, the `(header, content_len)` to hand to [`free_dynamic_string`].
fn build_borrowed_str(bytes: &[u8]) -> (i64, Option<(*mut u8, usize)>) {
    if bytes.len() <= 7 {
        (pack_inline(bytes) as i64, None)
    } else {
        let raw = alloc_dynamic_string(bytes.len());
        unsafe {
            // alloc wrote `[len][refcount=1]`; make the refcount static (word 1).
            std::ptr::write((raw as *mut i64).add(1), STATIC_REFCOUNT);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), raw.add(STR_HEADER_SIZE), bytes.len());
            (raw.add(STR_HEADER_SIZE) as i64, Some((raw, bytes.len())))
        }
    }
}

/// Read a flattened optional of type `ty` (an `Optional` over a scalar/`str`
/// core) from the sret buffer `buf` an optional-returning function wrote, into an
/// [`FfiValue::Opt`]. The layout mirrors codegen: an `i64` tag at offset 0 (`0` =
/// `none`; `k` = `k` nested `some`s) and the core value at [`OPT_VALUE_OFFSET`].
/// A present `str` core carries the one reference the callee retained on return
/// (see the composite-return path in `define_fn`), which we release here.
///
/// SAFETY: `buf` must point at a `{ i64 tag, core }` the callee filled for an
/// optional whose core is a scalar, `str`, or struct.
unsafe fn read_ffi_optional(
    buf: *const i64,
    ty: &Type,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let tag = unsafe { *buf };
    unsafe { read_ffi_optional_tag(buf, ty, tag, structs) }
}

/// Read a flattened `Result<ok, err>` from the sret buffer `buf` a
/// result-returning function wrote, into an [`FfiValue::Res`]. The layout
/// mirrors codegen: an `i64` tag at offset 0 (`1` = `Ok`, `0` = `Err`) and the
/// active side's payload at [`OPT_VALUE_OFFSET`] — read with [`read_ffi_core`],
/// which already reads a `Unit` side (e.g. `!Error`'s ok case) back as a
/// harmless `Int(0)`.
///
/// SAFETY: `buf` must point at a `{ i64 tag, value }` a `Result`-returning
/// callee filled, with `ok`/`err` each a scalar, `str`, `Unit`, or struct.
unsafe fn read_ffi_result(
    buf: *const i64,
    ty: &Type,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let (ok_ty, err_ty) = match ty {
        Type::Result(ok, err) => (ok.as_ref(), err.as_ref()),
        _ => unreachable!("read_ffi_result on a non-result type"),
    };
    let tag = unsafe { *buf };
    if tag == 1 {
        FfiValue::Res(Ok(Box::new(unsafe { read_ffi_core(buf, ok_ty, structs) })))
    } else {
        FfiValue::Res(Err(Box::new(unsafe {
            read_ffi_core(buf, err_ty, structs)
        })))
    }
}

/// Reconstruct the nested `Opt` for `ty` given the flattened `tag`. Because the
/// representation is flattened, every nesting level shares the same `buf`; we
/// peel one `Optional` layer per recursion, decrementing the tag, until either a
/// `none` (tag `0`) or the non-optional core.
unsafe fn read_ffi_optional_tag(
    buf: *const i64,
    ty: &Type,
    tag: i64,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let inner = match ty {
        Type::Optional(inner) => inner.as_ref(),
        _ => unreachable!("read_ffi_optional_tag on a non-optional type"),
    };
    if tag == 0 {
        return FfiValue::Opt(None);
    }
    let value = if matches!(inner, Type::Optional(_)) {
        unsafe { read_ffi_optional_tag(buf, inner, tag - 1, structs) }
    } else {
        unsafe { read_ffi_core(buf, inner, structs) }
    };
    FfiValue::Opt(Some(Box::new(value)))
}

/// Read the present core value at [`OPT_VALUE_OFFSET`] of an optional buffer: a
/// struct (read inline, field by field), a `str` (bytes copied out and its
/// retained reference released), or a scalar.
unsafe fn read_ffi_core(
    buf: *const i64,
    ty: &Type,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    if let Some(layout) = ffi_struct_layout(ty, structs) {
        // The struct payload sits inline starting at `OPT_VALUE_OFFSET`.
        let base = unsafe { (buf as *const u8).add(OPT_VALUE_OFFSET as usize) };
        return unsafe { read_ffi_struct(base, layout) };
    }
    // `OPT_VALUE_OFFSET` is 8 bytes, i.e. the next `i64` word.
    let raw = unsafe { *buf.add(1) };
    if is_str_repr(ty) {
        let rv = raw as *const u8;
        let mut tmp = [0u8; 8];
        let bytes = unsafe { str_bytes(rv, &mut tmp) };
        let s = String::from_utf8_lossy(bytes).into_owned();
        aipl_dec(rv);
        FfiValue::Str(s)
    } else {
        FfiValue::Int(raw)
    }
}

/// Read a struct (`layout`) the callee wrote into the sret buffer at `base` into
/// an [`FfiValue::Struct`] — one `(name, value)` per field, in declaration order,
/// each read at its byte offset. A scalar field is an `Int`; a `str` field's
/// bytes are copied out and the reference the callee retained on return released
/// (the composite return retains heap fields — see the `Return` path in
/// `compile_expr`), mirroring [`read_ffi_core`]. `call_values` has already
/// rejected any field that isn't a scalar or `str`.
///
/// SAFETY: `base` must point at a `layout`-shaped struct the callee filled, with
/// each `str` field carrying one retained reference.
unsafe fn read_ffi_struct(base: *const u8, layout: &StructLayout) -> FfiValue {
    let fields = layout
        .fields
        .iter()
        .map(|f| {
            let raw = unsafe { *(base.add(f.offset as usize) as *const i64) };
            let value = if is_str_repr(&f.ty) {
                let rv = raw as *const u8;
                let mut tmp = [0u8; 8];
                let bytes = unsafe { str_bytes(rv, &mut tmp) };
                let s = String::from_utf8_lossy(bytes).into_owned();
                aipl_dec(rv);
                FfiValue::Str(s)
            } else {
                FfiValue::Int(raw)
            };
            (f.name.clone(), value)
        })
        .collect();
    FfiValue::Struct(fields)
}

/// Name the user's `main` is exported as in the object file. The Rust
/// runtime in `runtime/aipl_runtime.rs` provides `int main()` which builds the
/// CLI args as a `str[]` and calls this symbol. Keeping it in one place avoids
/// drift between the two.
pub const BINARY_USER_MAIN: &str = "__aipl_user_main";

/// Name of a 1-byte data symbol the object exports telling the runtime whether
/// the user's `main` actually declared the CLI-args `str[]` parameter (`1`) or
/// had a synthetic one injected (`0`). When `0`, the runtime skips building the
/// args array entirely and passes null, so a `main` that ignores args costs no
/// allocation. Read by `runtime/aipl_runtime.rs`.
pub const MAIN_WANTS_ARGS_SYMBOL: &str = "__aipl_main_wants_args";

/// The type of `main`'s CLI-arguments parameter: `str[]`.
fn cli_args_ty() -> Type {
    Type::Array(Box::new(Type::Primitive(Primitive::Str)))
}

/// `main` declared with no return type: it returns nothing, and its exit code
/// is implicitly 0. As the program entry it still needs an `i64` result, so
/// codegen gives it one (emitting 0) — but its body is type-checked as unit,
/// so a trailing expression (an attempt to return a value) is an error.
fn is_unit_main(f: &aipl_mono::ConcreteFn) -> bool {
    f.name == "main" && f.return_ty.is_none()
}

/// `fn main() -> !Error`: an allowed entry variation. Its body yields a void-Ok
/// result; at the ABI level `main` still returns an `i64` exit code, which
/// codegen derives from the result — `ok()` → 0, `err(msg)` → print
/// `error: <msg>` and exit 1.
fn is_error_main(f: &aipl_mono::ConcreteFn) -> bool {
    f.name == "main"
        && matches!(&f.return_ty, Some(Type::Result(ok, err)) if is_unit(ok) && is_error(err))
}

/// A mutating method: `fn f(mut self: T, ...)`. It returns nothing to the
/// user and mutates its receiver.
fn is_mutating_fn(f: &aipl_mono::ConcreteFn) -> bool {
    f.params.first().is_some_and(|p| p.mutable)
}

/// The type a function actually yields at the ABI level — what its signature
/// returns and what a caller's `Call` sees. This is the *declared* return type
/// except for two entry-style cases that produce a value while their body is
/// checked as unit: a unit `main` yields its `i64` exit code, and a mutating
/// method yields its (mutated) `self`.
fn abi_return_ty(f: &aipl_mono::ConcreteFn) -> Type {
    if is_mutating_fn(f) {
        f.params[0].ty.clone()
    } else if is_unit_main(f) || is_error_main(f) {
        // Both produce an `i64` exit code: a unit `main` always 0, an `!Error`
        // `main` 0/1 derived from its result (see `compile_function`).
        Type::Primitive(Primitive::I64)
    } else {
        f.return_ty.clone().unwrap_or(Type::Unit)
    }
}

/// Fill `sig`'s params and returns for `f`'s ABI: a hidden sret pointer when
/// the (ABI) return is a struct, one i64 per declared parameter, then the
/// result — nothing for unit/struct(sret), `(tag, value)` for an optional, a
/// single i64 otherwise. Used by both the declaration and the definition so
/// they can't drift.
fn build_signature(
    sig: &mut Signature,
    f: &aipl_mono::ConcreteFn,
    structs: &HashMap<String, TypeDef>,
) {
    let abi = abi_return_ty(f);
    // Composites — structs and optionals (possibly nested) — are returned
    // through a hidden caller-provided pointer (sret), uniformly.
    let returns_composite = sret_size(&abi, structs).is_some();
    if returns_composite {
        sig.params.push(AbiParam::new(types::I64));
    }
    for p in &f.params {
        sig.params.push(AbiParam::new(cl_type_of(&p.ty)));
    }
    if is_unit(&abi) || returns_composite {
        // Unit yields no result; a composite is written through the sret pointer.
    } else {
        sig.returns.push(AbiParam::new(types::I64));
    }
}

/// Prepare `program`'s `main` for the AOT entry, which always receives the CLI
/// arguments as a `str[]` (the runtime passes one). If the user's `main`
/// declares no parameters, inject a synthetic, ignored one so the entry ABI is
/// uniform and the args array is still owned — and thus freed — by `main` via
/// the normal heap-parameter drop. Errors if `main` declares anything other
/// than a single `str[]` parameter.
///
/// Returns the rewritten program together with whether the user's `main`
/// *actually declared* the args parameter (vs. having one injected) — the
/// caller exports this as [`MAIN_WANTS_ARGS_SYMBOL`] so the runtime can skip
/// building the args array when nothing reads it.
///
/// (A `main` that returns nothing is handled in codegen, which gives the entry
/// an `i64` result emitting 0 while still checking the body is unit-typed.)
fn with_cli_args_main(program: &Program) -> Result<(Program, bool), Error> {
    let mut program = program.clone();
    let mut wants_args = false;
    for item in &mut program.items {
        let Item::Fn(f) = item else { continue };
        if f.name != "main" {
            continue;
        }
        match f.sig.params.as_slice() {
            [] => f.sig.params.push(Param {
                name: "__cli_args".to_string(),
                ty: cli_args_ty(),
                mutable: false,
                variadic: false,
                default: None,
            }),
            [p] if p.ty == cli_args_ty() => wants_args = true,
            _ => {
                return Err(Error::msg(
                    "\"main\" must take either no parameters or a single \"str[]\" (the CLI arguments)"
                        .to_string(),
                ));
            }
        }
    }
    Ok((program, wants_args))
}

/// Emit the [`MAIN_WANTS_ARGS_SYMBOL`] flag as a 1-byte exported data object so
/// the runtime can read it at startup.
fn emit_main_wants_args_flag(module: &mut ObjectModule, wants_args: bool) -> Result<(), Error> {
    let data_id = module
        .declare_data(MAIN_WANTS_ARGS_SYMBOL, Linkage::Export, false, false)
        .map_err(|e| Error::msg(format!("declare {MAIN_WANTS_ARGS_SYMBOL}: {e}")))?;
    let mut desc = DataDescription::new();
    desc.define(vec![u8::from(wants_args)].into_boxed_slice());
    module
        .define_data(data_id, &desc)
        .map_err(|e| Error::msg(format!("define {MAIN_WANTS_ARGS_SYMBOL}: {e}")))?;
    Ok(())
}

/// AOT compilation path: emits a relocatable object file that calls into
/// the AIPL runtime staticlib. Use [`ObjectCompilation::emit`] to get the
/// object-file bytes, which the driver writes to disk and links with
/// `clang` against the embedded runtime.
pub struct ObjectCompilation {
    module: ObjectModule,
    funcs: HashMap<String, FuncInfo>,
    ir: String,
}

impl ObjectCompilation {
    /// `instrument` enables the executed-instruction counter (a per-block call).
    /// Production builds pass `false`; only the test harness's separate
    /// measurement object passes `true`.
    pub fn new(
        program: &Program,
        name: &str,
        dbg: DebugOptions,
        instrument: bool,
    ) -> Result<Self, Error> {
        if !program
            .items
            .iter()
            .any(|i| matches!(i, Item::Fn(f) if f.name == "main"))
        {
            return Err(Error::msg(
                "binary build requires a \"main\" function".to_string(),
            ));
        }

        // Object files must be position-independent so the system linker
        // can lay them out as PIE on Linux/Mac and add the right reloc
        // entries on Windows.
        let mut flag_builder = settings::builder();
        flag_builder
            .set("use_colocated_libcalls", "false")
            .map_err(|e| Error::msg(format!("flag: {e}")))?;
        flag_builder
            .set("is_pic", "true")
            .map_err(|e| Error::msg(format!("flag: {e}")))?;
        let isa_builder = cranelift_native::builder()
            .map_err(|msg| Error::msg(format!("host machine not supported: {msg}")))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| Error::msg(format!("isa: {e}")))?;

        let builder = ObjectBuilder::new(isa, name, cranelift_module::default_libcall_names())
            .map_err(|e| Error::msg(format!("object builder: {e}")))?;
        let mut module = ObjectModule::new(builder);

        // `main` always receives the CLI args as a `str[]`; ensure its
        // signature reflects that before lowering. `main_wants_args` records
        // whether the user actually reads them, which we hand to the runtime.
        let (program, main_wants_args) = with_cli_args_main(program)?;
        let (funcs, _structs, ir) = compile_program(
            &mut module,
            &program,
            Some(BINARY_USER_MAIN),
            dbg,
            instrument,
        )?;
        emit_main_wants_args_flag(&mut module, main_wants_args)?;

        Ok(Self { module, funcs, ir })
    }

    pub fn ir(&self) -> &str {
        &self.ir
    }

    /// The names of the function instances monomorphization emitted into this
    /// object — exactly what codegen defined. Each generic specialization and
    /// owned/borrow/`str`-kept form is a distinct mangled instance (see the
    /// monomorphizer's `enqueue_full`); non-generic functions appear under their
    /// own name. Sorted, and excludes builtin imports (runtime externs, linked
    /// in separately, not emitted here). Used by the test harness's
    /// `--- monomorphizations ---` section.
    pub fn monomorphized_fns(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .funcs
            .iter()
            .filter(|(_, info)| matches!(info.link, FuncLink::User(_)))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Consume self and return the serialized object-file bytes.
    pub fn emit(self) -> Result<Vec<u8>, Error> {
        let product = self.module.finish();
        product
            .emit()
            .map_err(|e| Error::msg(format!("emit object: {e}")))
    }

    /// Convenience: write the object file to `path`.
    pub fn write_to(self, path: &Path) -> Result<(), Error> {
        let bytes = self.emit()?;
        std::fs::write(path, bytes).map_err(|e| Error::msg(format!("write {path:?}: {e}")))
    }
}

/// A struct or variant declaration, indexed by name for layout resolution.
#[derive(Clone, Copy)]
enum TypeDeclRef<'a> {
    Struct(&'a StructDecl),
    Variant(&'a aipl_syntax::ast::VariantDecl),
}

fn build_struct_layouts(
    program: &aipl_mono::MonoProgram,
) -> Result<HashMap<String, TypeDef>, Error> {
    // Index declarations (structs and variants together — they share one
    // namespace) by name up front, rejecting duplicates, so a field may name
    // a type declared later in the file. Layouts are then resolved in
    // dependency order: a struct- or variant-typed field is stored inline, so
    // the nested type's size must be known before the outer type's is.
    let mut decls: HashMap<&str, TypeDeclRef> = HashMap::new();
    for s in &program.structs {
        if decls
            .insert(s.name.as_str(), TypeDeclRef::Struct(s))
            .is_some()
        {
            return Err(Error::msg(format!(
                "duplicate struct definition {:?}",
                s.name
            )));
        }
    }
    for v in &program.variants {
        if decls
            .insert(v.name.as_str(), TypeDeclRef::Variant(v))
            .is_some()
        {
            return Err(Error::msg(format!(
                "duplicate type definition {:?}",
                v.name
            )));
        }
    }

    let mut layouts: HashMap<String, TypeDef> = HashMap::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    // Sorted so resolution order — and therefore which type a cycle error is
    // reported against — is deterministic across runs.
    let mut names: Vec<&str> = decls.keys().copied().collect();
    names.sort_unstable();
    for name in names {
        resolve_type_layout(name, &decls, &mut layouts, &mut on_stack)?;
    }
    Ok(layouts)
}

/// Compute (and memoize into `layouts`) the layout of the struct or variant
/// `name`, recursing into any struct- or variant-typed components first.
/// `on_stack` tracks the types currently being resolved so a cycle (which
/// would have infinite size) is reported rather than recursing forever.
fn resolve_type_layout(
    name: &str,
    decls: &HashMap<&str, TypeDeclRef>,
    layouts: &mut HashMap<String, TypeDef>,
    on_stack: &mut HashSet<String>,
) -> Result<(), Error> {
    if layouts.contains_key(name) {
        return Ok(());
    }
    let decl = *decls
        .get(name)
        .expect("resolve_type_layout called with a declared type");
    if !on_stack.insert(name.to_string()) {
        let kind = match decl {
            TypeDeclRef::Struct(_) => "struct",
            TypeDeclRef::Variant(_) => "variant",
        };
        return Err(Error::msg(format!(
            "{kind} {name}: recursive types have infinite size (a type cannot \
             contain itself, directly or transitively)"
        )));
    }
    let def = match decl {
        TypeDeclRef::Struct(s) => {
            TypeDef::Struct(build_struct_layout(s, decls, layouts, on_stack)?)
        }
        TypeDeclRef::Variant(v) => {
            TypeDef::Variant(build_variant_layout(v, decls, layouts, on_stack)?)
        }
    };
    on_stack.remove(name);
    layouts.insert(name.to_string(), def);
    Ok(())
}

/// Lay out a `struct`: fields are stored sequentially (no padding — every
/// field size is a multiple of 8), nested composites inline.
fn build_struct_layout(
    decl: &StructDecl,
    decls: &HashMap<&str, TypeDeclRef>,
    layouts: &mut HashMap<String, TypeDef>,
    on_stack: &mut HashSet<String>,
) -> Result<StructLayout, Error> {
    let mut fields = Vec::with_capacity(decl.fields.len());
    let mut offset: u32 = 0;
    for f in &decl.fields {
        // Allowed field types: i64/bool/char (by value), `str` or an array
        // (8-byte refcounted heap pointers), another declared struct or a
        // variant (stored inline — resolve it here so its size is known), or
        // an optional of a scalar/str/array (a 16-byte inline `{tag, value}`
        // composite).
        match &f.ty {
            Type::Primitive(
                Primitive::I64 | Primitive::Bool | Primitive::Char | Primitive::Str,
            ) => {}
            Type::Named(n) if decls.contains_key(n.as_str()) => {
                resolve_type_layout(n, decls, layouts, on_stack)?;
            }
            Type::Array(_) => {}
            Type::Optional(inner)
                if is_set_elem(inner) || matches!(inner.as_ref(), Type::Array(_)) => {}
            _ => {
                return Err(Error::msg(format!(
                    "struct {}: field {} has type {}, but struct fields must be i64, bool, char, str, a struct, a variant, an array, or an optional of (i64, bool, char, str, or an array)",
                    decl.name,
                    f.name,
                    type_name(&f.ty),
                )));
            }
        }
        // The nested struct/variant (if any) is now resolved, so its size is
        // in `layouts`.
        let size = field_size(&f.ty, layouts);
        fields.push(FieldLayout {
            name: f.name.clone(),
            ty: f.ty.clone(),
            offset,
        });
        // Advance to the next field
        offset += size;
    }
    Ok(StructLayout {
        fields,
        size: offset,
    })
}

/// Lay out a `variant`: the tag occupies offset 0, each case's payload is laid
/// out (like a struct) from `VARIANT_PAYLOAD_OFFSET`, and the whole value is
/// sized to the widest case so all cases share one payload region.
fn build_variant_layout(
    v: &aipl_syntax::ast::VariantDecl,
    decls: &HashMap<&str, TypeDeclRef>,
    layouts: &mut HashMap<String, TypeDef>,
    on_stack: &mut HashSet<String>,
) -> Result<VariantLayout, Error> {
    let mut cases = Vec::with_capacity(v.cases.len());
    let mut max_payload: u32 = 0;
    for c in &v.cases {
        let mut fields = Vec::with_capacity(c.payload.len());
        let mut offset = VARIANT_PAYLOAD_OFFSET;
        for ty in &c.payload {
            // A payload field is an array element / inline composite: a scalar,
            // `str`, an array, an optional, or a struct (resolved here so its
            // size is known) — never another variant directly (an inline
            // recursive sum type would have infinite size).
            let ok = match ty {
                _ if is_set_elem(ty) => true, // i64/bool/char/str
                Type::Array(_) | Type::Optional(_) => true,
                Type::Named(n) => match decls.get(n.as_str()) {
                    Some(TypeDeclRef::Struct(_)) => {
                        resolve_type_layout(n, decls, layouts, on_stack)?;
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
            if !ok {
                return Err(Error::msg(format!(
                    "variant {} case {}: payload type {} is not supported (use i64, bool, char, \
                     str, an array, an optional, or a struct)",
                    v.name,
                    c.name,
                    type_name(ty),
                )));
            }
            fields.push(FieldLayout {
                name: String::new(),
                ty: ty.clone(),
                offset,
            });
            offset += field_size(ty, layouts);
        }
        max_payload = max_payload.max(offset - VARIANT_PAYLOAD_OFFSET);
        cases.push(VariantCaseLayout {
            name: c.name.clone(),
            fields,
        });
    }
    Ok(VariantLayout {
        cases,
        size: VARIANT_PAYLOAD_OFFSET + max_payload,
    })
}

fn cl_type_of(_t: &Type) -> types::Type {
    types::I64
}

/// Re-canonicalize an `i64`-register value to integer type `name`'s width: a
/// narrow signed type is sign-extended from its low bits, an unsigned type is
/// zero-extended (masked low bits). Every integer lives in an `i64` register
/// kept in this canonical form, so arithmetic wraps correctly and
/// comparison/rendering see the mathematically-correct value. `i64`/`u64` are
/// already full width.
fn canon_int(builder: &mut FunctionBuilder, v: Value, p: Primitive) -> Value {
    let bits = p.int_bits().expect("integer type");
    if bits == 64 {
        return v;
    }
    let shift = i64::from(64 - bits);
    if p.int_signed() {
        let l = builder.ins().ishl_imm(v, shift);
        builder.ins().sshr_imm(l, shift)
    } else {
        let mask = (1i64 << bits) - 1;
        builder.ins().band_imm(v, mask)
    }
}

/// Emit an integer add or subtract for AIPL primitive `p`, in wrapping or
/// saturating mode — the forms the `+`/`-` operators resolve to
/// (`wrapping_add`/`saturating_add`/`wrapping_sub`/`saturating_sub`). Operands
/// `lv`/`rv` are canonical narrow ints in i64 registers (see [`canon_int`]); the
/// result is likewise canonical. `sub` selects subtraction.
///
/// - Wrapping: compute in i64 and re-canonicalize (drop the out-of-range bits).
/// - Saturating: clamp to `[min, max]` of the width. A narrow width computes
///   exactly in i64 (operands are small, so it can't overflow i64) and clamps; a
///   full `i64`/`u64` detects over/underflow from the operand/result signs
///   (Cranelift's saturating ops are SIMD-only, so this uses `icmp`/`select`). A
///   clamped in-range value is already its own canonical form.
fn emit_int_addsub(
    builder: &mut FunctionBuilder,
    lv: Value,
    rv: Value,
    p: Primitive,
    sub: bool,
    saturating: bool,
) -> Value {
    let raw = if sub {
        builder.ins().isub(lv, rv)
    } else {
        builder.ins().iadd(lv, rv)
    };
    if !saturating {
        return canon_int(builder, raw, p);
    }
    let bits = p.int_bits().expect("integer type");
    let signed = p.int_signed();
    if bits < 64 {
        // The i64 result is exact; clamp it to the width's range.
        let (min, max) = if signed {
            (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
        } else {
            (0, (1i64 << bits) - 1)
        };
        let maxc = builder.ins().iconst(types::I64, max);
        let over = builder.ins().icmp(IntCC::SignedGreaterThan, raw, maxc);
        let capped = builder.ins().select(over, maxc, raw);
        // A signed result can go below `min`; an unsigned one can only underflow
        // `0` on a subtraction (an add of non-negative operands never does).
        if signed || sub {
            let minc = builder.ins().iconst(types::I64, min);
            let under = builder.ins().icmp(IntCC::SignedLessThan, capped, minc);
            builder.ins().select(under, minc, capped)
        } else {
            capped
        }
    } else if signed {
        // Signed i64 over/underflow. Add: both operands share a sign differing from
        // the result's — `(a ^ r) & (b ^ r) < 0`. Sub: the operands differ in sign
        // and the result's sign differs from `a` — `(a ^ b) & (a ^ r) < 0`. Either
        // way, saturate toward `a`'s sign: `i64::MIN` if `a < 0`, else `i64::MAX`.
        let both = if sub {
            let ab = builder.ins().bxor(lv, rv);
            let ar = builder.ins().bxor(lv, raw);
            builder.ins().band(ab, ar)
        } else {
            let ar = builder.ins().bxor(lv, raw);
            let br = builder.ins().bxor(rv, raw);
            builder.ins().band(ar, br)
        };
        let overflowed = builder.ins().icmp_imm(IntCC::SignedLessThan, both, 0);
        let is_neg = builder.ins().icmp_imm(IntCC::SignedLessThan, lv, 0);
        let maxc = builder.ins().iconst(types::I64, i64::MAX);
        let minc = builder.ins().iconst(types::I64, i64::MIN);
        let sat = builder.ins().select(is_neg, minc, maxc);
        builder.ins().select(overflowed, sat, raw)
    } else if sub {
        // Unsigned u64 underflow: `a < b` borrows; saturate to `0`.
        let underflowed = builder.ins().icmp(IntCC::UnsignedLessThan, lv, rv);
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().select(underflowed, zero, raw)
    } else {
        // Unsigned u64 overflow: the wrapped sum is less than an operand.
        let overflowed = builder.ins().icmp(IntCC::UnsignedLessThan, raw, lv);
        let umax = builder.ins().iconst(types::I64, -1); // all ones = u64::MAX
        builder.ins().select(overflowed, umax, raw)
    }
}

/// Runtime builtins, declared lazily. Every builtin is an imported `aipl_*`
/// runtime function (cranelift-object emits an object symbol for *every*
/// declared import, used or not), so declaring them all up-front made each
/// program carry the whole builtin roster and made adding a builtin shift every
/// program's `binary size`. Instead, `id`/`import` declare a symbol only on
/// first reference and cache it, so a program's object carries exactly the
/// builtins it uses.
struct Builtins {
    /// `sym` → the module-level `FuncId` (declared once, shared across functions).
    imports: RefCell<HashMap<&'static str, FuncId>>,
    /// `FuncId` → the `FuncRef` for the *current* function under construction.
    /// A `FuncRef` is scoped to one `Function`; this is cleared at the start of
    /// each `define_*` call (before `module.clear_context` makes old refs stale).
    func_refs: RefCell<HashMap<FuncId, FuncRef>>,
}

/// Signature of a runtime import `sym`. All take/return i64 (pointers, ints, and
/// `bool`/`char` as i64); they differ only in arity and whether they return.
fn builtin_import_sig<M: Module>(module: &mut M, sym: &str) -> Signature {
    let sig = |params: usize, ret: bool| {
        let mut s = module.make_signature();
        for _ in 0..params {
            s.params.push(AbiParam::new(types::I64));
        }
        if ret {
            s.returns.push(AbiParam::new(types::I64));
        }
        s
    };
    match sym {
        "aipl_print" | "aipl_print_error" | "aipl_inc" | "aipl_dec" | "aipl_array_dec"
        | "aipl_arr_inc" | "aipl_count_insns" | "aipl_test_begin" => sig(1, false),
        // Test-runner hooks: `__test_end()`/`__test_begin(name)` return nothing;
        // `__test_summary()` returns the exit code; `__assert(cond, loc)`.
        "aipl_test_end" => sig(0, false),
        "aipl_test_summary" => sig(0, true),
        "aipl_assert" => sig(2, false),
        "aipl_arr_drop_str"
        | "aipl_arr_drop_arr"
        | "aipl_arr_retain_ptr"
        | "aipl_arr_drop_opt_str"
        | "aipl_arr_drop_opt_arr"
        | "aipl_arr_retain_opt"
        | "aipl_str_iter_init" => sig(2, false),
        "aipl_arr_load_bit" => sig(2, true),
        "aipl_arr_elem_ptr" => sig(3, true),
        // sret ptr + (program, args): writes the whole composite `Result` via
        // the hidden pointer, so it returns nothing.
        "aipl_execute_program" => sig(3, false),
        "aipl_str_alloc"
        | "aipl_i64_len"
        | "aipl_u64_len"
        | "aipl_str_len"
        | "aipl_str_is_all_whitespace"
        | "aipl_trim"
        | "aipl_trim_mut"
        | "aipl_str_hash"
        | "aipl_str_iter_next"
        | "aipl_read_file_to_string"
        | "aipl_str_reverse" => sig(1, true),
        "aipl_str_repeat"
        | "aipl_str_eq"
        | "aipl_str_starts_with"
        | "aipl_str_ends_with"
        | "aipl_str_contains"
        | "aipl_concat"
        | "aipl_concat_lazy"
        | "aipl_concat_mut"
        | "aipl_char_at"
        | "aipl_str_data"
        | "aipl_str_split"
        | "aipl_str_join"
        | "aipl_write_i64"
        | "aipl_write_u64"
        | "aipl_write_string_to_file" => sig(2, true),
        "aipl_write_bytes" | "aipl_array_new" | "aipl_array_with_cap" | "aipl_str_slice" => {
            sig(3, true)
        }
        "aipl_set_contains" | "aipl_dict_get" | "aipl_dict_contains_key" | "aipl_arr_reverse" => {
            sig(4, true)
        }
        "aipl_array_push" | "aipl_array_push_mut" => sig(5, true),
        "aipl_set_insert" | "aipl_set_union" | "aipl_set_union_mut" | "aipl_dict_insert"
        | "aipl_arr_slice" => sig(6, true),
        other => panic!("unknown builtin import symbol {other:?}"),
    }
}

impl Builtins {
    /// The `FuncId` for runtime import `sym`, declaring it (once) on first use.
    fn id<M: Module>(&self, module: &mut M, sym: &'static str) -> FuncId {
        if let Some(&id) = self.imports.borrow().get(sym) {
            return id;
        }
        let s = builtin_import_sig(module, sym);
        let id = module
            .declare_function(sym, Linkage::Import, &s)
            .unwrap_or_else(|e| panic!("declare builtin {sym}: {e}"));
        self.imports.borrow_mut().insert(sym, id);
        id
    }

    /// Import `sym` into `func` and return the call-ready `FuncRef`, reusing the
    /// cached ref if `sym` was already imported into this function.
    fn import<M: Module>(&self, module: &mut M, func: &mut Function, sym: &'static str) -> FuncRef {
        let id = self.id(module, sym);
        if let Some(&fref) = self.func_refs.borrow().get(&id) {
            return fref;
        }
        let fref = module.declare_func_in_func(id, func);
        self.func_refs.borrow_mut().insert(id, fref);
        fref
    }

    /// Clear the per-function `FuncRef` cache. Must be called at the start of
    /// every `define_*` function so stale refs from the previous `Function` are
    /// never reused (Cranelift clears `ctx.func` via `module.clear_context`).
    fn clear_func_cache(&self) {
        self.func_refs.borrow_mut().clear();
    }
}

fn register_builtins(funcs: &mut HashMap<String, FuncInfo>) -> Builtins {
    // Record the call-site-resolved builtins in `funcs` for type-checking and
    // method resolution. None are *declared* here — each `aipl_*` import is
    // declared lazily by `Builtins::id` on first reference (see the `Builtins`
    // doc), so a program carries only the builtin symbols it actually uses, and
    // adding a builtin doesn't shift every program's `binary size`.
    fn reg(
        funcs: &mut HashMap<String, FuncInfo>,
        name: &str,
        sym: &'static str,
        params: Vec<Type>,
        return_ty: Type,
        effects: &[&str],
    ) {
        funcs.insert(
            name.to_string(),
            FuncInfo {
                link: FuncLink::Builtin(sym),
                params,
                return_ty,
                effects: effects.iter().map(|s| s.to_string()).collect(),
                is_mutating: false,
                owned_params: Vec::new(),
            },
        );
    }
    // The user-facing builtins' call-site signatures come straight from the parsed
    // `BUILTIN_SIGNATURES` (the single source of truth) — only the mapping to each
    // runtime symbol is given here (mostly `__builtin_X` -> `aipl_X`, but e.g.
    // `split` -> `aipl_str_split`). Each is intercepted by a custom codegen arm, so
    // this `funcs` entry is only for call-site arg type-checking / method resolution.
    let decls = builtin_decls();
    let sig: HashMap<&str, &AstFn> = decls
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some((f.name.as_str(), f)),
            _ => None,
        })
        .collect();
    // (canonical builtin name, runtime symbol).
    const SIG_REGS: &[(&str, &str)] = &[
        ("__builtin_print", "aipl_print"),
        ("__builtin_split", "aipl_str_split"),
        ("__builtin_join", "aipl_str_join"),
        ("__builtin_read_file_to_string", "aipl_read_file_to_string"),
        (
            "__builtin_write_string_to_file",
            "aipl_write_string_to_file",
        ),
        ("__builtin_execute_program", "aipl_execute_program"),
        ("__builtin_trim", "aipl_trim"),
        ("__builtin_repeat", "aipl_str_repeat"),
        ("__builtin_is_all_whitespace", "aipl_str_is_all_whitespace"),
        // Test-runner hooks (used only by the `check` driver / `assert` lowering).
        ("__assert", "aipl_assert"),
        ("__test_begin", "aipl_test_begin"),
        ("__test_end", "aipl_test_end"),
        ("__test_summary", "aipl_test_summary"),
    ];
    for &(name, sym) in SIG_REGS {
        let f = sig
            .get(name)
            .unwrap_or_else(|| panic!("no BUILTIN_SIGNATURES entry for {name:?}"));
        let params: Vec<Type> = f.sig.param_types();
        let return_ty = f.sig.return_type();
        let effects: Vec<&str> = f.sig.effects.iter().map(String::as_str).collect();
        reg(funcs, name, sym, params, return_ty, &effects);
    }
    // Internal codegen helpers — `str + str` and the in-place `+`/`trim` variants
    // chosen at the call site. Not user builtins, so not in BUILTIN_SIGNATURES.
    reg(
        funcs,
        "__aipl_concat",
        "aipl_concat",
        vec![
            Type::Primitive(Primitive::Str),
            Type::Primitive(Primitive::Str),
        ],
        Type::Primitive(Primitive::Str),
        &[],
    );
    reg(
        funcs,
        "__aipl_concat_lazy",
        "aipl_concat_lazy",
        vec![
            Type::Primitive(Primitive::Str),
            Type::Primitive(Primitive::Str),
        ],
        Type::Primitive(Primitive::Str),
        &[],
    );
    reg(
        funcs,
        "__aipl_concat_mut",
        "aipl_concat_mut",
        vec![
            Type::Primitive(Primitive::Str),
            Type::Primitive(Primitive::Str),
        ],
        Type::Primitive(Primitive::Str),
        &[],
    );
    reg(
        funcs,
        "__aipl_trim_mut",
        "aipl_trim_mut",
        vec![Type::Primitive(Primitive::Str)],
        Type::Primitive(Primitive::Str),
        &[],
    );
    Builtins {
        imports: RefCell::new(HashMap::new()),
        func_refs: RefCell::new(HashMap::new()),
    }
}

/// Per-name entry in the function's local environment. Immutable bindings
/// (params, `let`) hold a cranelift Value directly; `let mut` bindings
/// live in an 8-byte stack slot so `set` can rewrite them in place.
///
/// A mut binding's type is held in a shared cell so `push` can *refine* it:
/// `mut a = []` starts as `__none__[]` and becomes e.g. `i64[]` after the
/// first `a.push(1)`. The cell is shared across env clones (which only ever
/// add bindings), so the refinement is visible to later statements.
#[derive(Clone)]
enum EnvBinding {
    Immut(Value, Type),
    /// A mutable binding in a stack slot. The `bool` is the *exclusive* flag:
    /// set when static analysis proved the binding's array is never aliased, so
    /// `push` may mutate it in place. Always false for non-array bindings.
    Mut(StackSlot, Rc<RefCell<Type>>, bool),
}

type Env = HashMap<String, EnvBinding>;

/// Read the current value of a binding (loading from its stack slot if
/// mut). All `Ident` lookups funnel through this so the rest of codegen
/// doesn't need to care about the storage model.
fn env_load(
    builder: &mut FunctionBuilder,
    name: &str,
    env: &Env,
    span: Span,
) -> Result<(Value, Type), Error> {
    let binding = env
        .get(name)
        .ok_or_else(|| Error::at(format!("unknown identifier {name:?}"), span.clone()))?;
    Ok(match binding {
        EnvBinding::Immut(v, t) => (*v, t.clone()),
        EnvBinding::Mut(slot, t, _) => {
            let v = builder.ins().stack_load(types::I64, *slot, 0);
            (v, t.borrow().clone())
        }
    })
}

/// Whether a value of type `actual` may flow where `expected` is wanted.
/// `none` (`Optional(__none__)`) and the empty array literal `[]`
/// (`Array(__none__)`) carry the placeholder element `__none__`, which unifies
/// with any concrete element in either direction — recursively through matching
/// optional/array layers, so e.g. `some(some(none))` (`__none__???`) fits
/// `i64???` and `[[]]` fits `i64[][]`.
fn coercible(actual: &Type, expected: &Type) -> bool {
    if actual == expected || is_none_inner(actual) || is_none_inner(expected) {
        return true;
    }
    // `str`, `Error`, and the internal concat-str all share a representation and
    // coerce freely among themselves (mirrors the checker's `Error`/`str` rule;
    // the concat-str of a `a + b` value fits a `str`/`Error` parameter). `char[]`
    // joins them too (`is_str_shaped`) — it shares `str`'s representation
    // entirely (see `is_char_array`), so it's a real bit-for-bit fit, not just
    // a logical one.
    if is_str_shaped(actual) && is_str_shaped(expected) {
        return true;
    }
    match (actual, expected) {
        (Type::Optional(a), Type::Optional(b)) => coercible(a, b),
        (Type::Array(a), Type::Array(b)) => coercible(a, b),
        (Type::Set(a), Type::Set(b)) => coercible(a, b),
        (Type::Dict(ak, av), Type::Dict(bk, bv)) => coercible(ak, bk) && coercible(av, bv),
        (Type::Result(ao, ae), Type::Result(bo, be)) => coercible(ao, bo) && coercible(ae, be),
        _ => false,
    }
}

/// Merge two branch/arm types (an `if`'s arms, a `match`'s arms): the common
/// type both coerce to, or `None` if they're incompatible. A `__none__` element
/// on either side takes the other's, recursively through matching layers (the
/// type-level counterpart of `coercible`).
fn merge_types(a: &Type, b: &Type) -> Option<Type> {
    if a == b || is_none_inner(b) {
        return Some(a.clone());
    }
    if is_none_inner(a) {
        return Some(b.clone());
    }
    // `Error` and `str` share a representation; their common type is a plain str.
    if (is_error(a) && *b == Type::Primitive(Primitive::Str))
        || (*a == Type::Primitive(Primitive::Str) && is_error(b))
    {
        return Some(Type::Primitive(Primitive::Str));
    }
    // `char[]` and `str` share a representation too (see `is_char_array`);
    // their common type is a plain str (`emit_eq` dispatches identically for
    // either — see `is_str_shaped` — so the choice is just a label).
    if (is_char_array(a) && *b == Type::Primitive(Primitive::Str))
        || (*a == Type::Primitive(Primitive::Str) && is_char_array(b))
    {
        return Some(Type::Primitive(Primitive::Str));
    }
    match (a, b) {
        (Type::Optional(x), Type::Optional(y)) => {
            Some(Type::Optional(Box::new(merge_types(x, y)?)))
        }
        (Type::Array(x), Type::Array(y)) => Some(Type::Array(Box::new(merge_types(x, y)?))),
        (Type::Set(x), Type::Set(y)) => Some(Type::Set(Box::new(merge_types(x, y)?))),
        (Type::Dict(xk, xv), Type::Dict(yk, yv)) => Some(Type::Dict(
            Box::new(merge_types(xk, yk)?),
            Box::new(merge_types(xv, yv)?),
        )),
        (Type::Result(xo, xe), Type::Result(yo, ye)) => Some(Type::Result(
            Box::new(merge_types(xo, yo)?),
            Box::new(merge_types(xe, ye)?),
        )),
        _ => None,
    }
}

fn expect_type(actual: &Type, expected: &Type, context: &str, span: Span) -> Result<(), Error> {
    if coercible(actual, expected) {
        return Ok(());
    }
    Err(Error::at(
        format!(
            "{context}: expected {}, got {}",
            type_name(expected),
            type_name(actual)
        ),
        span.clone(),
    ))
}

/// Reject binding a unit value (the result of a function that returns
/// nothing) to a name. Such a value can't be stored or used; the call
/// belongs in statement position instead (`print(x);`, not
/// `let _ = print(x);`).
fn reject_unit_binding(ty: &Type, name: &str, span: Span) -> Result<(), Error> {
    if is_unit(ty) {
        return Err(Error::at(
            format!(
                "cannot bind {name:?} to a value of type () — a function that returns nothing \
                 can't be assigned; call it as a statement instead (`expr;`)"
            ),
            span.clone(),
        ));
    }
    Ok(())
}

/// Replace whole-token occurrences of `from` in `s`, where "whole-token" means the
/// match is not immediately followed by an ASCII digit. This avoids replacing
/// "userextname7" inside "userextname70".
fn replace_whole_number_token(s: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(from) {
        let end = pos + from.len();
        if rest[end..].starts_with(|c: char| c.is_ascii_digit()) {
            // Part of a larger number-suffixed token; skip one character and retry.
            out.push_str(&rest[..pos + 1]);
            rest = &rest[pos + 1..];
        } else {
            out.push_str(&rest[..pos]);
            out.push_str(to);
            rest = &rest[end..];
        }
    }
    out.push_str(rest);
    out
}

/// Post-process a CLIF function's IR text to replace opaque `userextname<N>` tokens
/// (used by Cranelift's display for global-value symbol names) with the explicit
/// `u<ns>:<idx>` form for any entry that has `namespace == 1` (data-object refs).
///
/// Without this fix, round-tripping CLIF through text loses the data-symbol mapping:
/// `cranelift_reader::parse_functions` resolves `userextname<N>` by position in the
/// predeclared-names table (populated by `fn<K> = u0:M` declarations), which places
/// data refs at the wrong slot. Emitting `u1:<idx>` makes the reference self-describing
/// so the reader inserts the correct `UserExternalName { namespace: 1, index: idx }`.
fn fix_data_ref_names(func: &Function, ir: &str) -> String {
    let mut data_refs: Vec<(u32, u32)> = func
        .params
        .user_named_funcs()
        .iter()
        .filter_map(|(ref_, name)| {
            if name.namespace == 1 {
                Some((ref_.as_u32(), name.index))
            } else {
                None
            }
        })
        .collect();
    if data_refs.is_empty() {
        return ir.to_string();
    }
    // Process larger indices first so "userextname70" is handled before "userextname7".
    data_refs.sort_by(|(a, _), (b, _)| b.cmp(a));
    let mut result = ir.to_string();
    for (n, idx) in data_refs {
        let from = format!("userextname{n}");
        let to = format!("u1:{idx}");
        result = replace_whole_number_token(&result, &from, &to);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn define_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    id: FuncId,
    func: &aipl_mono::ConcreteFn,
    funcs: &HashMap<String, FuncInfo>,
    structs: &HashMap<String, TypeDef>,
    builtins: &Builtins,
    lit_ctr: &Cell<u32>,
    elem_rc: &RefCell<ElemRc>,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    // Parameters this instance owns (moved in): monomorphization marks them, so
    // they aren't dropped on entry-scope exit and `mut y = p` moves rather than
    // copies. Keyed by name for `Cx::owned_params`.
    let owned_params: HashSet<String> = func
        .params
        .iter()
        .filter(|p| p.owned)
        .map(|p| p.name.clone())
        .collect();
    // The body is checked against the *declared* return type; the *ABI* return
    // (what the signature emits) may differ for entry-style functions — a unit
    // `main` yields its i64 exit code, a mutating method its final `self`.
    let declared_ret = func.return_ty.clone().unwrap_or(Type::Unit);
    let abi_ret = abi_return_ty(func);
    let ret_composite = sret_size(&abi_ret, structs).is_some();
    let mutating = is_mutating_fn(func);
    let unit_main = is_unit_main(func);
    let error_main = is_error_main(func);
    build_signature(&mut ctx.func.signature, func, structs);

    // Source-variable legend, filled as bindings are created (params + locals) and
    // printed after the function below — see `Cx::bindings`. Declared out here so
    // it outlives the builder block (the builder borrows `ctx.func`, which the
    // legend's `display()` needs back).
    let bindings: RefCell<Vec<(String, String)>> = RefCell::new(Vec::new());
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let param_values: Vec<Value> = builder.block_params(entry).to_vec();
        // A struct return prepends a hidden sret pointer; the user's params
        // follow it.
        let (sret_val, user_params): (Option<Value>, &[Value]) = if ret_composite {
            (Some(param_values[0]), &param_values[1..])
        } else {
            (None, &param_values[..])
        };

        // Bind params. A `mut self` receiver lives in a reassignable slot so
        // the body can mutate it; its final value is returned. Heap params are
        // owned by the callee, so track them for release at function exit.
        let mut env: Env = HashMap::new();
        let mut scopes: Vec<Vec<Tracked>> = vec![Vec::new()];
        let mut self_slot: Option<StackSlot> = None;
        for (p, v) in func.params.iter().zip(user_params) {
            if p.mutable {
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(*v, slot, 0);
                env.insert(
                    p.name.clone(),
                    EnvBinding::Mut(slot, Rc::new(RefCell::new(p.ty.clone())), false),
                );
                bindings
                    .borrow_mut()
                    .push((p.name.clone(), format!("ss{}", slot.as_u32())));
                self_slot = Some(slot);
                // A `mut` array param's slot takes its own reference on the
                // current value (see `mut_binding_owns_slot_ref`), so a mutation
                // that replaces it inside a loop body stays owned across
                // iterations; the entry value-track below still keeps the entry
                // version alive to fn exit for borrows.
                if mut_binding_owns_slot_ref(&p.ty) {
                    emit_retain(&mut builder, module, builtins, structs, *v, &p.ty);
                    scopes[0].push(Tracked::slot(slot, &p.ty));
                }
            } else {
                env.insert(p.name.clone(), EnvBinding::Immut(*v, p.ty.clone()));
                bindings
                    .borrow_mut()
                    .push((p.name.clone(), format!("v{}", v.as_u32())));
            }
            // A heap parameter is owned by the callee and dropped at exit —
            // unless it's a moved-in owned param, whose ownership transfers to
            // the local it's moved into (so dropping it here would double-free).
            if is_heap(&p.ty) && !p.owned {
                scopes[0].push(Tracked::new(*v, &p.ty));
            }
        }

        let cx = Cx {
            env: &env,
            funcs,
            structs,
            builtins,
            effects: &func.effects,
            owned_params: &owned_params,
            lit_ctr,
            elem_rc,
            ret_ty: &abi_ret,
            sret: sret_val,
            error_main,
            bindings: &bindings,
        };
        let (body_ret, body_ty) = compile_expr(module, &mut builder, cx, &mut scopes, &func.body)?;
        // Enforce the declared return type. For a mutating method or unit
        // `main` that's unit, so a trailing value (an attempt to return one) is
        // an error. For a normal fn this also guards struct returns: the sret
        // copy below uses the declared layout.
        // A bare-literal body flexes to a narrow-int return type.
        let body_ty = aipl_syntax::flex_int_ty(&func.body, &body_ty, &declared_ret);
        expect_type(
            &body_ty,
            &declared_ret,
            "return value",
            func.body.span.clone(),
        )?;

        // The value actually returned: a mutating method yields its final
        // `self`; a unit `main`, 0; otherwise the body's value.
        let ret_val = if mutating {
            builder
                .ins()
                .stack_load(types::I64, self_slot.expect("mut self slot"), 0)
        } else if unit_main {
            builder.ins().iconst(types::I64, 0)
        } else if error_main {
            // Derive the exit code from the `!Error` result: ok() → 0; err(msg) →
            // print `error: <msg>` and 1. Read before the scope drop frees it.
            emit_error_main_exit_code(&mut builder, module, builtins, body_ret)
        } else {
            body_ret
        };

        // Hand the caller a ref on the returned value (retaining nested heap
        // for composites), then release this scope.
        if needs_drop(&abi_ret, structs) {
            emit_retain(&mut builder, module, builtins, structs, ret_val, &abi_ret);
        }
        let function_scope = scopes.pop().expect("function scope present");
        drop_scope(&mut builder, module, builtins, structs, function_scope);

        // Emit the return per the ABI return type: unit → nothing; a composite
        // (struct or optional) → copy into the caller's sret slot; else i64.
        if is_unit(&abi_ret) {
            builder.ins().return_(&[]);
        } else if ret_composite {
            let sret = sret_val.expect("composite return has an sret param");
            // Copy the *returned value's* own size: a `mut self` method yields
            // its receiver (`abi_ret`); otherwise the body value (which for an
            // optional may be a narrower `none` than the declared type).
            let src_ty = if mutating { &abi_ret } else { &body_ty };
            copy_composite(&mut builder, sret, ret_val, src_ty, structs);
            builder.ins().return_(&[]);
        } else {
            builder.ins().return_(&[ret_val]);
        }
        builder.finalize();
    }

    // Tag the function with its real `FuncId` so the printed IR header reads
    // `function u0:<id>` instead of the default `u0:0`. This makes each function
    // self-identify its id, which the dogfood-IR loader relies on to re-link the
    // checked-in CLIF (see `from_artifact`).
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    // Print the source-variable legend (params + locals → their CLIF value/slot)
    // as trailing comments, so a reader can map `v3`/`ss0` back to source names.
    // Comments are ignored by cranelift's reader, so checked-in `.clif` still loads.
    let legend = bindings.borrow();
    if !legend.is_empty() {
        ir_out.push_str("; source variables:\n");
        for (name, repr) in legend.iter() {
            ir_out.push_str(&format!(";   {repr} = {name}\n"));
        }
    }

    // Instrument *after* the IR dump (so `aipl ir` stays clean) and before
    // lowering: tally each basic block's instruction count when it executes.
    // Only when requested — JIT and production builds skip it (zero overhead).
    if instrument {
        let count_fn = builtins.id(module, "aipl_count_insns");
        instrument_insn_count(module, &mut ctx.func, count_fn);
    }

    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define {}: {e:?}", func.name)))?;
    module.clear_context(ctx);
    Ok(())
}

/// Instrument `func` to tally executed instructions: at the head of every basic
/// block, insert `aipl_count_insns(<that block's instruction count>)`. The count
/// per block is fixed at compile time, so the runtime sum over executed blocks
/// is a deterministic "CLIF instructions executed" measure (control flow is the
/// only thing that varies). The inserted `iconst`/`call` are added *after* the
/// per-block count is read, so instrumentation never counts itself. In a
/// non-instrumented build `aipl_count_insns` is a no-op forwarder.
fn instrument_insn_count<M: Module>(module: &mut M, func: &mut Function, count_fn: FuncId) {
    let fref = module.declare_func_in_func(count_fn, func);
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for block in blocks {
        let n = func.layout.block_insts(block).count() as i64;
        let Some(first) = func.layout.first_inst(block) else {
            continue; // unreachable: every block ends in a terminator
        };
        let mut pos = FuncCursor::new(func);
        pos.goto_inst(first);
        // Insert before `first`, in order: `n = iconst; call count_fn(n)`.
        let n_val = pos.ins().iconst(types::I64, n);
        pos.ins().call(fref, &[n_val]);
    }
}

/// True when a refcount op on the str-repr value `v` is statically known to be
/// a runtime no-op, so the `aipl_inc`/`aipl_dec` call would be pure overhead
/// and is elided. Recognized by the defining instruction:
///   - a constant that is null (`0`) or inline-tagged (`..01`, a packed <= 7
///     byte literal) — neither owns heap. A heap/view/rope pointer is never a
///     codegen-time constant, and a heap-tagged (`..00`) constant is excluded
///     anyway, so this can't misfire on a baked pointer;
///   - `symbol_value + STR_HEADER_SIZE` — a pointer into a static string
///     literal's data object, whose `STATIC_REFCOUNT` header makes the runtime
///     ignore every inc/dec on it (and which is never freed).
/// Best-effort by design: a literal that arrives through a block param, a
/// stack slot, or a component load isn't recognized, and its (no-op) rc call
/// is emitted exactly as before — eliding is only ever an optimization, never
/// required for balance, because rc ops on these representations don't count.
fn rc_statically_noop(func: &Function, v: Value) -> bool {
    use cranelift::codegen::ir::{instructions::InstructionData, Opcode, ValueDef};
    let ValueDef::Result(inst, _) = func.dfg.value_def(v) else {
        return false;
    };
    match func.dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => {
            let raw = imm.bits();
            raw == 0 || raw & TAG_MASK as i64 == INLINE_TAG as i64
        }
        InstructionData::BinaryImm64 {
            opcode: Opcode::IaddImm,
            arg,
            imm,
        } if imm.bits() == STR_HEADER_SIZE as i64 => {
            matches!(
                func.dfg.value_def(arg),
                ValueDef::Result(def, _)
                    if func.dfg.insts[def].opcode() == Opcode::SymbolValue
            )
        }
        _ => false,
    }
}

fn emit_inc<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    v: Value,
) {
    if rc_statically_noop(builder.func, v) {
        return;
    }
    let local = builtins.import(module, builder.func, "aipl_inc");
    builder.ins().call(local, &[v]);
}

/// Lower `s[i]` to a `char?`: call the runtime `aipl_char_at` (the byte at `i`
/// as `0..=255`, or `-1` out of bounds) and wrap
/// it into a flattened `{tag, value}` optional slot (tag = in-bounds, value = the
/// raw byte; unobservable when `none`). `s_v` is balanced with an `inc` because
/// the runtime consumes (decs) the receiver. Returns the slot address; the type
/// is `char?`.
fn emit_char_at<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    s_v: Value,
    i_v: Value,
) -> Value {
    emit_inc(builder, module, builtins, s_v);
    let local = builtins.import(module, builder.func, "aipl_char_at");
    let inst = builder.ins().call(local, &[s_v, i_v]);
    let raw = builder.inst_results(inst)[0];
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
    let is_some_b = builder.ins().icmp_imm(IntCC::NotEqual, raw, -1);
    let tag = builder.ins().uextend(types::I64, is_some_b);
    builder.ins().stack_store(tag, slot, 0);
    builder.ins().stack_store(raw, slot, 8);
    builder.ins().stack_addr(types::I64, slot, 0)
}

/// A heap-bearing value tracked for release at scope exit, paired with the
/// type needed to drop it. A `str` decs, an array decs (the runtime frees the
/// block — and, once elements can be heap, drops them via the block's drop-fn),
/// and a composite recursively drops its components.
/// What a tracking entry releases at scope exit. `Value` is a fixed pointer
/// (the common case). `Slot` re-loads a stack slot first — used for an
/// exclusive mutable array, whose pointer can change under in-place `push`
/// (a grow relocates the block), so we must drop whatever it points at *now*.
#[derive(Clone)]
enum Owned {
    Value(Value),
    Slot(StackSlot),
}

#[derive(Clone)]
struct Tracked {
    owned: Owned,
    ty: Type,
}

impl Tracked {
    fn new(val: Value, ty: &Type) -> Self {
        Tracked {
            owned: Owned::Value(val),
            ty: ty.clone(),
        }
    }
    fn slot(slot: StackSlot, ty: &Type) -> Self {
        Tracked {
            owned: Owned::Slot(slot),
            ty: ty.clone(),
        }
    }
}

/// Release every heap ref accumulated in a scope.
fn drop_scope<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    scope: Vec<Tracked>,
) {
    for t in scope {
        let v = match t.owned {
            Owned::Value(v) => v,
            Owned::Slot(slot) => builder.ins().stack_load(types::I64, slot, 0),
        };
        emit_drop(builder, module, builtins, structs, v, &t.ty);
    }
}

/// Whether a value of type `ty` owns any heap references that must be released
/// when it dies. Strings and arrays are heap; a struct/optional needs a drop
/// iff a component does.
fn needs_drop(ty: &Type, structs: &HashMap<String, TypeDef>) -> bool {
    match ty {
        // `str` (and `Error`, which shares its heap representation) is dropped
        // like a heap pointer; the other primitives own no heap.
        _ if is_str_repr(ty) => true,
        // An untyped `[]`/`none` element/core carries nothing to drop — it's
        // never actually a live value (an empty array holds no elements; a
        // bare `none` optional's payload is garbage), so this reaches codegen
        // (e.g. picking an empty array literal's element drop-fn) but is
        // always vacuously drop-free.
        Type::Primitive(_) | Type::Unit | Type::NoneInner => false,
        // Already handled by the `is_str_repr` guard above.
        Type::ConcatStr => unreachable!(),
        Type::Named(n) => match structs.get(n) {
            Some(TypeDef::Struct(s)) => s.fields.iter().any(|f| needs_drop(&f.ty, structs)),
            // A variant needs cleanup if any case's payload field does.
            Some(TypeDef::Variant(v)) => v
                .cases
                .iter()
                .any(|c| c.fields.iter().any(|f| needs_drop(&f.ty, structs))),
            None => false,
        },
        Type::Array(_) | Type::Set(_) | Type::Dict(_, _) => true,
        Type::Optional(inner) => needs_drop(inner, structs),
        // A result needs cleanup if either payload does (only the active one is
        // released, dispatched on the tag — see `emit_rc`).
        Type::Result(ok, err) => needs_drop(ok, structs) || needs_drop(err, structs),
        // Function types are erased by monomorphization; never a runtime value.
        Type::Fn(_, _) => false,
        // Tuple type annotations are lowered to Named by lower_tuples before codegen.
        Type::Tuple(_) => unreachable!("Type::Tuple must be lowered before codegen"),
        // `Any`/`EmptyArrayArg`/`NoneLiteralArg` are resolved away by
        // monomorphization (the latter two collapse to `Array`/`Optional` of
        // `NoneInner` — see `subst_vars`) — codegen never sees them directly.
        Type::Any | Type::EmptyArrayArg | Type::NoneLiteralArg => {
            unreachable!("compiler pseudo-type reached codegen")
        }
    }
}

/// A composite is stored *inline* and handled by the address of its storage
/// (struct, optional); scalars / `str` / arrays are 8-byte values.
fn is_composite(ty: &Type, structs: &HashMap<String, TypeDef>) -> bool {
    matches!(ty, Type::Optional(_) | Type::Result(_, _))
        || matches!(ty, Type::Named(n) if structs.contains_key(n))
}

/// Read the component of type `ty` at `base + offset`: an inline composite is
/// addressed (`base + offset`); a scalar/str/array is loaded as an i64.
fn component(
    builder: &mut FunctionBuilder,
    base: Value,
    offset: u32,
    ty: &Type,
    structs: &HashMap<String, TypeDef>,
) -> Value {
    if is_composite(ty, structs) {
        builder.ins().iadd_imm(base, offset as i64)
    } else {
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), base, offset as i32)
    }
}

/// Store a value `v` of static type `src_ty` into the slot at address `slot`. A
/// composite (an optional) is addressed by `v`, so copy its bytes; a scalar or
/// pointer is a single 8-byte value. Mirrors how `component` reads it back.
fn store_array_elem(
    builder: &mut FunctionBuilder,
    slot: Value,
    v: Value,
    src_ty: &Type,
    structs: &HashMap<String, TypeDef>,
) {
    if is_composite(src_ty, structs) {
        copy_composite(builder, slot, v, src_ty, structs);
    } else {
        builder.ins().store(MemFlags::trusted(), v, slot, 0);
    }
}

/// Write `some(x)` into the flattened optional slot at `slot` (size
/// `8 + sizeof(Core)`), where `x_val`/`x_ty` is the wrapped value. If `x` is
/// itself an optional, the tag is its tag + 1 and the shared core value is
/// carried through unchanged; otherwise `x` *is* the core and the tag is 1.
/// Does not retain — the slot now aliases the core heap, so the caller balances
/// ownership (typically `emit_retain(slot, Optional(x_ty))`, which incs the core
/// only when the result is fully `some`). Mirrors how `match`/`emit_render`
/// peel a layer.
fn emit_build_some(
    builder: &mut FunctionBuilder,
    slot: Value,
    x_val: Value,
    x_ty: &Type,
    structs: &HashMap<String, TypeDef>,
) {
    let core = opt_core(x_ty);
    let (tag, core_val) = match x_ty {
        // Wrapping an optional: bump its tag, reuse its core value (at offset 8).
        Type::Optional(_) => {
            let inner_tag = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), x_val, 0);
            let tag = builder.ins().iadd_imm(inner_tag, 1);
            let cv = component(builder, x_val, OPT_VALUE_OFFSET, core, structs);
            (tag, cv)
        }
        // Wrapping a non-optional: it is the core, tag 1.
        _ => (builder.ins().iconst(types::I64, 1), x_val),
    };
    builder.ins().store(MemFlags::trusted(), tag, slot, 0);
    let val_addr = builder.ins().iadd_imm(slot, OPT_VALUE_OFFSET as i64);
    store_array_elem(builder, val_addr, core_val, core, structs);
}

// ---------------------------------------------------------------------------
// Array element representation
//
// `bool[]` is bit-packed: 8 elements per byte (see the runtime's
// "bit-packed" note). Every other element type uses a byte stride of
// `elem_size_of`. The packing is decided in one place, `is_bit_packed`, and
// the runtime is told to bit-pack by passing an element size of 0
// (`runtime_elem_size`). Reads go through `load_array_elem` so indexing and
// `for`-loops share the same (un)packing logic.
// ---------------------------------------------------------------------------

/// Whether an array of element type `elem` is bit-packed (only plain `bool`).
/// A `bool?`, `bool[]`, or struct-with-`bool` element is *not* — packing is for
/// the bare `bool` element alone.
fn is_bit_packed(elem: &Type) -> bool {
    matches!(elem, Type::Primitive(Primitive::Bool))
}

/// Whether `ty` is `char[]` — the one array element type that shares `str`'s
/// runtime representation entirely (tag scheme, heap layout, refcounting)
/// rather than the generic array block, since a `char` is a single byte and
/// `str`'s content is just packed bytes (see the "Representation dispatch"
/// doc above `StrRepr`). `char[]` stays a distinct compile-time `Type` (it
/// still displays and type-checks as `char[]`, not `str`) — only its runtime
/// construction/access/refcounting is redirected to the `str` runtime, at
/// every site that would otherwise use the generic array runtime.
fn is_char_array(ty: &Type) -> bool {
    matches!(ty, Type::Array(elem) if **elem == Type::Primitive(Primitive::Char))
}

/// Whether a `mut` binding of type `ty` follows the slot-owned-reference model:
/// its stack slot holds one reference of its own on the binding's *current*
/// value (retained at declaration and at every replacement, released when
/// replaced), with a slot-track in the declaration scope releasing the final
/// value. Per-version value-tracks ("region tracks") are kept alongside, so a
/// non-retaining borrow (`let alias = a`) of any version stays valid until the
/// scope where that version was created exits. This is what keeps a
/// non-exclusive binding correct when a mutating call replaces it inside a
/// *loop* body: the per-iteration region track dies with the iteration, but the
/// slot's own reference carries the current value across iterations.
///
/// Only non-`char[]` arrays for now: `char[]` is str-shaped (different rc entry
/// points and an inline representation that isn't a pointer), `str` has its own
/// established slot model, and sets/dicts keep the value-track model until a
/// case demands otherwise.
fn mut_binding_owns_slot_ref(ty: &Type) -> bool {
    matches!(ty, Type::Array(_)) && !is_char_array(ty)
}

/// Whether `ty`'s runtime value is str-shaped: a real `str`/`Error`/concat-str
/// (`is_str_repr`), or `char[]` (`is_char_array`). Deliberately kept separate
/// from `is_str_repr` itself (rather than folding `char[]` into that shared
/// helper) so this only widens the specific array-construction/access/
/// refcounting dispatch sites that need it — not every one of `is_str_repr`'s
/// many other consumers (FFI marshaling, monomorphization's generic-instance
/// naming, the concat-str pseudo-type, etc.), which weren't audited for a
/// `char[]` value flowing through them.
fn is_str_shaped(ty: &Type) -> bool {
    is_str_repr(ty) || is_char_array(ty)
}

/// A bare `[]` literal has no element type to infer from, so it's built as the
/// generic (array-shaped) empty collection — `Type::Array(NoneInner)` — same
/// as an empty `i64[]`/`bool[]`/etc., since those all happen to share one
/// physical empty-array representation. `char[]` doesn't: it's str-shaped
/// (see `is_char_array`), so an empty array-shaped value passed where a
/// `char[]` is expected would misinterpret the header layout downstream. This
/// substitutes the canonical empty `str` value (inline, length 0) in that one
/// case, freeing the now-unused throwaway empty array block first. A known
/// narrow gap: only call sites that route through this (currently just
/// function-call arguments) get the fixup — an empty literal flowing into a
/// `char[]`-typed struct field or `return` isn't covered.
fn coerce_empty_to_char_array<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    v: Value,
    actual: &Type,
    expected: &Type,
) -> Value {
    let is_empty_placeholder = matches!(actual, Type::Array(inner) if is_none_inner(inner));
    if is_char_array(expected) && is_empty_placeholder {
        let dec = builtins.import(module, builder.func, "aipl_array_dec");
        builder.ins().call(dec, &[v]);
        builder.ins().iconst(types::I64, 1) // pack_inline(&[]): tag (0 << 2) | 1
    } else {
        v
    }
}

/// The `elem_size` argument handed to the array runtime: a byte stride, or the
/// sentinel 0 for a bit-packed `bool` array.
fn runtime_elem_size(elem: &Type, structs: &HashMap<String, TypeDef>) -> i64 {
    if is_bit_packed(elem) {
        0
    } else {
        elem_size_of(elem, structs)
    }
}

/// Read element `idx` of the array whose element region starts at
/// `arr_ptr + ARR_ELEMS_OFFSET`. Returns a bit-unpacked `bool` (0/1), a loaded
/// scalar/pointer, or the address of an inline composite. Used by indexing and
/// `for`-loops so both honor the element type's representation.
/// Strip array representation tag bits from a pointer in Cranelift IR.
/// This is the IR equivalent of `arr_untag` in the runtime.
fn arr_base(builder: &mut FunctionBuilder, arr_ptr: Value) -> Value {
    builder.ins().band_imm(arr_ptr, !(ARR_TAG_MASK as i64))
}

/// Load the length of an array (any repr) in Cranelift IR.  Strips the tag
/// before reading, because tagged pointers are not valid addresses.
fn load_arr_len(builder: &mut FunctionBuilder, arr_ptr: Value) -> Value {
    let u = arr_base(builder, arr_ptr);
    builder
        .ins()
        .load(types::I64, MemFlags::trusted(), u, ARR_LEN_OFFSET as i32)
}

fn load_array_elem<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    arr_ptr: Value,
    idx: Value,
    elem: &Type,
    structs: &HashMap<String, TypeDef>,
) -> Value {
    // Inline tag check: fast heap path, slow extern path for non-heap reprs.
    // Values are passed through a stack slot (not block params) so that the
    // two-path merge stays compatible with Cranelift's block-arg API.
    let val_slot = i64_slot(builder);
    let tag = builder.ins().band_imm(arr_ptr, ARR_TAG_MASK as i64);
    let is_heap = builder
        .ins()
        .icmp_imm(IntCC::Equal, tag, ARR_HEAP_TAG as i64);
    let heap_block = builder.create_block();
    let slow_block = builder.create_block();
    let merge = builder.create_block();
    builder
        .ins()
        .brif(is_heap, heap_block, &[], slow_block, &[]);

    // Fast path: heap array — untag and use inline arithmetic.
    builder.switch_to_block(heap_block);
    builder.seal_block(heap_block);
    let untagged = arr_base(builder, arr_ptr);
    let base = builder.ins().iadd_imm(untagged, ARR_ELEMS_OFFSET as i64);
    let heap_val = if is_bit_packed(elem) {
        let byte_off = builder.ins().ushr_imm(idx, 3);
        let byte_addr = builder.ins().iadd(base, byte_off);
        let byte = builder
            .ins()
            .load(types::I8, MemFlags::trusted(), byte_addr, 0);
        let byte = builder.ins().uextend(types::I64, byte);
        let bit_idx = builder.ins().band_imm(idx, 7);
        let shifted = builder.ins().ushr(byte, bit_idx);
        builder.ins().band_imm(shifted, 1)
    } else {
        let stride = elem_size_of(elem, structs);
        let off = builder.ins().imul_imm(idx, stride);
        let addr = builder.ins().iadd(base, off);
        if is_composite(elem, structs) {
            addr
        } else {
            builder.ins().load(types::I64, MemFlags::trusted(), addr, 0)
        }
    };
    builder.ins().stack_store(heap_val, val_slot, 0);
    builder.ins().jump(merge, &[]);

    // Slow path: non-heap repr — call runtime dispatch.
    builder.switch_to_block(slow_block);
    builder.seal_block(slow_block);
    let slow_val = if is_bit_packed(elem) {
        let f = builtins.import(module, builder.func, "aipl_arr_load_bit");
        let call = builder.ins().call(f, &[arr_ptr, idx]);
        builder.inst_results(call)[0]
    } else {
        let stride_v = builder
            .ins()
            .iconst(types::I64, elem_size_of(elem, structs));
        let f = builtins.import(module, builder.func, "aipl_arr_elem_ptr");
        let call = builder.ins().call(f, &[arr_ptr, idx, stride_v]);
        let addr = builder.inst_results(call)[0];
        if is_composite(elem, structs) {
            addr
        } else {
            builder.ins().load(types::I64, MemFlags::trusted(), addr, 0)
        }
    };
    builder.ins().stack_store(slow_val, val_slot, 0);
    builder.ins().jump(merge, &[]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    builder.ins().stack_load(types::I64, val_slot, 0)
}

/// Byte `idx` of a str-shaped `char[]` (see `is_char_array`), without
/// consuming it. `aipl_char_at` decs its receiver internally, so this
/// pre-incs to balance — mirroring `emit_char_at`, but returning the raw byte
/// (`idx` is trusted in-bounds; no optional wrapping) for callers that
/// already know the index is valid, like a fold over every element.
fn load_char_array_byte<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    arr_ptr: Value,
    idx: Value,
) -> Value {
    emit_inc(builder, module, builtins, arr_ptr);
    let f = builtins.import(module, builder.func, "aipl_char_at");
    let inst = builder.ins().call(f, &[arr_ptr, idx]);
    builder.inst_results(inst)[0]
}

/// Sequence length for a `str`-shaped `char[]` (see `is_char_array`) or a
/// real array/set/dict — the common "how many elements" query. Dispatches on
/// `ty` (not the runtime value), since `char[]` stays str-shaped
/// unconditionally.
fn seq_len<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    ptr: Value,
    ty: &Type,
) -> Value {
    if is_char_array(ty) {
        let f = builtins.import(module, builder.func, "aipl_str_len");
        let inst = builder.ins().call(f, &[ptr]);
        builder.inst_results(inst)[0]
    } else {
        load_arr_len(builder, ptr)
    }
}

/// Element `idx` of a `str`-shaped `char[]` (see `is_char_array`) or a real
/// array — the common "read element `idx`" query, mirroring `seq_len`.
/// `idx` is trusted in-bounds.
fn seq_elem<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    ptr: Value,
    idx: Value,
    arr_ty: &Type,
) -> Value {
    if is_char_array(arr_ty) {
        load_char_array_byte(module, builder, builtins, ptr, idx)
    } else if let Type::Array(elem) = arr_ty {
        load_array_elem(module, builder, builtins, ptr, idx, elem, structs)
    } else {
        unreachable!("seq_elem called with a non-array type")
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RcOp {
    Retain,
    Drop,
}

/// Retain (`aipl_inc`) or drop (`aipl_dec` / free) every heap reference
/// reachable from `v` of type `ty`. The discipline: storing a value into a
/// container or handing it to a callee *retains*; releasing the owner *drops*;
/// a composite recurses into its components.
fn emit_rc<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    ty: &Type,
    op: RcOp,
) {
    if !needs_drop(ty, structs) {
        return;
    }
    match ty {
        // `str` (and `Error`, a refcounted str pointer, and `char[]`, which
        // shares `str`'s representation — see `is_char_array`) — inc/dec the
        // pointer.
        _ if is_str_repr(ty) || is_char_array(ty) => {
            // Skip the call entirely when `v` is a literal the runtime would
            // ignore anyway (static/inline — see `rc_statically_noop`).
            if rc_statically_noop(builder.func, v) {
                return;
            }
            let sym = match op {
                RcOp::Retain => "aipl_inc",
                RcOp::Drop => "aipl_dec",
            };
            let local = builtins.import(module, builder.func, sym);
            builder.ins().call(local, &[v]);
        }
        // Other primitives own no heap (and `needs_drop` gated them out above).
        Type::Primitive(_) | Type::Unit => {}
        Type::Array(_) | Type::Set(_) | Type::Dict(_, _) => {
            // A set/dict shares the array heap block, so refcounting is
            // identical. Retain bumps the block's refcount (co-ownership of the
            // whole array — elements are untouched). Drop routes through
            // `aipl_array_dec`, which releases the elements via the drop-fn
            // stored in the array header before freeing (for a dict that drop-fn
            // releases each pair's key and value). Arrays use `aipl_arr_inc`
            // (not `aipl_inc`) because `aipl_inc` uses string tag dispatch and
            // would misread the array repr tag bits.
            let sym = match op {
                RcOp::Retain => "aipl_arr_inc",
                RcOp::Drop => "aipl_array_dec",
            };
            let local = builtins.import(module, builder.func, sym);
            builder.ins().call(local, &[v]);
        }
        Type::Named(n) => match structs.get(n) {
            Some(TypeDef::Struct(_)) => {
                // Recurse over the struct's heap-bearing fields. Clone the field
                // list so we don't borrow `structs` across the recursive calls.
                let fields: Vec<(u32, Type)> = structs[n]
                    .as_struct()
                    .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                    .unwrap_or_default();
                for (offset, fty) in fields {
                    if needs_drop(&fty, structs) {
                        let fv = component(builder, v, offset, &fty, structs);
                        emit_rc(builder, module, builtins, structs, fv, &fty, op);
                    }
                }
            }
            // A variant: dispatch on the runtime tag, then recurse over the
            // active case's heap fields (only that case's payload is live).
            Some(TypeDef::Variant(_)) => {
                emit_variant_rc(builder, module, builtins, structs, v, n, op);
            }
            None => {}
        },
        Type::Optional(_) => {
            // The flattened slot holds {tag, core}; the core heap is owned only
            // when the whole chain is `some` (tag == depth). `some^k(none)` for
            // k < depth carries no heap, so its garbage value is never touched.
            let depth = opt_depth(ty) as i64;
            let core = opt_core(ty);
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), v, 0);
            let full = builder.ins().icmp_imm(IntCC::Equal, tag, depth);
            let then_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(full, then_b, &[], merge, &[]);
            builder.switch_to_block(then_b);
            builder.seal_block(then_b);
            let core_v = component(builder, v, OPT_VALUE_OFFSET, core, structs);
            emit_rc(builder, module, builtins, structs, core_v, core, op);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
        }
        Type::Result(ok_ty, err_ty) => {
            // tag 1 = Ok, 0 = Err; the 8-byte value at `OPT_VALUE_OFFSET` holds
            // the active payload. Release/retain whichever side is live (and only
            // when that side carries heap).
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), v, 0);
            let rc_side = |b: &mut FunctionBuilder, m: &mut M, want_tag: i64, side: &Type| {
                if !needs_drop(side, structs) {
                    return;
                }
                let is_side = b.ins().icmp_imm(IntCC::Equal, tag, want_tag);
                let then_b = b.create_block();
                let merge = b.create_block();
                b.ins().brif(is_side, then_b, &[], merge, &[]);
                b.switch_to_block(then_b);
                b.seal_block(then_b);
                let sv = component(b, v, OPT_VALUE_OFFSET, side, structs);
                emit_rc(b, m, builtins, structs, sv, side, op);
                b.ins().jump(merge, &[]);
                b.switch_to_block(merge);
                b.seal_block(merge);
            };
            rc_side(builder, module, 1, ok_ty);
            rc_side(builder, module, 0, err_ty);
        }
        // Unreachable: `needs_drop` returns false for function types (erased by
        // monomorphization), so this arm is guarded out above.
        Type::Fn(_, _) => {}
        // Tuple type annotations are lowered to Named by lower_tuples before codegen.
        Type::Tuple(_) => unreachable!("Type::Tuple must be lowered before codegen"),
        // Already handled by the `is_str_repr` guard above.
        Type::ConcatStr => unreachable!(),
        // `needs_drop` panics on these (resolved away by monomorphization), so
        // the guard above already returned.
        Type::Any | Type::NoneInner | Type::EmptyArrayArg | Type::NoneLiteralArg => {
            unreachable!()
        }
    }
}

/// Retain/drop the heap payload of a variant value at `v`: branch on the runtime
/// tag and recurse into the active case's heap-bearing fields (the other cases'
/// payload regions are inactive and must not be touched).
fn emit_variant_rc<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    name: &str,
    op: RcOp,
) {
    // Clone (tag, heap fields) per case so we don't borrow `structs` across the
    // recursive `emit_rc` calls. Skip cases with no heap payload.
    let cases: Vec<(usize, Vec<(u32, Type)>)> = structs[name]
        .as_variant()
        .map(|vl| {
            vl.cases
                .iter()
                .enumerate()
                .filter_map(|(tag, c)| {
                    let hf: Vec<(u32, Type)> = c
                        .fields
                        .iter()
                        .filter(|f| needs_drop(&f.ty, structs))
                        .map(|f| (f.offset, f.ty.clone()))
                        .collect();
                    (!hf.is_empty()).then_some((tag, hf))
                })
                .collect()
        })
        .unwrap_or_default();
    if cases.is_empty() {
        return;
    }
    let tag = builder.ins().load(types::I64, MemFlags::trusted(), v, 0);
    let done = builder.create_block();
    for (k, fields) in cases {
        let case_b = builder.create_block();
        let next_b = builder.create_block();
        let is_k = builder.ins().icmp_imm(IntCC::Equal, tag, k as i64);
        builder.ins().brif(is_k, case_b, &[], next_b, &[]);
        builder.switch_to_block(case_b);
        builder.seal_block(case_b);
        for (offset, fty) in fields {
            let fv = component(builder, v, offset, &fty, structs);
            emit_rc(builder, module, builtins, structs, fv, &fty, op);
        }
        builder.ins().jump(done, &[]);
        builder.switch_to_block(next_b);
        builder.seal_block(next_b);
    }
    builder.ins().jump(done, &[]);
    builder.switch_to_block(done);
    builder.seal_block(done);
}

/// A fresh 8-byte, 8-aligned stack slot — used by `emit_eq` to carry a running
/// 0/1 result (and a loop index) across the blocks its composite branches make.
fn i64_slot(builder: &mut FunctionBuilder) -> StackSlot {
    builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3))
}

/// Emit `self.starts_with(other)` / `self.ends_with(other)` (`is_ends`) for two
/// arrays of element type `elem`, returning an `i64` 0/1. True iff `other`'s
/// elements equal a contiguous prefix (`!is_ends`) or suffix (`is_ends`) of
/// `self`'s — i.e. `other` is no longer than `self` and every element matches at
/// the aligned offset. Borrows both arrays (`emit_eq` balances its own per-
/// element refs, like the array branch of `==`).
fn emit_arr_starts_ends<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    self_ptr: Value,
    other_ptr: Value,
    elem: &Type,
    is_ends: bool,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let la = load_arr_len(builder, self_ptr);
    let lb = load_arr_len(builder, other_ptr);
    // A pattern longer than the source can't be a prefix/suffix.
    let fits = builder.ins().icmp(IntCC::SignedLessThanOrEqual, lb, la);
    // Both arrays are the untyped empty literal (`[].starts_with([])`): there's
    // no element type to compare, so length-fits is the whole answer (and `lb`
    // is 0, so it's `true`). Skip the element loop — `emit_eq` can't lower a
    // `__none__` element.
    if is_none_inner(elem) {
        return Ok(builder.ins().uextend(types::I64, fits));
    }
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, res, 0);
    let pre = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(fits, pre, &[], merge, &[]);

    builder.switch_to_block(pre);
    builder.seal_block(pre);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(one, res, 0); // optimistic: all matched
                                            // `ends_with` compares `self[la - lb + i]` to `other[i]`; `starts_with`
                                            // uses offset 0.
    let offset = if is_ends {
        builder.ins().isub(la, lb)
    } else {
        zero
    };
    let idx = i64_slot(builder);
    builder.ins().stack_store(zero, idx, 0);
    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, lb);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    let si = builder.ins().iadd(offset, i);
    let el = load_array_elem(module, builder, builtins, self_ptr, si, elem, structs);
    let er = load_array_elem(module, builder, builtins, other_ptr, i, elem, structs);
    let ee = emit_eq(module, builder, builtins, structs, el, er, elem)?;
    let cont = builder.create_block();
    let neq = builder.create_block();
    builder.ins().brif(ee, cont, &[], neq, &[]);
    builder.switch_to_block(neq);
    builder.seal_block(neq);
    builder.ins().stack_store(zero, res, 0);
    builder.ins().jump(exit, &[]);
    builder.switch_to_block(cont);
    builder.seal_block(cont);
    let next = builder.ins().iadd_imm(i, 1);
    builder.ins().stack_store(next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, res, 0))
}

/// Emit a structural equality test for two values of type `ty` and return an
/// `i64` 0/1. The checker guarantees both operands share `ty` (up to `none`/
/// empty-collection coercion), so this dispatches purely on `ty`:
///   - scalars (i64/bool/char): `icmp eq`
///   - str: `aipl_str_eq` (inc each input first — it consumes a ref)
///   - optional: tags equal, and (when both fully `some`) the cores equal
///   - array: same length, then elementwise-equal in order
///   - set: same length, then every left element is in the right set
///     (order-independent — via the runtime `aipl_set_contains` scan)
///   - struct: every field equal
///   - variant: same tag, then the active case's payload fields equal
///
/// Both operands are borrowed; `str_eq`'s consumed refs are balanced by the
/// incs, so no scope tracking is needed.
fn emit_eq<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    lv: Value,
    rv: Value,
    ty: &Type,
) -> Result<Value, Error> {
    Ok(match ty {
        // All integer widths (and bool/char) compare by their canonical i64
        // register value — distinct values have distinct canonical reps.
        Type::Primitive(p) if !matches!(p, Primitive::Str) => {
            let b = builder.ins().icmp(IntCC::Equal, lv, rv);
            builder.ins().uextend(types::I64, b)
        }
        // `str` (and `Error`, and `char[]` — see `is_str_shaped`) compares by
        // its byte content.
        _ if is_str_shaped(ty) => {
            // `str_eq` consumes a ref from each input; these are borrowed, so inc
            // first to keep them owned by whoever owns them.
            emit_inc(builder, module, builtins, lv);
            emit_inc(builder, module, builtins, rv);
            let f = builtins.import(module, builder.func, "aipl_str_eq");
            let inst = builder.ins().call(f, &[lv, rv]);
            builder.inst_results(inst)[0]
        }
        Type::Optional(_) => {
            let depth = opt_depth(ty) as i64;
            let core = opt_core(ty);
            let tl = builder.ins().load(types::I64, MemFlags::trusted(), lv, 0);
            let tr = builder.ins().load(types::I64, MemFlags::trusted(), rv, 0);
            let tags_eq = builder.ins().icmp(IntCC::Equal, tl, tr);
            let res = i64_slot(builder);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, res, 0);
            let then_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(tags_eq, then_b, &[], merge, &[]);
            builder.switch_to_block(then_b);
            builder.seal_block(then_b);
            // Tags equal. The cores matter only when the chain is fully `some`
            // (tag == depth); a `none` at some layer makes tag equality decisive.
            let is_full = builder.ins().icmp_imm(IntCC::Equal, tl, depth);
            let core_b = builder.create_block();
            let one_b = builder.create_block();
            builder.ins().brif(is_full, core_b, &[], one_b, &[]);
            builder.switch_to_block(one_b);
            builder.seal_block(one_b);
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(one, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(core_b);
            builder.seal_block(core_b);
            let cl = component(builder, lv, OPT_VALUE_OFFSET, core, structs);
            let cr = component(builder, rv, OPT_VALUE_OFFSET, core, structs);
            let ce = emit_eq(module, builder, builtins, structs, cl, cr, core)?;
            builder.ins().stack_store(ce, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, res, 0)
        }
        Type::Array(elem) => {
            let ll = load_arr_len(builder, lv);
            let rl = load_arr_len(builder, rv);
            let len_eq = builder.ins().icmp(IntCC::Equal, ll, rl);
            // Both empty (untyped element) → length equality is the whole answer,
            // and there's no element type to recurse into.
            if is_none_inner(elem) {
                builder.ins().uextend(types::I64, len_eq)
            } else {
                let res = i64_slot(builder);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(zero, res, 0);
                let pre = builder.create_block();
                let merge = builder.create_block();
                builder.ins().brif(len_eq, pre, &[], merge, &[]);
                builder.switch_to_block(pre);
                builder.seal_block(pre);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().stack_store(one, res, 0); // optimistic: all-equal
                let idx = i64_slot(builder);
                builder.ins().stack_store(zero, idx, 0);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, ll);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                let el = load_array_elem(module, builder, builtins, lv, i, elem, structs);
                let er = load_array_elem(module, builder, builtins, rv, i, elem, structs);
                let ee = emit_eq(module, builder, builtins, structs, el, er, elem)?;
                let cont = builder.create_block();
                let neq = builder.create_block();
                builder.ins().brif(ee, cont, &[], neq, &[]);
                builder.switch_to_block(neq);
                builder.seal_block(neq);
                builder.ins().stack_store(zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cont);
                builder.seal_block(cont);
                let next = builder.ins().iadd_imm(i, 1);
                builder.ins().stack_store(next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(merge);
                builder.seal_block(merge);
                builder.ins().stack_load(types::I64, res, 0)
            }
        }
        Type::Set(elem) => {
            // Order-independent: same length and every element of the left set is
            // a member of the right (distinct elements + equal sizes ⇒ equal sets).
            let ll = load_arr_len(builder, lv);
            let rl = load_arr_len(builder, rv);
            let len_eq = builder.ins().icmp(IntCC::Equal, ll, rl);
            if is_none_inner(elem) {
                builder.ins().uextend(types::I64, len_eq)
            } else {
                let res = i64_slot(builder);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(zero, res, 0);
                let pre = builder.create_block();
                let merge = builder.create_block();
                builder.ins().brif(len_eq, pre, &[], merge, &[]);
                builder.switch_to_block(pre);
                builder.seal_block(pre);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().stack_store(one, res, 0);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(elem, structs));
                let idx = i64_slot(builder);
                builder.ins().stack_store(zero, idx, 0);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, ll);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                let el = load_array_elem(module, builder, builtins, lv, i, elem, structs);
                let xslot = i64_slot(builder);
                builder.ins().stack_store(el, xslot, 0);
                let xptr = builder.ins().stack_addr(types::I64, xslot, 0);
                let f = builtins.import(module, builder.func, "aipl_set_contains");
                let str_cmp = builder.ins().iconst(
                    types::I64,
                    i64::from(**elem == Type::Primitive(Primitive::Str)),
                );
                let inst = builder.ins().call(f, &[rv, xptr, esz, str_cmp]);
                let c = builder.inst_results(inst)[0];
                let cont = builder.create_block();
                let missing = builder.create_block();
                builder.ins().brif(c, cont, &[], missing, &[]);
                builder.switch_to_block(missing);
                builder.seal_block(missing);
                builder.ins().stack_store(zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cont);
                builder.seal_block(cont);
                let next = builder.ins().iadd_imm(i, 1);
                builder.ins().stack_store(next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(merge);
                builder.seal_block(merge);
                builder.ins().stack_load(types::I64, res, 0)
            }
        }
        Type::Named(n) if structs.get(n).and_then(TypeDef::as_struct).is_some() => {
            // Clone field (offset, type) pairs so we don't borrow `structs` across
            // the recursive calls. AND-fold every field's equality.
            let fields: Vec<(u32, Type)> = structs[n]
                .as_struct()
                .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                .unwrap_or_default();
            let res = i64_slot(builder);
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(one, res, 0);
            for (offset, fty) in fields {
                let fl = component(builder, lv, offset, &fty, structs);
                let fr = component(builder, rv, offset, &fty, structs);
                let fe = emit_eq(module, builder, builtins, structs, fl, fr, &fty)?;
                let cur = builder.ins().stack_load(types::I64, res, 0);
                let new = builder.ins().band(cur, fe);
                builder.ins().stack_store(new, res, 0);
            }
            builder.ins().stack_load(types::I64, res, 0)
        }
        Type::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            // Clone each case's (tag, payload fields) so we don't borrow `structs`
            // across the recursive calls.
            let cases: Vec<(usize, Vec<(u32, Type)>)> = structs[n]
                .as_variant()
                .map(|vl| {
                    vl.cases
                        .iter()
                        .enumerate()
                        .map(|(tag, c)| {
                            (
                                tag,
                                c.fields.iter().map(|f| (f.offset, f.ty.clone())).collect(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let tl = builder.ins().load(types::I64, MemFlags::trusted(), lv, 0);
            let tr = builder.ins().load(types::I64, MemFlags::trusted(), rv, 0);
            let tags_eq = builder.ins().icmp(IntCC::Equal, tl, tr);
            let res = i64_slot(builder);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, res, 0);
            let then_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(tags_eq, then_b, &[], merge, &[]);
            builder.switch_to_block(then_b);
            builder.seal_block(then_b);
            // Same tag ⇒ equal unless the active case carries a payload to compare;
            // a nullary/no-field case is already equal (res stays 1).
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(one, res, 0);
            for (k, fields) in cases {
                if fields.is_empty() {
                    continue;
                }
                let case_b = builder.create_block();
                let next_b = builder.create_block();
                let is_k = builder.ins().icmp_imm(IntCC::Equal, tl, k as i64);
                builder.ins().brif(is_k, case_b, &[], next_b, &[]);
                builder.switch_to_block(case_b);
                builder.seal_block(case_b);
                for (offset, fty) in fields {
                    let fl = component(builder, lv, offset, &fty, structs);
                    let fr = component(builder, rv, offset, &fty, structs);
                    let fe = emit_eq(module, builder, builtins, structs, fl, fr, &fty)?;
                    let cur = builder.ins().stack_load(types::I64, res, 0);
                    let new = builder.ins().band(cur, fe);
                    builder.ins().stack_store(new, res, 0);
                }
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(next_b);
                builder.seal_block(next_b);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, res, 0)
        }
        Type::Dict(k, v) => {
            // Equal iff same length and every left pair's key is bound in the
            // right to an equal value (distinct keys + equal sizes ⇒ equal maps).
            let ll = load_arr_len(builder, lv);
            let rl = load_arr_len(builder, rv);
            let len_eq = builder.ins().icmp(IntCC::Equal, ll, rl);
            if is_none_inner(k) {
                builder.ins().uextend(types::I64, len_eq)
            } else {
                let pair_size = 8 + elem_size_of(v, structs);
                let str_cmp = builder.ins().iconst(
                    types::I64,
                    i64::from(**k == Type::Primitive(Primitive::Str)),
                );
                let psz = builder.ins().iconst(types::I64, pair_size);
                let res = i64_slot(builder);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(zero, res, 0);
                let pre = builder.create_block();
                let merge = builder.create_block();
                builder.ins().brif(len_eq, pre, &[], merge, &[]);
                builder.switch_to_block(pre);
                builder.seal_block(pre);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().stack_store(one, res, 0); // optimistic
                let idx = i64_slot(builder);
                builder.ins().stack_store(zero, idx, 0);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, ll);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                // Address of left pair `i`, its key (offset 0) and value (8).
                let lv_base = arr_base(builder, lv);
                let lelems = builder.ins().iadd_imm(lv_base, ARR_ELEMS_OFFSET as i64);
                let off = builder.ins().imul_imm(i, pair_size);
                let lpair = builder.ins().iadd(lelems, off);
                let key = component(builder, lpair, 0, k, structs);
                let kslot = i64_slot(builder);
                builder.ins().stack_store(key, kslot, 0);
                let kptr = builder.ins().stack_addr(types::I64, kslot, 0);
                let f = builtins.import(module, builder.func, "aipl_dict_get");
                let inst = builder.ins().call(f, &[rv, kptr, psz, str_cmp]);
                let rslot = builder.inst_results(inst)[0];
                let found = builder.ins().icmp_imm(IntCC::NotEqual, rslot, 0);
                let cmp_b = builder.create_block();
                let missing = builder.create_block();
                builder.ins().brif(found, cmp_b, &[], missing, &[]);
                builder.switch_to_block(missing);
                builder.seal_block(missing);
                builder.ins().stack_store(zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cmp_b);
                builder.seal_block(cmp_b);
                // Right value is at the slot's offset 0; left at the pair's 8.
                let lval = component(builder, lpair, 8, v, structs);
                let rval = component(builder, rslot, 0, v, structs);
                let ve = emit_eq(module, builder, builtins, structs, lval, rval, v)?;
                let cont = builder.create_block();
                let neq = builder.create_block();
                builder.ins().brif(ve, cont, &[], neq, &[]);
                builder.switch_to_block(neq);
                builder.seal_block(neq);
                builder.ins().stack_store(zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cont);
                builder.seal_block(cont);
                let next = builder.ins().iadd_imm(i, 1);
                builder.ins().stack_store(next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(merge);
                builder.seal_block(merge);
                builder.ins().stack_load(types::I64, res, 0)
            }
        }
        Type::Result(ok_ty, err_ty) => {
            // Equal iff same tag and the active payload (by that side's type)
            // is equal. tag 1 = Ok, 0 = Err; both payloads live at the value
            // offset.
            let tl = builder.ins().load(types::I64, MemFlags::trusted(), lv, 0);
            let tr = builder.ins().load(types::I64, MemFlags::trusted(), rv, 0);
            let tags_eq = builder.ins().icmp(IntCC::Equal, tl, tr);
            let res = i64_slot(builder);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, res, 0);
            let cmp_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(tags_eq, cmp_b, &[], merge, &[]);
            builder.switch_to_block(cmp_b);
            builder.seal_block(cmp_b);
            // Tags equal: compare the live payload by the matching side's type.
            let is_ok = builder.ins().icmp_imm(IntCC::Equal, tl, 1);
            let ok_b = builder.create_block();
            let err_b = builder.create_block();
            builder.ins().brif(is_ok, ok_b, &[], err_b, &[]);
            // A side that is unit (void-Ok `!E`) carries no payload, and a
            // `__none__` side is unconstructible (so its branch is dead) — either
            // way equal tags suffice, so compare the payload only for real types.
            let payload_trivial = |t: &Type| is_unit(t) || is_none_inner(t);
            builder.switch_to_block(ok_b);
            builder.seal_block(ok_b);
            let e = if payload_trivial(ok_ty) {
                builder.ins().iconst(types::I64, 1)
            } else {
                let lo = component(builder, lv, OPT_VALUE_OFFSET, ok_ty, structs);
                let ro = component(builder, rv, OPT_VALUE_OFFSET, ok_ty, structs);
                emit_eq(module, builder, builtins, structs, lo, ro, ok_ty)?
            };
            builder.ins().stack_store(e, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(err_b);
            builder.seal_block(err_b);
            let e = if payload_trivial(err_ty) {
                builder.ins().iconst(types::I64, 1)
            } else {
                let le = component(builder, lv, OPT_VALUE_OFFSET, err_ty, structs);
                let re = component(builder, rv, OPT_VALUE_OFFSET, err_ty, structs);
                emit_eq(module, builder, builtins, structs, le, re, err_ty)?
            };
            builder.ins().stack_store(e, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, res, 0)
        }
        other => {
            return Err(Error::msg(format!(
                "equality is not supported for type {}",
                type_name(other)
            )));
        }
    })
}

/// The tag (and, for a variant, the field layout) a literal `ok(..)`/`err(..)`/
/// named-variant-case call would build, resolved without building it.
enum CtorShape {
    /// Result's `ok`/`err`: the payload type isn't known from the call alone
    /// (`Result` isn't in `structs`) — it's resolved from the other side's
    /// concrete `Result(ok_ty, err_ty)` once that side is compiled.
    Result { is_ok: bool },
    /// A user-defined variant case, resolved statically via `variant_ctor`.
    Variant {
        tag: usize,
        fields: Vec<(u32, Type)>,
    },
}

/// Recognizes `e` as a literal `ok(..)`/`err(..)`/named-variant-case
/// constructor — either call syntax (`Circle(1)`) or, for a nullary case, a
/// bare unshadowed identifier (`Empty`, mirroring how `ExprKind::Ident`
/// codegen itself resolves one) — purely from its AST shape, no codegen.
/// `None` if it's neither.
fn ctor_shape(cx: Cx, e: &Expr) -> Option<CtorShape> {
    match &e.kind {
        ExprKind::Call(name, _, _) => {
            if name == "ok" || name == "err" {
                return Some(CtorShape::Result {
                    is_ok: name == "ok",
                });
            }
            let (_, tag, fields) = variant_ctor(cx.structs, name)?;
            Some(CtorShape::Variant { tag, fields })
        }
        ExprKind::Ident(name) if !cx.env.contains_key(name) => {
            let (_, tag, fields) = variant_ctor(cx.structs, name)?;
            Some(CtorShape::Variant { tag, fields })
        }
        _ => None,
    }
}

/// Fast path for `x == Ctor(..)` (either order, and `!=` too), where `Ctor` is
/// `ok`/`err` or any user-defined variant case: compare the *other* side's tag
/// directly against the constructor's known tag, and only compile/compare its
/// fields (directly, never wrapped in the constructor) when the tags match —
/// instead of materializing a synthetic value for the constructor side just to
/// have `emit_eq` immediately load its tag back out and walk it apart. Returns
/// `None` when neither/both sides are such a literal, decided purely from the
/// AST shape before anything is compiled, so the caller can fall back to the
/// generic path with no risk of double-compiling an operand; once it commits
/// past that check it always returns `Some(..)` or a genuine `Err`.
fn compile_ctor_eq<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    op: char,
    l: &Expr,
    r: &Expr,
) -> Result<Option<(Value, Type)>, Error> {
    let structs = cx.structs;
    let builtins = cx.builtins;
    let (other, ctor_expr, shape) = match (ctor_shape(cx, l), ctor_shape(cx, r)) {
        (Some(shape), None) => (r, l, shape),
        (None, Some(shape)) => (l, r, shape),
        _ => return Ok(None),
    };
    // A bare nullary-case identifier (`Empty`) takes no arguments, mirroring
    // how its own `ExprKind::Ident` codegen constructs it with `&[]`.
    let ctor_args: &[Expr] = match &ctor_expr.kind {
        ExprKind::Call(_, args, _) => args.as_slice(),
        ExprKind::Ident(_) => &[],
        _ => unreachable!("ctor_shape only matches Call/Ident expressions"),
    };
    let opn = if op == 'E' { "==" } else { "!=" };
    let (ov, ot) = compile_expr(module, builder, cx, scopes, other)?;
    let (expect_tag, fields) = match shape {
        CtorShape::Result { is_ok } => {
            let Type::Result(ok_ty, err_ty) = &ot else {
                return Err(Error::at(
                    format!(
                        "\"{opn}\" between a result and {}: both sides must be the same type",
                        type_name(&ot)
                    ),
                    other.span.clone(),
                ));
            };
            let payload_ty = if is_ok {
                (**ok_ty).clone()
            } else {
                (**err_ty).clone()
            };
            // A trivial payload (void `ok()`, or an unconstructible `__none__`
            // side) carries nothing to compare — matching tags alone means equal.
            let fields = if is_unit(&payload_ty) || is_none_inner(&payload_ty) {
                vec![]
            } else {
                vec![(OPT_VALUE_OFFSET, payload_ty)]
            };
            (i64::from(is_ok), fields)
        }
        CtorShape::Variant { tag, fields } => {
            if !matches!(&ot, Type::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some())
            {
                return Err(Error::at(
                    format!(
                        "\"{opn}\" between a variant and {}: both sides must be the same type",
                        type_name(&ot)
                    ),
                    other.span.clone(),
                ));
            }
            if fields.len() != ctor_args.len() {
                return Err(Error::at(
                    format!(
                        "variant constructor takes {} argument(s), got {}",
                        fields.len(),
                        ctor_args.len()
                    ),
                    ctor_expr.span.clone(),
                ));
            }
            (tag as i64, fields)
        }
    };
    let tag = builder.ins().load(types::I64, MemFlags::trusted(), ov, 0);
    let tag_matches = builder.ins().icmp_imm(IntCC::Equal, tag, expect_tag);
    // A tagless match (void `ok()`, or a nullary variant case) carries nothing
    // to compare — matching tags alone means equal.
    let eq = if fields.is_empty() {
        builder.ins().uextend(types::I64, tag_matches)
    } else {
        let res = i64_slot(builder);
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().stack_store(zero, res, 0);
        let cmp_b = builder.create_block();
        let merge = builder.create_block();
        builder.ins().brif(tag_matches, cmp_b, &[], merge, &[]);
        // Only reached when tags match: compile each field expression
        // directly (never wrapped in the constructor) and AND-fold its
        // equality against the other side's corresponding field, borrowed
        // in place. Chained directly in SSA (not through the `res` slot) so a
        // single-field case (every `ok`/`err`, and most variant cases) costs
        // no more than a bare comparison.
        builder.switch_to_block(cmp_b);
        builder.seal_block(cmp_b);
        scopes.push(Vec::new());
        let mut fields_eq = None;
        for ((offset, fty), arg) in fields.iter().zip(ctor_args.iter()) {
            let other_field = component(builder, ov, *offset, fty, structs);
            let (cv, _) = compile_expr(module, builder, cx, scopes, arg)?;
            let feq = emit_eq(module, builder, builtins, structs, other_field, cv, fty)?;
            fields_eq = Some(match fields_eq {
                None => feq,
                Some(acc) => builder.ins().band(acc, feq),
            });
        }
        drop_scope(
            builder,
            module,
            builtins,
            structs,
            scopes.pop().expect("ctor-eq fields scope"),
        );
        builder.ins().stack_store(
            fields_eq.expect("fields is non-empty in this branch"),
            res,
            0,
        );
        builder.ins().jump(merge, &[]);
        builder.switch_to_block(merge);
        builder.seal_block(merge);
        builder.ins().stack_load(types::I64, res, 0)
    };
    let result = if op == 'N' {
        builder.ins().bxor_imm(eq, 1)
    } else {
        eq
    };
    Ok(Some((result, Type::Primitive(Primitive::Bool))))
}

/// splitmix64 finalizer: a strong avalanche mix of an i64. Used to hash scalars
/// and to fold lengths/tags into a hash. Cheap — a few shifts/xors/multiplies —
/// and diffuses low bits well, so sequential keys (1, 2, 3) don't cluster in a
/// power-of-two hash table (the eventual hash-dict/set use case).
fn emit_scalar_hash(builder: &mut FunctionBuilder, x: Value) -> Value {
    let mul = |b: &mut FunctionBuilder, v: Value, k: u64| {
        let kc = b.ins().iconst(types::I64, k as i64);
        b.ins().imul(v, kc)
    };
    let s = builder.ins().ushr_imm(x, 30);
    let x = builder.ins().bxor(x, s);
    let x = mul(builder, x, 0xbf58_476d_1ce4_e5b9);
    let s = builder.ins().ushr_imm(x, 27);
    let x = builder.ins().bxor(x, s);
    let x = mul(builder, x, 0x94d0_49bb_1331_11eb);
    let s = builder.ins().ushr_imm(x, 31);
    builder.ins().bxor(x, s)
}

/// Order-sensitive hash fold `(acc ^ child) * K` (K = golden-ratio odd
/// multiplier). Folds a child hash into a running accumulator for sequences,
/// struct fields, and variant payloads — where element order is significant
/// (matching their order-dependent `==`).
fn emit_hash_combine(builder: &mut FunctionBuilder, acc: Value, child: Value) -> Value {
    let x = builder.ins().bxor(acc, child);
    let k = builder
        .ins()
        .iconst(types::I64, 0x9e37_79b9_7f4a_7c15u64 as i64);
    builder.ins().imul(x, k)
}

/// FNV-1a offset basis, reused as the seed for composite (struct/pair) folds.
const HASH_SEED: i64 = 0xcbf2_9ce4_8422_2325u64 as i64;

/// Hash the elements of an array/set block `arr` (element type `elem`), folding
/// each element's hash into `seed`. `commutative` (sets) folds with a
/// commutative `+` so element order doesn't affect the result (matching set
/// `==`); otherwise (arrays) folds order-sensitively.
fn emit_seq_hash<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    arr: Value,
    elem: &Type,
    seed: Value,
    commutative: bool,
) -> Result<Value, Error> {
    let len = load_arr_len(builder, arr);
    let acc = i64_slot(builder);
    builder.ins().stack_store(seed, acc, 0);
    let idx = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, idx, 0);
    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);
    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder.ins().brif(more, body, &[], exit, &[]);
    builder.switch_to_block(body);
    builder.seal_block(body);
    let el = load_array_elem(module, builder, builtins, arr, i, elem, structs);
    let h = emit_hash(module, builder, builtins, structs, el, elem)?;
    let cur = builder.ins().stack_load(types::I64, acc, 0);
    let new = if commutative {
        builder.ins().iadd(cur, h)
    } else {
        emit_hash_combine(builder, cur, h)
    };
    builder.ins().stack_store(new, acc, 0);
    let next = builder.ins().iadd_imm(i, 1);
    builder.ins().stack_store(next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);
    builder.switch_to_block(exit);
    builder.seal_block(exit);
    Ok(builder.ins().stack_load(types::I64, acc, 0))
}

/// Structural hash of `v` (static type `ty`) as an i64 — consistent with
/// `emit_eq` (`a == b` ⇒ `hash(a) == hash(b)`). Scalars use the splitmix64
/// finalizer; `str` uses FNV-1a (`aipl_str_hash`); composites fold child hashes
/// — order-sensitively for arrays/structs/variant payloads, commutatively for
/// sets/dicts (matching their order-independent `==`). Borrows `v` (no refcount
/// change). Rejects `Fn`, like `emit_eq`.
fn emit_hash<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    ty: &Type,
) -> Result<Value, Error> {
    Ok(match ty {
        Type::Primitive(p) if !matches!(p, Primitive::Str) => emit_scalar_hash(builder, v),
        // `str` (and `Error`, and `char[]` — see `is_str_shaped`) hashes by
        // its byte content.
        _ if is_str_shaped(ty) => {
            let f = builtins.import(module, builder.func, "aipl_str_hash");
            let inst = builder.ins().call(f, &[v]);
            builder.inst_results(inst)[0]
        }
        Type::Optional(_) => {
            // hash(tag), combined with the core's hash only when fully `some`
            // (tag == depth) — so `none`/`some^k(none)` hash by tag alone.
            let depth = opt_depth(ty) as i64;
            let core = opt_core(ty);
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), v, 0);
            let base = emit_scalar_hash(builder, tag);
            let acc = i64_slot(builder);
            builder.ins().stack_store(base, acc, 0);
            let is_full = builder.ins().icmp_imm(IntCC::Equal, tag, depth);
            let core_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(is_full, core_b, &[], merge, &[]);
            builder.switch_to_block(core_b);
            builder.seal_block(core_b);
            let cv = component(builder, v, OPT_VALUE_OFFSET, core, structs);
            let ch = emit_hash(module, builder, builtins, structs, cv, core)?;
            let combined = emit_hash_combine(builder, base, ch);
            builder.ins().stack_store(combined, acc, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, acc, 0)
        }
        Type::Array(elem) => {
            let len = load_arr_len(builder, v);
            let seed = emit_scalar_hash(builder, len);
            if is_none_inner(elem) {
                seed
            } else {
                emit_seq_hash(module, builder, builtins, structs, v, elem, seed, false)?
            }
        }
        Type::Set(elem) => {
            let len = load_arr_len(builder, v);
            let seed = emit_scalar_hash(builder, len);
            if is_none_inner(elem) {
                seed
            } else {
                emit_seq_hash(module, builder, builtins, structs, v, elem, seed, true)?
            }
        }
        Type::Named(n) if structs.get(n).and_then(TypeDef::as_struct).is_some() => {
            let fields: Vec<(u32, Type)> = structs[n]
                .as_struct()
                .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                .unwrap_or_default();
            let acc = i64_slot(builder);
            let seed = builder.ins().iconst(types::I64, HASH_SEED);
            builder.ins().stack_store(seed, acc, 0);
            for (offset, fty) in fields {
                let fv = component(builder, v, offset, &fty, structs);
                let h = emit_hash(module, builder, builtins, structs, fv, &fty)?;
                let cur = builder.ins().stack_load(types::I64, acc, 0);
                let new = emit_hash_combine(builder, cur, h);
                builder.ins().stack_store(new, acc, 0);
            }
            builder.ins().stack_load(types::I64, acc, 0)
        }
        Type::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            let cases: Vec<(usize, Vec<(u32, Type)>)> = structs[n]
                .as_variant()
                .map(|vl| {
                    vl.cases
                        .iter()
                        .enumerate()
                        .map(|(tag, c)| {
                            (
                                tag,
                                c.fields.iter().map(|f| (f.offset, f.ty.clone())).collect(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), v, 0);
            let acc = i64_slot(builder);
            let base = emit_scalar_hash(builder, tag);
            builder.ins().stack_store(base, acc, 0);
            let merge = builder.create_block();
            for (k, fields) in cases {
                if fields.is_empty() {
                    continue; // tag alone already hashed
                }
                let case_b = builder.create_block();
                let next_b = builder.create_block();
                let is_k = builder.ins().icmp_imm(IntCC::Equal, tag, k as i64);
                builder.ins().brif(is_k, case_b, &[], next_b, &[]);
                builder.switch_to_block(case_b);
                builder.seal_block(case_b);
                for (offset, fty) in fields {
                    let fv = component(builder, v, offset, &fty, structs);
                    let h = emit_hash(module, builder, builtins, structs, fv, &fty)?;
                    let cur = builder.ins().stack_load(types::I64, acc, 0);
                    let new = emit_hash_combine(builder, cur, h);
                    builder.ins().stack_store(new, acc, 0);
                }
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(next_b);
                builder.seal_block(next_b);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, acc, 0)
        }
        Type::Dict(key_ty, val_ty) => {
            // Order-independent over pairs (matching dict `==`): fold each pair's
            // (key, value) combine commutatively. Within a pair the combine is
            // order-sensitive so `{1: 2}` and `{2: 1}` differ.
            let len = load_arr_len(builder, v);
            let seed = emit_scalar_hash(builder, len);
            if is_none_inner(key_ty) {
                seed
            } else {
                let pair_size = 8 + elem_size_of(val_ty, structs);
                let acc = i64_slot(builder);
                builder.ins().stack_store(seed, acc, 0);
                let idx = i64_slot(builder);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(zero, idx, 0);
                let v_base = arr_base(builder, v);
                let elems = builder.ins().iadd_imm(v_base, ARR_ELEMS_OFFSET as i64);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                let off = builder.ins().imul_imm(i, pair_size);
                let pair = builder.ins().iadd(elems, off);
                let kv = component(builder, pair, 0, key_ty, structs);
                let kh = emit_hash(module, builder, builtins, structs, kv, key_ty)?;
                let vv = component(builder, pair, 8, val_ty, structs);
                let vh = emit_hash(module, builder, builtins, structs, vv, val_ty)?;
                let pseed = builder.ins().iconst(types::I64, HASH_SEED);
                let pk = emit_hash_combine(builder, pseed, kh);
                let ph = emit_hash_combine(builder, pk, vh);
                let cur = builder.ins().stack_load(types::I64, acc, 0);
                let new = builder.ins().iadd(cur, ph); // commutative over pairs
                builder.ins().stack_store(new, acc, 0);
                let next = builder.ins().iadd_imm(i, 1);
                builder.ins().stack_store(next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().stack_load(types::I64, acc, 0)
            }
        }
        Type::Result(ok_ty, err_ty) => {
            // hash(tag) combined with the active payload's hash (by its type).
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), v, 0);
            let base = emit_scalar_hash(builder, tag);
            let acc = i64_slot(builder);
            builder.ins().stack_store(base, acc, 0);
            let is_ok = builder.ins().icmp_imm(IntCC::Equal, tag, 1);
            let ok_b = builder.create_block();
            let err_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(is_ok, ok_b, &[], err_b, &[]);
            // A unit (void-Ok) or `__none__` (unconstructible) side carries no
            // payload — its hash is just the tag's.
            let payload_trivial = |t: &Type| is_unit(t) || is_none_inner(t);
            builder.switch_to_block(ok_b);
            builder.seal_block(ok_b);
            if !payload_trivial(ok_ty) {
                let okv = component(builder, v, OPT_VALUE_OFFSET, ok_ty, structs);
                let h = emit_hash(module, builder, builtins, structs, okv, ok_ty)?;
                let c = emit_hash_combine(builder, base, h);
                builder.ins().stack_store(c, acc, 0);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(err_b);
            builder.seal_block(err_b);
            if !payload_trivial(err_ty) {
                let errv = component(builder, v, OPT_VALUE_OFFSET, err_ty, structs);
                let h = emit_hash(module, builder, builtins, structs, errv, err_ty)?;
                let c = emit_hash_combine(builder, base, h);
                builder.ins().stack_store(c, acc, 0);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, acc, 0)
        }
        other => {
            return Err(Error::msg(format!(
                "hash is not supported for type {}",
                type_name(other)
            )));
        }
    })
}

fn emit_drop<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    ty: &Type,
) {
    emit_rc(builder, module, builtins, structs, v, ty, RcOp::Drop);
}

fn emit_retain<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    ty: &Type,
) {
    emit_rc(builder, module, builtins, structs, v, ty, RcOp::Retain);
}

/// Make a `mut` binding's slot the sole owner of one reference to the value `v`
/// (type `ty`) just stored into it. Heap types only — scalars own no heap, so
/// this is a no-op for them. It reconciles the current scope's tracking so that
/// exactly one reference reaches the slot, to be released once by the binding's
/// slot-track at scope exit:
///   - an owned-parameter move (`mut y = p`) carries no value-track and hands its
///     sole reference over as-is;
///   - a freshly produced value left its value-track on top of the scope — take
///     ownership by popping that track (no inc/dec);
///   - a borrowed value (an `Ident` to another binding, or a component read that
///     left no track) is retained, so the slot co-owns it alongside its source.
///
/// This is what lets a `mut` binding be reassigned across a scope boundary (e.g.
/// `set x = x[..]` in a loop body) safely: the slot, not the inner scope, owns
/// the live value, so it survives the inner scope's teardown.
fn own_value_into_slot<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    scopes: &mut [Vec<Tracked>],
    v: Value,
    ty: &Type,
    value: &Expr,
    owned_params: &HashSet<String>,
) {
    if !needs_drop(ty, structs) {
        return;
    }
    if let ExprKind::Ident(n) = &value.kind {
        // An owned-parameter move transfers its existing reference with no inc.
        if owned_params.contains(n) {
            return;
        }
        // Any other identifier is a borrow; the slot takes its own reference.
        emit_retain(builder, module, builtins, structs, v, ty);
        return;
    }
    // A freshly produced value tracks itself on top of the scope: transfer that
    // reference to the slot. A non-`Ident` borrow that left no such track (e.g. a
    // component read) falls through to a retain.
    let took = matches!(
        scopes.last().and_then(|s| s.last()),
        Some(Tracked { owned: Owned::Value(tv), .. }) if *tv == v
    );
    if took {
        scopes.last_mut().expect("scope").pop();
    } else {
        emit_retain(builder, module, builtins, structs, v, ty);
    }
}

// ---------------------------------------------------------------------------
// Optional representation
//
// A type's representation reflects its *structure*, not a naive nesting of its
// parts. An optional chain `Optional^n(Core)` — where `Core` is the innermost
// non-optional type — is stored flat as `{ tag: i64, value: <Core> }`, a single
// `Core`-sized value field shared by the whole chain. The tag counts how many
// `some` layers are present:
//
//   tag 0           => none                 (outermost `none`)
//   tag k  (0<k<n)  => some^k(none)         (value field is garbage)
//   tag n           => some^n(<core value>) (value field live)
//
// So `T?`, `T??`, `T???`, … over the same `Core` are all `8 + sizeof(Core)`
// bytes; deeper nesting only widens the tag's range, never the value. The core
// heap (if any) is owned exactly when `tag == n`. `opt_core` / `opt_depth`
// recover `Core` and `n` from a type; every optional operation (build, unwrap,
// render, retain/drop) is expressed in terms of them.
// ---------------------------------------------------------------------------

/// The innermost non-optional type of `ty`, peeling every `Optional` layer. The
/// whole optional chain shares one value field sized for this core.
fn opt_core(ty: &Type) -> &Type {
    match ty {
        Type::Optional(inner) => opt_core(inner),
        _ => ty,
    }
}

/// The number of `Optional` layers wrapping `ty` (0 if not an optional). This is
/// the maximum tag of the flattened representation — the tag equals the depth
/// exactly when the core value is present.
fn opt_depth(ty: &Type) -> u32 {
    match ty {
        Type::Optional(inner) => 1 + opt_depth(inner),
        _ => 0,
    }
}

/// Byte offset of an optional's value field, past the 8-byte tag.
const OPT_VALUE_OFFSET: u32 = 8;

/// Inline byte size of a value of type `ty` when stored as an array element, an
/// optional's payload, or a struct field. Scalars and heap pointers (`str`/
/// array) are 8 bytes; an optional chain is `8 (tag) + sizeof(Core)` regardless
/// of nesting depth (see "Optional representation" above); a struct is its
/// layout size. Known at compile time, so it's passed to the array runtime as a
/// constant rather than stored.
fn elem_size_of(ty: &Type, structs: &HashMap<String, TypeDef>) -> i64 {
    match ty {
        Type::Optional(_) => OPT_VALUE_OFFSET as i64 + elem_size_of(opt_core(ty), structs),
        // A result is `{ tag, value }` where `value` is sized to the wider
        // payload (8 bytes for v1's scalar/str payloads → 16 total).
        Type::Result(ok, err) => {
            OPT_VALUE_OFFSET as i64 + elem_size_of(ok, structs).max(elem_size_of(err, structs))
        }
        Type::Named(n) => structs.get(n).map_or(8, |t| t.size() as i64),
        _ => 8,
    }
}

/// The element drop-fn pointer to store in an array's header for element type
/// `elem`, as an i64 Cranelift value (0 when elements need no per-element
/// cleanup — scalars, optionals of scalars). `str`/array/optional-of-str-or-
/// array elements use fixed runtime helpers; anything else needing cleanup (a
/// struct, an optional of a struct, a nested optional carrying heap) uses an
/// on-demand-generated per-type helper.
fn array_drop_fn_addr<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    cx: Cx,
    elem: &Type,
) -> Value {
    let b = cx.builtins;
    let id = match elem {
        Type::Primitive(Primitive::Str) => Some(b.id(module, "aipl_arr_drop_str")),
        // `char[]` shares `str`'s representation (see `is_char_array`), so a
        // nested `char[]` element (e.g. in `char[][]`) is freed the same way
        // a `str` element is, not via the generic array-element drop-fn.
        Type::Array(_) if is_char_array(elem) => Some(b.id(module, "aipl_arr_drop_str")),
        Type::Array(_) => Some(b.id(module, "aipl_arr_drop_arr")),
        Type::Optional(inner) if matches!(inner.as_ref(), Type::Primitive(Primitive::Str)) => {
            Some(b.id(module, "aipl_arr_drop_opt_str"))
        }
        Type::Optional(inner) if matches!(inner.as_ref(), Type::Array(_)) => {
            Some(b.id(module, "aipl_arr_drop_opt_arr"))
        }
        _ if needs_drop(elem, cx.structs) => Some(elem_rc_ids(module, cx, elem).0),
        _ => None,
    };
    fn_addr_or_zero(builder, module, id)
}

/// The element *retain*-fn pointer for element type `elem` (mirrors
/// `array_drop_fn_addr`): incs the heap content of each element when an array's
/// elements are copied (the new array co-owns them). 0 for scalar elements.
fn array_retain_fn_addr<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    cx: Cx,
    elem: &Type,
) -> Value {
    let b = cx.builtins;
    let id = match elem {
        Type::Primitive(Primitive::Str) => Some(b.id(module, "aipl_arr_retain_ptr")),
        Type::Array(_) => Some(b.id(module, "aipl_arr_retain_ptr")),
        Type::Optional(inner) if matches!(inner.as_ref(), Type::Primitive(Primitive::Str)) => {
            Some(b.id(module, "aipl_arr_retain_opt"))
        }
        Type::Optional(inner) if matches!(inner.as_ref(), Type::Array(_)) => {
            Some(b.id(module, "aipl_arr_retain_opt"))
        }
        _ if needs_drop(elem, cx.structs) => Some(elem_rc_ids(module, cx, elem).1),
        _ => None,
    };
    fn_addr_or_zero(builder, module, id)
}

/// Declare (once, cached) the per-element-type `(drop, retain)` helper functions
/// for `elem` and record them to be defined after the main function loop. The
/// element type's own size/layout drives the generated loop body.
fn elem_rc_ids<M: Module>(module: &mut M, cx: Cx, elem: &Type) -> (FuncId, FuncId) {
    let key = type_name(elem);
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(ids) = er.fns.get(&key) {
        return *ids;
    }
    let n = er.ctr;
    er.ctr += 1;
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // elems
    sig.params.push(AbiParam::new(types::I64)); // len
    let drop_id = module
        .declare_function(&format!("__arr_drop_{n}"), Linkage::Local, &sig)
        .expect("declare elem drop");
    let retain_id = module
        .declare_function(&format!("__arr_retain_{n}"), Linkage::Local, &sig)
        .expect("declare elem retain");
    er.fns.insert(key, (drop_id, retain_id));
    er.pending.push((elem.clone(), drop_id, retain_id));
    (drop_id, retain_id)
}

/// Declare (once, cached) the `(drop, retain)` helpers for a dict's pair-array
/// elements, where each element is `[key: K][value: V]`. Mirrors `elem_rc_ids`
/// but the generated body releases/retains both halves of every pair.
fn pair_rc_ids<M: Module>(module: &mut M, cx: Cx, k: &Type, v: &Type) -> (FuncId, FuncId) {
    let key = (type_name(k), type_name(v));
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(ids) = er.pair_fns.get(&key) {
        return *ids;
    }
    let n = er.ctr;
    er.ctr += 1;
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // elems
    sig.params.push(AbiParam::new(types::I64)); // len
    let drop_id = module
        .declare_function(&format!("__dict_drop_{n}"), Linkage::Local, &sig)
        .expect("declare pair drop");
    let retain_id = module
        .declare_function(&format!("__dict_retain_{n}"), Linkage::Local, &sig)
        .expect("declare pair retain");
    er.pair_fns.insert(key, (drop_id, retain_id));
    er.pair_pending
        .push((k.clone(), v.clone(), drop_id, retain_id));
    (drop_id, retain_id)
}

/// The element drop/retain-fn pointers a dict's pair-array stores in its header,
/// as i64 Cranelift values (0 when no pair half needs cleanup — e.g. an
/// `#{i64: i64}`). The generated helper releases/retains the key then the value.
fn pair_rc_fn_addrs<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    cx: Cx,
    k: &Type,
    v: &Type,
) -> (Value, Value) {
    if needs_drop(k, cx.structs) || needs_drop(v, cx.structs) {
        let (d, r) = pair_rc_ids(module, cx, k, v);
        (
            fn_addr_or_zero(builder, module, Some(d)),
            fn_addr_or_zero(builder, module, Some(r)),
        )
    } else {
        let zero = builder.ins().iconst(types::I64, 0);
        (zero, zero)
    }
}

/// Define a generated dict pair drop/retain helper: `(elems, len)` strides by the
/// pair size (`8 + sizeof(V)`) and applies `op` to each pair's key (offset 0,
/// always 8 bytes) and value (offset 8).
#[allow(clippy::too_many_arguments)]
fn define_pair_rc_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    id: FuncId,
    key_ty: &Type,
    val_ty: &Type,
    op: RcOp,
    ir_out: &mut String,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // elems
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // len
    let pair_size = 8 + elem_size_of(val_ty, structs);
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let elems = builder.block_params(entry)[0];
        let len = builder.block_params(entry)[1];

        let slot =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().stack_store(zero, slot, 0);
        let header = builder.create_block();
        let body = builder.create_block();
        let exit = builder.create_block();
        builder.ins().jump(header, &[]);

        builder.switch_to_block(header);
        let i = builder.ins().stack_load(types::I64, slot, 0);
        let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
        builder.ins().brif(more, body, &[], exit, &[]);

        builder.switch_to_block(body);
        builder.seal_block(body);
        let off = builder.ins().imul_imm(i, pair_size);
        let pair = builder.ins().iadd(elems, off);
        // Key at offset 0 (a scalar/str: `component` loads its 8 bytes), value at
        // offset 8 (a composite is addressed, a scalar/str/array loaded).
        let kv = component(&mut builder, pair, 0, key_ty, structs);
        emit_rc(&mut builder, module, builtins, structs, kv, key_ty, op);
        let vv = component(&mut builder, pair, 8, val_ty, structs);
        emit_rc(&mut builder, module, builtins, structs, vv, val_ty, op);
        let next = builder.ins().iadd_imm(i, 1);
        builder.ins().stack_store(next, slot, 0);
        builder.ins().jump(header, &[]);
        builder.seal_block(header);

        builder.switch_to_block(exit);
        builder.seal_block(exit);
        builder.ins().return_(&[]);
        builder.finalize();
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define pair rc fn: {e}")))?;
    module.clear_context(ctx);
    Ok(())
}

/// Define a generated per-element array drop/retain helper: `(elems, len)` that
/// strides by the element size and retains/drops each element via `emit_rc`.
fn define_elem_rc_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    id: FuncId,
    elem: &Type,
    op: RcOp,
    ir_out: &mut String,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // elems
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // len
    let esz = elem_size_of(elem, structs);
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let elems = builder.block_params(entry)[0];
        let len = builder.block_params(entry)[1];

        let slot =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().stack_store(zero, slot, 0);
        let header = builder.create_block();
        let body = builder.create_block();
        let exit = builder.create_block();
        builder.ins().jump(header, &[]);

        builder.switch_to_block(header);
        let i = builder.ins().stack_load(types::I64, slot, 0);
        let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
        builder.ins().brif(more, body, &[], exit, &[]);

        builder.switch_to_block(body);
        builder.seal_block(body);
        let off = builder.ins().imul_imm(i, esz);
        let addr = builder.ins().iadd(elems, off);
        let elem_val = component(&mut builder, addr, 0, elem, structs);
        emit_rc(&mut builder, module, builtins, structs, elem_val, elem, op);
        let next = builder.ins().iadd_imm(i, 1);
        builder.ins().stack_store(next, slot, 0);
        builder.ins().jump(header, &[]);
        builder.seal_block(header);

        builder.switch_to_block(exit);
        builder.seal_block(exit);
        builder.ins().return_(&[]);
        builder.finalize();
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define elem rc fn: {e}")))?;
    module.clear_context(ctx);
    Ok(())
}

fn fn_addr_or_zero<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    id: Option<FuncId>,
) -> Value {
    match id {
        Some(id) => {
            let fref = module.declare_func_in_func(id, builder.func);
            builder.ins().func_addr(types::I64, fref)
        }
        None => builder.ins().iconst(types::I64, 0),
    }
}

// Call a registered single-argument renderer builtin (by `funcs` name) on
// `value`, returning the fresh `str` result. The argument is borrowed (the
// renderer never drops it).
// ---------- Two-pass `to_str` rendering ----------
//
// `to_str` runs `emit_render` twice: once to *measure* the total byte length,
// then — after one `aipl_str_alloc` — to *write* the bytes into a moving cursor.
// Both passes share the same structural IR; only the leaf operations differ.
// Every `emit_render*` returns the byte length of what it renders (used to size
// the buffer in the measure pass; ignored, but cheap, in the write pass) and,
// in `Write` mode, advances the cursor as it writes.
// Where a render pass sends its output.
#[derive(Clone, Copy)]
enum Sink {
    /// Only compute lengths — emit no writes.
    Measure,
    /// Write bytes, advancing the `*mut u8` cursor held in this stack slot.
    Write(StackSlot),
}

/// In `Write` mode, copy `n` bytes from `src` to the cursor and advance it.
fn sink_bytes<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    sink: Sink,
    src: Value,
    n: Value,
) {
    if let Sink::Write(cur) = sink {
        let dst = builder.ins().stack_load(types::I64, cur, 0);
        let f = cx.builtins.import(module, builder.func, "aipl_write_bytes");
        let inst = builder.ins().call(f, &[dst, src, n]);
        let adv = builder.inst_results(inst)[0];
        builder.ins().stack_store(adv, cur, 0);
    }
}

/// In `Write` mode, write one byte (low 8 bits of `byte`) and advance the cursor.
fn sink_byte(builder: &mut FunctionBuilder, sink: Sink, byte: Value) {
    if let Sink::Write(cur) = sink {
        let dst = builder.ins().stack_load(types::I64, cur, 0);
        builder.ins().istore8(MemFlags::trusted(), byte, dst, 0);
        let adv = builder.ins().iadd_imm(dst, 1);
        builder.ins().stack_store(adv, cur, 0);
    }
}

/// Emit a fixed literal piece (brackets, separators, labels, constructor names).
/// Returns its (compile-time-constant) byte length; writes it in `Write` mode.
fn emit_lit<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    sink: Sink,
    bytes: &[u8],
) -> Result<Value, Error> {
    if let Sink::Write(_) = sink {
        let ptr = emit_str_literal(module, builder, cx, bytes)?;
        let n = builder.ins().iconst(types::I64, bytes.len() as i64);
        sink_bytes(module, builder, cx, sink, ptr, n);
    }
    Ok(builder.ins().iconst(types::I64, bytes.len() as i64))
}

/// Measure (and, in `Write` mode, write) `value` of static type `ty`, returning
/// its rendered byte length. Debug-style: chars `'c'`, strings `"s"`, arrays
/// `[a, b]`, optionals `some(x)`/`none`, structs `P { f: v }`, variants
/// `Ctor(f, ...)`.
fn emit_render<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    value: Value,
    ty: &Type,
    sink: Sink,
) -> Result<Value, Error> {
    let b = cx.builtins;
    Ok(match ty {
        // Signed integers (i8/i16/i32/i64) render via the signed formatter — the
        // canonical i64 register value is the signed value. Unsigned ones use the
        // unsigned formatter (u64 can exceed i64's range).
        Type::Primitive(p) if p.is_int() => {
            let (len_fn, write_fn) = if p.int_signed() {
                ("aipl_i64_len", "aipl_write_i64")
            } else {
                ("aipl_u64_len", "aipl_write_u64")
            };
            let len = {
                let f = b.import(module, builder.func, len_fn);
                let inst = builder.ins().call(f, &[value]);
                builder.inst_results(inst)[0]
            };
            if let Sink::Write(cur) = sink {
                let dst = builder.ins().stack_load(types::I64, cur, 0);
                let f = b.import(module, builder.func, write_fn);
                let inst = builder.ins().call(f, &[dst, value]);
                let adv = builder.inst_results(inst)[0];
                builder.ins().stack_store(adv, cur, 0);
            }
            len
        }
        Type::Primitive(Primitive::Bool) => {
            // "true" (4) or "false" (5).
            let four = builder.ins().iconst(types::I64, 4);
            let five = builder.ins().iconst(types::I64, 5);
            let len = builder.ins().select(value, four, five);
            if let Sink::Write(_) = sink {
                let t = emit_str_literal(module, builder, cx, b"true")?;
                let f = emit_str_literal(module, builder, cx, b"false")?;
                let ptr = builder.ins().select(value, t, f);
                sink_bytes(module, builder, cx, sink, ptr, len);
            }
            len
        }
        Type::Primitive(Primitive::Char) => {
            // 'c' — three bytes.
            if let Sink::Write(_) = sink {
                let quote = builder.ins().iconst(types::I64, b'\'' as i64);
                sink_byte(builder, sink, quote);
                sink_byte(builder, sink, value);
                sink_byte(builder, sink, quote);
            }
            builder.ins().iconst(types::I64, 3)
        }
        // `str` (and `Error`) renders as its content in double quotes.
        _ if is_str_repr(ty) => {
            // "s" — the content (no escaping) wrapped in double quotes.
            let content = {
                let f = b.import(module, builder.func, "aipl_str_len");
                let inst = builder.ins().call(f, &[value]);
                builder.inst_results(inst)[0]
            };
            if let Sink::Write(_) = sink {
                let quote = builder.ins().iconst(types::I64, b'"' as i64);
                sink_byte(builder, sink, quote);
                // Resolve a contiguous content pointer for any representation
                // (inline/owned/view) via `aipl_str_data`: it spills an inline
                // value's bytes into `scratch` and returns the in-place pointer
                // for owned/view. (`content`, the length, already handles all
                // three — `aipl_str_len`.)
                let scratch = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                let scratch_addr = builder.ins().stack_addr(types::I64, scratch, 0);
                let f = b.import(module, builder.func, "aipl_str_data");
                let inst = builder.ins().call(f, &[value, scratch_addr]);
                let src = builder.inst_results(inst)[0];
                sink_bytes(module, builder, cx, sink, src, content);
                sink_byte(builder, sink, quote);
            }
            builder.ins().iadd_imm(content, 2)
        }
        // `char[]` is str-shaped (see `is_char_array`) but keeps its own
        // array-bracket rendering (`['a', 'b', 'c']`, not `str`'s `"abc"`) —
        // read via the same byte cursor `for`-loop iteration uses, since
        // `value` is a real `str` underneath, not a generic array block.
        _ if is_char_array(ty) => emit_render_char_array(module, builder, cx, value, sink)?,
        Type::Array(elem) => emit_render_seq(module, builder, cx, value, elem, sink, b'[', b']')?,
        Type::Set(elem) => emit_render_seq(module, builder, cx, value, elem, sink, b'{', b'}')?,
        Type::Dict(k, v) => emit_render_dict(module, builder, cx, value, k, v, sink)?,
        Type::Optional(_) => emit_render_optional(module, builder, cx, value, ty, sink)?,
        Type::Result(ok, err) => emit_render_result(module, builder, cx, value, ok, err, sink)?,
        Type::Named(n) if cx.structs.get(n).and_then(TypeDef::as_struct).is_some() => {
            emit_render_struct(module, builder, cx, value, n, sink)?
        }
        Type::Named(n) if cx.structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            emit_render_variant(module, builder, cx, value, n, sink)?
        }
        other => {
            return Err(Error::msg(format!(
                "to_str: rendering {} is not yet supported",
                type_name(other)
            )));
        }
    })
}

/// Materialize a fresh static string literal (`[STATIC_REFCOUNT][bytes][NUL]`)
/// and return a pointer past its header. Used for the fixed pieces `to_str`
/// stitches around rendered values; `STATIC_REFCOUNT` makes inc/dec no-ops, so
/// these flow through concatenation (which decs its inputs) safely.
fn emit_str_literal<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    bytes: &[u8],
) -> Result<Value, Error> {
    let n = cx.lit_ctr.get();
    cx.lit_ctr.set(n + 1);
    let data_id = module
        .declare_data(&format!("__tostr_lit_{n}"), Linkage::Local, false, false)
        .map_err(|e| Error::msg(format!("declare lit: {e}")))?;
    // Static string layout: [len: i64][refcount = STATIC][bytes][NUL]; pointer
    // points past both header words.
    let mut content = Vec::with_capacity(STR_HEADER_SIZE + bytes.len() + 1);
    content.extend_from_slice(&(bytes.len() as i64).to_le_bytes());
    content.extend_from_slice(&STATIC_REFCOUNT.to_le_bytes());
    content.extend_from_slice(bytes);
    content.push(0);
    let mut desc = DataDescription::new();
    desc.set_align(8);
    desc.define(content.into_boxed_slice());
    module
        .define_data(data_id, &desc)
        .map_err(|e| Error::msg(format!("define lit: {e}")))?;
    let gv = module.declare_data_in_func(data_id, builder.func);
    let base = builder.ins().symbol_value(types::I64, gv);
    Ok(builder.ins().iadd_imm(base, STR_HEADER_SIZE as i64))
}

/// Build a file-op `Result` value `{tag, value@8}` from a runtime call's raw
/// result. Success is `raw != 0` (a non-null contents pointer for read, or `1`
/// for write): tag 1, value = `raw` when `ok_is_value` (read's contents str)
/// else 0 (write's unit Ok). Failure → tag 0, value = a fresh static `err_msg`
/// literal (`STATIC_REFCOUNT`, so it needs no cleanup). Returns the slot pointer.
fn emit_file_result<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    raw: Value,
    ok_is_value: bool,
    err_msg: &[u8],
) -> Result<Value, Error> {
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
    let ptr = builder.ins().stack_addr(types::I64, slot, 0);
    let ok_b = builder.create_block();
    let err_b = builder.create_block();
    let merge = builder.create_block();
    let is_ok = builder.ins().icmp_imm(IntCC::NotEqual, raw, 0);
    builder.ins().brif(is_ok, ok_b, &[], err_b, &[]);

    builder.switch_to_block(ok_b);
    builder.seal_block(ok_b);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().store(MemFlags::trusted(), one, ptr, 0);
    let ok_val = if ok_is_value {
        raw
    } else {
        builder.ins().iconst(types::I64, 0)
    };
    builder
        .ins()
        .store(MemFlags::trusted(), ok_val, ptr, OPT_VALUE_OFFSET as i32);
    builder.ins().jump(merge, &[]);

    builder.switch_to_block(err_b);
    builder.seal_block(err_b);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().store(MemFlags::trusted(), zero, ptr, 0);
    let msg = emit_str_literal(module, builder, cx, err_msg)?;
    builder
        .ins()
        .store(MemFlags::trusted(), msg, ptr, OPT_VALUE_OFFSET as i32);
    builder.ins().jump(merge, &[]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(ptr)
}

/// Compute the `i64` exit code for an `fn main() -> !Error` from its result slot:
/// `ok()` (tag 1) → 0; `err(msg)` (tag 0) → print `error: <msg>` to stderr and
/// yield 1. Reads (borrows) the err message — the caller's scope drop frees the
/// result afterward.
fn emit_error_main_exit_code<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    result_ptr: Value,
) -> Value {
    let tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), result_ptr, 0);
    let ok_b = builder.create_block();
    let err_b = builder.create_block();
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    builder.ins().brif(tag, ok_b, &[], err_b, &[]); // tag 1 = ok, 0 = err

    builder.switch_to_block(ok_b);
    builder.seal_block(ok_b);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().jump(merge, &[BlockArg::Value(zero)]);

    builder.switch_to_block(err_b);
    builder.seal_block(err_b);
    let msg = builder.ins().load(
        types::I64,
        MemFlags::trusted(),
        result_ptr,
        OPT_VALUE_OFFSET as i32,
    );
    let f = builtins.import(module, builder.func, "aipl_print_error");
    builder.ins().call(f, &[msg]);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().jump(merge, &[BlockArg::Value(one)]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    builder.block_params(merge)[0]
}

/// Render an optional chain `Optional^n(Core)` from its flattened `{tag, core}`
/// slot (borrowed). Recurses one tag level at a time: `none` at tag 0, else
/// `some(<rest>)` where `<rest>` renders the same slot one layer shallower, down
/// to the core value (read at `OPT_VALUE_OFFSET`) at the innermost level.
fn emit_render_optional<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    slot_ptr: Value,
    opt_ty: &Type,
    sink: Sink,
) -> Result<Value, Error> {
    let tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), slot_ptr, 0);
    render_opt_level(
        module,
        builder,
        cx,
        slot_ptr,
        tag,
        opt_depth(opt_ty),
        opt_core(opt_ty),
        sink,
    )
}

/// One level of optional rendering: branch on whether `tag` is nonzero, emitting
/// `none` or `some(<inner>)`. `<inner>` is the next-shallower level (tag - 1)
/// when `depth > 1`, or the rendered core value when `depth == 1`. The merge
/// block carries the rendered byte length.
#[allow(clippy::too_many_arguments)]
fn render_opt_level<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    slot_ptr: Value,
    tag: Value,
    depth: u32,
    core: &Type,
    sink: Sink,
) -> Result<Value, Error> {
    let some_b = builder.create_block();
    let none_b = builder.create_block();
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    builder.ins().brif(tag, some_b, &[], none_b, &[]);

    builder.switch_to_block(none_b);
    builder.seal_block(none_b);
    let none_len = emit_lit(module, builder, cx, sink, b"none")?;
    builder.ins().jump(merge, &[BlockArg::Value(none_len)]);

    builder.switch_to_block(some_b);
    builder.seal_block(some_b);
    let open = emit_lit(module, builder, cx, sink, b"some(")?;
    let inner = if depth == 1 {
        // Innermost layer: the core value lives in the shared value field.
        let core_val = component(builder, slot_ptr, OPT_VALUE_OFFSET, core, cx.structs);
        emit_render(module, builder, cx, core_val, core, sink)?
    } else {
        // A `some` of a shallower optional: same slot, tag one lower.
        let dec = builder.ins().iadd_imm(tag, -1);
        render_opt_level(module, builder, cx, slot_ptr, dec, depth - 1, core, sink)?
    };
    let close = emit_lit(module, builder, cx, sink, b")")?;
    let len = builder.ins().iadd(open, inner);
    let len = builder.ins().iadd(len, close);
    builder.ins().jump(merge, &[BlockArg::Value(len)]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.block_params(merge)[0])
}

/// Render a result from its `{tag, value}` slot (borrowed): `ok(<okval>)` when
/// tag != 0, else `err(<errval>)`. The active payload lives at
/// `OPT_VALUE_OFFSET`; each branch renders it by its side's type. The merge
/// block carries the rendered byte length.
fn emit_render_result<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    slot_ptr: Value,
    ok_ty: &Type,
    err_ty: &Type,
    sink: Sink,
) -> Result<Value, Error> {
    let tag = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), slot_ptr, 0);
    let ok_b = builder.create_block();
    let err_b = builder.create_block();
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    builder.ins().brif(tag, ok_b, &[], err_b, &[]); // tag 1 = Ok, 0 = Err

    // A unit (void-Ok `!E`) side renders as `ok()`; a `__none__` side is
    // unconstructible (dead branch) — render the bare ctor either way.
    let payload_trivial = |t: &Type| is_unit(t) || is_none_inner(t);
    builder.switch_to_block(ok_b);
    builder.seal_block(ok_b);
    let len = if payload_trivial(ok_ty) {
        emit_lit(module, builder, cx, sink, b"ok()")?
    } else {
        let open = emit_lit(module, builder, cx, sink, b"ok(")?;
        let okv = component(builder, slot_ptr, OPT_VALUE_OFFSET, ok_ty, cx.structs);
        let inner = emit_render(module, builder, cx, okv, ok_ty, sink)?;
        let close = emit_lit(module, builder, cx, sink, b")")?;
        let len = builder.ins().iadd(open, inner);
        builder.ins().iadd(len, close)
    };
    builder.ins().jump(merge, &[BlockArg::Value(len)]);

    builder.switch_to_block(err_b);
    builder.seal_block(err_b);
    let len = if payload_trivial(err_ty) {
        emit_lit(module, builder, cx, sink, b"err()")?
    } else {
        let open = emit_lit(module, builder, cx, sink, b"err(")?;
        let errv = component(builder, slot_ptr, OPT_VALUE_OFFSET, err_ty, cx.structs);
        let inner = emit_render(module, builder, cx, errv, err_ty, sink)?;
        let close = emit_lit(module, builder, cx, sink, b")")?;
        let len = builder.ins().iadd(open, inner);
        builder.ins().iadd(len, close)
    };
    builder.ins().jump(merge, &[BlockArg::Value(len)]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.block_params(merge)[0])
}

/// Render a struct as `Name { field: <value>, ... }`, recursing on each field
/// (read via `component`, which loads a scalar/str/array or addresses an inline
/// composite). Returns the rendered byte length.
fn emit_render_struct<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    base: Value,
    sname: &str,
    sink: Sink,
) -> Result<Value, Error> {
    // Snapshot the fields so we don't borrow `cx.structs` across the recursive
    // `emit_render`/`emit_lit` calls.
    let layout = cx
        .structs
        .get(sname)
        .and_then(TypeDef::as_struct)
        .expect("struct layout");
    let fields: Vec<(String, u32, Type)> = layout
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.offset, f.ty.clone()))
        .collect();
    let mut len = emit_lit(
        module,
        builder,
        cx,
        sink,
        format!("{} {{ ", display_name(sname)).as_bytes(),
    )?;
    for (i, (fname, offset, fty)) in fields.iter().enumerate() {
        if i > 0 {
            let sep = emit_lit(module, builder, cx, sink, b", ")?;
            len = builder.ins().iadd(len, sep);
        }
        let label = emit_lit(module, builder, cx, sink, format!("{fname}: ").as_bytes())?;
        len = builder.ins().iadd(len, label);
        let fval = component(builder, base, *offset, fty, cx.structs);
        let fstr = emit_render(module, builder, cx, fval, fty, sink)?;
        len = builder.ins().iadd(len, fstr);
    }
    let close = emit_lit(module, builder, cx, sink, b" }")?;
    Ok(builder.ins().iadd(len, close))
}

/// Render a variant as `Ctor(f0, f1)` (or just `Ctor` for a nullary case):
/// branch on the runtime tag, render the active case's constructor name and its
/// parenthesized payload fields. The merge block carries the byte length.
fn emit_render_variant<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    base: Value,
    name: &str,
    sink: Sink,
) -> Result<Value, Error> {
    // Snapshot (ctor, fields) per case so we don't borrow `cx.structs` across
    // the recursive `emit_render` calls.
    let cases: Vec<(String, Vec<(u32, Type)>)> = cx.structs[name]
        .as_variant()
        .expect("variant layout")
        .cases
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                c.fields.iter().map(|f| (f.offset, f.ty.clone())).collect(),
            )
        })
        .collect();
    let tag = builder.ins().load(types::I64, MemFlags::trusted(), base, 0);
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    for (k, (ctor, fields)) in cases.into_iter().enumerate() {
        let case_b = builder.create_block();
        let next_b = builder.create_block();
        let is_k = builder.ins().icmp_imm(IntCC::Equal, tag, k as i64);
        builder.ins().brif(is_k, case_b, &[], next_b, &[]);
        builder.switch_to_block(case_b);
        builder.seal_block(case_b);
        let mut len = emit_lit(module, builder, cx, sink, ctor.as_bytes())?;
        if !fields.is_empty() {
            let open = emit_lit(module, builder, cx, sink, b"(")?;
            len = builder.ins().iadd(len, open);
            for (i, (offset, fty)) in fields.iter().enumerate() {
                if i > 0 {
                    let sep = emit_lit(module, builder, cx, sink, b", ")?;
                    len = builder.ins().iadd(len, sep);
                }
                let fval = component(builder, base, *offset, fty, cx.structs);
                let fstr = emit_render(module, builder, cx, fval, fty, sink)?;
                len = builder.ins().iadd(len, fstr);
            }
            let close = emit_lit(module, builder, cx, sink, b")")?;
            len = builder.ins().iadd(len, close);
        }
        builder.ins().jump(merge, &[BlockArg::Value(len)]);
        builder.switch_to_block(next_b);
        builder.seal_block(next_b);
    }
    // Unreachable at runtime (the tag always names a case), but the block must
    // produce a length for `merge`.
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().jump(merge, &[BlockArg::Value(zero)]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.block_params(merge)[0])
}

/// Add `v` to the i64 length accumulator in `slot`.
fn add_len(builder: &mut FunctionBuilder, slot: StackSlot, v: Value) {
    let cur = builder.ins().stack_load(types::I64, slot, 0);
    let sum = builder.ins().iadd(cur, v);
    builder.ins().stack_store(sum, slot, 0);
}

/// Render a `char[]` as `['a', 'b', 'c']` (empty: `[]`) — `emit_render_seq`'s
/// bracket style, but reading bytes via the same cursor `for`-loop iteration
/// uses (`aipl_str_iter_init`/`_next`) instead of `load_array_elem`, since
/// `arr` is str-shaped (see `is_char_array`), not a generic array block.
/// Borrows `arr` (the cursor is read-only, like `for`'s).
fn emit_render_char_array<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    arr: Value,
    sink: Sink,
) -> Result<Value, Error> {
    let b = cx.builtins;
    let len_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, len_slot, 0);
    let open_len = emit_lit(module, builder, cx, sink, b"[")?;
    add_len(builder, len_slot, open_len);

    let cur = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        ITER_SIZE as u32,
        3,
    ));
    let cur_addr = builder.ins().stack_addr(types::I64, cur, 0);
    let init_f = b.import(module, builder.func, "aipl_str_iter_init");
    builder.ins().call(init_f, &[cur_addr, arr]);

    // 1 until the first element has been rendered, then 0 — controls the
    // leading ", " separator (mirrors `emit_render_seq`'s `is_first` check,
    // but this cursor has no index to compare against 0).
    let first_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(one, first_slot, 0);

    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let next_f = b.import(module, builder.func, "aipl_str_iter_next");
    let inst = builder.ins().call(next_f, &[cur_addr]);
    let byte_i64 = builder.inst_results(inst)[0];
    let more = builder
        .ins()
        .icmp_imm(IntCC::SignedGreaterThanOrEqual, byte_i64, 0);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    let is_first = builder.ins().stack_load(types::I64, first_slot, 0);
    let is_first_b = builder.ins().icmp_imm(IntCC::NotEqual, is_first, 0);
    let sep_b = builder.create_block();
    let after_sep = builder.create_block();
    builder.ins().brif(is_first_b, after_sep, &[], sep_b, &[]);
    builder.switch_to_block(sep_b);
    builder.seal_block(sep_b);
    let sep = emit_lit(module, builder, cx, sink, b", ")?;
    add_len(builder, len_slot, sep);
    builder.ins().jump(after_sep, &[]);
    builder.switch_to_block(after_sep);
    builder.seal_block(after_sep);
    let zero_flag = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero_flag, first_slot, 0);

    let elem_len = emit_render(
        module,
        builder,
        cx,
        byte_i64,
        &Type::Primitive(Primitive::Char),
        sink,
    )?;
    add_len(builder, len_slot, elem_len);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    let close_len = emit_lit(module, builder, cx, sink, b"]")?;
    add_len(builder, len_slot, close_len);
    Ok(builder.ins().stack_load(types::I64, len_slot, 0))
}

/// Render an array (`[a, b, c]`) or set (`{a, b, c}`) — the two share the heap
/// block, so they share this renderer, differing only in the bracket bytes —
/// by looping over its elements and rendering each via `emit_render` (recursing
/// on the static element type). Borrows the container and its elements (no
/// inc/dec). Used for element types the monomorphic runtime renderers don't
/// cover — notably nested arrays (`T[][]`), which recurse back through here.
fn emit_render_seq<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    arr: Value,
    elem_ty: &Type,
    sink: Sink,
    open: u8,
    close: u8,
) -> Result<Value, Error> {
    // An untyped element (`__none__`) means an empty `[]`/`#{}` literal — render
    // it directly, since the element type has no renderer to recurse into.
    if is_none_inner(elem_ty) {
        return emit_lit(module, builder, cx, sink, &[open, close]);
    }
    // Running byte length, in a slot so it carries across the loop's back-edge.
    let len_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, len_slot, 0);
    let open_len = emit_lit(module, builder, cx, sink, &[open])?;
    add_len(builder, len_slot, open_len);

    let idx =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    builder.ins().stack_store(zero, idx, 0);
    let count = load_arr_len(builder, arr);

    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, count);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    // ", " before every element but the first.
    let is_first = builder.ins().icmp_imm(IntCC::Equal, i, 0);
    let sep_b = builder.create_block();
    let after_sep = builder.create_block();
    builder.ins().brif(is_first, after_sep, &[], sep_b, &[]);
    builder.switch_to_block(sep_b);
    builder.seal_block(sep_b);
    let sep = emit_lit(module, builder, cx, sink, b", ")?;
    add_len(builder, len_slot, sep);
    builder.ins().jump(after_sep, &[]);
    builder.switch_to_block(after_sep);
    builder.seal_block(after_sep);

    // Read element i (honoring the element representation — a bit-unpacked
    // `bool`, a loaded scalar/pointer, or a composite's address) and render it.
    let elem_val = load_array_elem(module, builder, cx.builtins, arr, i, elem_ty, cx.structs);
    let elem_len = emit_render(module, builder, cx, elem_val, elem_ty, sink)?;
    add_len(builder, len_slot, elem_len);

    let next = builder.ins().iadd_imm(i, 1);
    builder.ins().stack_store(next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    let close_len = emit_lit(module, builder, cx, sink, &[close])?;
    add_len(builder, len_slot, close_len);
    Ok(builder.ins().stack_load(types::I64, len_slot, 0))
}

/// Render a dict as `{k0: v0, k1: v1, ...}` (empty: `{}`). Mirrors
/// `emit_render_seq`, but each pair-array element renders as its key, `": "`,
/// then its value. The key is at the pair's offset 0, the value at 8.
fn emit_render_dict<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    dict: Value,
    key_ty: &Type,
    val_ty: &Type,
    sink: Sink,
) -> Result<Value, Error> {
    // An untyped empty `#{:}` has no key/value renderer to recurse into.
    if is_none_inner(key_ty) {
        return emit_lit(module, builder, cx, sink, b"{}");
    }
    let pair_size = 8 + elem_size_of(val_ty, cx.structs);
    let len_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, len_slot, 0);
    let open_len = emit_lit(module, builder, cx, sink, b"{")?;
    add_len(builder, len_slot, open_len);

    let idx =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    builder.ins().stack_store(zero, idx, 0);
    let count = load_arr_len(builder, dict);
    let dict_base = arr_base(builder, dict);
    let elems = builder.ins().iadd_imm(dict_base, ARR_ELEMS_OFFSET as i64);

    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, count);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    // ", " before every pair but the first.
    let is_first = builder.ins().icmp_imm(IntCC::Equal, i, 0);
    let sep_b = builder.create_block();
    let after_sep = builder.create_block();
    builder.ins().brif(is_first, after_sep, &[], sep_b, &[]);
    builder.switch_to_block(sep_b);
    builder.seal_block(sep_b);
    let sep = emit_lit(module, builder, cx, sink, b", ")?;
    add_len(builder, len_slot, sep);
    builder.ins().jump(after_sep, &[]);
    builder.switch_to_block(after_sep);
    builder.seal_block(after_sep);

    let off = builder.ins().imul_imm(i, pair_size);
    let pair = builder.ins().iadd(elems, off);
    let kv = component(builder, pair, 0, key_ty, cx.structs);
    let klen = emit_render(module, builder, cx, kv, key_ty, sink)?;
    add_len(builder, len_slot, klen);
    let colon = emit_lit(module, builder, cx, sink, b": ")?;
    add_len(builder, len_slot, colon);
    let vv = component(builder, pair, 8, val_ty, cx.structs);
    let vlen = emit_render(module, builder, cx, vv, val_ty, sink)?;
    add_len(builder, len_slot, vlen);

    let next = builder.ins().iadd_imm(i, 1);
    builder.ins().stack_store(next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    let close_len = emit_lit(module, builder, cx, sink, b"}")?;
    add_len(builder, len_slot, close_len);
    Ok(builder.ins().stack_load(types::I64, len_slot, 0))
}

/// The top-level entry for a `to_str(..)` expression: emit a call to the per-type
/// `__to_str_<n>` helper (declared on demand, defined after the main function
/// loop — see `define_tostr_fn`) and track its fresh `str` result for release.
/// Generating the rendering IR once per type and *calling* it — instead of
/// inlining the whole two-pass render at every `to_str` site — keeps the binary
/// small when a type is rendered in more than one place.
fn emit_to_str<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut [Vec<Tracked>],
    value: Value,
    ty: &Type,
) -> Result<Value, Error> {
    let id = tostr_func(module, cx, ty);
    let fref = module.declare_func_in_func(id, builder.func);
    let inst = builder.ins().call(fref, &[value]);
    // The helper borrows `value` (renders without consuming it) and returns a
    // freshly built `str` with one reference, owned by us — track it for drop at
    // scope exit (an inline result no-ops; a heap result is freed via `aipl_dec`,
    // which dispatches on the low bit).
    let result = builder.inst_results(inst)[0];
    scopes
        .last_mut()
        .expect("scope")
        .push(Tracked::new(result, &Type::Primitive(Primitive::Str)));
    Ok(result)
}

/// Declare (once, cached) the per-type `__to_str_<n>(value) -> str` rendering
/// helper for `ty`, recording it to be defined after the main function loop
/// (when the build context is free). Returns its function id.
fn tostr_func<M: Module>(module: &mut M, cx: Cx, ty: &Type) -> FuncId {
    let key = type_name(ty);
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(id) = er.tostr_fns.get(&key) {
        return *id;
    }
    let n = er.ctr;
    er.ctr += 1;
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // value
    sig.returns.push(AbiParam::new(types::I64)); // str
    let id = module
        .declare_function(&format!("__to_str_{n}"), Linkage::Local, &sig)
        .expect("declare to_str helper");
    er.tostr_fns.insert(key, id);
    er.tostr_pending.push((ty.clone(), id));
    id
}

/// Define a generated `__to_str_<n>(value) -> str` helper: render `value` of type
/// `ty` to a fresh `str` and return it. One allocation: a measure pass computes
/// the total byte length, `aipl_str_alloc` reserves exactly that, and a write
/// pass fills it through a moving cursor. The result is *returned* (one
/// reference, no scope tracking) — the caller (`emit_to_str`) tracks it.
#[allow(clippy::too_many_arguments)]
fn define_tostr_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    funcs: &HashMap<String, FuncInfo>,
    structs: &HashMap<String, TypeDef>,
    builtins: &Builtins,
    lit_ctr: &Cell<u32>,
    elem_rc: &RefCell<ElemRc>,
    id: FuncId,
    ty: &Type,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // value
    ctx.func.signature.returns.push(AbiParam::new(types::I64)); // str
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let value = builder.block_params(entry)[0];

        // `emit_render` only reads `builtins`/`structs`/`lit_ctr`/`elem_rc`; the
        // rest of `Cx` is irrelevant to rendering, so feed it trivial values.
        let env: Env = HashMap::new();
        let owned_params: HashSet<String> = HashSet::new();
        let unit = Type::Unit;
        // A synthesized renderer has no source-level bindings, so its legend is
        // empty (and never printed — this Cx isn't on the `define_fn` path).
        let no_bindings: RefCell<Vec<(String, String)>> = RefCell::new(Vec::new());
        let cx = Cx {
            env: &env,
            funcs,
            structs,
            builtins,
            effects: &[],
            owned_params: &owned_params,
            lit_ctr,
            elem_rc,
            ret_ty: &unit,
            sret: None,
            error_main: false,
            bindings: &no_bindings,
        };

        // Pass 1: measure the total length.
        let len = emit_render(module, &mut builder, cx, value, ty, Sink::Measure)?;

        // SSO: a result of <= 7 bytes is built *inline* (no allocation). The write
        // target is chosen per branch — a zeroed 8-byte stack scratch (content at
        // byte 1) for the inline case, or a fresh heap buffer for the big case
        // (`aipl_str_alloc` runs only on that branch). A single write pass then
        // fills whichever the cursor points at, and the result is picked
        // branchlessly: the inline value is `<scratch i64> | (len<<2 | 1)` (byte 0
        // was 0, so the OR sets the tag/len); the big value is the heap pointer.
        let scratch =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let cursor =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let heap_slot =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let is_small = builder.ins().icmp_imm(IntCC::SignedLessThanOrEqual, len, 7);

        let small_block = builder.create_block();
        let big_block = builder.create_block();
        let write_block = builder.create_block();
        builder
            .ins()
            .brif(is_small, small_block, &[], big_block, &[]);

        builder.switch_to_block(small_block);
        builder.seal_block(small_block);
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().stack_store(zero, scratch, 0);
        let scratch_addr = builder.ins().stack_addr(types::I64, scratch, 0);
        let content_start = builder.ins().iadd_imm(scratch_addr, 1);
        builder.ins().stack_store(content_start, cursor, 0);
        builder.ins().jump(write_block, &[]);

        builder.switch_to_block(big_block);
        builder.seal_block(big_block);
        let alloc = builtins.import(module, builder.func, "aipl_str_alloc");
        let inst = builder.ins().call(alloc, &[len]);
        let buf = builder.inst_results(inst)[0];
        builder.ins().stack_store(buf, cursor, 0);
        builder.ins().stack_store(buf, heap_slot, 0);
        builder.ins().jump(write_block, &[]);

        builder.switch_to_block(write_block);
        builder.seal_block(write_block);
        // One write pass into whichever buffer the cursor was seeded with.
        emit_render(module, &mut builder, cx, value, ty, Sink::Write(cursor))?;
        let raw = builder.ins().stack_load(types::I64, scratch, 0);
        let shifted = builder.ins().ishl_imm(len, 2);
        let tag = builder.ins().bor_imm(shifted, 1);
        let inline_val = builder.ins().bor(raw, tag);
        let heap_val = builder.ins().stack_load(types::I64, heap_slot, 0);
        let result = builder.ins().select(is_small, inline_val, heap_val);
        builder.ins().return_(&[result]);
        builder.finalize();
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    // Instrument after the IR dump and before lowering, mirroring `define_fn`, so
    // rendering work stays counted in the `instructions executed` metric.
    if instrument {
        let count_fn = builtins.id(module, "aipl_count_insns");
        instrument_insn_count(module, &mut ctx.func, count_fn);
    }
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define to_str helper: {e}")))?;
    module.clear_context(ctx);
    Ok(())
}

/// Read-only context threaded through `compile_expr` unchanged on almost every
/// recursive call. Bundling it keeps the call sites short; the only field that
/// varies (when a binding comes into scope) is `env`, set via
/// `Cx { env: &new_env, ..cx }`. It's `Copy` (all shared refs), so passing it
/// by value and spreading it is cheap.
#[derive(Clone, Copy)]
struct Cx<'a> {
    env: &'a Env,
    funcs: &'a HashMap<String, FuncInfo>,
    structs: &'a HashMap<String, TypeDef>,
    builtins: &'a Builtins,
    /// The enclosing function's declared effects (for effect-subset checks).
    effects: &'a [String],
    /// Names of parameters this instance owns (moved in). `mut y = p` for such a
    /// `p` is a move: `y` becomes exclusive and `p` is not separately dropped.
    /// Empty for a borrow instance.
    owned_params: &'a HashSet<String>,
    /// Global counter for unique names of the static string literals `to_str`
    /// synthesizes (separators, struct/field labels, `some(`/`none`).
    lit_ctr: &'a Cell<u32>,
    /// On-demand cache of per-element-type array drop/retain helper functions
    /// (for element types the fixed runtime helpers don't cover — structs and
    /// struct/optional combinations). Declared here when first needed and
    /// *defined* after the main function loop (when the build context is free).
    elem_rc: &'a RefCell<ElemRc>,
    /// The enclosing function's ABI return type. Used by the `__map_result`
    /// intrinsic to reinterpret an in-place-mapped buffer as the result element
    /// type (the buffer's static type is the *input* element type).
    ret_ty: &'a Type,
    /// The enclosing function's hidden struct-return pointer, when it returns a
    /// composite (struct/optional/result). The `?` operator's early Err return
    /// copies the propagated error into it. `None` for a scalar/unit return.
    sret: Option<Value>,
    /// Whether the enclosing function is an `fn main() -> !Error` (its ABI return
    /// is the `i64` exit code, not a result). The `?` operator's early Err return
    /// then prints `error: <msg>` and returns exit code 1 instead of an sret copy.
    error_main: bool,
    /// Sink for a source-variable legend: each named binding (param, `let`,
    /// `let mut`, `for` variable, `match` payload) records `(source name, its CLIF
    /// repr — `v<n>` for a value, `ss<n>` for a `mut` stack slot)`. Emitted as
    /// trailing `;` comments after the function so the printed IR is readable;
    /// cranelift's reader ignores the comments, so checked-in `.clif` still loads.
    /// Shared across the function's nested scopes (env clones spread `..cx`).
    bindings: &'a RefCell<Vec<(String, String)>>,
}

/// Per-element-type array drop/retain helpers, generated on demand. `fns` maps a
/// type name to its `(drop, retain)` function ids; `pending` lists the ones
/// still to be defined (with the element type to loop over).
#[derive(Default)]
struct ElemRc {
    fns: HashMap<String, (FuncId, FuncId)>,
    pending: Vec<(Type, FuncId, FuncId)>,
    // Per-`(key, value)` drop/retain helpers for a dict's pair-array elements (a
    // pair is `[key][value]`, so its cleanup releases the key *and* the value).
    pair_fns: HashMap<(String, String), (FuncId, FuncId)>,
    pair_pending: Vec<(Type, Type, FuncId, FuncId)>,
    // Per-type `to_str` rendering helpers: `__to_str_<n>(value) -> str`. Maps a
    // type name to its function id; `tostr_pending` lists the ones still to be
    // defined (with the type to render). One function per type, so the rendering
    // IR is generated once instead of inlined at every `to_str` site.
    tostr_fns: HashMap<String, FuncId>,
    tostr_pending: Vec<(Type, FuncId)>,
    ctr: u32,
}

/// Emit a direct call to `info` with `args` (already including any receiver as
/// the first arg). Handles arity/effect checks, argument retain, struct-return
/// (sret) and optional (tag, value) ABI, and tracking the result for release.
/// Returns the call's value and `info.return_ty`. (The mutating-method call
/// path reuses this, then stores the result back into the receiver variable.)
#[allow(clippy::too_many_arguments)]
/// How to dispatch and bind a `match`, resolved from the scrutinee's type.
enum MatchPlan {
    /// Optional: tag != 0 routes to arm `some`, else arm `none`. `inner` is the
    /// some arm's binding type.
    Optional {
        inner: Type,
        some: usize,
        none: usize,
    },
    /// Variant: arm `i` is selected when the tag equals `arm_tags[i]`, and binds
    /// the case's payload `(offset, type)` fields.
    Variant {
        arm_tags: Vec<usize>,
        payloads: Vec<Vec<(u32, Type)>>,
    },
}

/// Validate a `match`'s arms against the scrutinee type and resolve each arm's
/// tag + payload layout. Mirrors the checker's exhaustiveness/arity rules (it's
/// a backstop, and codegen runs on monomorphized output the checker already saw).
fn plan_match(
    scrut_ty: &Type,
    arms: &[MatchArm],
    structs: &HashMap<String, TypeDef>,
    scrut_span: Span,
) -> Result<MatchPlan, Error> {
    match scrut_ty {
        Type::Optional(inner) => {
            let find = |ctor: &str| {
                arms.iter()
                    .position(|a| a.pattern.ctor_name() == Some(ctor))
            };
            for a in arms {
                if !matches!(a.pattern.ctor_name(), Some("some") | Some("none")) {
                    return Err(Error::at(
                        format!(
                            "\"match\" on an optional expects \"some\"/\"none\", got {:?}",
                            a.pattern.ctor_name().unwrap_or("")
                        ),
                        a.span.clone(),
                    ));
                }
            }
            let some = find("some").ok_or_else(|| {
                Error::at("match is missing the \"some(v)\" arm", scrut_span.clone())
            })?;
            let none = find("none").ok_or_else(|| {
                Error::at("match is missing the \"none\" arm", scrut_span.clone())
            })?;
            Ok(MatchPlan::Optional {
                inner: (**inner).clone(),
                some,
                none,
            })
        }
        Type::Result(ok, err) => {
            // A Result matches like a 2-case variant: tag 1 = ok, 0 = err, with a
            // single payload field at OPT_VALUE_OFFSET typed by the active side.
            let find = |ctor: &str| {
                arms.iter()
                    .position(|a| a.pattern.ctor_name() == Some(ctor))
            };
            for a in arms {
                if !matches!(a.pattern.ctor_name(), Some("ok") | Some("err")) {
                    return Err(Error::at(
                        format!(
                            "\"match\" on a result expects \"ok\"/\"err\", got {:?}",
                            a.pattern.ctor_name().unwrap_or("")
                        ),
                        a.span.clone(),
                    ));
                }
            }
            let ok_i = find("ok").ok_or_else(|| {
                Error::at("match is missing the \"ok(v)\" arm", scrut_span.clone())
            })?;
            let err_i = find("err").ok_or_else(|| {
                Error::at("match is missing the \"err(e)\" arm", scrut_span.clone())
            })?;
            let mut arm_tags = vec![0usize; arms.len()];
            let mut payloads = vec![Vec::new(); arms.len()];
            arm_tags[ok_i] = 1;
            // A void-Ok (`!E`) binds nothing in its `ok` arm.
            if !is_unit(ok) {
                payloads[ok_i] = vec![(OPT_VALUE_OFFSET, (**ok).clone())];
            }
            arm_tags[err_i] = 0;
            payloads[err_i] = vec![(OPT_VALUE_OFFSET, (**err).clone())];
            Ok(MatchPlan::Variant { arm_tags, payloads })
        }
        Type::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            let vl = structs[n].as_variant().expect("variant layout");
            let mut arm_tags = Vec::with_capacity(arms.len());
            let mut payloads = Vec::with_capacity(arms.len());
            let mut seen = HashSet::new();
            for arm in arms {
                let name = arm.pattern.ctor_name().unwrap_or("");
                let (tag, case) = vl.case(name).ok_or_else(|| {
                    Error::at(format!("{n} has no constructor {name:?}"), arm.span.clone())
                })?;
                if !seen.insert(tag) {
                    return Err(Error::at(
                        format!("duplicate \"{name}\" arm"),
                        arm.span.clone(),
                    ));
                }
                arm_tags.push(tag);
                payloads.push(
                    case.fields
                        .iter()
                        .map(|f| (f.offset, f.ty.clone()))
                        .collect(),
                );
            }
            if seen.len() != vl.cases.len() {
                let missing: Vec<&str> = vl
                    .cases
                    .iter()
                    .enumerate()
                    .filter(|(t, _)| !seen.contains(t))
                    .map(|(_, c)| c.name.as_str())
                    .collect();
                return Err(Error::at(
                    format!("non-exhaustive match: missing {}", missing.join(", ")),
                    scrut_span.clone(),
                ));
            }
            Ok(MatchPlan::Variant { arm_tags, payloads })
        }
        _ => Err(Error::at(
            format!(
                "match scrutinee must be an optional or variant, got {}",
                type_name(scrut_ty)
            ),
            scrut_span.clone(),
        )),
    }
}

/// Read arm `i`'s payload bindings from the scrutinee at `ptr` (tag already in
/// `tag`). Each is `(name, value, type)` borrowed from the scrutinee.
fn bind_match_arm(
    builder: &mut FunctionBuilder,
    plan: &MatchPlan,
    arm: &MatchArm,
    i: usize,
    ptr: Value,
    tag: Value,
    structs: &HashMap<String, TypeDef>,
) -> Vec<(String, Value, Type)> {
    match plan {
        MatchPlan::Optional { inner, some, .. } => {
            if i != *some {
                return Vec::new(); // the `none` arm binds nothing
            }
            // Unwrap one optional layer: a non-optional core is read in place; a
            // nested optional is materialized in a fresh slot with `tag - 1`
            // (sharing the core value) — see "Optional representation".
            let value = if matches!(inner, Type::Optional(_)) {
                let core = opt_core(inner);
                let islot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    elem_size_of(inner, structs) as u32,
                    3,
                ));
                let ibase = builder.ins().stack_addr(types::I64, islot, 0);
                let dec = builder.ins().iadd_imm(tag, -1);
                builder.ins().store(MemFlags::trusted(), dec, ibase, 0);
                let core_val = component(builder, ptr, OPT_VALUE_OFFSET, core, structs);
                let va = builder.ins().iadd_imm(ibase, OPT_VALUE_OFFSET as i64);
                store_array_elem(builder, va, core_val, core, structs);
                ibase
            } else {
                component(builder, ptr, OPT_VALUE_OFFSET, inner, structs)
            };
            vec![(arm.pattern.bindings()[0].clone(), value, inner.clone())]
        }
        MatchPlan::Variant { payloads, .. } => arm
            .pattern
            .bindings()
            .iter()
            .zip(&payloads[i])
            .map(|(name, (offset, ty))| {
                let value = component(builder, ptr, *offset, ty, structs);
                (name.clone(), value, ty.clone())
            })
            .collect(),
    }
}

/// If `name` is a variant constructor, return its `(variant, tag, payload
/// fields)`. Constructors share a global namespace (the loader/checker reject
/// duplicates), so the first match across all variants is the one.
fn variant_ctor(
    structs: &HashMap<String, TypeDef>,
    name: &str,
) -> Option<(String, usize, Vec<(u32, Type)>)> {
    for (vname, def) in structs {
        if let Some(vl) = def.as_variant() {
            if let Some((tag, case)) = vl.case(name) {
                let fields = case
                    .fields
                    .iter()
                    .map(|f| (f.offset, f.ty.clone()))
                    .collect();
                return Some((vname.clone(), tag, fields));
            }
        }
    }
    None
}

/// Build a variant value `Ctor(args..)` in a fresh stack slot: store the tag,
/// then each payload field (retaining heap payloads, like a struct). Returns the
/// slot address (variants are addressed composites) and the variant type.
#[allow(clippy::too_many_arguments)]
fn compile_variant<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    vname: &str,
    tag: usize,
    fields: &[(u32, Type)],
    args: &[Expr],
    span: Span,
) -> Result<(Value, Type), Error> {
    if args.len() != fields.len() {
        return Err(Error::at(
            format!(
                "variant {vname} constructor takes {} argument(s), got {}",
                fields.len(),
                args.len()
            ),
            span.clone(),
        ));
    }
    let size = cx.structs[vname].size();
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 3));
    let base = builder.ins().stack_addr(types::I64, slot, 0);
    let tag_v = builder.ins().iconst(types::I64, tag as i64);
    builder.ins().store(MemFlags::trusted(), tag_v, base, 0);
    for ((offset, fty), arg) in fields.iter().zip(args) {
        let (v, actual) = compile_expr(module, builder, cx, scopes, arg)?;
        expect_type(&actual, fty, "constructor argument", arg.span.clone())?;
        let dst = builder.ins().iadd_imm(base, *offset as i64);
        store_array_elem(builder, dst, v, fty, cx.structs);
        // The variant co-owns each heap payload field — retain on store.
        emit_retain(builder, module, cx.builtins, cx.structs, v, fty);
    }
    let vty = Type::Named(vname.to_string());
    if needs_drop(&vty, cx.structs) {
        scopes
            .last_mut()
            .expect("scope")
            .push(Tracked::new(base, &vty));
    }
    Ok((base, vty))
}

/// A fresh, empty `T[]` (len 0). Used to normalize a `none` variadic argument.
/// The shape a `starts_with`/`ends_with` pattern was monomorphized to (encoded
/// in the call-name suffix by mono): the sequence, a single element, or an
/// optional element.
#[derive(Clone, Copy, PartialEq)]
enum SeShape {
    Seq,
    Elem,
    Opt,
}

/// Parse a (possibly shape-suffixed) `starts_with`/`ends_with` builtin name into
/// `(is_ends, shape)`, or `None` if it isn't one. Mono appends `$ve`/`$vo` for
/// the element/optional monomorphizations; the bare name is the sequence form.
fn starts_ends_variant(name: &str) -> Option<(bool, SeShape)> {
    let (base, shape) = if let Some(b) = name.strip_suffix("$ve") {
        (b, SeShape::Elem)
    } else if let Some(b) = name.strip_suffix("$vo") {
        (b, SeShape::Opt)
    } else {
        (name, SeShape::Seq)
    };
    match base {
        "__builtin_starts_with" => Some((false, shape)),
        "__builtin_ends_with" => Some((true, shape)),
        _ => None,
    }
}

/// Parse a (possibly shape-suffixed) `contains` builtin name into its needle
/// shape, or `None` if it isn't one. Same suffix encoding as
/// [`starts_ends_variant`]: mono appends `$ve`/`$vo` for the element/optional
/// monomorphizations; the bare name is the sequence form.
fn contains_shape(name: &str) -> Option<SeShape> {
    let (base, shape) = if let Some(b) = name.strip_suffix("$ve") {
        (b, SeShape::Elem)
    } else if let Some(b) = name.strip_suffix("$vo") {
        (b, SeShape::Opt)
    } else {
        (name, SeShape::Seq)
    };
    (base == "__builtin_contains").then_some(shape)
}

/// Build a one-char inline `str` value from a `char` register value `c`: the
/// inline layout is byte0 = `(1 << 2) | 1` (= 5) and content byte = `c`, so the
/// value is `5 | (c << 8)`. No allocation, no refcount (see the SSO note). This
/// is the `__char_to_str` builtin emitted by variadic `char*` specialization.
fn emit_char_to_str(builder: &mut FunctionBuilder, c: Value) -> Value {
    let shifted = builder.ins().ishl_imm(c, 8);
    builder.ins().bor_imm(shifted, 5)
}

/// `arr.starts_with(x)` / `arr.ends_with(x)` for a single element `x` of type
/// `elem` — the `$ve` (element) monomorphization. True iff `arr` is non-empty
/// and its first (`!is_ends`) or last (`is_ends`) element structurally equals
/// `x`, with no intermediate array built. Borrows `arr`; `emit_eq` balances its
/// own per-element refs.
fn emit_arr_starts_ends_elem<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    arr: Value,
    elem_val: Value,
    elem: &Type,
    is_ends: bool,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let len = load_arr_len(builder, arr);
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, res, 0);
    let nonempty = builder.ins().icmp(IntCC::SignedGreaterThan, len, zero);
    let chk = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(nonempty, chk, &[], merge, &[]);
    builder.switch_to_block(chk);
    builder.seal_block(chk);
    let idx = if is_ends {
        builder.ins().iadd_imm(len, -1)
    } else {
        zero
    };
    let e = load_array_elem(module, builder, builtins, arr, idx, elem, structs);
    let eq = emit_eq(module, builder, builtins, structs, e, elem_val, elem)?;
    builder.ins().stack_store(eq, res, 0);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, res, 0))
}

/// `arr.contains(x)` for a single element `x` of type `elem` — the `$ve`
/// (element) monomorphization. True iff any element of `arr` structurally
/// equals `x`, scanning forward with early exit. Borrows `arr`; `emit_eq`
/// balances its own per-element refs.
fn emit_arr_contains_elem<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    arr: Value,
    elem_val: Value,
    elem: &Type,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let len = load_arr_len(builder, arr);
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, res, 0);
    let idx = i64_slot(builder);
    builder.ins().stack_store(zero, idx, 0);
    let header = builder.create_block();
    let body = builder.create_block();
    let found = builder.create_block();
    let merge = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder.ins().brif(more, body, &[], merge, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    let e = load_array_elem(module, builder, builtins, arr, i, elem, structs);
    let eq = emit_eq(module, builder, builtins, structs, e, elem_val, elem)?;
    let cont = builder.create_block();
    builder.ins().brif(eq, found, &[], cont, &[]);
    builder.switch_to_block(cont);
    builder.seal_block(cont);
    let next = builder.ins().iadd_imm(i, 1);
    builder.ins().stack_store(next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(found);
    builder.seal_block(found);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(one, res, 0);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, res, 0))
}

/// `arr.contains(other)` for two arrays of element type `elem` — the sequence
/// monomorphization. True iff `other`'s elements equal a contiguous run of
/// `arr`'s at any offset (the array analog of a substring search; the empty
/// needle always matches). A naive O(la·lb) window scan — fine for the array
/// sizes structural `emit_eq` comparison is fine for. Borrows both arrays;
/// `emit_eq` balances its own per-element refs.
fn emit_arr_contains_seq<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    self_ptr: Value,
    other_ptr: Value,
    elem: &Type,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let la = load_arr_len(builder, self_ptr);
    let lb = load_arr_len(builder, other_ptr);
    // Both arrays are the untyped empty literal (`[].contains([])`): the empty
    // needle matches, and there's no element type to compare. Skip the loops —
    // `emit_eq` can't lower a `__none__` element.
    if is_none_inner(elem) {
        let empty = builder.ins().icmp_imm(IntCC::Equal, lb, 0);
        return Ok(builder.ins().uextend(types::I64, empty));
    }
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(zero, res, 0);
    // A needle longer than the receiver can't occur in it (and for `lb <= la`
    // the window offsets `0..=la-lb` are valid; `lb == 0` matches at offset 0).
    let fits = builder.ins().icmp(IntCC::SignedLessThanOrEqual, lb, la);
    let outer_pre = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(fits, outer_pre, &[], merge, &[]);

    builder.switch_to_block(outer_pre);
    builder.seal_block(outer_pre);
    let limit = builder.ins().isub(la, lb);
    let start = i64_slot(builder);
    builder.ins().stack_store(zero, start, 0);
    let idx = i64_slot(builder);
    let outer_header = builder.create_block();
    let outer_body = builder.create_block();
    let inner_header = builder.create_block();
    let inner_body = builder.create_block();
    let next_start = builder.create_block();
    let found = builder.create_block();
    builder.ins().jump(outer_header, &[]);

    builder.switch_to_block(outer_header);
    let s = builder.ins().stack_load(types::I64, start, 0);
    let more_windows = builder.ins().icmp(IntCC::SignedLessThanOrEqual, s, limit);
    builder
        .ins()
        .brif(more_windows, outer_body, &[], merge, &[]);

    builder.switch_to_block(outer_body);
    builder.seal_block(outer_body);
    builder.ins().stack_store(zero, idx, 0);
    builder.ins().jump(inner_header, &[]);

    builder.switch_to_block(inner_header);
    let i = builder.ins().stack_load(types::I64, idx, 0);
    let more_elems = builder.ins().icmp(IntCC::SignedLessThan, i, lb);
    // The whole needle matched at this window — found.
    builder.ins().brif(more_elems, inner_body, &[], found, &[]);

    builder.switch_to_block(inner_body);
    builder.seal_block(inner_body);
    let si = builder.ins().iadd(s, i);
    let el = load_array_elem(module, builder, builtins, self_ptr, si, elem, structs);
    let er = load_array_elem(module, builder, builtins, other_ptr, i, elem, structs);
    let ee = emit_eq(module, builder, builtins, structs, el, er, elem)?;
    let inner_cont = builder.create_block();
    builder.ins().brif(ee, inner_cont, &[], next_start, &[]);
    builder.switch_to_block(inner_cont);
    builder.seal_block(inner_cont);
    let next_i = builder.ins().iadd_imm(i, 1);
    builder.ins().stack_store(next_i, idx, 0);
    builder.ins().jump(inner_header, &[]);
    builder.seal_block(inner_header);

    builder.switch_to_block(next_start);
    builder.seal_block(next_start);
    let next_s = builder.ins().iadd_imm(s, 1);
    builder.ins().stack_store(next_s, start, 0);
    builder.ins().jump(outer_header, &[]);
    builder.seal_block(outer_header);

    builder.switch_to_block(found);
    builder.seal_block(found);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(one, res, 0);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, res, 0))
}

fn compile_call<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    name: &str,
    info: &FuncInfo,
    args: &[Expr],
    span: Span,
) -> Result<(Value, Type), Error> {
    let Cx {
        structs,
        builtins,
        effects: current_effects,
        ..
    } = cx;
    let disp = display_name(name);
    if info.params.len() != args.len() {
        return Err(Error::at(
            format!(
                "fn {disp:?} expects {} arg(s), got {}",
                info.params.len(),
                args.len()
            ),
            span.clone(),
        ));
    }
    // Callee's effects must be a subset of the current function's declared ones.
    for effect in &info.effects {
        if !current_effects.iter().any(|e| e == effect) {
            return Err(Error::at(
                format!(
                    "fn {disp:?} has effect \"!{effect}\" but the calling function does not declare it"
                ),
                span.clone(),
            ));
        }
    }
    let mut arg_values = Vec::with_capacity(args.len());
    for (idx, (arg, expected)) in args.iter().zip(info.params.iter()).enumerate() {
        let (v, actual) = compile_expr(module, builder, cx, scopes, arg)?;
        // A bare literal argument flexes to a narrow-int parameter (its
        // i64-register value is already canonical when it fits — checker-verified).
        let actual = aipl_syntax::flex_int_ty(arg, &actual, expected);
        expect_type(
            &actual,
            expected,
            &format!("fn {disp:?} arg {idx}"),
            arg.span.clone(),
        )?;
        let v = coerce_empty_to_char_array(builder, module, builtins, v, &actual, expected);
        arg_values.push(v);
    }
    // Hand each heap (str/array) arg to the callee. A borrowed param is
    // retained (the callee decs it on return, the caller keeps its own ref). An
    // *owned* param is moved: the arg is a fresh, uniquely-owned value, so we
    // transfer our sole reference (no inc) and stop tracking it here — the
    // callee consumes it. Monomorphization only marks a param owned when the arg
    // is fresh, so it's the matching tracked value in this scope.
    for (idx, (v, expected)) in arg_values.iter().zip(info.params.iter()).enumerate() {
        if !is_heap(expected) {
            continue;
        }
        if info.owned_params.contains(&idx) {
            let scope = scopes.last_mut().expect("scope");
            match scope
                .iter()
                .rposition(|t| matches!(t.owned, Owned::Value(tv) if tv == *v))
            {
                Some(pos) => {
                    scope.remove(pos);
                }
                // Not separately tracked: retain so refcounts stay balanced.
                None => emit_inc(builder, module, builtins, *v),
            }
        } else {
            emit_inc(builder, module, builtins, *v);
        }
    }
    // A composite result (struct or optional) is returned through a caller-
    // provided pointer (sret): allocate a slot of its size and pass its address.
    let sret = sret_size(&info.return_ty, structs).map(|size| {
        let slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        builder.ins().stack_addr(types::I64, slot, 0)
    });
    let call_args: Vec<Value> = match sret {
        Some(s) => std::iter::once(s).chain(arg_values).collect(),
        None => arg_values,
    };
    // A user function is already declared; a builtin's import is declared lazily
    // here on first reference.
    let callee_id = match info.link {
        FuncLink::User(id) => id,
        FuncLink::Builtin(sym) => builtins.id(module, sym),
    };
    let local_callee = module.declare_func_in_func(callee_id, builder.func);
    let inst = builder.ins().call(local_callee, &call_args);
    let ret_v = if let Some(s) = sret {
        s
    } else if is_unit(&info.return_ty) {
        builder.ins().iconst(types::I64, 0)
    } else {
        builder.inst_results(inst)[0]
    };
    if needs_drop(&info.return_ty, structs) {
        scopes
            .last_mut()
            .expect("scope")
            .push(Tracked::new(ret_v, &info.return_ty));
    }
    Ok((ret_v, info.return_ty.clone()))
}

/// Compile a call expression, dispatched on the callee `name` — the
/// `ExprKind::Call` handling extracted from [`compile_expr`] so expression
/// dispatch stays readable. `args`/`style` are the call's arguments and method-
/// call flag; `span` is the call site. Reserved builtin / intrinsic names are
/// matched first; the wildcard arm compiles an ordinary (monomorphized) user or
/// builtin call via [`compile_call`].
fn compile_call_expr<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    name: &str,
    args: &[Expr],
    style: bool,
    span: Span,
) -> Result<(Value, Type), Error> {
    let Cx {
        env,
        funcs,
        structs,
        builtins,
        effects: _,
        owned_params: _,
        lit_ctr: _,
        elem_rc: _,
        ret_ty: _,
        sret: _,
        error_main: _,
        bindings: _,
    } = cx;
    Ok(match name {
        "__builtin_wrapping_add"
        | "__builtin_saturating_add"
        | "__builtin_wrapping_sub"
        | "__builtin_saturating_sub" => {
            // `a + b` / `a - b` resolved (in the loader) to their bound integer
            // arithmetic builtin. Both operands are the same integer type
            // (checker-verified); a bare literal flexes to the other's width. The
            // flavor (wrapping/saturating) and operation (add/sub) are the only
            // differences — see `emit_int_addsub`. Scalar ints carry no refcount,
            // so there's nothing to track.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("{name:?} expects 2 arguments, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (lv, lt) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (rv, rt) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let lt = aipl_syntax::flex_int_ty(&args[0], &lt, &rt);
            let rt = aipl_syntax::flex_int_ty(&args[1], &rt, &lt);
            let p = match (&lt, &rt) {
                (Type::Primitive(a), Type::Primitive(b)) if a.is_int() && a == b => *a,
                _ => {
                    expect_type(
                        &lt,
                        &Type::Primitive(Primitive::I64),
                        "arithmetic operand",
                        args[0].span.clone(),
                    )?;
                    expect_type(
                        &rt,
                        &Type::Primitive(Primitive::I64),
                        "arithmetic operand",
                        args[1].span.clone(),
                    )?;
                    Primitive::I64
                }
            };
            let sub = name.ends_with("_sub");
            let saturating = name.starts_with("__builtin_saturating_");
            let out = emit_int_addsub(builder, lv, rv, p, sub, saturating);
            (out, Type::Primitive(p))
        }
        "__builtin_to_str" => {
            // Generic `to_str(x)`: render by the argument's static type.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"to_str\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let s = emit_to_str(module, builder, cx, scopes, v, &t)?;
            (s, Type::Primitive(Primitive::Str))
        }
        "__template_interp" => {
            // Template-literal interpolation: pass `str` through as-is; convert
            // any other type via `to_str`. This avoids wrapping string values in
            // quotes (which `to_str` would do).
            if args.len() != 1 {
                return Err(Error::at(
                    format!(
                        "\"__template_interp\" expects 1 argument, got {}",
                        args.len()
                    ),
                    span.clone(),
                ));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if is_str_repr(&t) {
                (v, Type::Primitive(Primitive::Str))
            } else {
                let s = emit_to_str(module, builder, cx, scopes, v, &t)?;
                (s, Type::Primitive(Primitive::Str))
            }
        }
        "__builtin_hash" => {
            // Generic `hash(x) -> i64`: structural hash by the argument's static
            // type. Borrows the argument (no consume), so its scope-track is
            // untouched.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"hash\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let h = emit_hash(module, builder, cx.builtins, cx.structs, v, &t)?;
            (h, Type::Primitive(Primitive::I64))
        }
        "__builtin_minimum" | "__builtin_maximum" => {
            // `arr.minimum()` / `arr.maximum()`: smallest / largest element as
            // `T?` (`none` if empty). Elements are comparable scalars (the checker
            // restricts to integers/char), so the optional owns no heap. Walks
            // the array with a running accumulator, like the `for`-loop arm.
            if args.len() != 1 {
                return Err(Error::at(
                    format!(
                        "{:?} expects 1 argument, got {}",
                        display_name(name),
                        args.len()
                    ),
                    span.clone(),
                ));
            }
            let is_min = name == "__builtin_minimum";
            let (arr_ptr, arr_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let elem = match &arr_ty {
                Type::Array(e) => (**e).clone(),
                _ => {
                    return Err(Error::at(
                        format!("{:?} of one argument expects an array", display_name(name)),
                        args[0].span.clone(),
                    ))
                }
            };
            let opt_ty = Type::Optional(Box::new(elem.clone()));
            // Result slot: a scalar optional `{tag, value}` (no heap, no drop).
            let rslot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&opt_ty, structs) as u32,
                3,
            ));
            let result_ptr = builder.ins().stack_addr(types::I64, rslot, 0);
            let len = seq_len(module, builder, builtins, arr_ptr, &arr_ty);
            let zero = builder.ins().iconst(types::I64, 0);
            let is_empty = builder.ins().icmp(IntCC::Equal, len, zero);
            let empty_b = builder.create_block();
            let nonempty_b = builder.create_block();
            let done = builder.create_block();
            builder.ins().brif(is_empty, empty_b, &[], nonempty_b, &[]);

            // Empty: `none` — tag 0 (the value field is unused).
            builder.switch_to_block(empty_b);
            builder.seal_block(empty_b);
            builder
                .ins()
                .store(MemFlags::trusted(), zero, result_ptr, 0);
            builder.ins().jump(done, &[]);

            // Non-empty: acc = elem[0]; then fold elem[1..] keeping the
            // smaller (`min`) / larger (`max`); finally `some(acc)`.
            builder.switch_to_block(nonempty_b);
            builder.seal_block(nonempty_b);
            let acc_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let i_slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let acc0 = seq_elem(module, builder, builtins, structs, arr_ptr, zero, &arr_ty);
            builder.ins().stack_store(acc0, acc_slot, 0);
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(one, i_slot, 0);
            let header = builder.create_block();
            let body = builder.create_block();
            let after = builder.create_block();
            builder.ins().jump(header, &[]);

            builder.switch_to_block(header);
            let i = builder.ins().stack_load(types::I64, i_slot, 0);
            let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
            builder.ins().brif(more, body, &[], after, &[]);

            builder.switch_to_block(body);
            builder.seal_block(body);
            let e = seq_elem(module, builder, builtins, structs, arr_ptr, i, &arr_ty);
            let acc = builder.ins().stack_load(types::I64, acc_slot, 0);
            let cc = if is_min {
                IntCC::SignedLessThan
            } else {
                IntCC::SignedGreaterThan
            };
            let take = builder.ins().icmp(cc, e, acc);
            let new_acc = builder.ins().select(take, e, acc);
            builder.ins().stack_store(new_acc, acc_slot, 0);
            let inext = builder.ins().iadd_imm(i, 1);
            builder.ins().stack_store(inext, i_slot, 0);
            builder.ins().jump(header, &[]);
            builder.seal_block(header);

            builder.switch_to_block(after);
            builder.seal_block(after);
            let acc = builder.ins().stack_load(types::I64, acc_slot, 0);
            emit_build_some(builder, result_ptr, acc, &elem, structs);
            builder.ins().jump(done, &[]);

            builder.switch_to_block(done);
            builder.seal_block(done);
            (result_ptr, opt_ty)
        }
        "__builtin_min" | "__builtin_max" => {
            // `min(a, b)` / `max(a, b)` on `i64`: compare and select the smaller
            // or larger. Both operands are scalars (own no heap), so no inc/dec.
            let disp = display_name(name);
            if args.len() != 2 {
                return Err(Error::at(
                    format!("{disp:?} expects 2 arguments, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (a, at) = compile_expr(module, builder, cx, scopes, &args[0])?;
            expect_type(
                &at,
                &Type::Primitive(Primitive::I64),
                "min/max operand",
                args[0].span.clone(),
            )?;
            let (b, bt) = compile_expr(module, builder, cx, scopes, &args[1])?;
            expect_type(
                &bt,
                &Type::Primitive(Primitive::I64),
                "min/max operand",
                args[1].span.clone(),
            )?;
            // `min`: keep `a` when `a < b`; `max`: keep `a` when `a > b`.
            let cc = if name == "__builtin_min" {
                IntCC::SignedLessThan
            } else {
                IntCC::SignedGreaterThan
            };
            let cond = builder.ins().icmp(cc, a, b);
            let r = builder.ins().select(cond, a, b);
            (r, Type::Primitive(Primitive::I64))
        }
        "__builtin_split" => {
            // `split(s, sep) -> str[]`: the runtime builds the array of parts
            // (views of `s` for long parts, copies for short). It consumes both
            // str refs, so inc each first (our scope-tracked refs must survive),
            // then track the fresh array for drop.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("split expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (s_v, s_t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            expect_type(
                &s_t,
                &Type::Primitive(Primitive::Str),
                "split receiver",
                args[0].span.clone(),
            )?;
            let (sep_v, sep_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            expect_type(
                &sep_t,
                &Type::Primitive(Primitive::Str),
                "split separator",
                args[1].span.clone(),
            )?;
            emit_inc(builder, module, builtins, s_v);
            emit_inc(builder, module, builtins, sep_v);
            let f = builtins.import(module, builder.func, "aipl_str_split");
            let inst = builder.ins().call(f, &[s_v, sep_v]);
            let result = builder.inst_results(inst)[0];
            let ty = Type::Array(Box::new(Type::Primitive(Primitive::Str)));
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(result, &ty));
            (result, ty)
        }
        "__builtin_read_file_to_string" => {
            // `(str) -> str!str`: ok(contents) on success, err(message) on any
            // failure. The runtime returns the contents pointer or null; codegen
            // wraps it into the Result with a static error message. The
            // `!read_files` effect is already enforced by the checker.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("read_file_to_string expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (name_v, name_t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            expect_type(
                &name_t,
                &Type::Primitive(Primitive::Str),
                "read_file_to_string filename",
                args[0].span.clone(),
            )?;
            // The runtime consumes (decs) the filename ref, so inc to keep our
            // scope-tracked ref alive.
            emit_inc(builder, module, builtins, name_v);
            let local = builtins.import(module, builder.func, "aipl_read_file_to_string");
            let inst = builder.ins().call(local, &[name_v]);
            let raw = builder.inst_results(inst)[0];
            let result_ty = Type::Result(
                Box::new(Type::Primitive(Primitive::Str)),
                Box::new(error_ty()),
            );
            let ptr = emit_file_result(
                module,
                builder,
                cx,
                raw,
                /*ok_is_value=*/ true,
                b"could not read file",
            )?;
            // The ok payload is a fresh, owned str (the err is a static literal):
            // track so it's released at scope exit (drop decs it only on tag 1).
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &result_ty));
            (ptr, result_ty)
        }
        "__builtin_write_string_to_file" => {
            // `(str, str) -> !str`: ok() on success, err(message) on failure. The
            // runtime returns 1/0; codegen wraps it into the void-Ok Result with a
            // static error message. The `!write_files` effect is checker-enforced.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("write_string_to_file expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (path_v, path_t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            expect_type(
                &path_t,
                &Type::Primitive(Primitive::Str),
                "write_string_to_file path",
                args[0].span.clone(),
            )?;
            let (data_v, data_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            expect_type(
                &data_t,
                &Type::Primitive(Primitive::Str),
                "write_string_to_file contents",
                args[1].span.clone(),
            )?;
            // The runtime consumes (decs) both str args, so inc to keep ours alive.
            emit_inc(builder, module, builtins, path_v);
            emit_inc(builder, module, builtins, data_v);
            let local = builtins.import(module, builder.func, "aipl_write_string_to_file");
            let inst = builder.ins().call(local, &[path_v, data_v]);
            let code = builder.inst_results(inst)[0];
            let result_ty = Type::Result(Box::new(Type::Unit), Box::new(error_ty()));
            let ptr = emit_file_result(
                module,
                builder,
                cx,
                code,
                /*ok_is_value=*/ false,
                b"could not write file",
            )?;
            // Neither payload needs freeing (ok is unit, err is a static literal),
            // so no scope tracking is required.
            (ptr, result_ty)
        }
        "__builtin_value_or" => {
            // `value_or(opt: T?, default: T) -> T` — the optional's value if
            // present, else the default. Optionals of i64/bool/char all carry
            // a single i64 value, so this is a tag `select` with no per-type
            // specialization needed.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("value_or expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (opt_ptr, opt_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let inner = match &opt_ty {
                Type::Optional(i) => (**i).clone(),
                _ => {
                    return Err(Error::at(
                        format!(
                            "value_or is only callable on an optional, got {}",
                            type_name(&opt_ty)
                        ),
                        args[0].span.clone(),
                    ));
                }
            };
            let (default_v, default_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // The result type is the optional's element type — or, for a bare
            // `none` receiver (unknown element), the default's type.
            let result_ty = if is_none_inner(&inner) {
                default_ty.clone()
            } else {
                expect_type(
                    &default_ty,
                    &inner,
                    "value_or default",
                    args[1].span.clone(),
                )?;
                inner
            };
            if !is_array_elem(&result_ty) {
                return Err(Error::at(
                    format!(
                        "value_or's value must be a scalar, str, or array, not {} (use match to \
                         unwrap a nested optional)",
                        type_name(&result_ty)
                    ),
                    span.clone(),
                ));
            }
            let tag = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), opt_ptr, 0);
            let value = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), opt_ptr, 8);
            // select(tag, value, default): value when tag != 0, else default.
            let result = builder.ins().select(tag, value, default_v);
            // The result is borrowed (from the optional, or it's the default,
            // which the caller's scope also tracks); retain it as an
            // independently-owned ref and track it for release.
            if needs_drop(&result_ty, structs) {
                emit_retain(builder, module, builtins, structs, result, &result_ty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(result, &result_ty));
            }
            (result, result_ty)
        }
        "__builtin_is_some" => {
            // `is_some(opt: T?) -> bool` — true when the optional is present.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"is_some\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (ptr, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if !matches!(&recv_ty, Type::Optional(_)) {
                return Err(Error::at(
                    format!(
                        "\"is_some\" is only callable on an optional, got {}",
                        type_name(&recv_ty)
                    ),
                    args[0].span.clone(),
                ));
            }
            // `is_some` is the *outermost* layer: any nonzero tag is `some`
            // (the tag can be > 1 for a nested optional), so normalize to 0/1.
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), ptr, 0);
            let nz = builder.ins().icmp_imm(IntCC::NotEqual, tag, 0);
            let b = builder.ins().uextend(types::I64, nz);
            (b, Type::Primitive(Primitive::Bool))
        }
        "__builtin_is_space" => {
            // `c.is_space() -> bool` — true when c is ASCII whitespace.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"is_space\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (c, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if !matches!(&recv_ty, Type::Primitive(Primitive::Char)) {
                return Err(Error::at(
                    format!(
                        "\"is_space\" is only callable on a char, got {}",
                        type_name(&recv_ty)
                    ),
                    args[0].span.clone(),
                ));
            }
            // Check if c is ' ', '\t', '\n', or '\r' (32, 9, 10, 13).
            let sp = builder.ins().icmp_imm(IntCC::Equal, c, 32);
            let tab = builder.ins().icmp_imm(IntCC::Equal, c, 9);
            let lf = builder.ins().icmp_imm(IntCC::Equal, c, 10);
            let cr = builder.ins().icmp_imm(IntCC::Equal, c, 13);
            let or1 = builder.ins().bor(sp, tab);
            let or2 = builder.ins().bor(lf, cr);
            let result = builder.ins().bor(or1, or2);
            let b = builder.ins().uextend(types::I64, result);
            (b, Type::Primitive(Primitive::Bool))
        }
        "__builtin_is_digit" => {
            // `c.is_digit() -> bool` — true when c is '0'..'9'.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"is_digit\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (c, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if !matches!(&recv_ty, Type::Primitive(Primitive::Char)) {
                return Err(Error::at(
                    format!(
                        "\"is_digit\" is only callable on a char, got {}",
                        type_name(&recv_ty)
                    ),
                    args[0].span.clone(),
                ));
            }
            // Check if '0' (48) <= c <= '9' (57).
            let ge_0 = builder
                .ins()
                .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, c, 48);
            let le_9 = builder
                .ins()
                .icmp_imm(IntCC::UnsignedLessThanOrEqual, c, 57);
            let result = builder.ins().band(ge_0, le_9);
            let b = builder.ins().uextend(types::I64, result);
            (b, Type::Primitive(Primitive::Bool))
        }
        "__builtin_len" => {
            // `len(a) -> i64` — element/byte count. Reads `a` without consuming
            // it (it stays live in the caller's scope), so no inc/dec.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("fn \"len\" expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (ptr, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let len = if is_str_shaped(&t) {
                // A `str` (or a str-shaped `char[]`, see `is_str_shaped`) stores
                // no length field (it can be inline/owned/view); `aipl_str_len`
                // computes the byte length for any representation.
                let f = builtins.import(module, builder.func, "aipl_str_len");
                let inst = builder.ins().call(f, &[ptr]);
                builder.inst_results(inst)[0]
            } else if matches!(t, Type::Array(_) | Type::Set(_) | Type::Dict(_, _)) {
                // A set/dict shares the array layout, so its element/pair count is
                // the same `len` field.
                load_arr_len(builder, ptr)
            } else {
                return Err(Error::at(
                    format!(
                        "\"len\" expects a str, array, set, or dict, got {}",
                        type_name(&t)
                    ),
                    args[0].span.clone(),
                ));
            };
            (len, Type::Primitive(Primitive::I64))
        }
        "__builtin_reverse" => {
            // `xs.reverse() -> T[]` / `s.reverse() -> str` — new sequence with
            // elements (or bytes) in reverse order. Consumes `self` (callers pre-inc).
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"reverse\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (ptr, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if is_str_repr(&t) {
                emit_inc(builder, module, builtins, ptr);
                let f = builtins.import(module, builder.func, "aipl_str_reverse");
                let inst = builder.ins().call(f, &[ptr]);
                let result = builder.inst_results(inst)[0];
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(result, &Type::Primitive(Primitive::Str)));
                (result, Type::Primitive(Primitive::Str))
            } else if is_char_array(&t) {
                // Str-shaped (see `is_char_array`), but — unlike a bare `str`
                // receiver above — keeps its nominal `char[]` type.
                emit_inc(builder, module, builtins, ptr);
                let f = builtins.import(module, builder.func, "aipl_str_reverse");
                let inst = builder.ins().call(f, &[ptr]);
                let result = builder.inst_results(inst)[0];
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(result, &t));
                (result, t)
            } else if let Type::Array(elem) = &t {
                let elem = (**elem).clone();
                emit_inc(builder, module, builtins, ptr);
                let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
                let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(&elem, structs));
                let f = builtins.import(module, builder.func, "aipl_arr_reverse");
                let inst = builder.ins().call(f, &[ptr, drop_fn, retain_fn, esz]);
                let view = builder.inst_results(inst)[0];
                let arr_ty = Type::Array(Box::new(elem));
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(view, &arr_ty));
                (view, arr_ty)
            } else {
                return Err(Error::at(
                    format!("\"reverse\" expects a str or array, got {}", type_name(&t)),
                    args[0].span.clone(),
                ));
            }
        }
        _ if starts_ends_variant(name).is_some() => {
            // `s.starts_with(p)` / `s.ends_with(p) -> bool` over a `str` (byte
            // compare) or `T[]` (element-wise structural compare). The pattern is
            // variadic; monomorphization has already resolved its shape into the
            // name suffix (`$ve` element, `$vo` optional, none = the sequence),
            // so each shape is implemented directly here — its own
            // monomorphization. The empty pattern always matches; a pattern
            // longer than the receiver never does.
            let (is_ends, shape) = starts_ends_variant(name).unwrap();
            if args.len() != 2 {
                return Err(Error::at(
                    format!(
                        "{:?} expects 2 args, got {}",
                        display_name(name),
                        args.len()
                    ),
                    span.clone(),
                ));
            }
            let (recv, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (pat_v, pat_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let result = if is_str_shaped(&recv_ty) {
                // `char*` pattern. The str runtime consumes (decs) both refs, so
                // each str handed to it is pre-inc'd; a built 1-char string is
                // inline, so its inc/dec are no-ops. For the optional shape `none`
                // matches (a `""` prefix/suffix); `some(c)` compares the 1-char
                // string. The element/optional value is materialized only as an
                // inline string — no allocation.
                let sym = if is_ends {
                    "aipl_str_ends_with"
                } else {
                    "aipl_str_starts_with"
                };
                // The 1-char-or-whole `str` pattern to compare directly, or
                // `None` for the optional shape (handled with a tag branch).
                let pat: Option<Value> = match shape {
                    SeShape::Seq => Some(pat_v),
                    SeShape::Elem => Some(emit_char_to_str(builder, pat_v)),
                    SeShape::Opt => None,
                };
                if let Some(pat) = pat {
                    emit_inc(builder, module, builtins, recv);
                    emit_inc(builder, module, builtins, pat);
                    let f = builtins.import(module, builder.func, sym);
                    let inst = builder.ins().call(f, &[recv, pat]);
                    builder.inst_results(inst)[0]
                } else {
                    // Optional `char?`: `none` → true; `some(c)` → str compare.
                    let res = i64_slot(builder);
                    let tag = builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), pat_v, 0);
                    let is_some = builder.ins().icmp_imm(IntCC::NotEqual, tag, 0);
                    let some_b = builder.create_block();
                    let none_b = builder.create_block();
                    let merge = builder.create_block();
                    builder.ins().brif(is_some, some_b, &[], none_b, &[]);
                    builder.switch_to_block(none_b);
                    builder.seal_block(none_b);
                    let one = builder.ins().iconst(types::I64, 1);
                    builder.ins().stack_store(one, res, 0);
                    builder.ins().jump(merge, &[]);
                    builder.switch_to_block(some_b);
                    builder.seal_block(some_b);
                    let cv = builder.ins().load(
                        types::I64,
                        MemFlags::trusted(),
                        pat_v,
                        OPT_VALUE_OFFSET as i32,
                    );
                    let s = emit_char_to_str(builder, cv);
                    emit_inc(builder, module, builtins, recv);
                    let f = builtins.import(module, builder.func, sym);
                    let inst = builder.ins().call(f, &[recv, s]);
                    let r = builder.inst_results(inst)[0];
                    builder.ins().stack_store(r, res, 0);
                    builder.ins().jump(merge, &[]);
                    builder.switch_to_block(merge);
                    builder.seal_block(merge);
                    builder.ins().stack_load(types::I64, res, 0)
                }
            } else {
                // `T[]` pattern — element-wise structural compare. Borrows both
                // arrays; `emit_eq` balances its own per-element refs.
                let self_elem = match &recv_ty {
                    Type::Array(e) => (**e).clone(),
                    other => {
                        return Err(Error::at(
                            format!(
                                "{:?} expects a str or array, got {}",
                                display_name(name),
                                type_name(other)
                            ),
                            args[0].span.clone(),
                        ));
                    }
                };
                // The comparison element type: the receiver's, or — for an empty
                // `[]` receiver (untyped element) — the pattern's.
                let elem = if !is_none_inner(&self_elem) {
                    self_elem
                } else {
                    match (shape, &pat_ty) {
                        (SeShape::Elem, t) => t.clone(),
                        (_, Type::Array(e) | Type::Optional(e)) => (**e).clone(),
                        (_, t) => t.clone(),
                    }
                };
                match shape {
                    SeShape::Seq => {
                        emit_arr_starts_ends(module, builder, cx, recv, pat_v, &elem, is_ends)?
                    }
                    SeShape::Elem => {
                        emit_arr_starts_ends_elem(module, builder, cx, recv, pat_v, &elem, is_ends)?
                    }
                    SeShape::Opt => {
                        // `none` → true; `some(v)` → single-element compare.
                        let res = i64_slot(builder);
                        let tag = builder
                            .ins()
                            .load(types::I64, MemFlags::trusted(), pat_v, 0);
                        let is_some = builder.ins().icmp_imm(IntCC::NotEqual, tag, 0);
                        let some_b = builder.create_block();
                        let none_b = builder.create_block();
                        let merge = builder.create_block();
                        builder.ins().brif(is_some, some_b, &[], none_b, &[]);
                        builder.switch_to_block(none_b);
                        builder.seal_block(none_b);
                        let one = builder.ins().iconst(types::I64, 1);
                        builder.ins().stack_store(one, res, 0);
                        builder.ins().jump(merge, &[]);
                        builder.switch_to_block(some_b);
                        builder.seal_block(some_b);
                        let cv = component(builder, pat_v, OPT_VALUE_OFFSET, &elem, structs);
                        let r = emit_arr_starts_ends_elem(
                            module, builder, cx, recv, cv, &elem, is_ends,
                        )?;
                        builder.ins().stack_store(r, res, 0);
                        builder.ins().jump(merge, &[]);
                        builder.switch_to_block(merge);
                        builder.seal_block(merge);
                        builder.ins().stack_load(types::I64, res, 0)
                    }
                }
            };
            (result, Type::Primitive(Primitive::Bool))
        }
        _ if contains_shape(name).is_some() => {
            // `s.contains(n) -> bool` over a `str` (byte window compare) or
            // `T[]` (element-wise structural compare). The needle is variadic;
            // monomorphization has already resolved its shape into the name
            // suffix (`$ve` element, `$vo` optional, none = the sequence), so
            // each shape is implemented directly here. The empty needle always
            // matches; a `none` needle is nothing to find, so it never does
            // (unlike `starts_with`/`ends_with`, whose `none` is the
            // always-matching empty pattern).
            let shape = contains_shape(name).unwrap();
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"contains\" expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (recv, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (ndl_v, ndl_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let result = if is_str_shaped(&recv_ty) {
                // `char*` needle. The str runtime consumes (decs) both refs, so
                // each str handed to it is pre-inc'd; a built 1-char string is
                // inline, so its inc/dec are no-ops. The element/optional value
                // is materialized only as an inline string — no allocation.
                let ndl: Option<Value> = match shape {
                    SeShape::Seq => Some(ndl_v),
                    SeShape::Elem => Some(emit_char_to_str(builder, ndl_v)),
                    SeShape::Opt => None,
                };
                if let Some(ndl) = ndl {
                    emit_inc(builder, module, builtins, recv);
                    emit_inc(builder, module, builtins, ndl);
                    let f = builtins.import(module, builder.func, "aipl_str_contains");
                    let inst = builder.ins().call(f, &[recv, ndl]);
                    builder.inst_results(inst)[0]
                } else {
                    // Optional `char?`: `none` → false; `some(c)` → window scan.
                    let res = i64_slot(builder);
                    let zero = builder.ins().iconst(types::I64, 0);
                    builder.ins().stack_store(zero, res, 0);
                    let tag = builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ndl_v, 0);
                    let is_some = builder.ins().icmp_imm(IntCC::NotEqual, tag, 0);
                    let some_b = builder.create_block();
                    let merge = builder.create_block();
                    builder.ins().brif(is_some, some_b, &[], merge, &[]);
                    builder.switch_to_block(some_b);
                    builder.seal_block(some_b);
                    let cv = builder.ins().load(
                        types::I64,
                        MemFlags::trusted(),
                        ndl_v,
                        OPT_VALUE_OFFSET as i32,
                    );
                    let s = emit_char_to_str(builder, cv);
                    emit_inc(builder, module, builtins, recv);
                    let f = builtins.import(module, builder.func, "aipl_str_contains");
                    let inst = builder.ins().call(f, &[recv, s]);
                    let r = builder.inst_results(inst)[0];
                    builder.ins().stack_store(r, res, 0);
                    builder.ins().jump(merge, &[]);
                    builder.switch_to_block(merge);
                    builder.seal_block(merge);
                    builder.ins().stack_load(types::I64, res, 0)
                }
            } else {
                // `T*` needle — element-wise structural compare. Borrows both
                // arrays; `emit_eq` balances its own per-element refs.
                let self_elem = match &recv_ty {
                    Type::Array(e) => (**e).clone(),
                    other => {
                        return Err(Error::at(
                            format!(
                                "\"contains\" expects a str or array, got {}",
                                type_name(other)
                            ),
                            args[0].span.clone(),
                        ));
                    }
                };
                // The comparison element type: the receiver's, or — for an empty
                // `[]` receiver (untyped element) — the needle's.
                let elem = if !is_none_inner(&self_elem) {
                    self_elem
                } else {
                    match (shape, &ndl_ty) {
                        (SeShape::Elem, t) => t.clone(),
                        (_, Type::Array(e) | Type::Optional(e)) => (**e).clone(),
                        (_, t) => t.clone(),
                    }
                };
                match shape {
                    SeShape::Seq => emit_arr_contains_seq(module, builder, cx, recv, ndl_v, &elem)?,
                    SeShape::Elem => {
                        emit_arr_contains_elem(module, builder, cx, recv, ndl_v, &elem)?
                    }
                    SeShape::Opt if is_none_inner(&elem) => {
                        // `[].contains(none)`: an untyped `none` needle in an
                        // untyped empty array — nothing to find.
                        builder.ins().iconst(types::I64, 0)
                    }
                    SeShape::Opt => {
                        // `none` → false; `some(v)` → single-element scan.
                        let res = i64_slot(builder);
                        let zero = builder.ins().iconst(types::I64, 0);
                        builder.ins().stack_store(zero, res, 0);
                        let tag = builder
                            .ins()
                            .load(types::I64, MemFlags::trusted(), ndl_v, 0);
                        let is_some = builder.ins().icmp_imm(IntCC::NotEqual, tag, 0);
                        let some_b = builder.create_block();
                        let merge = builder.create_block();
                        builder.ins().brif(is_some, some_b, &[], merge, &[]);
                        builder.switch_to_block(some_b);
                        builder.seal_block(some_b);
                        let cv = component(builder, ndl_v, OPT_VALUE_OFFSET, &elem, structs);
                        let r = emit_arr_contains_elem(module, builder, cx, recv, cv, &elem)?;
                        builder.ins().stack_store(r, res, 0);
                        builder.ins().jump(merge, &[]);
                        builder.switch_to_block(merge);
                        builder.seal_block(merge);
                        builder.ins().stack_load(types::I64, res, 0)
                    }
                }
            };
            (result, Type::Primitive(Primitive::Bool))
        }
        "__char_to_str" => {
            // Internal: a single `char` to a one-char inline `str`. Emitted by
            // variadic `char*` specialization (see mono's `specialize_variadic`).
            if args.len() != 1 {
                return Err(Error::at(
                    format!("__char_to_str expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (c, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            (
                emit_char_to_str(builder, c),
                Type::Primitive(Primitive::Str),
            )
        }
        "__builtin_has" => {
            // `has(s: T{}, x: T) -> bool` — set membership. Borrows the set
            // (it stays live in the caller's scope), so no inc/dec.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"has\" expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (set_ptr, set_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let elem = match &set_ty {
                Type::Set(inner) => (**inner).clone(),
                other => {
                    return Err(Error::at(
                        format!("\"has\" expects a set, got {}", type_name(other)),
                        args[0].span.clone(),
                    ));
                }
            };
            let (x_v, x_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // An empty set (`__none__` element) holds nothing — always false —
            // and has no element type to check the argument against.
            if is_none_inner(&elem) {
                (
                    builder.ins().iconst(types::I64, 0),
                    Type::Primitive(Primitive::Bool),
                )
            } else {
                expect_type(&x_ty, &elem, "has element", args[1].span.clone())?;
                // The runtime reads the queried value through a pointer; spill
                // the scalar (a `bool` is read back as i64) and pass its address.
                let s = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(x_v, s, 0);
                let x_ptr = builder.ins().stack_addr(types::I64, s, 0);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(&elem, structs));
                let f = builtins.import(module, builder.func, "aipl_set_contains");
                let str_cmp = builder.ins().iconst(
                    types::I64,
                    i64::from(elem == Type::Primitive(Primitive::Str)),
                );
                let inst = builder.ins().call(f, &[set_ptr, x_ptr, esz, str_cmp]);
                (
                    builder.inst_results(inst)[0],
                    Type::Primitive(Primitive::Bool),
                )
            }
        }
        "__builtin_union" => {
            // `union(a: T{}, b: T{}) -> T{}` — a fresh set of all distinct
            // elements of both. (The in-place `set a = a.union(b)` reuse for an
            // exclusive `a` is handled in the Assign arm.) `aipl_set_union`
            // consumes both inputs, so inc both to balance our scope tracks.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"union\" expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (a_ptr, a_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (b_ptr, b_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // Both sides must be the same set type (up to an empty-`#{}` operand,
            // whose element merges to the concrete side).
            let merged = merge_types(&a_ty, &b_ty);
            let Some(result_ty @ Type::Set(_)) = merged else {
                return Err(Error::at(
                    format!(
                        "\"union\" expects two sets of the same type, got {} and {}",
                        type_name(&a_ty),
                        type_name(&b_ty)
                    ),
                    span.clone(),
                ));
            };
            let Type::Set(elem) = &result_ty else {
                unreachable!()
            };
            emit_inc(builder, module, builtins, a_ptr);
            emit_inc(builder, module, builtins, b_ptr);
            let drop_fn = array_drop_fn_addr(builder, module, cx, elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, elem);
            let esz = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(elem, structs));
            let str_cmp = builder.ins().iconst(
                types::I64,
                i64::from(**elem == Type::Primitive(Primitive::Str)),
            );
            let f = builtins.import(module, builder.func, "aipl_set_union");
            let inst = builder
                .ins()
                .call(f, &[a_ptr, b_ptr, drop_fn, retain_fn, esz, str_cmp]);
            let res = builder.inst_results(inst)[0];
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(res, &result_ty));
            (res, result_ty)
        }
        "__builtin_get" => {
            // `get(d: #{K: V}, key: K) -> V?` — the bound value, else `none`.
            // Borrows the dict (no inc/dec); the matched value is retained into
            // the `some` result, so it outlives the dict.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"get\" expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (dict_ptr, dict_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (key_ty, val_ty) = match &dict_ty {
                Type::Dict(k, v) => ((**k).clone(), (**v).clone()),
                other => {
                    return Err(Error::at(
                        format!("\"get\" expects a dict, got {}", type_name(other)),
                        args[0].span.clone(),
                    ));
                }
            };
            let (key_v, key_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let result_ty = Type::Optional(Box::new(val_ty.clone()));
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&result_ty, structs) as u32,
                3,
            ));
            let sbase = builder.ins().stack_addr(types::I64, slot, 0);
            // An empty dict (`__none__` key/value) holds nothing — always `none`.
            if is_none_inner(&key_ty) {
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(zero, slot, 0);
                (sbase, result_ty)
            } else {
                expect_type(&key_t, &key_ty, "get key", args[1].span.clone())?;
                // Spill the key and pass its address (a `bool` reads back as i64,
                // a `str` as its pointer).
                let ks = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(key_v, ks, 0);
                let key_ptr = builder.ins().stack_addr(types::I64, ks, 0);
                let pair_size = 8 + elem_size_of(&val_ty, structs);
                let psz = builder.ins().iconst(types::I64, pair_size);
                let str_cmp = builder.ins().iconst(
                    types::I64,
                    i64::from(key_ty == Type::Primitive(Primitive::Str)),
                );
                let f = builtins.import(module, builder.func, "aipl_dict_get");
                let inst = builder.ins().call(f, &[dict_ptr, key_ptr, psz, str_cmp]);
                let vslot = builder.inst_results(inst)[0]; // value addr, or 0
                let found = builder.ins().icmp_imm(IntCC::NotEqual, vslot, 0);
                let some_b = builder.create_block();
                let none_b = builder.create_block();
                let merge_b = builder.create_block();
                builder.ins().brif(found, some_b, &[], none_b, &[]);

                builder.switch_to_block(some_b);
                builder.seal_block(some_b);
                // Read the value at the slot (a composite is addressed, a
                // scalar/str/array loaded), build `some(value)` and retain its
                // heap so it outlives the borrowed dict.
                let val = component(builder, vslot, 0, &val_ty, structs);
                emit_build_some(builder, sbase, val, &val_ty, structs);
                emit_retain(builder, module, builtins, structs, sbase, &result_ty);
                builder.ins().jump(merge_b, &[]);

                builder.switch_to_block(none_b);
                builder.seal_block(none_b);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(zero, slot, 0);
                builder.ins().jump(merge_b, &[]);

                builder.switch_to_block(merge_b);
                builder.seal_block(merge_b);
                if needs_drop(&result_ty, structs) {
                    scopes
                        .last_mut()
                        .expect("scope")
                        .push(Tracked::new(sbase, &result_ty));
                }
                (sbase, result_ty)
            }
        }
        "__builtin_contains_key" => {
            // `contains_key(d: #{K: V}, key: K) -> bool`. Borrows the dict.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"contains_key\" expects 2 args, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (dict_ptr, dict_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (key_ty, val_ty) = match &dict_ty {
                Type::Dict(k, v) => ((**k).clone(), (**v).clone()),
                other => {
                    return Err(Error::at(
                        format!("\"contains_key\" expects a dict, got {}", type_name(other)),
                        args[0].span.clone(),
                    ));
                }
            };
            let (key_v, key_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            if is_none_inner(&key_ty) {
                (
                    builder.ins().iconst(types::I64, 0),
                    Type::Primitive(Primitive::Bool),
                )
            } else {
                expect_type(&key_t, &key_ty, "contains_key key", args[1].span.clone())?;
                let ks = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(key_v, ks, 0);
                let key_ptr = builder.ins().stack_addr(types::I64, ks, 0);
                let pair_size = 8 + elem_size_of(&val_ty, structs);
                let psz = builder.ins().iconst(types::I64, pair_size);
                let str_cmp = builder.ins().iconst(
                    types::I64,
                    i64::from(key_ty == Type::Primitive(Primitive::Str)),
                );
                let f = builtins.import(module, builder.func, "aipl_dict_contains_key");
                let inst = builder.ins().call(f, &[dict_ptr, key_ptr, psz, str_cmp]);
                (
                    builder.inst_results(inst)[0],
                    Type::Primitive(Primitive::Bool),
                )
            }
        }
        "__filter_keep" => {
            // Internal (in-place `filter`): `__filter_keep(arr, w, e)` stores
            // element `e` at slot `w` with a raw pointer copy — no refcount
            // change, since ownership relocates from `e`'s read slot to slot `w`.
            let (a_ptr, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (w, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let (e, _) = compile_expr(module, builder, cx, scopes, &args[2])?;
            let base = builder.ins().iadd_imm(a_ptr, ARR_ELEMS_OFFSET as i64);
            let off = builder.ins().imul_imm(w, 8);
            let addr = builder.ins().iadd(base, off);
            builder.ins().store(MemFlags::trusted(), e, addr, 0);
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        "__filter_drop" => {
            // Internal (in-place `filter`): release a filtered-out element. The
            // surrounding `for` loop retains/releases `e` each iteration (a
            // no-op net), so this single drop removes the array's ownership of
            // it. A no-op for scalar elements (`needs_drop` is false).
            let (e, ety) = compile_expr(module, builder, cx, scopes, &args[0])?;
            emit_drop(builder, module, builtins, structs, e, &ety);
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        "__filter_truncate" => {
            // Internal (in-place `filter`): set the array's length to `w`. The
            // dead tail `[w, old_len)` holds relocated/stale pointers and is
            // never released (the block is later freed by its capacity).
            let (a_ptr, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (w, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
            builder
                .ins()
                .store(MemFlags::trusted(), w, a_ptr, ARR_LEN_OFFSET as i32);
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        "__map_set" => {
            // Internal (in-place `map`): `__map_set(arr, i, new, old)` overwrites
            // slot `i` with the mapped value `new` (a `U`), then releases the
            // `old` element it replaced (a `T`). `new` is a fresh result tracked
            // for release at the iteration's end, so retain it for the slot —
            // exactly `push`'s discipline. Retain/drop are no-ops for scalars.
            //
            // `T` and `U` are both 8-byte (non-composite — the in-place gate),
            // so the slot stride is 8 and a plain store fits. When `T != U` the
            // array's stored element drop-fn (set for `T` when the buffer was
            // built) is now wrong, so patch it to `U`'s here.
            let (a_ptr, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (i_val, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let (new_val, new_ty) = compile_expr(module, builder, cx, scopes, &args[2])?;
            let (old_val, old_ty) = compile_expr(module, builder, cx, scopes, &args[3])?;
            let base = builder.ins().iadd_imm(a_ptr, ARR_ELEMS_OFFSET as i64);
            let off = builder.ins().imul_imm(i_val, 8);
            let addr = builder.ins().iadd(base, off);
            builder.ins().store(MemFlags::trusted(), new_val, addr, 0);
            emit_retain(builder, module, builtins, structs, new_val, &new_ty);
            emit_drop(builder, module, builtins, structs, old_val, &old_ty);
            let new_drop = array_drop_fn_addr(builder, module, cx, &new_ty);
            builder.ins().store(
                MemFlags::trusted(),
                new_drop,
                a_ptr,
                ARR_DROPFN_OFFSET as i32,
            );
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        "__map_result" => {
            // Internal (in-place `map`): hand the reused buffer back reinterpreted
            // as the enclosing function's declared return type (`U[]`). `$a`'s
            // static type is still `T[]`, but the elements are now `U` and the
            // drop-fn has been patched, so this is a runtime no-op (same pointer)
            // that only re-types the value for the return-type check.
            let (a_ptr, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            (a_ptr, cx.ret_ty.clone())
        }
        "__builtin_with_capacity" => {
            // Internal (emitted by `map`): allocate an empty array reserved to
            // the given capacity. Element type is unknown (`__none__`) like an
            // empty `[]`; it's refined and its drop-fn set by the first `push`.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("with_capacity expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (cap, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            expect_type(
                &t,
                &Type::Primitive(Primitive::I64),
                "with_capacity capacity",
                args[0].span.clone(),
            )?;
            // Element type unknown here (`__none__`); the drop-fn and element
            // size are settled by the first `push`. `map`/`filter` only pre-size
            // 8-byte-element outputs (optional outputs use a plain `[]`).
            let with_cap = builtins.import(module, builder.func, "aipl_array_with_cap");
            let zero = builder.ins().iconst(types::I64, 0); // drop_fn
            let esz = builder.ins().iconst(types::I64, 8); // elem_size
            let inst = builder.ins().call(with_cap, &[cap, zero, esz]);
            let ptr = builder.inst_results(inst)[0];
            let arr_ty = Type::Array(Box::new(Type::NoneInner));
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &arr_ty));
            (ptr, arr_ty)
        }
        "__builtin_push" => {
            // `push` mutates an array in place, so it must be written as a
            // method call (`xs.push(x)`) on a *mutable* array variable: the
            // receiver is `args[0]` and the grown array is stored back into it.
            // Value semantics are kept — a possibly-shared array is copied
            // first (the old block, still referenced elsewhere, is untouched);
            // an exclusive one is grown in place.
            if !style {
                return Err(Error::at(
                    "\"push\" modifies an array in place; call it as a method on a \
                     mutable array variable: \"xs.push(x)\""
                        .to_string(),
                    span.clone(),
                ));
            }
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"push\" expects 1 argument, got {}", args.len() - 1),
                    span.clone(),
                ));
            }
            let receiver = &args[0];
            let value = &args[1];
            let ExprKind::Ident(var) = &receiver.kind else {
                return Err(Error::at(
                    "\"push\" must be called on a mutable array variable, e.g. \"xs.push(x)\""
                        .to_string(),
                    receiver.span.clone(),
                ));
            };
            let (slot, ty_cell, exclusive) = match env.get(var) {
                Some(EnvBinding::Mut(slot, cell, excl)) => (*slot, cell.clone(), *excl),
                Some(EnvBinding::Immut(_, _)) => {
                    return Err(Error::at(
                        format!(
                            "cannot \"push\" to immutable binding {var:?}; declare it with \"mut\""
                        ),
                        receiver.span.clone(),
                    ));
                }
                None => {
                    return Err(Error::at(
                        format!("unknown identifier {var:?}"),
                        receiver.span.clone(),
                    ));
                }
            };
            let elem_ty = match &*ty_cell.borrow() {
                Type::Array(inner) => (**inner).clone(),
                other => {
                    return Err(Error::at(
                        format!("\"push\" requires an array, got {}", type_name(other)),
                        receiver.span.clone(),
                    ));
                }
            };
            let arr_ptr = builder.ins().stack_load(types::I64, slot, 0);
            let (x_v, x_ty) = compile_expr(module, builder, cx, scopes, value)?;
            let elem_was_none = is_none_inner(&elem_ty);
            // An empty array (`__none__` element) takes its element type from
            // the first pushed value; otherwise the value must match.
            let result_elem = if elem_was_none {
                let ok = is_array_elem(&x_ty)
                    || matches!(&x_ty, Type::Optional(_))
                    || matches!(&x_ty, Type::Named(n) if structs.contains_key(n));
                if !ok {
                    return Err(Error::at(
                        format!(
                            "\"push\" element must be i64, bool, char, str, or an array, got {}",
                            type_name(&x_ty)
                        ),
                        value.span.clone(),
                    ));
                }
                x_ty
            } else {
                expect_type(&x_ty, &elem_ty, "push element", value.span.clone())?;
                elem_ty
            };
            let drop_fn = array_drop_fn_addr(builder, module, cx, &result_elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, &result_elem);
            let esz = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(&result_elem, structs));
            // The runtime copies `elem_size` bytes from `x_ptr` (or packs a bit
            // for a `bool` array). A composite element (an optional) is already
            // addressed; a scalar/pointer value is spilled to a slot for its
            // address (a `bool` is spilled too — the runtime reads it as i64).
            let x_ptr = if is_composite(&result_elem, structs) {
                x_v
            } else {
                let s = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(x_v, s, 0);
                builder.ins().stack_addr(types::I64, s, 0)
            };
            let new_arr_ty = Type::Array(Box::new(result_elem));
            if is_char_array(&new_arr_ty) {
                // `char[]` is str-shaped (see `is_char_array`) and `str` has no
                // in-place-growable form yet: every push rebuilds a fresh
                // `str` (old bytes + the new byte) rather than growing in
                // place — but the *ownership bookkeeping* still follows the
                // same `exclusive`-vs-shared split as a real array push below,
                // just swapping in `str` construction for `aipl_array_push[_mut]`.
                //
                // `mut cs = []` starts as the generic (array-shaped) empty
                // placeholder — see `coerce_empty_to_char_array` — since an
                // untyped `[]` has no way to know its first push will be a
                // `char`. Converting *that* specific value requires knowing
                // this is genuinely the *first* ever push to it — a fact
                // `elem_was_none` only captures at compile time, for *this*
                // call site. A push inside a loop compiles once but runs
                // every iteration, so on the second iteration the "first
                // push" code would wrongly re-run against a value that's
                // already been converted to a real `str` on the iteration
                // before. There's no cheap runtime way to tell those apart
                // (both are ordinary heap pointers), so this specific
                // transition is rejected rather than risking silent
                // corruption — initialize with a real `char` first instead.
                if elem_was_none {
                    return Err(Error::at(
                        "cannot push a char onto an array that started as an untyped empty \
                         literal (`mut cs = []`) — initialize it with a char first, e.g. \
                         \"mut cs = ['a']\", since the first push can't be proven to run \
                         only once"
                            .to_string(),
                        receiver.span.clone(),
                    ));
                }
                let len_f = builtins.import(module, builder.func, "aipl_str_len");
                let inst = builder.ins().call(len_f, &[arr_ptr]);
                let old_len = builder.inst_results(inst)[0];
                let new_len = builder.ins().iadd_imm(old_len, 1);
                let alloc_f = builtins.import(module, builder.func, "aipl_str_alloc");
                let inst = builder.ins().call(alloc_f, &[new_len]);
                let buf = builder.inst_results(inst)[0];
                let scratch = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                let scratch_addr = builder.ins().stack_addr(types::I64, scratch, 0);
                let data_f = builtins.import(module, builder.func, "aipl_str_data");
                let inst = builder.ins().call(data_f, &[arr_ptr, scratch_addr]);
                let src = builder.inst_results(inst)[0];
                let copy_f = builtins.import(module, builder.func, "aipl_write_bytes");
                builder.ins().call(copy_f, &[buf, src, old_len]);
                let dst_addr = builder.ins().iadd(buf, old_len);
                builder.ins().istore8(MemFlags::trusted(), x_v, dst_addr, 0);
                builder.ins().stack_store(buf, slot, 0);
                if exclusive {
                    // Statically proven unaliased: the old value is already
                    // owned solely by this binding's slot-track (from
                    // `LetMut`), so free it for real — no compensating inc —
                    // and don't track `buf` separately; the slot-track now
                    // covers it instead.
                    emit_rc(
                        builder,
                        module,
                        builtins,
                        structs,
                        arr_ptr,
                        &new_arr_ty,
                        RcOp::Drop,
                    );
                } else {
                    // Possibly shared: the old value is owned by a separate
                    // track elsewhere (the binding's initial value-track, or
                    // — for a `mut` parameter — its function-entry track),
                    // which will drop it once at that track's scope exit.
                    // Unlike `aipl_array_push` below (which *consumes* its
                    // input, requiring a compensating pre-inc), building `buf`
                    // only *borrows* `arr_ptr` (`aipl_str_len`/`aipl_str_data`
                    // don't touch its refcount) — so the old track's single
                    // eventual drop is already exactly right; the slot simply
                    // stops pointing at it. `buf` is fresh and gets its own
                    // track.
                    scopes
                        .last_mut()
                        .expect("scope")
                        .push(Tracked::new(buf, &new_arr_ty));
                }
            } else if exclusive {
                // Statically proven unaliased: mutate in place. No pre-inc and
                // no new value-track — the binding's slot-track (added at
                // `LetMut`) already owns the block, even after a relocating grow.
                let local = builtins.import(module, builder.func, "aipl_array_push_mut");
                let inst = builder
                    .ins()
                    .call(local, &[arr_ptr, x_ptr, drop_fn, retain_fn, esz]);
                let new_ptr = builder.inst_results(inst)[0];
                builder.ins().stack_store(new_ptr, slot, 0);
            } else {
                // Possibly shared: `aipl_array_push` copies its arg into a fresh
                // block, then decs the arg — that dec releases the *slot's* own
                // reference on the old value (see `mut_binding_owns_slot_ref`);
                // the old version's creation-scope value-track still owns it, so
                // aliases/borrows of it stay valid to that scope's exit. The
                // fresh block's sole reference becomes the slot's; the extra
                // retain + value-track below is the new version's region track,
                // keeping it borrowable to the *current* scope's exit — while
                // the slot's own reference carries it across loop iterations.
                let local = builtins.import(module, builder.func, "aipl_array_push");
                let inst = builder
                    .ins()
                    .call(local, &[arr_ptr, x_ptr, drop_fn, retain_fn, esz]);
                let new_ptr = builder.inst_results(inst)[0];
                builder.ins().stack_store(new_ptr, slot, 0);
                emit_retain(builder, module, builtins, structs, new_ptr, &new_arr_ty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(new_ptr, &new_arr_ty));
            }
            // Refine the binding's element type (e.g. `mut a = []` → `i64[]`).
            *ty_cell.borrow_mut() = new_arr_ty;
            // `push` mutates; it produces no value.
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        _ if Primitive::from_name(name).is_some_and(Primitive::is_int) => {
            // Integer conversion builtin `i8(x)`/`u32(x)`/… — convert the (integer)
            // argument to this width by re-canonicalizing the i64-register value.
            let p = Primitive::from_name(name).expect("guard checked it's an integer primitive");
            if args.len() != 1 {
                return Err(Error::at(
                    format!("{name:?} conversion expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if !is_int_ty(&t) {
                return Err(Error::at(
                    format!("{name:?} converts an integer, got {}", type_name(&t)),
                    args[0].span.clone(),
                ));
            }
            (canon_int(builder, v, p), Type::Primitive(p))
        }
        "ok" | "err" => {
            // A Result `{ tag, value }` (tag 1 = ok, 0 = err). The unbound side is
            // `__none__`, resolved by coercion at the use site (like bare `none`).
            let is_ok = name == "ok";
            // `ok()` with no argument is the void success of a `!E` result: tag 1
            // with an unused (zeroed) value region, Ok side `unit`.
            if is_ok && args.is_empty() {
                let res_ty = Type::Result(Box::new(Type::Unit), Box::new(Type::NoneInner));
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    elem_size_of(&res_ty, structs) as u32,
                    3,
                ));
                let ptr = builder.ins().stack_addr(types::I64, slot, 0);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().store(MemFlags::trusted(), one, ptr, 0);
                let zero = builder.ins().iconst(types::I64, 0);
                builder
                    .ins()
                    .store(MemFlags::trusted(), zero, ptr, OPT_VALUE_OFFSET as i32);
                // No payload to retain or drop (unit / __none__ never need it).
                return Ok((ptr, res_ty));
            }
            if args.len() != 1 {
                return Err(Error::at(
                    format!("fn {name:?} expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            match &t {
                Type::Primitive(
                    Primitive::I64 | Primitive::Bool | Primitive::Char | Primitive::Str,
                ) => {}
                _ if is_error(&t) => {}
                // A struct payload is an inline composite (like an optional's
                // core): `elem_size_of`/`store_array_elem`/`emit_rc` already
                // size, copy, and refcount it generically once it's addressed
                // as a `Result` payload, so only variants remain unsupported
                // here. An array payload is a single refcounted pointer word,
                // handled by the same machinery (an empty literal's element
                // type coerces at the use site, like any `[]`).
                Type::Named(n) if structs.get(n).is_some_and(|d| d.as_struct().is_some()) => {}
                Type::Array(_) => {}
                _ => {
                    return Err(Error::at(
                        format!(
                            "{name:?} payload must be a scalar, str, struct, or array, got {}",
                            type_name(&t)
                        ),
                        args[0].span.clone(),
                    ));
                }
            }
            let res_ty = if is_ok {
                Type::Result(Box::new(t.clone()), Box::new(Type::NoneInner))
            } else {
                Type::Result(Box::new(Type::NoneInner), Box::new(t.clone()))
            };
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&res_ty, structs) as u32,
                3,
            ));
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            let tag = builder.ins().iconst(types::I64, if is_ok { 1 } else { 0 });
            builder.ins().store(MemFlags::trusted(), tag, ptr, 0);
            let val_addr = builder.ins().iadd_imm(ptr, OPT_VALUE_OFFSET as i64);
            store_array_elem(builder, val_addr, v, &t, structs);
            // The payload (a str) may be heap — co-own it.
            emit_retain(builder, module, builtins, structs, ptr, &res_ty);
            if needs_drop(&res_ty, structs) {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(ptr, &res_ty));
            }
            (ptr, res_ty)
        }
        "some" => {
            if args.len() != 1 {
                return Err(Error::at(
                    format!("fn \"some\" expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            match &t {
                Type::Primitive(
                    Primitive::I64 | Primitive::Bool | Primitive::Char | Primitive::Str,
                ) => {}
                // A struct (`Point?`) is stored inline, as is a nested optional
                // (`some(some(..))` → `T??`) and an array.
                Type::Named(n) if structs.contains_key(n) => {}
                Type::Array(_) | Type::Optional(_) => {}
                _ => {
                    return Err(Error::at(
                        format!(
                            "\"some\" argument: optional of {} is not supported",
                            type_name(&t)
                        ),
                        args[0].span.clone(),
                    ));
                }
            }
            // Flattened optional: `8 (tag) + sizeof(Core)`, independent of the
            // nesting depth (a nested `some(some(..))` reuses one core value
            // field, just with a higher tag).
            let opt_ty = Type::Optional(Box::new(t.clone()));
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&opt_ty, structs) as u32,
                3,
            ));
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            emit_build_some(builder, ptr, v, &t, structs);
            // The slot now aliases the core heap (when fully `some`) — retain it
            // as a co-owner; `emit_retain` incs only when tag == depth.
            emit_retain(builder, module, builtins, structs, ptr, &opt_ty);
            if needs_drop(&opt_ty, structs) {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(ptr, &opt_ty));
            }
            (ptr, opt_ty)
        }
        _ if style && funcs.get(name).is_some_and(|i| i.is_mutating) => {
            // A method-style call to a mutating user method behaves like
            // `set v = foo(v, args)`: `foo` takes the receiver (`args[0]`) by
            // value, returns the mutated receiver, and we store it back into the
            // variable. (A *free* call of a mutating fn was rewritten to
            // copy-and-modify during monomorphization, so it never reaches here
            // as a mutating call.)
            let info = funcs.get(name).cloned().expect("mutating fn present");
            let disp = display_name(name);
            let receiver = &args[0];
            let ExprKind::Ident(var) = &receiver.kind else {
                return Err(Error::at(
                    format!("mutating method {disp:?} must be called on a mutable variable"),
                    receiver.span.clone(),
                ));
            };
            let (slot, ty_cell) = match env.get(var) {
                Some(EnvBinding::Mut(slot, cell, _)) => (*slot, cell.clone()),
                Some(EnvBinding::Immut(_, _)) => {
                    return Err(Error::at(
                        format!(
                            "cannot call mutating method {disp:?} on immutable binding {var:?}; \
                             declare it with `mut`"
                        ),
                        receiver.span.clone(),
                    ));
                }
                None => {
                    return Err(Error::at(
                        format!("unknown identifier {var:?}"),
                        receiver.span.clone(),
                    ));
                }
            };
            // For an array receiver, the binding's slot owns a reference on its
            // current value (see `mut_binding_owns_slot_ref`): snapshot the old
            // value so the slot's reference on it can be released after the call
            // replaces it. (The callee borrows the receiver — `compile_call`
            // retains it for the callee's own drop — so the snapshot stays live
            // through the call.)
            let old_ty = ty_cell.borrow().clone();
            let old = if mut_binding_owns_slot_ref(&old_ty) {
                Some(builder.ins().stack_load(types::I64, slot, 0))
            } else {
                None
            };
            // `args` is already the effective list `[receiver, method args..]`;
            // its result is the mutated self.
            let (new_self, _) =
                compile_call(module, builder, cx, scopes, name, &info, args, span.clone())?;
            // Store the mutated receiver back, and refine the variable's type
            // to it (e.g. a `mut a = []` receiver pinned by the method's self).
            builder.ins().stack_store(new_self, slot, 0);
            if let Some(old) = old {
                // The slot takes its own reference on the mutated receiver (the
                // call-return value-track stays as the new version's region
                // track — it dies with the current scope, e.g. a loop body,
                // while the slot's reference carries the value onward), then
                // releases its reference on the replaced value.
                emit_retain(builder, module, builtins, structs, new_self, &old_ty);
                emit_drop(builder, module, builtins, structs, old, &old_ty);
            }
            *ty_cell.borrow_mut() = info.return_ty.clone();
            // A mutating method yields nothing.
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        _ => {
            // A variant constructor `Ctor(args..)` builds an inline tagged value.
            if let Some((vname, tag, fields)) = variant_ctor(structs, name) {
                return compile_variant(
                    module,
                    builder,
                    cx,
                    scopes,
                    &vname,
                    tag,
                    &fields,
                    args,
                    span.clone(),
                );
            }
            let info = funcs
                .get(name)
                .cloned()
                .ok_or_else(|| undefined_fn(name, span.clone()))?;
            if info.is_mutating {
                let disp = display_name(name);
                return Err(Error::at(
                    format!(
                        "fn {disp:?} mutates its receiver; call it as a method: \"v.{disp}(...)\""
                    ),
                    span.clone(),
                ));
            }
            // `name` already names the right instance (borrow or owned) chosen
            // by monomorphization; `compile_call` moves the owned args in.
            compile_call(module, builder, cx, scopes, name, &info, args, span.clone())?
        }
    })
}

/// Emit `recv[a..b]` for any sliceable receiver — the shared tail of
/// `ExprKind::Slice` and the Span-index sugar in `ExprKind::Index`:
///
/// - `str` → `aipl_str_slice` (a buffer-sharing view for a large heap source,
///   else a copy); an open-ended `b` of `None` is filled with `aipl_str_len`.
/// - `char[]` → same runtime path (it shares `str`'s representation, see
///   `is_char_array`) but keeps its nominal `char[]` type, like `reverse`.
/// - `T[]` → `aipl_arr_slice`, a fresh heap array copying the element range
///   (each element retained); `None` becomes `i64::MAX`, which the runtime
///   clamps to the length.
///
/// Every runtime path *borrows* the receiver and clamps both bounds, so the
/// call site just tracks the fresh result for drop.
#[allow(clippy::too_many_arguments)]
fn emit_slice<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    recv_v: Value,
    recv_ty: &Type,
    a_v: Value,
    b_v: Option<Value>,
    recv_span: &Span,
) -> Result<(Value, Type), Error> {
    let structs = cx.structs;
    let builtins = cx.builtins;
    // Exact `str` plus `char[]`, not the broader `is_str_shaped` — matching
    // the char-at scope in `ExprKind::Index` (`Error`/concat-str receivers
    // aren't part of the slice surface).
    if *recv_ty == Type::Primitive(Primitive::Str) || is_char_array(recv_ty) {
        let b_v = match b_v {
            Some(b) => b,
            None => {
                let len_f = builtins.import(module, builder.func, "aipl_str_len");
                let inst = builder.ins().call(len_f, &[recv_v]);
                builder.inst_results(inst)[0]
            }
        };
        let f = builtins.import(module, builder.func, "aipl_str_slice");
        let inst = builder.ins().call(f, &[recv_v, a_v, b_v]);
        let result = builder.inst_results(inst)[0];
        scopes
            .last_mut()
            .expect("scope")
            .push(Tracked::new(result, recv_ty));
        return Ok((result, recv_ty.clone()));
    }
    if let Type::Array(elem) = recv_ty {
        let elem = (**elem).clone();
        let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
        let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
        let esz = builder
            .ins()
            .iconst(types::I64, runtime_elem_size(&elem, structs));
        let b_v = b_v.unwrap_or_else(|| builder.ins().iconst(types::I64, i64::MAX));
        let f = builtins.import(module, builder.func, "aipl_arr_slice");
        let inst = builder
            .ins()
            .call(f, &[recv_v, a_v, b_v, drop_fn, retain_fn, esz]);
        let result = builder.inst_results(inst)[0];
        scopes
            .last_mut()
            .expect("scope")
            .push(Tracked::new(result, recv_ty));
        return Ok((result, recv_ty.clone()));
    }
    Err(Error::at(
        format!("cannot slice a value of type {}", type_name(recv_ty)),
        recv_span.clone(),
    ))
}

fn compile_expr<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    expr: &Expr,
) -> Result<(Value, Type), Error> {
    // Destructure into the names the body already uses, so only the signature
    // and call sites change. `cx` itself stays in scope for `..cx` spreads.
    let Cx {
        env,
        funcs,
        structs,
        builtins,
        effects: _,
        owned_params: _,
        lit_ctr: _,
        elem_rc: _,
        ret_ty: _,
        sret: _,
        error_main: _,
        bindings: _,
    } = cx;
    let span = expr.span.clone();
    Ok(match &expr.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        // Unit carries no value; hand back a placeholder i64 the unit type
        // forbids anyone from consuming, mirroring the unit-call result.
        ExprKind::Unit => (builder.ins().iconst(types::I64, 0), Type::Unit),
        ExprKind::Return(value) => {
            // Early return: evaluate the value, hand the caller a reference,
            // release *every* live scope (we're leaving the function), then return
            // per the ABI — mirroring the function epilogue. Whatever follows in
            // the block is unreachable, so a fresh (dead) block receives it.
            let (rv, rty) = compile_expr(module, builder, cx, scopes, value)?;
            let ret_val = if cx.error_main {
                // `fn main() -> !Error`: derive the exit code (printing
                // `error: <msg>` on the err side) before releasing the scopes.
                emit_error_main_exit_code(builder, module, builtins, rv)
            } else {
                rv
            };
            if needs_drop(cx.ret_ty, structs) {
                emit_retain(builder, module, builtins, structs, ret_val, cx.ret_ty);
            }
            for scope in scopes.iter() {
                for t in scope {
                    let v = match t.owned {
                        Owned::Value(v) => v,
                        Owned::Slot(slot) => builder.ins().stack_load(types::I64, slot, 0),
                    };
                    emit_drop(builder, module, builtins, structs, v, &t.ty);
                }
            }
            if is_unit(cx.ret_ty) {
                builder.ins().return_(&[]);
            } else if sret_size(cx.ret_ty, structs).is_some() {
                let sret = cx.sret.expect("composite return has an sret pointer");
                copy_composite(builder, sret, ret_val, &rty, structs);
                builder.ins().return_(&[]);
            } else {
                builder.ins().return_(&[ret_val]);
            }
            // Unreachable continuation: subsequent statements compile into here and
            // are dropped as dead code.
            let dead = builder.create_block();
            builder.switch_to_block(dead);
            builder.seal_block(dead);
            (builder.ins().iconst(types::I64, 0), Type::Unit)
        }
        ExprKind::Num(n) => (
            builder.ins().iconst(types::I64, *n),
            Type::Primitive(Primitive::I64),
        ),
        ExprKind::Bool(b) => (
            builder.ins().iconst(types::I64, if *b { 1 } else { 0 }),
            Type::Primitive(Primitive::Bool),
        ),
        ExprKind::Char(c) => (
            builder.ins().iconst(types::I64, i64::from(*c)),
            Type::Primitive(Primitive::Char),
        ),
        ExprKind::Str(s) => {
            let content = s.as_bytes();
            // SSO: a literal of <= 7 bytes is an *inline* str value — emit it as a
            // constant, with no data object, allocation, or refcount. (inc/dec
            // no-op on it; the surrounding scope-drop is a no-op too, so it needs
            // no tracking.)
            if content.len() <= 7 {
                let packed = pack_inline(content) as usize as i64;
                return Ok((
                    builder.ins().iconst(types::I64, packed),
                    Type::Primitive(Primitive::Str),
                ));
            }
            // Static literal: emit [refcount: STATIC_REFCOUNT][bytes][null]
            // into the data section. Pointer points past the 8-byte header.
            // A source literal's span.clone() is unique, so it names the symbol; a
            // *synthesized* literal carries the dummy span.clone() (0,0) and several may
            // share it (e.g. the `check` driver's test names), so disambiguate
            // those with a counter. Real literals keep their span.clone()-based name so
            // object layout (and the `binary size` perf metric) is unchanged.
            let data_name = if span.start == 0 && span.end == 0 {
                let n = cx.lit_ctr.get();
                cx.lit_ctr.set(n + 1);
                format!("__str_synth_{n}")
            } else {
                format!("__str_{}_{}", span.start, span.end)
            };
            let data_id = module
                .declare_data(&data_name, Linkage::Local, false, false)
                .map_err(|e| Error::msg(format!("declare data: {e}")))?;
            // Static string layout: [len: i64][refcount = STATIC][bytes][NUL];
            // the pointer points past both header words.
            let mut bytes = Vec::with_capacity(STR_HEADER_SIZE + content.len() + 1);
            bytes.extend_from_slice(&(content.len() as i64).to_le_bytes());
            bytes.extend_from_slice(&STATIC_REFCOUNT.to_le_bytes());
            bytes.extend_from_slice(content);
            bytes.push(0);
            let mut desc = DataDescription::new();
            // 8-byte align so the i64 header words read safely.
            desc.set_align(8);
            desc.define(bytes.into_boxed_slice());
            // A struct-field default that is a string literal is materialized at
            // every construction site omitting that field, each carrying the
            // default expression's original span — so the span-derived symbol name
            // repeats across sites. The bytes are identical (same span ⇒ same
            // source literal) and `declare_data` already returned the existing id,
            // so a repeat `define_data` is redundant: tolerate the duplicate and
            // reuse the first definition. (The synth path uses unique counter names,
            // so it never hits this.)
            match module.define_data(data_id, &desc) {
                Ok(()) | Err(cranelift_module::ModuleError::DuplicateDefinition(_)) => {}
                Err(e) => return Err(Error::msg(format!("define data: {e}"))),
            }
            let gv = module.declare_data_in_func(data_id, builder.func);
            let base = builder.ins().symbol_value(types::I64, gv);
            let ptr = builder.ins().iadd_imm(base, STR_HEADER_SIZE as i64);
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &Type::Primitive(Primitive::Str)));
            (ptr, Type::Primitive(Primitive::Str))
        }
        ExprKind::Ident(name) => {
            // A local binding shadows everything; an unbound name may be a
            // nullary variant constructor (e.g. `Empty`).
            if env.contains_key(name) {
                env_load(builder, name, env, span.clone())?
            } else if let Some((vname, tag, fields)) = variant_ctor(structs, name) {
                compile_variant(
                    module,
                    builder,
                    cx,
                    scopes,
                    &vname,
                    tag,
                    &fields,
                    &[],
                    span.clone(),
                )?
            } else {
                env_load(builder, name, env, span.clone())?
            }
        }
        ExprKind::None => {
            // Allocate a 16-byte slot with tag = 0. Value field stays
            // undefined (callers must check is_some before touching it).
            // Type is Optional(__none__) — implicitly converts to any
            // Optional(T) via expect_type.
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                16,
                3,
            ));
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, slot, 0);
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            (ptr, Type::Optional(Box::new(Type::NoneInner)))
        }
        ExprKind::Call(name, args, style) => compile_call_expr(
            module,
            builder,
            cx,
            scopes,
            name,
            args,
            *style,
            span.clone(),
        )?,
        ExprKind::Construct(name, field_inits) => {
            let layout = structs
                .get(name)
                .and_then(TypeDef::as_struct)
                .ok_or_else(|| {
                    Error::at(
                        format!("unknown struct {:?}", display_name(name)),
                        span.clone(),
                    )
                })?;
            if field_inits.len() != layout.fields.len() {
                return Err(Error::at(
                    format!(
                        "struct {:?} expects {} field(s), got {}",
                        display_name(name),
                        layout.fields.len(),
                        field_inits.len()
                    ),
                    span.clone(),
                ));
            }
            let slot = alloc_struct_slot(builder, layout);
            for init in field_inits {
                let field = layout.field(&init.name).ok_or_else(|| {
                    Error::at(
                        format!(
                            "struct {:?} has no field {:?}",
                            display_name(name),
                            init.name
                        ),
                        init.value.span.clone(),
                    )
                })?;
                let (offset, fty) = (field.offset, field.ty.clone());
                let (v, actual) = compile_expr(module, builder, cx, scopes, &init.value)?;
                // `expect_type` (not `==`) so a `none` / empty `[]` value
                // coerces into an optional / array field.
                expect_type(
                    &actual,
                    &fty,
                    &format!("struct {:?} field {:?}", display_name(name), init.name),
                    init.value.span.clone(),
                )?;
                // A scalar/heap field is an 8-byte value; an optional field is
                // a 16-byte inline composite, so copy its bytes from the source
                // slot rather than storing the pointer.
                if is_composite(&fty, structs) {
                    let size = field_size(&fty, structs);
                    let mut o = 0u32;
                    while o < size {
                        let chunk =
                            builder
                                .ins()
                                .load(types::I64, MemFlags::trusted(), v, o as i32);
                        builder.ins().stack_store(chunk, slot, (offset + o) as i32);
                        o += 8;
                    }
                } else {
                    builder.ins().stack_store(v, slot, offset as i32);
                }
                // The struct co-owns each heap field (recursing into an
                // optional's value) — retain on store.
                emit_retain(builder, module, builtins, structs, v, &fty);
            }
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            let sty = Type::Named(name.clone());
            if needs_drop(&sty, structs) {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(ptr, &sty));
            }
            (ptr, sty)
        }
        ExprKind::Field(obj, field_name) => {
            let (obj_ptr, obj_ty) = compile_expr(module, builder, cx, scopes, obj)?;
            let Type::Named(ref struct_name) = obj_ty else {
                return Err(Error::at(
                    format!(
                        "field access on non-struct value of type {}",
                        type_name(&obj_ty)
                    ),
                    obj.span.clone(),
                ));
            };
            let layout = structs
                .get(struct_name)
                .and_then(TypeDef::as_struct)
                .ok_or_else(|| {
                    Error::at(
                        format!(
                            "field access on non-struct value of type {:?}",
                            display_name(struct_name)
                        ),
                        obj.span.clone(),
                    )
                })?;
            let field = layout.field(field_name).ok_or_else(|| {
                Error::at(
                    format!(
                        "struct {:?} has no field {field_name:?}",
                        display_name(struct_name)
                    ),
                    span.clone(),
                )
            })?;
            let (foff, fty) = (field.offset, field.ty.clone());
            // A scalar/heap field loads as an 8-byte value; an optional field
            // is an inline composite, so its "value" is the address of that
            // storage within the struct.
            let v = component(builder, obj_ptr, foff, &fty, structs);
            // The result is borrowed from the struct (which still owns its
            // copy); retain it so it's an independently-owned ref, and track
            // it for release.
            if needs_drop(&fty, structs) {
                emit_retain(builder, module, builtins, structs, v, &fty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(v, &fty));
            }
            (v, fty)
        }
        ExprKind::Neg(inner) => {
            let (v, t) = compile_expr(module, builder, cx, scopes, inner)?;
            expect_type(
                &t,
                &Type::Primitive(Primitive::I64),
                "unary \"-\"",
                inner.span.clone(),
            )?;
            (builder.ins().ineg(v), Type::Primitive(Primitive::I64))
        }
        ExprKind::Not(inner) => {
            let (v, t) = compile_expr(module, builder, cx, scopes, inner)?;
            expect_type(
                &t,
                &Type::Primitive(Primitive::Bool),
                "unary \"!\"",
                inner.span.clone(),
            )?;
            (
                builder.ins().bxor_imm(v, 1),
                Type::Primitive(Primitive::Bool),
            )
        }
        ExprKind::Binop(l, op, r) => {
            if matches!(*op, 'E' | 'N') {
                if let Some(result) = compile_ctor_eq(module, builder, cx, scopes, *op, l, r)? {
                    return Ok(result);
                }
            }
            let (lv, lt) = compile_expr(module, builder, cx, scopes, l)?;
            let (rv, rt) = compile_expr(module, builder, cx, scopes, r)?;
            // A bare literal operand flexes to the other's integer type — its
            // i64-register value is already the canonical narrow rep (the checker
            // verified the fit), so only the static type needs relabeling.
            let lt = aipl_syntax::flex_int_ty(l, &lt, &rt);
            let rt = aipl_syntax::flex_int_ty(r, &rt, &lt);
            match op {
                // `+` is integer add only. User `+` resolves to a call to its
                // bound `wrapping_add`/`saturating_add` (intrinsified above), so a
                // primitive `+` Binop here is the increment sugar (`set n++`) or
                // mono's own index arithmetic — always wrapping. String concat is
                // `+++` (`'C'`).
                '+' => {
                    if is_int_ty(&lt) && lt == rt {
                        let Type::Primitive(p) = &lt else {
                            unreachable!()
                        };
                        (
                            emit_int_addsub(builder, lv, rv, *p, false, false),
                            lt.clone(),
                        )
                    } else {
                        expect_type(
                            &lt,
                            &Type::Primitive(Primitive::I64),
                            "arithmetic operand",
                            l.span.clone(),
                        )?;
                        expect_type(
                            &rt,
                            &Type::Primitive(Primitive::I64),
                            "arithmetic operand",
                            r.span.clone(),
                        )?;
                        (builder.ins().iadd(lv, rv), Type::Primitive(Primitive::I64))
                    }
                }
                // `+++` string concatenation. `Error` is str-represented, so it
                // concatenates like a `str`. Builds a *lazy concat node* (see
                // `aipl_concat_lazy`) rather than copying eagerly — the result is
                // still a `str` to the source, in the concat representation.
                'C' => {
                    if is_str_repr(&lt) && is_str_repr(&rt) {
                        // The concat node takes ownership of its inputs; inc both
                        // before the call so our local refs balance.
                        emit_inc(builder, module, builtins, lv);
                        emit_inc(builder, module, builtins, rv);
                        let local = builtins.import(module, builder.func, "aipl_concat_lazy");
                        let inst = builder.ins().call(local, &[lv, rv]);
                        let ret = builder.inst_results(inst)[0];
                        scopes
                            .last_mut()
                            .expect("scope")
                            .push(Tracked::new(ret, &Type::Primitive(Primitive::Str)));
                        (ret, Type::Primitive(Primitive::Str))
                    } else {
                        return Err(Error::at(
                            "\"+++\" concatenates strings: both sides must be str".to_string(),
                            span.clone(),
                        ));
                    }
                }
                '-' | '*' | '/' | '%' => {
                    if is_int_ty(&lt) && lt == rt {
                        let Type::Primitive(p) = &lt else {
                            unreachable!()
                        };
                        let signed = p.int_signed();
                        let raw = match op {
                            '-' => builder.ins().isub(lv, rv),
                            '*' => builder.ins().imul(lv, rv),
                            '/' if signed => builder.ins().sdiv(lv, rv),
                            '/' => builder.ins().udiv(lv, rv),
                            '%' if signed => builder.ins().srem(lv, rv),
                            '%' => builder.ins().urem(lv, rv),
                            _ => unreachable!(),
                        };
                        (canon_int(builder, raw, *p), lt.clone())
                    } else {
                        expect_type(
                            &lt,
                            &Type::Primitive(Primitive::I64),
                            "arithmetic operand",
                            l.span.clone(),
                        )?;
                        expect_type(
                            &rt,
                            &Type::Primitive(Primitive::I64),
                            "arithmetic operand",
                            r.span.clone(),
                        )?;
                        let v = match op {
                            '-' => builder.ins().isub(lv, rv),
                            '*' => builder.ins().imul(lv, rv),
                            '/' => builder.ins().sdiv(lv, rv),
                            '%' => builder.ins().srem(lv, rv),
                            _ => unreachable!(),
                        };
                        (v, Type::Primitive(Primitive::I64))
                    }
                }
                'E' | 'N' => {
                    // Structural equality for any two values of the same type
                    // (the checker already verified compatibility). Compute the
                    // common, fully-concrete type — `merge_types` resolves a
                    // `none`/`[]`/`#{}` operand against the other side — then walk
                    // it with `emit_eq`. `!=` is the bitwise negation of `==`.
                    let opn = if *op == 'E' { "==" } else { "!=" };
                    if matches!(lt, Type::Fn(_, _)) || matches!(rt, Type::Fn(_, _)) {
                        return Err(Error::at(
                            format!("\"{opn}\" is not supported for function values"),
                            span.clone(),
                        ));
                    }
                    let Some(cmp_ty) = merge_types(&lt, &rt) else {
                        return Err(Error::at(
                            format!(
                                "\"{opn}\" between {} and {}: both sides must be the same type",
                                type_name(&lt),
                                type_name(&rt),
                            ),
                            span.clone(),
                        ));
                    };
                    let eq = emit_eq(module, builder, builtins, structs, lv, rv, &cmp_ty)?;
                    let result = if *op == 'N' {
                        builder.ins().bxor_imm(eq, 1)
                    } else {
                        eq
                    };
                    (result, Type::Primitive(Primitive::Bool))
                }
                '<' | '>' | 'L' | 'G' => {
                    // Unsigned integers compare with the unsigned predicates;
                    // signed ones (and i64) with the signed predicates. Operands
                    // are kept canonically sign-/zero-extended, so an i64-register
                    // comparison is correct either way.
                    let signed = match &lt {
                        Type::Primitive(p) if is_int_ty(&lt) && lt == rt => p.int_signed(),
                        _ => {
                            expect_type(
                                &lt,
                                &Type::Primitive(Primitive::I64),
                                "comparison operand",
                                l.span.clone(),
                            )?;
                            expect_type(
                                &rt,
                                &Type::Primitive(Primitive::I64),
                                "comparison operand",
                                r.span.clone(),
                            )?;
                            true
                        }
                    };
                    let cc = match (op, signed) {
                        ('<', true) => IntCC::SignedLessThan,
                        ('<', false) => IntCC::UnsignedLessThan,
                        ('>', true) => IntCC::SignedGreaterThan,
                        ('>', false) => IntCC::UnsignedGreaterThan,
                        ('L', true) => IntCC::SignedLessThanOrEqual,
                        ('L', false) => IntCC::UnsignedLessThanOrEqual,
                        ('G', true) => IntCC::SignedGreaterThanOrEqual,
                        ('G', false) => IntCC::UnsignedGreaterThanOrEqual,
                        _ => unreachable!(),
                    };
                    let b = builder.ins().icmp(cc, lv, rv);
                    (
                        builder.ins().uextend(types::I64, b),
                        Type::Primitive(Primitive::Bool),
                    )
                }
                'A' | 'O' => {
                    expect_type(
                        &lt,
                        &Type::Primitive(Primitive::Bool),
                        "logical operand",
                        l.span.clone(),
                    )?;
                    expect_type(
                        &rt,
                        &Type::Primitive(Primitive::Bool),
                        "logical operand",
                        r.span.clone(),
                    )?;
                    let v = match op {
                        'A' => builder.ins().band(lv, rv),
                        'O' => builder.ins().bor(lv, rv),
                        _ => unreachable!(),
                    };
                    (v, Type::Primitive(Primitive::Bool))
                }
                other => {
                    return Err(Error::at(format!("unsupported op {other:?}"), span.clone()));
                }
            }
        }
        ExprKind::If(cond, then_e, else_e) => {
            let (cond_v, cond_ty) = compile_expr(module, builder, cx, scopes, cond)?;
            expect_type(
                &cond_ty,
                &Type::Primitive(Primitive::Bool),
                "if condition",
                cond.span.clone(),
            )?;

            let then_block = builder.create_block();
            let else_block = builder.create_block();
            let merge_block = builder.create_block();
            builder.append_block_param(merge_block, types::I64);

            builder.ins().brif(cond_v, then_block, &[], else_block, &[]);

            // Each branch gets its own scope: anything allocated inside is
            // released before jumping to merge. The merge value is inc'd
            // first so it survives the branch dec.
            builder.switch_to_block(then_block);
            builder.seal_block(then_block);
            scopes.push(Vec::new());
            let (then_v, then_ty) = compile_expr(module, builder, cx, scopes, then_e)?;
            if needs_drop(&then_ty, structs) {
                emit_retain(builder, module, builtins, structs, then_v, &then_ty);
            }
            drop_scope(
                builder,
                module,
                builtins,
                cx.structs,
                scopes.pop().expect("then scope"),
            );
            builder.ins().jump(merge_block, &[BlockArg::Value(then_v)]);

            builder.switch_to_block(else_block);
            builder.seal_block(else_block);
            scopes.push(Vec::new());
            let (else_v, else_ty) = compile_expr(module, builder, cx, scopes, else_e)?;
            // Branch types must agree, with one twist: if one branch is
            // bare `none` (Optional(__none__)) and the other is a concrete
            // Optional(T), the result type is the concrete one.
            let merged_ty = merge_types(&then_ty, &else_ty).ok_or_else(|| {
                Error::at(
                    format!(
                        "if branches have mismatched types: then is {}, else is {}",
                        type_name(&then_ty),
                        type_name(&else_ty)
                    ),
                    span.clone(),
                )
            })?;
            if needs_drop(&else_ty, structs) {
                emit_retain(builder, module, builtins, structs, else_v, &else_ty);
            }
            drop_scope(
                builder,
                module,
                builtins,
                cx.structs,
                scopes.pop().expect("else scope"),
            );
            builder.ins().jump(merge_block, &[BlockArg::Value(else_v)]);

            builder.switch_to_block(merge_block);
            builder.seal_block(merge_block);
            let result = builder.block_params(merge_block)[0];
            // The merge value carries one ref (each branch retained it). Track
            // it in the surrounding scope so we release it on the way out.
            if needs_drop(&merged_ty, structs) {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(result, &merged_ty));
            }
            (result, merged_ty)
        }
        ExprKind::Seq(first, rest) => {
            // Evaluate `first` purely for effect and discard its value. Any
            // heap refs it allocated stay tracked in the current scope and
            // are released at scope exit, just like a discarded binding's
            // would be. Then evaluate and yield `rest`.
            compile_expr(module, builder, cx, scopes, first)?;
            compile_expr(module, builder, cx, scopes, rest)?
        }
        // Monomorphization lifts lambdas into synthesized functions before
        // codegen; the checker rejects them otherwise. So none reach here.
        ExprKind::Lambda(_, _) => unreachable!("lambda reached codegen"),
        // Monomorphization lowers TupleLit to Construct before codegen.
        ExprKind::TupleLit(_) => unreachable!("TupleLit must be lowered before codegen"),
        ExprKind::Let(name, value, body) => {
            // Evaluate the binding's value in the current scope (so any
            // string refcounts allocated by the value are already tracked
            // for dec at scope exit). Then extend the env with the new
            // name and compile the body.
            let (v, t) = compile_expr(module, builder, cx, scopes, value)?;
            reject_unit_binding(&t, name, value.span.clone())?;
            let mut new_env = env.clone();
            cx.bindings
                .borrow_mut()
                .push((name.clone(), format!("v{}", v.as_u32())));
            new_env.insert(name.clone(), EnvBinding::Immut(v, t));
            compile_expr(
                module,
                builder,
                Cx {
                    env: &new_env,
                    ..cx
                },
                scopes,
                body,
            )?
        }
        ExprKind::LetMut(name, value, body) => {
            let (v, t) = compile_expr(module, builder, cx, scopes, value)?;
            reject_unit_binding(&t, name, value.span.clone())?;
            // 8-byte slot, 8-byte aligned: fits any i64/bool/char/str.
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            builder.ins().stack_store(v, slot, 0);
            // In-place mutation optimization: a heap binding initialized from a
            // fresh literal (an array literal, or a `str` literal for `set s =
            // s + ..`) and never aliased in `body` is "exclusive" — `push` / `+`
            // may mutate it in place. Re-own it via the slot rather than the
            // literal's value-track, so a relocating grow is still dropped
            // exactly once (the slot-track loads the current pointer at exit).
            let fresh_literal = match &t {
                // A reserved-capacity array (`map`'s pre-sized output) is just as
                // fresh and unaliased as an `[..]` literal, so it's eligible for
                // the in-place `push` path too.
                Type::Array(_) => {
                    matches!(&value.kind, ExprKind::ArrayLit(_))
                        || matches!(&value.kind, ExprKind::Call(n, _, _) if n == "__builtin_with_capacity")
                }
                Type::Primitive(Primitive::Str) => matches!(&value.kind, ExprKind::Str(_)),
                _ => false,
            };
            // `mut y = p` where `p` is a moved-in owned parameter: take ownership
            // (no copy, no extra inc) so `y` is exclusive. The parameter's own
            // drop was suppressed, so there's no value-track to pop.
            let owned_move =
                matches!(&value.kind, ExprKind::Ident(n) if cx.owned_params.contains(n));
            // `allow_tail_move`: a mut binding returned (or moved out) in tail
            // position is a last-use move, not an alias, so it stays exclusive.
            let exclusive =
                (fresh_literal || owned_move) && aipl_mono::binding_is_exclusive(name, body, true);
            if is_str_repr(&t) {
                // A `str` binding's slot owns exactly one reference to its current
                // value, released once at scope exit by this slot-track. `set`
                // preserves the invariant (drop the old value, take ownership of
                // the new), so the binding can be reassigned — even across a nested
                // scope, e.g. `set s = s[..]` in a loop body — without leaking or
                // freeing a value the slot still points at.
                own_value_into_slot(
                    builder,
                    module,
                    builtins,
                    structs,
                    scopes,
                    v,
                    &t,
                    value,
                    cx.owned_params,
                );
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::slot(slot, &t));
            } else if exclusive {
                // Arrays/sets/dicts keep the in-place-mutation model: a fresh,
                // unaliased binding is re-owned via its slot so a relocating
                // `push`/`union` grow is still dropped exactly once.
                let scope = scopes.last_mut().expect("scope");
                if fresh_literal {
                    scope.pop(); // the literal's value-track (just pushed)
                }
                scope.push(Tracked::slot(slot, &t));
            } else if mut_binding_owns_slot_ref(&t) {
                // A non-exclusive `mut` array: the slot takes its *own* reference
                // on the current value (see `mut_binding_owns_slot_ref`), released
                // by this slot-track at scope exit or by the mutation that
                // replaces it. The value's existing ownership (a fresh literal's
                // value-track, or a borrowed source binding) is untouched, so
                // borrows of this version stay valid to scope exit as before.
                emit_retain(builder, module, builtins, structs, v, &t);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::slot(slot, &t));
            }
            let mut new_env = env.clone();
            cx.bindings
                .borrow_mut()
                .push((name.clone(), format!("ss{}", slot.as_u32())));
            new_env.insert(
                name.clone(),
                EnvBinding::Mut(slot, Rc::new(RefCell::new(t)), exclusive),
            );
            compile_expr(
                module,
                builder,
                Cx {
                    env: &new_env,
                    ..cx
                },
                scopes,
                body,
            )?
        }
        ExprKind::Assign(lhs, value, body) => {
            // Mono's `infer` desugars every field-path LHS to a bare-ident
            // store, so only idents reach codegen.
            let ExprKind::Ident(name) = &lhs.kind else {
                unreachable!("field assignment is desugared during monomorphization")
            };
            // `set recv.f(args)` (parsed as `set recv = recv.f(args)`): a mutating
            // method call on the assign target. The call's own codegen (the
            // `push` / mutating-method arms) writes the mutated result back into
            // `recv`'s slot, so just run it and continue — no separate store, and
            // no type-check against the binding (the call yields unit).
            let is_writeback_call = matches!(
                &value.kind,
                ExprKind::Call(f, cargs, true)
                    if !cargs.is_empty()
                        && matches!(&cargs[0].kind, ExprKind::Ident(recv) if recv == name)
                        && (f == "__builtin_push" || funcs.get(f).is_some_and(|i| i.is_mutating))
            );
            if is_writeback_call {
                compile_expr(module, builder, cx, scopes, value)?;
                return compile_expr(module, builder, cx, scopes, body);
            }
            // `set name = value;` — store to an existing mut binding.
            let binding = env.get(name).cloned().ok_or_else(|| {
                Error::at(format!("set: undeclared variable {name:?}"), span.clone())
            })?;
            let (slot, expected_ty, exclusive) = match binding {
                EnvBinding::Mut(slot, ty, excl) => (slot, ty.borrow().clone(), excl),
                EnvBinding::Immut(_, _) => {
                    return Err(Error::at(
                        format!(
                            "set: cannot assign to immutable binding {name:?} (use \"let mut\")"
                        ),
                        span.clone(),
                    ));
                }
            };
            // In-place concat: `set s = s + r` on an exclusive `str` binding
            // grows `s`'s buffer (realloc) and appends `r`, instead of building
            // a fresh string each time. The binding is slot-tracked, so the
            // (possibly relocated) buffer is still dropped exactly once.
            if exclusive && expected_ty == Type::Primitive(Primitive::Str) {
                // In-place concat: `set s = s + r` grows s's buffer (realloc) and
                // appends r, instead of building a fresh string each time. The
                // binding is slot-tracked, so the (possibly relocated) buffer is
                // still dropped exactly once.
                if let ExprKind::Binop(l, '+', r) = &value.kind {
                    if matches!(&l.kind, ExprKind::Ident(n) if n == name) {
                        let s_ptr = builder.ins().stack_load(types::I64, slot, 0);
                        let (rv, rt) = compile_expr(module, builder, cx, scopes, r)?;
                        expect_type(
                            &rt,
                            &Type::Primitive(Primitive::Str),
                            "concat operand",
                            r.span.clone(),
                        )?;
                        // `aipl_concat_mut` decs its second arg; inc first so r's
                        // own track balances. `s` is reused, not dec'd.
                        emit_inc(builder, module, builtins, rv);
                        let local = builtins.import(module, builder.func, "aipl_concat_mut");
                        let inst = builder.ins().call(local, &[s_ptr, rv]);
                        let new_ptr = builder.inst_results(inst)[0];
                        builder.ins().stack_store(new_ptr, slot, 0);
                        // No new track — the binding's slot-track owns the result.
                        return compile_expr(module, builder, cx, scopes, body);
                    }
                }
                // In-place trim: `set s = trim(s)` / `set s = s.trim()` shifts and
                // shrinks s's buffer in place rather than allocating a new string.
                // Both forms fold to the call `trim(s)` with args `[s]`.
                let trims_self = matches!(
                    &value.kind,
                    ExprKind::Call(f, cargs, _)
                        if f == "__builtin_trim"
                            && cargs.len() == 1
                            && matches!(&cargs[0].kind, ExprKind::Ident(n) if n == name)
                );
                if trims_self {
                    let s_ptr = builder.ins().stack_load(types::I64, slot, 0);
                    // `aipl_trim_mut` reuses `s` (no dec), so no pre-inc needed.
                    let local = builtins.import(module, builder.func, "aipl_trim_mut");
                    let inst = builder.ins().call(local, &[s_ptr]);
                    let new_ptr = builder.inst_results(inst)[0];
                    builder.ins().stack_store(new_ptr, slot, 0);
                    // No new track — the binding's slot-track owns the result.
                    return compile_expr(module, builder, cx, scopes, body);
                }
            }
            // In-place union: `set a = a.union(b)` on an exclusive set binding
            // extends `a`'s allocation with `b`'s elements rather than building a
            // fresh set (mirrors the in-place `+`/`trim` above). Skipped when
            // `a`'s element type is still `__none__` (a `mut a = #{}`); that falls
            // through to the copy path, which merges to `b`'s concrete type.
            if exclusive {
                if let Type::Set(elem) = &expected_ty {
                    // `set a = a.union(b)` / `set a = union(a, b)` both fold to
                    // the call `union(a, b)` with args `[a, b]`.
                    let other = match &value.kind {
                        ExprKind::Call(f, cargs, _)
                            if f == "__builtin_union"
                                && cargs.len() == 2
                                && matches!(&cargs[0].kind, ExprKind::Ident(n) if n == name) =>
                        {
                            Some(&cargs[1])
                        }
                        _ => None,
                    };
                    if let (Some(other), false) = (other, is_none_inner(elem)) {
                        let a_ptr = builder.ins().stack_load(types::I64, slot, 0);
                        let (b_ptr, b_ty) = compile_expr(module, builder, cx, scopes, other)?;
                        expect_type(&b_ty, &expected_ty, "union operand", other.span.clone())?;
                        // `aipl_set_union_mut` decs `b`; inc first so b's own track
                        // balances. `a` is reused, not dec'd.
                        emit_inc(builder, module, builtins, b_ptr);
                        let drop_fn = array_drop_fn_addr(builder, module, cx, elem);
                        let retain_fn = array_retain_fn_addr(builder, module, cx, elem);
                        let esz = builder
                            .ins()
                            .iconst(types::I64, runtime_elem_size(elem, structs));
                        let str_cmp = builder.ins().iconst(
                            types::I64,
                            i64::from(**elem == Type::Primitive(Primitive::Str)),
                        );
                        let local = builtins.import(module, builder.func, "aipl_set_union_mut");
                        let inst = builder
                            .ins()
                            .call(local, &[a_ptr, b_ptr, drop_fn, retain_fn, esz, str_cmp]);
                        let new_ptr = builder.inst_results(inst)[0];
                        builder.ins().stack_store(new_ptr, slot, 0);
                        // No new track — the binding's slot-track owns the result.
                        return compile_expr(module, builder, cx, scopes, body);
                    }
                }
            }
            // For a `str` or (non-`char[]`) array binding (whose slot owns a
            // reference on its current value — see `LetMut` and
            // `mut_binding_owns_slot_ref`), snapshot the slot's current value so
            // it can be released after the store. Read before evaluating the new
            // value: `set s = f(s)` reads `s` but never writes it, so the
            // snapshot holds. Sets/dicts and scalars keep the plain store (their
            // in-place / value-track model).
            let arr_slot_ref = mut_binding_owns_slot_ref(&expected_ty);
            let old = if is_str_repr(&expected_ty) || arr_slot_ref {
                Some(builder.ins().stack_load(types::I64, slot, 0))
            } else {
                None
            };
            let (v, t) = compile_expr(module, builder, cx, scopes, value)?;
            expect_type(&t, &expected_ty, "set", value.span.clone())?;
            if let Some(old) = old {
                if arr_slot_ref {
                    // Array: the slot takes its *own* reference on the new value
                    // (the value's existing ownership — a fresh literal's
                    // value-track, or a borrowed source binding — is untouched,
                    // preserving borrows of it), then releases its reference on
                    // the replaced value. Aliases of the old value keep it alive
                    // through its own creation-scope track.
                    emit_retain(builder, module, builtins, structs, v, &expected_ty);
                    builder.ins().stack_store(v, slot, 0);
                    emit_drop(builder, module, builtins, structs, old, &expected_ty);
                } else {
                    // `str`: take sole ownership of the new value for the slot,
                    // then release the reference the slot held before —
                    // preserving the slot-track's single-reference invariant
                    // across the reassignment.
                    own_value_into_slot(
                        builder,
                        module,
                        builtins,
                        structs,
                        scopes,
                        v,
                        &expected_ty,
                        value,
                        cx.owned_params,
                    );
                    builder.ins().stack_store(v, slot, 0);
                    emit_drop(builder, module, builtins, structs, old, &expected_ty);
                }
            } else {
                builder.ins().stack_store(v, slot, 0);
            }
            // Body uses the unchanged env; the slot has been updated in-place
            // so subsequent Ident lookups will load the new value.
            compile_expr(module, builder, cx, scopes, body)?
        }
        ExprKind::For(var, iterable, body) => {
            // `for (let v : iterable) { body }`. Over a `str` this walks
            // byte-by-byte until NUL (binding `v: char`); over a `T[]` it
            // walks index 0..len (binding `v: T`). Body's value is
            // discarded; the loop evaluates to i64 0.
            let (it_ptr, it_ty) = compile_expr(module, builder, cx, scopes, iterable)?;

            // For a `str` iterable, set up a char cursor: a small codegen-stacked
            // struct the runtime advances byte-by-byte. It streams every
            // representation — including a rope, leaf-by-leaf without
            // materializing — so the header just pulls the next byte (`-1` at the
            // end). For an array this is unused.
            let str_cursor = if it_ty == Type::Primitive(Primitive::Str) || is_char_array(&it_ty) {
                let cur = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    ITER_SIZE as u32,
                    3,
                ));
                let cur_addr = builder.ins().stack_addr(types::I64, cur, 0);
                let init_f = builtins.import(module, builder.func, "aipl_str_iter_init");
                builder.ins().call(init_f, &[cur_addr, it_ptr]);
                cur_addr
            } else {
                it_ptr // unused for the array branch
            };

            // Index slot, initialized to 0.
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, slot, 0);

            let header = builder.create_block();
            let body_block = builder.create_block();
            let exit = builder.create_block();
            builder.ins().jump(header, &[]);

            // Header: load i, decide whether to continue, and (for arrays)
            // fetch the element. `var_value`/`var_ty` are what the body's
            // loop variable binds to.
            builder.switch_to_block(header);
            let i = builder.ins().stack_load(types::I64, slot, 0);
            let (var_value, var_ty) = match &it_ty {
                t if *t == Type::Primitive(Primitive::Str) || is_char_array(t) => {
                    // Pull the next byte from the cursor; `-1` signals the end (so
                    // a rope is walked leaf-by-leaf, never flattened, and we never
                    // index out of bounds).
                    let next_f = builtins.import(module, builder.func, "aipl_str_iter_next");
                    let inst = builder.ins().call(next_f, &[str_cursor]);
                    let byte_i64 = builder.inst_results(inst)[0];
                    let more = builder
                        .ins()
                        .icmp_imm(IntCC::SignedGreaterThanOrEqual, byte_i64, 0);
                    builder.ins().brif(more, body_block, &[], exit, &[]);
                    (byte_i64, Type::Primitive(Primitive::Char))
                }
                Type::Array(inner) => {
                    let elem_ty = (**inner).clone();
                    let len = load_arr_len(builder, it_ptr);
                    let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
                    builder.ins().brif(more, body_block, &[], exit, &[]);
                    // Fetch element i in the body block (it's only valid there).
                    // Switch now; the element read (a bit-unpack for `bool`, a
                    // load or composite address otherwise) happens here.
                    builder.switch_to_block(body_block);
                    let elem = load_array_elem(
                        module,
                        builder,
                        cx.builtins,
                        it_ptr,
                        i,
                        &elem_ty,
                        cx.structs,
                    );
                    (elem, elem_ty)
                }
                _ => {
                    return Err(Error::at(
                        format!(
                            "for-loop iterable must be a str or array, got {}",
                            type_name(&it_ty)
                        ),
                        iterable.span.clone(),
                    ));
                }
            };

            // Body: bind var, run body in fresh refcount scope, advance i.
            // (For the array case we already switched to body_block above.)
            if it_ty == Type::Primitive(Primitive::Str) || is_char_array(&it_ty) {
                builder.switch_to_block(body_block);
            }
            builder.seal_block(body_block);
            let mut body_env = env.clone();
            cx.bindings
                .borrow_mut()
                .push((var.clone(), format!("v{}", var_value.as_u32())));
            body_env.insert(var.clone(), EnvBinding::Immut(var_value, var_ty.clone()));
            scopes.push(Vec::new());
            // The element is borrowed from the array (which still owns its
            // copy); take a fresh ref so the binding owns one for the
            // iteration, and track it for drop at iteration end.
            if needs_drop(&var_ty, cx.structs) {
                emit_retain(builder, module, builtins, cx.structs, var_value, &var_ty);
                scopes
                    .last_mut()
                    .expect("for-body scope")
                    .push(Tracked::new(var_value, &var_ty));
            }
            let _ = compile_expr(
                module,
                builder,
                Cx {
                    env: &body_env,
                    ..cx
                },
                scopes,
                body,
            )?;
            // Any heap values the body materialized this iteration die here.
            drop_scope(
                builder,
                module,
                builtins,
                cx.structs,
                scopes.pop().expect("for-body scope"),
            );
            let next = builder.ins().iadd_imm(i, 1);
            builder.ins().stack_store(next, slot, 0);
            builder.ins().jump(header, &[]);
            builder.seal_block(header);

            builder.switch_to_block(exit);
            builder.seal_block(exit);
            (
                builder.ins().iconst(types::I64, 0),
                Type::Primitive(Primitive::I64),
            )
        }
        ExprKind::While(cond, body) => {
            // `while (cond) { body }`. Re-evaluates `cond` each iteration and
            // runs `body` while it's true. Body's value is discarded; the loop
            // evaluates to i64 0 (like `for`). Both `cond` and `body` see the
            // enclosing env, so a `mut` declared before the loop can be tested in
            // `cond` and reassigned (`set`) in `body` across iterations.
            let header = builder.create_block();
            let body_block = builder.create_block();
            let exit = builder.create_block();
            builder.ins().jump(header, &[]);

            // Header: evaluate the condition in a fresh scope so any heap temps
            // it materializes are released whether or not we enter the body.
            builder.switch_to_block(header);
            scopes.push(Vec::new());
            let (cond_v, cond_ty) = compile_expr(module, builder, cx, scopes, cond)?;
            expect_type(
                &cond_ty,
                &Type::Primitive(Primitive::Bool),
                "while condition",
                cond.span.clone(),
            )?;
            drop_scope(
                builder,
                module,
                builtins,
                cx.structs,
                scopes.pop().expect("while-cond scope"),
            );
            builder.ins().brif(cond_v, body_block, &[], exit, &[]);

            // Body: run in a fresh refcount scope (its iteration-local heap
            // values die at the end of each pass), then loop back to re-test.
            builder.switch_to_block(body_block);
            builder.seal_block(body_block);
            scopes.push(Vec::new());
            let _ = compile_expr(module, builder, cx, scopes, body)?;
            drop_scope(
                builder,
                module,
                builtins,
                cx.structs,
                scopes.pop().expect("while-body scope"),
            );
            builder.ins().jump(header, &[]);
            builder.seal_block(header);

            builder.switch_to_block(exit);
            builder.seal_block(exit);
            (
                builder.ins().iconst(types::I64, 0),
                Type::Primitive(Primitive::I64),
            )
        }
        ExprKind::Match(scrutinee, arms) => {
            let (ptr, scrut_ty) = compile_expr(module, builder, cx, scopes, scrutinee)?;

            let merge = builder.create_block();
            builder.append_block_param(merge, types::I64);
            let arm_blocks: Vec<_> = arms.iter().map(|_| builder.create_block()).collect();

            // A `str` scrutinee dispatches by *content equality* — no tag word —
            // so `plan` is `None` for it (its arms bind nothing); any other
            // matchable type dispatches on its tag via a `MatchPlan`.
            let plan = if is_str_repr(&scrut_ty) {
                // Compare the scrutinee against each string-literal arm in turn,
                // routing a match to that arm; the wildcard `_` arm is the final
                // fallthrough. The scrutinee is *borrowed* (each `aipl_str_eq`
                // consumes a ref from both operands, so `emit_inc` first); its one
                // real drop is left to enclosing-scope tracking.
                let wildcard = arms
                    .iter()
                    .position(|a| matches!(a.pattern, Pattern::Wildcard))
                    .expect("a str match has a `_` arm (checked)");
                for (i, arm) in arms.iter().enumerate() {
                    let Pattern::Str(lit) = &arm.pattern else {
                        continue;
                    };
                    let lit_expr = Expr::new(ExprKind::Str(lit.clone()), scrutinee.span.clone());
                    let (lit_val, _) = compile_expr(module, builder, cx, scopes, &lit_expr)?;
                    emit_inc(builder, module, builtins, ptr);
                    emit_inc(builder, module, builtins, lit_val);
                    let eq_f = builtins.import(module, builder.func, "aipl_str_eq");
                    let inst = builder.ins().call(eq_f, &[ptr, lit_val]);
                    let eq = builder.inst_results(inst)[0];
                    let next = builder.create_block();
                    builder.ins().brif(eq, arm_blocks[i], &[], next, &[]);
                    builder.switch_to_block(next);
                    builder.seal_block(next);
                }
                builder.ins().jump(arm_blocks[wildcard], &[]);
                None
            } else if let Type::Array(elem) = &scrut_ty {
                // An array scrutinee also dispatches by *value* (no tag word): each
                // arm checks exact length then elementwise equality, with the `_`
                // arm as the final fallthrough. Arms bind nothing, so `plan` is
                // `None`.
                let elem: &Type = elem;
                let wildcard = arms
                    .iter()
                    .position(|a| matches!(a.pattern, Pattern::Wildcard))
                    .expect("an array match has a `_` arm (checked)");
                // Compile every pattern element up front in *this* (dominating)
                // block, so a tracked literal (a heap str / nested array) is defined
                // where it dominates both the per-arm comparison below and the
                // enclosing-scope drop. (Scalar literals are plain constants.)
                let arm_lits: Vec<Vec<Value>> = arms
                    .iter()
                    .map(|arm| {
                        let mut vals = Vec::new();
                        if let Pattern::Array(elems) = &arm.pattern {
                            for e in elems {
                                let (v, _) = compile_expr(module, builder, cx, scopes, e)?;
                                vals.push(v);
                            }
                        }
                        Ok(vals)
                    })
                    .collect::<Result<_, Error>>()?;
                let scrut_len = seq_len(module, builder, builtins, ptr, &scrut_ty);
                for (i, arm) in arms.iter().enumerate() {
                    let Pattern::Array(elems) = &arm.pattern else {
                        continue;
                    };
                    // Length must match before any element load (else out of bounds).
                    let len_ok =
                        builder
                            .ins()
                            .icmp_imm(IntCC::Equal, scrut_len, elems.len() as i64);
                    let check = builder.create_block();
                    let next = builder.create_block();
                    builder.ins().brif(len_ok, check, &[], next, &[]);
                    builder.switch_to_block(check);
                    builder.seal_block(check);
                    // Length matches: AND together the elementwise comparisons. The
                    // scrutinee elements are borrowed; `emit_eq` balances any ref it
                    // consumes (the `str` arm) with its own incs.
                    let mut matched = builder.ins().iconst(types::I64, 1);
                    for j in 0..elems.len() {
                        let idx = builder.ins().iconst(types::I64, j as i64);
                        let scrut_elem =
                            seq_elem(module, builder, builtins, structs, ptr, idx, &scrut_ty);
                        let eq = emit_eq(
                            module,
                            builder,
                            builtins,
                            structs,
                            scrut_elem,
                            arm_lits[i][j],
                            elem,
                        )?;
                        matched = builder.ins().band(matched, eq);
                    }
                    builder.ins().brif(matched, arm_blocks[i], &[], next, &[]);
                    builder.switch_to_block(next);
                    builder.seal_block(next);
                }
                builder.ins().jump(arm_blocks[wildcard], &[]);
                None
            } else {
                let tag = builder.ins().load(types::I64, MemFlags::trusted(), ptr, 0);
                // Plan each arm's tag + payload bindings up front (snapshotting the
                // variant layout so it isn't borrowed across the body compilations).
                let plan = plan_match(&scrut_ty, arms, structs, scrutinee.span.clone())?;
                // Dispatch on the tag to the matching arm's block.
                match &plan {
                    MatchPlan::Optional { some, none, .. } => {
                        builder
                            .ins()
                            .brif(tag, arm_blocks[*some], &[], arm_blocks[*none], &[]);
                    }
                    MatchPlan::Variant { arm_tags, .. } => {
                        // `tag == arm_tags[i]` routes to arm i; exhaustiveness
                        // (checked in `plan_match`) makes the last arm the only
                        // remaining tag, so it's the final fallthrough.
                        for i in 0..arm_blocks.len() - 1 {
                            let next = builder.create_block();
                            let hit = builder
                                .ins()
                                .icmp_imm(IntCC::Equal, tag, arm_tags[i] as i64);
                            builder.ins().brif(hit, arm_blocks[i], &[], next, &[]);
                            builder.switch_to_block(next);
                            builder.seal_block(next);
                        }
                        builder.ins().jump(arm_blocks[arm_blocks.len() - 1], &[]);
                    }
                }
                Some((plan, tag))
            };

            let mut merged_ty: Option<Type> = None;
            for (i, arm) in arms.iter().enumerate() {
                builder.switch_to_block(arm_blocks[i]);
                builder.seal_block(arm_blocks[i]);
                scopes.push(Vec::new());
                // Read this arm's payload bindings (borrowed from the scrutinee);
                // a `str` arm (`plan` is `None`) binds nothing.
                let binds = match &plan {
                    Some((plan, tag)) => bind_match_arm(builder, plan, arm, i, ptr, *tag, structs),
                    None => Vec::new(),
                };
                let mut arm_env = env.clone();
                for (name, value, ty) in &binds {
                    cx.bindings
                        .borrow_mut()
                        .push((name.clone(), format!("v{}", value.as_u32())));
                    arm_env.insert(name.clone(), EnvBinding::Immut(*value, ty.clone()));
                    // The binding is borrowed from the scrutinee (which still
                    // owns its copy); retain it for the arm, released at arm exit.
                    if needs_drop(ty, structs) {
                        emit_retain(builder, module, builtins, structs, *value, ty);
                        scopes
                            .last_mut()
                            .expect("arm scope")
                            .push(Tracked::new(*value, ty));
                    }
                }
                let (av, at) = compile_expr(
                    module,
                    builder,
                    Cx {
                        env: &arm_env,
                        ..cx
                    },
                    scopes,
                    &arm.body,
                )?;
                if needs_drop(&at, structs) {
                    emit_retain(builder, module, builtins, structs, av, &at);
                }
                drop_scope(
                    builder,
                    module,
                    builtins,
                    cx.structs,
                    scopes.pop().expect("arm scope"),
                );
                builder.ins().jump(merge, &[BlockArg::Value(av)]);
                merged_ty = Some(match merged_ty {
                    None => at,
                    Some(prev) => merge_types(&prev, &at).ok_or_else(|| {
                        Error::at(
                            format!(
                                "match arms have mismatched types: {} vs {}",
                                type_name(&prev),
                                type_name(&at),
                            ),
                            span.clone(),
                        )
                    })?,
                });
            }

            builder.switch_to_block(merge);
            builder.seal_block(merge);
            let result = builder.block_params(merge)[0];
            let merged_ty = merged_ty.unwrap_or(Type::Primitive(Primitive::I64));
            if needs_drop(&merged_ty, structs) {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(result, &merged_ty));
            }
            (result, merged_ty)
        }
        ExprKind::ArrayLit(elems) => {
            // All elements must share one primitive type. An empty
            // literal has element type `__none__` and coerces to any
            // concrete `T[]` (mirrors bare `none`).
            let mut elem_ty: Option<Type> = None;
            let mut vals = Vec::with_capacity(elems.len());
            for el in elems {
                let (v, t) = compile_expr(module, builder, cx, scopes, el)?;
                match &elem_ty {
                    None => {
                        let ok = is_array_elem(&t)
                            || matches!(&t, Type::Optional(_))
                            || matches!(&t, Type::Named(n) if structs.contains_key(n));
                        if !ok {
                            return Err(Error::at(
                                format!(
                                    "array elements must be i64, bool, char, str, or an array, got {}",
                                    type_name(&t)
                                ),
                                el.span.clone(),
                            ));
                        }
                        elem_ty = Some(t.clone());
                    }
                    Some(expected) => expect_type(&t, expected, "array element", el.span.clone())?,
                }
                vals.push((v, t));
            }
            let elem = elem_ty.unwrap_or(Type::NoneInner);
            let arr_ty = Type::Array(Box::new(elem.clone()));
            if is_char_array(&arr_ty) {
                // `char[]` is str-shaped (see `is_char_array`): build a heap
                // `str` buffer and write each element's byte directly, rather
                // than a generic array block. `vals` is non-empty here — an
                // empty `[]` never infers `elem == char` (it stays the
                // untyped `NoneInner` element and takes the generic path
                // below), so there's no empty/SSO case to special-case.
                // Always heap-allocates (no small-string inlining yet).
                let len = builder.ins().iconst(types::I64, vals.len() as i64);
                let alloc = builtins.import(module, builder.func, "aipl_str_alloc");
                let inst = builder.ins().call(alloc, &[len]);
                let buf = builder.inst_results(inst)[0];
                for (i, (v, _)) in vals.into_iter().enumerate() {
                    let addr = builder.ins().iadd_imm(buf, i as i64);
                    builder.ins().istore8(MemFlags::trusted(), v, addr, 0);
                }
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(buf, &arr_ty));
                return Ok((buf, arr_ty));
            }
            let len = builder.ins().iconst(types::I64, elems.len() as i64);
            let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
            let esz_v = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(&elem, structs));
            let new_local = builtins.import(module, builder.func, "aipl_array_new");
            let inst = builder.ins().call(new_local, &[len, drop_fn, esz_v]);
            let ptr = builder.inst_results(inst)[0];
            let elems_base = builder.ins().iadd_imm(ptr, ARR_ELEMS_OFFSET as i64);
            if is_bit_packed(&elem) {
                // Pack 8 bools per byte: build each data byte from its (up to 8)
                // element bits and store it. Bools carry no heap (no retain).
                for (j, chunk) in vals.chunks(8).enumerate() {
                    let mut byte = builder.ins().iconst(types::I64, 0);
                    for (k, (v, _)) in chunk.iter().enumerate() {
                        let bit = builder.ins().ishl_imm(*v, k as i64);
                        byte = builder.ins().bor(byte, bit);
                    }
                    let addr = builder.ins().iadd_imm(elems_base, j as i64);
                    builder.ins().istore8(MemFlags::trusted(), byte, addr, 0);
                }
            } else {
                let esz = elem_size_of(&elem, structs);
                for (i, (v, src_ty)) in vals.into_iter().enumerate() {
                    let slot = builder.ins().iadd_imm(elems_base, i as i64 * esz);
                    // Copy the element's own size (a `none` is narrower than a
                    // wider optional element slot — its unread tail is don't-care).
                    store_array_elem(builder, slot, v, &src_ty, structs);
                    // The array co-owns each heap element — retain on store.
                    emit_retain(builder, module, builtins, structs, v, &elem);
                }
            }
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &arr_ty));
            (ptr, arr_ty)
        }
        ExprKind::SetLit(elems) => {
            // A set reuses the array heap block. Pre-size to the literal length
            // (an upper bound), then insert each element deduplicated via
            // `aipl_set_insert`. For `str` elements the block carries the array
            // `str` drop/retain helpers (so it frees/retains its strings) and
            // membership compares by content; scalars need neither. An empty
            // `#{}` is `__none__`-typed and coerces to any `T{}`.
            let mut elem_ty: Option<Type> = None;
            let mut vals = Vec::with_capacity(elems.len());
            for el in elems {
                let (v, t) = compile_expr(module, builder, cx, scopes, el)?;
                match &elem_ty {
                    None => {
                        if !is_set_elem(&t) {
                            return Err(Error::at(
                                format!(
                                    "set elements must be i64, bool, char, or str, got {}",
                                    type_name(&t)
                                ),
                                el.span.clone(),
                            ));
                        }
                        elem_ty = Some(t.clone());
                    }
                    Some(expected) => expect_type(&t, expected, "set element", el.span.clone())?,
                }
                vals.push(v);
            }
            let elem = elem_ty.unwrap_or(Type::NoneInner);
            let esz = runtime_elem_size(&elem, structs);
            let esz_v = builder.ins().iconst(types::I64, esz);
            // `str` elements are heap: store the array `str` drop/retain helpers
            // so the set frees/retains them, and compare membership by content.
            let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
            let str_cmp = builder.ins().iconst(
                types::I64,
                i64::from(elem == Type::Primitive(Primitive::Str)),
            );
            let cap = builder.ins().iconst(types::I64, elems.len() as i64);
            let with_cap = builtins.import(module, builder.func, "aipl_array_with_cap");
            let inst = builder.ins().call(with_cap, &[cap, drop_fn, esz_v]);
            let mut ptr = builder.inst_results(inst)[0];
            let insert = builtins.import(module, builder.func, "aipl_set_insert");
            for v in vals {
                // `aipl_set_insert` reads the element through a pointer; spill
                // the value (a `bool` is read back as i64, a `str` as its
                // pointer) and pass its address.
                let s = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(v, s, 0);
                let x_ptr = builder.ins().stack_addr(types::I64, s, 0);
                let inst = builder
                    .ins()
                    .call(insert, &[ptr, x_ptr, drop_fn, retain_fn, esz_v, str_cmp]);
                ptr = builder.inst_results(inst)[0];
            }
            let set_ty = Type::Set(Box::new(elem));
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &set_ty));
            (ptr, set_ty)
        }
        ExprKind::DictLit(pairs) => {
            // A dict reuses the array heap block, one element per `(key, value)`
            // pair laid out as `[key: 8][value: sizeof(V)]`. Pre-size to the
            // literal length (an upper bound — duplicate keys collapse), then
            // insert each pair via `aipl_dict_insert` (last-binding-wins). The
            // block carries the pair drop/retain helpers so it frees/retains each
            // pair's key and value; key membership compares by content for `str`.
            // An empty `#{:}` is `__none__`-typed and coerces to any `#{K: V}`.
            let mut key_ty: Option<Type> = None;
            let mut val_ty: Option<Type> = None;
            let mut vals = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                let (kv, kt) = compile_expr(module, builder, cx, scopes, k)?;
                let (vv, vt) = compile_expr(module, builder, cx, scopes, v)?;
                match &key_ty {
                    None => {
                        if !is_dict_key(&kt) {
                            return Err(Error::at(
                                format!(
                                    "dict keys must be i64, bool, char, or str, got {}",
                                    type_name(&kt)
                                ),
                                k.span.clone(),
                            ));
                        }
                        key_ty = Some(kt.clone());
                        val_ty = Some(vt.clone());
                    }
                    Some(expected_k) => {
                        expect_type(&kt, expected_k, "dict key", k.span.clone())?;
                        expect_type(&vt, val_ty.as_ref().unwrap(), "dict value", v.span.clone())?;
                    }
                }
                vals.push((kv, vv));
            }
            let key = key_ty.unwrap_or(Type::NoneInner);
            let val = val_ty.unwrap_or(Type::NoneInner);
            let pair_size = 8 + elem_size_of(&val, structs);
            let psz = builder.ins().iconst(types::I64, pair_size);
            let (drop_fn, retain_fn) = pair_rc_fn_addrs(builder, module, cx, &key, &val);
            let str_cmp = builder.ins().iconst(
                types::I64,
                i64::from(key == Type::Primitive(Primitive::Str)),
            );
            let cap = builder.ins().iconst(types::I64, pairs.len() as i64);
            let with_cap = builtins.import(module, builder.func, "aipl_array_with_cap");
            let inst = builder.ins().call(with_cap, &[cap, drop_fn, psz]);
            let mut ptr = builder.inst_results(inst)[0];
            let insert = builtins.import(module, builder.func, "aipl_dict_insert");
            for (kv, vv) in vals {
                // Assemble the pair `[key][value]` in a scratch slot, then insert
                // it (the inserter copies the bytes and retains the key/value, so
                // the dict co-owns them alongside the originals in scope).
                let pbuf = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    pair_size as u32,
                    3,
                ));
                let pbase = builder.ins().stack_addr(types::I64, pbuf, 0);
                store_array_elem(builder, pbase, kv, &key, structs);
                let vaddr = builder.ins().iadd_imm(pbase, 8);
                store_array_elem(builder, vaddr, vv, &val, structs);
                let inst = builder
                    .ins()
                    .call(insert, &[ptr, pbase, drop_fn, retain_fn, psz, str_cmp]);
                ptr = builder.inst_results(inst)[0];
            }
            let dict_ty = Type::Dict(Box::new(key), Box::new(val));
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &dict_ty));
            (ptr, dict_ty)
        }
        ExprKind::Index(obj, index) => {
            let (recv_v, recv_ty) = compile_expr(module, builder, cx, scopes, obj)?;
            let (idx_v, idx_t) = compile_expr(module, builder, cx, scopes, index)?;

            // `s[span]` — a `Span` index is slice sugar for
            // `s[span.start..span.end]`: load the two bound fields from the
            // struct (evaluated once, receiver first) and slice exactly like
            // `ExprKind::Slice`.
            if matches!(&idx_t, Type::Named(n) if n == "__builtin_Span") {
                let layout = structs
                    .get("__builtin_Span")
                    .and_then(TypeDef::as_struct)
                    .ok_or_else(|| {
                        Error::at("Span struct layout missing (compiler bug)", span.clone())
                    })?;
                let i64_ty = Type::Primitive(Primitive::I64);
                let mut bound = |field: &str| -> Result<Value, Error> {
                    let f = layout.field(field).ok_or_else(|| {
                        Error::at(
                            format!("Span struct has no field {field:?} (compiler bug)"),
                            span.clone(),
                        )
                    })?;
                    Ok(component(builder, idx_v, f.offset, &i64_ty, structs))
                };
                let a_v = bound("start")?;
                let b_v = bound("end")?;
                return emit_slice(
                    module,
                    builder,
                    cx,
                    scopes,
                    recv_v,
                    &recv_ty,
                    a_v,
                    Some(b_v),
                    &obj.span,
                );
            }

            expect_type(
                &idx_t,
                &Type::Primitive(Primitive::I64),
                "index",
                index.span.clone(),
            )?;

            // `s[i]` on a `str` (or a str-shaped `char[]`, see `is_char_array`)
            // yields `char?` — the byte at `i`, via the runtime `aipl_char_at`.
            // (Exact-`str` plus `char[]`, not the broader `is_str_shaped`: the
            // original check was an exact `Str` match, not `is_str_repr` — kept
            // that scope, since `Error`/concat-str indexing wasn't audited.)
            if recv_ty == Type::Primitive(Primitive::Str) || is_char_array(&recv_ty) {
                let ptr = emit_char_at(builder, module, builtins, recv_v, idx_v);
                return Ok((
                    ptr,
                    Type::Optional(Box::new(Type::Primitive(Primitive::Char))),
                ));
            }

            let arr_ptr = recv_v;
            let elem_ty = match &recv_ty {
                Type::Array(inner) => (**inner).clone(),
                _ => {
                    return Err(Error::at(
                        format!("cannot index a value of type {}", type_name(&recv_ty)),
                        obj.span.clone(),
                    ));
                }
            };

            // The result is `elem?`: `some(<element>)` in bounds, `none` out of
            // bounds — exactly the `some`/`none` constructors. Indexing a `T?[]`
            // wraps one more optional layer, so the result is a genuine `T??`
            // whose flattened slot is `8 (tag) + sizeof(Core)`, independent of
            // the element's own (possibly wider) array stride `esz`.
            let result_ty = Type::Optional(Box::new(elem_ty.clone()));
            // Guard the load behind a branch so an out-of-bounds index
            // never dereferences past the allocation.
            let len = load_arr_len(builder, arr_ptr);
            let ge0 = builder
                .ins()
                .icmp_imm(IntCC::SignedGreaterThanOrEqual, idx_v, 0);
            let lt_len = builder.ins().icmp(IntCC::SignedLessThan, idx_v, len);
            let in_bounds = builder.ins().band(ge0, lt_len);

            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&result_ty, structs) as u32,
                3,
            ));
            let sbase = builder.ins().stack_addr(types::I64, slot, 0);
            let in_block = builder.create_block();
            let out_block = builder.create_block();
            let merge_block = builder.create_block();
            builder.ins().brif(in_bounds, in_block, &[], out_block, &[]);

            builder.switch_to_block(in_block);
            builder.seal_block(in_block);
            // Read the element (a bit-unpacked `bool`, a scalar/pointer, or a
            // composite address), then build `some(element)` into the result
            // slot and retain its core heap (`emit_retain` incs only when the
            // result is fully `some`).
            let elem_val =
                load_array_elem(module, builder, builtins, arr_ptr, idx_v, &elem_ty, structs);
            emit_build_some(builder, sbase, elem_val, &elem_ty, structs);
            emit_retain(builder, module, builtins, structs, sbase, &result_ty);
            builder.ins().jump(merge_block, &[]);

            builder.switch_to_block(out_block);
            builder.seal_block(out_block);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, slot, 0);
            builder.ins().jump(merge_block, &[]);

            builder.switch_to_block(merge_block);
            builder.seal_block(merge_block);
            let ptr = sbase;
            if needs_drop(&result_ty, structs) {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(ptr, &result_ty));
            }
            (ptr, result_ty)
        }
        ExprKind::Slice(obj, start, end) => {
            // `s[start..end]` — see `emit_slice` for the receiver dispatch
            // (str / char[] / array) and ownership notes.
            let (s_v, s_ty) = compile_expr(module, builder, cx, scopes, obj)?;
            let (a_v, a_t) = compile_expr(module, builder, cx, scopes, start)?;
            expect_type(
                &a_t,
                &Type::Primitive(Primitive::I64),
                "slice start",
                start.span.clone(),
            )?;
            let b_v = match end {
                Some(end) => {
                    let (b_v, b_t) = compile_expr(module, builder, cx, scopes, end)?;
                    expect_type(
                        &b_t,
                        &Type::Primitive(Primitive::I64),
                        "slice end",
                        end.span.clone(),
                    )?;
                    Some(b_v)
                }
                None => None,
            };
            return emit_slice(module, builder, cx, scopes, s_v, &s_ty, a_v, b_v, &obj.span);
        }
        ExprKind::Try(inner) => {
            // `r?` propagates: evaluate the result `r`; on Err, rebuild the
            // enclosing function's Err-result from the same payload and return
            // early; on Ok, yield the unwrapped Ok value.
            let (rptr, rty) = compile_expr(module, builder, cx, scopes, inner)?;
            let Type::Result(ok_in, err_in) = &rty else {
                return Err(Error::at(
                    format!("\"?\" operand must be a result, got {}", type_name(&rty)),
                    inner.span.clone(),
                ));
            };
            // The enclosing context must be able to receive the propagated error:
            // a result-returning function (early-return its Err via sret), or an
            // `fn main() -> !Error` (print `error: <msg>` and exit 1).
            let ret_err = match cx.ret_ty {
                Type::Result(_, ret_err) => Some(ret_err),
                _ if cx.error_main => None, // err type is `Error`
                _ => {
                    return Err(Error::at(
                        "\"?\" can only be used in a function that returns a result",
                        span.clone(),
                    ));
                }
            };
            // The propagated error must fit the enclosing function's err side
            // (`Error` for an `!Error` main).
            let enclosing_err = match ret_err {
                Some(e) => (**e).clone(),
                None => error_ty(),
            };
            if !coercible(err_in, &enclosing_err) {
                return Err(Error::at(
                    format!(
                        "\"?\" propagates a {} error, but the enclosing function returns \
                         errors of type {}",
                        type_name(err_in),
                        type_name(&enclosing_err)
                    ),
                    span.clone(),
                ));
            }
            let ok_ty = (**ok_in).clone();
            let err_in_ty = (**err_in).clone();
            let tag = builder.ins().load(types::I64, MemFlags::trusted(), rptr, 0);
            let ok_block = builder.create_block();
            let err_block = builder.create_block();
            // tag 1 = ok → continue; tag 0 = err → early return.
            builder.ins().brif(tag, ok_block, &[], err_block, &[]);

            // --- Err: early return. ---
            builder.switch_to_block(err_block);
            builder.seal_block(err_block);
            if cx.error_main {
                // `?` in `fn main() -> !Error`: print `error: <msg>` and exit 1.
                // Read the err payload (borrowed) before the scope drop frees it.
                let msg = component(builder, rptr, OPT_VALUE_OFFSET, &err_in_ty, structs);
                let f = builtins.import(module, builder.func, "aipl_print_error");
                builder.ins().call(f, &[msg]);
                for scope in scopes.iter() {
                    for t in scope {
                        let v = match t.owned {
                            Owned::Value(v) => v,
                            Owned::Slot(slot) => builder.ins().stack_load(types::I64, slot, 0),
                        };
                        emit_drop(builder, module, builtins, structs, v, &t.ty);
                    }
                }
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().return_(&[one]);
            } else {
                // The result repr is layout-identical across Ok sides (16-byte
                // `{tag, value@8}`, scalar/str payload), so the scrutinee — already
                // tag 0 with the err payload at offset 8 — *is* the enclosing
                // Err-result. Co-own its (possibly heap) payload for the caller,
                // then release every live scope before leaving the function.
                emit_retain(builder, module, builtins, structs, rptr, cx.ret_ty);
                for scope in scopes.iter() {
                    for t in scope {
                        let v = match t.owned {
                            Owned::Value(v) => v,
                            Owned::Slot(slot) => builder.ins().stack_load(types::I64, slot, 0),
                        };
                        emit_drop(builder, module, builtins, structs, v, &t.ty);
                    }
                }
                let sret = cx.sret.expect("result-returning fn has an sret pointer");
                copy_composite(builder, sret, rptr, cx.ret_ty, structs);
                builder.ins().return_(&[]);
            }

            // --- Ok: unwrap the value and carry on. ---
            builder.switch_to_block(ok_block);
            builder.seal_block(ok_block);
            // A void-Ok (`!E`) unwraps to unit — there's no payload to read.
            if is_unit(&ok_ty) {
                return Ok((builder.ins().iconst(types::I64, 0), Type::Unit));
            }
            let val = component(builder, rptr, OPT_VALUE_OFFSET, &ok_ty, structs);
            // A heap Ok payload is now a fresh co-owner of the scrutinee's heap
            // (which still drops its own ref at scope exit) — retain and track it
            // exactly like a call result.
            if needs_drop(&ok_ty, structs) {
                emit_retain(builder, module, builtins, structs, val, &ok_ty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(val, &ok_ty));
            }
            (val, ok_ty)
        }
    })
}

fn alloc_struct_slot(builder: &mut FunctionBuilder, layout: &StructLayout) -> StackSlot {
    let data = StackSlotData::new(StackSlotKind::ExplicitSlot, layout.size, 3);
    builder.create_sized_stack_slot(data)
}

/// Byte size of a struct field of type `ty`. An optional is stored inline as
/// `8 (tag) + sizeof(Core)` (a nested `T??` is no wider than `T?` — see
/// "Optional representation"); a nested struct is stored inline at its own size;
/// every other allowed field type is an 8-byte scalar or heap pointer. The
/// nested struct's layout must already be resolved.
fn field_size(ty: &Type, structs: &HashMap<String, TypeDef>) -> u32 {
    match ty {
        Type::Optional(_) => elem_size_of(ty, structs) as u32,
        Type::Named(n) => structs.get(n).map_or(8, |t| t.size()),
        _ => 8,
    }
}

/// Size in bytes of a value returned/passed by hidden pointer (sret), or `None`
/// if it's a plain 8-byte value. Both optionals (`{tag, value}`, possibly
/// nested) and structs are returned this way — uniformly, by pointer.
fn sret_size(ty: &Type, structs: &HashMap<String, TypeDef>) -> Option<u32> {
    match ty {
        Type::Optional(_) | Type::Result(_, _) => Some(elem_size_of(ty, structs) as u32),
        Type::Named(n) => structs.get(n).map(|t| t.size()),
        _ => None,
    }
}

/// Copy a composite value (`ty` is a struct or optional) of `src`'s size from
/// the address `src` into the address `dst`, word by word. The source's static
/// type fixes the byte count: a value can only be stored where its own type (or
/// a wider optional, for `none`) is expected, and the slack past a `none`'s
/// `tag` is never read — so copying the source size is always safe.
fn copy_composite(
    builder: &mut FunctionBuilder,
    dst: Value,
    src: Value,
    ty: &Type,
    structs: &HashMap<String, TypeDef>,
) {
    let size = sret_size(ty, structs).unwrap_or(8);
    let mut o = 0u32;
    while o < size {
        let chunk = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), src, o as i32);
        builder
            .ins()
            .store(MemFlags::trusted(), chunk, dst, o as i32);
        o += 8;
    }
}

/// True for heap-allocated, refcounted value types (str, arrays, and sets —
/// sets share the array heap block). These get tracked for `dec` at scope exit
/// and `inc`'d when handed to a callee or returned.
fn is_heap(t: &Type) -> bool {
    *t == Type::Primitive(Primitive::Str)
        || is_error(t)
        || matches!(t, Type::Array(_) | Type::Set(_))
}

/// The name to show in diagnostics for a (possibly canonicalized) fn
/// name: strip the internal `__builtin_` prefix so errors talk about
/// `print`, not `__builtin_print`.
fn display_name(name: &str) -> &str {
    name.strip_prefix("__builtin_").unwrap_or(name)
}

/// Error for a call/method to an unknown name. If the name is an
/// importable builtin, nudge the user toward the missing import rather
/// than leaving them puzzled.
fn undefined_fn(name: &str, span: Span) -> Error {
    if IMPORTABLE_BUILTINS.contains(&name) {
        Error::at(
            format!(
                "\"{name}\" is a builtin; import it with \"import {{ {name} }} from builtins;\""
            ),
            span.clone(),
        )
    } else {
        Error::at(format!("call to undefined fn {name:?}"), span.clone())
    }
}
