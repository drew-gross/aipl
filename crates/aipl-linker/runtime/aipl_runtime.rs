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

// ---------- Per-builtin call counts (instrumented build only) ----------
//
// `INSN_COUNT` above counts CLIF instructions executed in *AIPL* code, where a
// call into this runtime is one instruction however much work it does. That
// makes the boundary invisible: inlining a runtime fast path raises the count
// while doing strictly less work, and moving a loop into the runtime lowers it
// while doing the same work. These counts are the missing half — how many times
// each builtin actually ran — and they double as a profile of which builtins are
// worth optimizing.
//
// Counted at *entry*, so a builtin that recurses (a `dec` cascading through a
// rope's children) or calls another counts every invocation: the question is how
// often the code runs, not how often AIPL asked for it.
#[cfg(aipl_instrument)]
mod builtin_calls {
    use core::sync::atomic::AtomicU64;

    /// Every counted entry point, NUL-terminated for `fputs` and sorted so the
    /// reported breakdown has a stable order. The `IDX_*` constants below index
    /// this list; both are generated from the `pub extern "C" fn aipl_*` set.
    pub const NAMES: &[&[u8]] = &[
        b"aipl_arr_drop_arr\0",
        b"aipl_arr_drop_opt_arr\0",
        b"aipl_arr_drop_opt_str\0",
        b"aipl_arr_drop_str\0",
        b"aipl_arr_elem_ptr\0",
        b"aipl_arr_extend\0",
        b"aipl_arr_inc\0",
        b"aipl_arr_load_bit\0",
        b"aipl_arr_reserve\0",
        b"aipl_arr_retain_opt\0",
        b"aipl_arr_retain_ptr\0",
        b"aipl_arr_reverse\0",
        b"aipl_arr_slice\0",
        b"aipl_arr_sort\0",
        b"aipl_array_dec\0",
        b"aipl_array_new\0",
        b"aipl_array_push\0",
        b"aipl_array_push_mut\0",
        b"aipl_array_with_cap\0",
        b"aipl_assert\0",
        b"aipl_char_at\0",
        b"aipl_concat\0",
        b"aipl_concat_lazy\0",
        b"aipl_concat_mut\0",
        b"aipl_dec\0",
        b"aipl_dict_contains_key\0",
        b"aipl_dict_get\0",
        b"aipl_dict_insert\0",
        b"aipl_execute_program\0",
        b"aipl_i64_len\0",
        b"aipl_inc\0",
        b"aipl_list_files\0",
        b"aipl_monotonic_now\0",
        b"aipl_now_nanos\0",
        b"aipl_print\0",
        b"aipl_print_error\0",
        b"aipl_read_file_to_string\0",
        b"aipl_rec_alloc\0",
        b"aipl_rec_dec_strong\0",
        b"aipl_rec_dec_weak\0",
        b"aipl_rec_inc_strong\0",
        b"aipl_rec_inc_weak\0",
        b"aipl_set_contains\0",
        b"aipl_set_insert\0",
        b"aipl_set_union\0",
        b"aipl_set_union_mut\0",
        b"aipl_shim_get\0",
        b"aipl_shim_set\0",
        b"aipl_str_alloc\0",
        b"aipl_str_cmp\0",
        b"aipl_str_contains\0",
        b"aipl_str_data\0",
        b"aipl_str_ends_with\0",
        b"aipl_str_eq\0",
        b"aipl_str_hash\0",
        b"aipl_str_is_all_whitespace\0",
        b"aipl_str_iter_init\0",
        b"aipl_str_iter_next\0",
        b"aipl_str_join\0",
        b"aipl_str_len\0",
        b"aipl_str_repeat\0",
        b"aipl_str_reverse\0",
        b"aipl_str_slice\0",
        b"aipl_str_sort\0",
        b"aipl_str_split\0",
        b"aipl_str_starts_with\0",
        b"aipl_str_starts_with_at\0",
        b"aipl_test_begin\0",
        b"aipl_test_end\0",
        b"aipl_test_fail\0",
        b"aipl_test_fail_none\0",
        b"aipl_test_summary\0",
        b"aipl_trim\0",
        b"aipl_trim_mut\0",
        b"aipl_u64_len\0",
        b"aipl_write_bytes\0",
        b"aipl_write_i64\0",
        b"aipl_write_string_to_file\0",
        b"aipl_write_u64\0",
    ];

    pub static COUNTS: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];

    pub const AIPL_ARR_DROP_ARR: usize = 0;
    pub const AIPL_ARR_DROP_OPT_ARR: usize = 1;
    pub const AIPL_ARR_DROP_OPT_STR: usize = 2;
    pub const AIPL_ARR_DROP_STR: usize = 3;
    pub const AIPL_ARR_ELEM_PTR: usize = 4;
    pub const AIPL_ARR_EXTEND: usize = 5;
    pub const AIPL_ARR_INC: usize = 6;
    pub const AIPL_ARR_LOAD_BIT: usize = 7;
    pub const AIPL_ARR_RESERVE: usize = 8;
    pub const AIPL_ARR_RETAIN_OPT: usize = 9;
    pub const AIPL_ARR_RETAIN_PTR: usize = 10;
    pub const AIPL_ARR_REVERSE: usize = 11;
    pub const AIPL_ARR_SLICE: usize = 12;
    pub const AIPL_ARR_SORT: usize = 13;
    pub const AIPL_ARRAY_DEC: usize = 14;
    pub const AIPL_ARRAY_NEW: usize = 15;
    pub const AIPL_ARRAY_PUSH: usize = 16;
    pub const AIPL_ARRAY_PUSH_MUT: usize = 17;
    pub const AIPL_ARRAY_WITH_CAP: usize = 18;
    pub const AIPL_ASSERT: usize = 19;
    pub const AIPL_CHAR_AT: usize = 20;
    pub const AIPL_CONCAT: usize = 21;
    pub const AIPL_CONCAT_LAZY: usize = 22;
    pub const AIPL_CONCAT_MUT: usize = 23;
    pub const AIPL_DEC: usize = 24;
    pub const AIPL_DICT_CONTAINS_KEY: usize = 25;
    pub const AIPL_DICT_GET: usize = 26;
    pub const AIPL_DICT_INSERT: usize = 27;
    pub const AIPL_EXECUTE_PROGRAM: usize = 28;
    pub const AIPL_I64_LEN: usize = 29;
    pub const AIPL_INC: usize = 30;
    pub const AIPL_LIST_FILES: usize = 31;
    pub const AIPL_MONOTONIC_NOW: usize = 32;
    pub const AIPL_NOW_NANOS: usize = 33;
    pub const AIPL_PRINT: usize = 34;
    pub const AIPL_PRINT_ERROR: usize = 35;
    pub const AIPL_READ_FILE_TO_STRING: usize = 36;
    pub const AIPL_REC_ALLOC: usize = 37;
    pub const AIPL_REC_DEC_STRONG: usize = 38;
    pub const AIPL_REC_DEC_WEAK: usize = 39;
    pub const AIPL_REC_INC_STRONG: usize = 40;
    pub const AIPL_REC_INC_WEAK: usize = 41;
    pub const AIPL_SET_CONTAINS: usize = 42;
    pub const AIPL_SET_INSERT: usize = 43;
    pub const AIPL_SET_UNION: usize = 44;
    pub const AIPL_SET_UNION_MUT: usize = 45;
    pub const AIPL_SHIM_GET: usize = 46;
    pub const AIPL_SHIM_SET: usize = 47;
    pub const AIPL_STR_ALLOC: usize = 48;
    pub const AIPL_STR_CMP: usize = 49;
    pub const AIPL_STR_CONTAINS: usize = 50;
    pub const AIPL_STR_DATA: usize = 51;
    pub const AIPL_STR_ENDS_WITH: usize = 52;
    pub const AIPL_STR_EQ: usize = 53;
    pub const AIPL_STR_HASH: usize = 54;
    pub const AIPL_STR_IS_ALL_WHITESPACE: usize = 55;
    pub const AIPL_STR_ITER_INIT: usize = 56;
    pub const AIPL_STR_ITER_NEXT: usize = 57;
    pub const AIPL_STR_JOIN: usize = 58;
    pub const AIPL_STR_LEN: usize = 59;
    pub const AIPL_STR_REPEAT: usize = 60;
    pub const AIPL_STR_REVERSE: usize = 61;
    pub const AIPL_STR_SLICE: usize = 62;
    pub const AIPL_STR_SORT: usize = 63;
    pub const AIPL_STR_SPLIT: usize = 64;
    pub const AIPL_STR_STARTS_WITH: usize = 65;
    pub const AIPL_STR_STARTS_WITH_AT: usize = 66;
    pub const AIPL_TEST_BEGIN: usize = 67;
    pub const AIPL_TEST_END: usize = 68;
    pub const AIPL_TEST_FAIL: usize = 69;
    pub const AIPL_TEST_FAIL_NONE: usize = 70;
    pub const AIPL_TEST_SUMMARY: usize = 71;
    pub const AIPL_TRIM: usize = 72;
    pub const AIPL_TRIM_MUT: usize = 73;
    pub const AIPL_U64_LEN: usize = 74;
    pub const AIPL_WRITE_BYTES: usize = 75;
    pub const AIPL_WRITE_I64: usize = 76;
    pub const AIPL_WRITE_STRING_TO_FILE: usize = 77;
    pub const AIPL_WRITE_U64: usize = 78;
}

// ---------- Per-AIPL-function call counts (instrumented build only) ----------
//
// The other half of the same story as `builtin_calls` above: how many times each
// *compiled AIPL* function was entered. Codegen inserts `aipl_count_call(name)`
// at the head of every function's entry block in the instrumented build, where
// `name` points at a static NUL-terminated copy of that function's object-symbol
// name — so, unlike the builtin list, the set of names is per-program and known
// only to the object being run.
//
// That rules out a fixed table, so this is a small open-addressed one keyed on
// the *pointer*: one name object per function means pointer identity is name
// identity, and probing never has to compare strings. Sized generously against
// the largest thing the corpus compiles (the dogfooded formatter, ~200
// functions). Overflowing it would otherwise undercount invisibly, which is the
// one failure mode worth engineering against: `OVERFLOW` tallies the calls that
// found no slot, and the harness turns any nonzero into a hard failure naming
// `CAP`.
#[cfg(aipl_instrument)]
mod fn_calls {
    use core::sync::atomic::{AtomicU64, AtomicUsize};

    /// Slot count. Power of two (the hash masks rather than divides), and kept
    /// well above the real load factor so probe chains stay short.
    pub const CAP: usize = 2048;

    /// Name pointer per slot, `0` meaning empty. Paired by index with `COUNTS`.
    pub static NAMES: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
    pub static COUNTS: [AtomicU64; CAP] = [const { AtomicU64::new(0) }; CAP];
    /// Calls that found no free slot — always 0 in practice; reported so a
    /// too-small `CAP` shows up as a failure rather than as quietly low counts.
    pub static OVERFLOW: AtomicU64 = AtomicU64::new(0);

    /// Fibonacci hash of a name pointer. The low 3 bits are always 0 (data
    /// objects are aligned), so shift them out before mixing.
    pub fn slot_of(p: usize) -> usize {
        ((p >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize & (CAP - 1)
    }
}

/// Tally one call to the builtin at `$idx`. Compiles to nothing in the default
/// build, like every other counter here.
macro_rules! count_builtin {
    ($idx:expr) => {
        #[cfg(aipl_instrument)]
        builtin_calls::COUNTS[$idx].fetch_add(1, Ordering::Relaxed);
    };
}

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

/// Tally one entry into the compiled AIPL function whose object-symbol name is
/// the static NUL-terminated string at `name`. Codegen emits one call per
/// function, at the head of its entry block, in the instrumented build only —
/// so a function that tail-calls itself counts every re-entry, and a `$tail`
/// participant counts its trampoline separately from its body, matching how the
/// two appear as separate symbols in the object's code breakdown.
///
/// Like `aipl_count_insns`, this is a no-op forwarder in the default build.
#[no_mangle]
pub extern "C" fn aipl_count_call(name: *const c_char) {
    #[cfg(aipl_instrument)]
    {
        // Linear probe from the pointer's home slot: claim the first empty slot
        // (this name's first call) or bump the one already holding it. Bounded
        // by `CAP`, so a full table gives up rather than spinning.
        let key = name as usize;
        let mut i = fn_calls::slot_of(key);
        for _ in 0..fn_calls::CAP {
            let held = fn_calls::NAMES[i].load(Ordering::Relaxed);
            if held == 0 {
                fn_calls::NAMES[i].store(key, Ordering::Relaxed);
            } else if held != key {
                i = (i + 1) & (fn_calls::CAP - 1);
                continue;
            }
            fn_calls::COUNTS[i].fetch_add(1, Ordering::Relaxed);
            return;
        }
        fn_calls::OVERFLOW.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(not(aipl_instrument))]
    let _ = name;
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
    count_builtin!(builtin_calls::AIPL_CONCAT_LAZY);
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
    count_builtin!(builtin_calls::AIPL_INC);
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
    count_builtin!(builtin_calls::AIPL_DEC);
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
    count_builtin!(builtin_calls::AIPL_PRINT);
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
    count_builtin!(builtin_calls::AIPL_PRINT_ERROR);
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
    count_builtin!(builtin_calls::AIPL_CHAR_AT);
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
    count_builtin!(builtin_calls::AIPL_STR_IS_ALL_WHITESPACE);
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
    count_builtin!(builtin_calls::AIPL_READ_FILE_TO_STRING);
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
    count_builtin!(builtin_calls::AIPL_WRITE_STRING_TO_FILE);
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

/// `list_files(dir) -> str[]?`: every file at or below `dir`, walked
/// recursively, as a fresh `str[]` of `dir`-prefixed paths — or null on any
/// failure (unreadable directory, non-UTF-8 name, unknowable entry kind, path
/// too long, unsupported platform), which codegen wraps into `err`. Order is
/// whatever the filesystem yields; callers that need determinism sort. A
/// directory is descended into rather than listed, and a symlink counts as a
/// file (never followed), so the walk can't cycle. POSIX-only for now
/// (`cfg(unix)`). Decrements `dir` per the refcount protocol. Mirrors
/// `aipl_list_files` in the JIT runtime.
#[no_mangle]
pub extern "C" fn aipl_list_files(dir: *const u8) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_LIST_FILES);
    let result = unsafe { list_files_impl(dir) };
    aipl_dec(dir);
    result
}

#[cfg(unix)]
unsafe fn list_files_impl(dir: *const u8) -> *const u8 {
    unsafe { list_unix::list_files(dir) }
}

#[cfg(not(unix))]
unsafe fn list_files_impl(_dir: *const u8) -> *const u8 {
    core::ptr::null()
}

/// The recursive directory walk behind `list_files`, on POSIX `opendir`/
/// `readdir`. Reading `struct dirent` means knowing its layout, which is
/// per-platform (see [`D_TYPE_OFFSET`]) — hence a POSIX-gated module rather
/// than the portable-libc style the rest of the runtime uses.
#[cfg(unix)]
mod list_unix {
    use super::{
        aipl_arr_drop_str, aipl_array_dec, aipl_array_push_mut, aipl_array_with_cap, make_str,
        memcpy, str_bytes, strlen, ArrDropFn,
    };
    use core::ffi::{c_char, c_int, c_void};

    // On x86_64 macOS the plain `opendir`/`readdir` symbols are the legacy
    // 32-bit-inode variants with a different `struct dirent` layout; the C
    // headers redirect to these `$INODE64` ones, so we name them directly.
    // arm64 macOS has only the 64-bit-inode versions, under the bare names.
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    extern "C" {
        #[link_name = "opendir$INODE64"]
        fn opendir(name: *const c_char) -> *mut c_void;
        #[link_name = "readdir$INODE64"]
        fn readdir(dirp: *mut c_void) -> *const u8;
        fn closedir(dirp: *mut c_void) -> c_int;
    }
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    extern "C" {
        fn opendir(name: *const c_char) -> *mut c_void;
        fn readdir(dirp: *mut c_void) -> *const u8;
        fn closedir(dirp: *mut c_void) -> c_int;
    }

    // Byte offsets of `d_type` and `d_name` within `struct dirent`. macOS (with
    // the 64-bit inode layout above): `d_ino: u64, d_seekoff: u64, d_reclen:
    // u16, d_namlen: u16, d_type: u8, d_name`. Linux/BSD 64-bit: `d_ino: u64,
    // d_off: i64, d_reclen: u16, d_type: u8, d_name`.
    #[cfg(target_os = "macos")]
    const D_TYPE_OFFSET: usize = 20;
    #[cfg(target_os = "macos")]
    const D_NAME_OFFSET: usize = 21;
    #[cfg(not(target_os = "macos"))]
    const D_TYPE_OFFSET: usize = 18;
    #[cfg(not(target_os = "macos"))]
    const D_NAME_OFFSET: usize = 19;

    /// `d_type` values: the filesystem doesn't know the kind, and a directory.
    /// Everything else counts as a file (a symlink included — it isn't followed).
    const DT_UNKNOWN: u8 = 0;
    const DT_DIR: u8 = 4;

    /// Longest path the walk will build, NUL included — the usual `PATH_MAX`.
    /// A deeper tree fails the listing rather than truncating a path.
    const PATH_CAP: usize = 4096;

    /// The walk's mutable state: one reusable NUL-terminated path buffer (each
    /// level appends `/name` at its parent's length, so no per-frame copy) and
    /// the `str[]` being accumulated.
    struct Walk {
        path: [u8; PATH_CAP],
        out: *const u8,
    }

    /// Walk `dir` and return the `str[]` of files beneath it, or null on any
    /// failure (a partially built array is released first).
    pub unsafe fn list_files(dir: *const u8) -> *const u8 {
        let mut dbuf = [0u8; 8];
        let bytes = unsafe { str_bytes(dir, &mut dbuf) };
        if bytes.is_empty() || bytes.len() >= PATH_CAP {
            return core::ptr::null();
        }
        let drop_fn = aipl_arr_drop_str as ArrDropFn as usize as i64;
        let mut w = Walk {
            path: [0u8; PATH_CAP],
            out: aipl_array_with_cap(0, drop_fn, 8),
        };
        unsafe {
            memcpy(
                w.path.as_mut_ptr() as *mut c_void,
                bytes.as_ptr() as *const c_void,
                bytes.len(),
            );
            if !walk(&mut w, bytes.len(), drop_fn) {
                aipl_array_dec(w.out);
                return core::ptr::null();
            }
        }
        w.out
    }

    /// Descend into the directory named by the first `len` bytes of `w.path`,
    /// appending every file beneath it to `w.out`. False if the walk couldn't be
    /// completed — the caller then discards the whole listing.
    unsafe fn walk(w: &mut Walk, len: usize, drop_fn: i64) -> bool {
        w.path[len] = 0;
        let d = unsafe { opendir(w.path.as_ptr() as *const c_char) };
        if d.is_null() {
            return false;
        }
        loop {
            let ent = unsafe { readdir(d) };
            if ent.is_null() {
                break; // end of directory
            }
            let name = unsafe { ent.add(D_NAME_OFFSET) };
            if unsafe { is_dot_entry(name) } {
                continue;
            }
            let kind = unsafe { *ent.add(D_TYPE_OFFSET) };
            let nlen = unsafe { strlen(name as *const c_char) };
            // Append `/name` after the parent path (which never gets a second
            // separator if it already ends in one).
            let mut end = len;
            if end == 0 || w.path[end - 1] != b'/' {
                w.path[end] = b'/';
                end += 1;
            }
            // The entry kind decides whether to descend, so an entry the
            // filesystem won't classify fails the listing rather than guessing.
            let ok = if kind == DT_UNKNOWN || end + nlen + 1 > PATH_CAP {
                false
            } else {
                unsafe {
                    memcpy(
                        w.path.as_mut_ptr().add(end) as *mut c_void,
                        name as *const c_void,
                        nlen,
                    );
                    if kind == DT_DIR {
                        walk(w, end + nlen, drop_fn)
                    } else {
                        push_file(w, end + nlen, drop_fn)
                    }
                }
            };
            if !ok {
                unsafe { closedir(d) };
                return false;
            }
        }
        unsafe { closedir(d) };
        true
    }

    /// Push the first `end` bytes of `w.path` onto the listing as a fresh str.
    /// False for a path that isn't UTF-8 (an AIPL `str` the program could only
    /// mangle further downstream).
    unsafe fn push_file(w: &mut Walk, end: usize, drop_fn: i64) -> bool {
        let bytes = unsafe { core::slice::from_raw_parts(w.path.as_ptr(), end) };
        if core::str::from_utf8(bytes).is_err() {
            return false;
        }
        let slot = make_str(bytes) as i64;
        // retain-fn 0: the fresh string's one reference moves into the array.
        w.out = unsafe {
            aipl_array_push_mut(
                w.out,
                &slot as *const i64 as *const u8,
                drop_fn,
                0,
                8,
            )
        };
        true
    }

    /// Whether `name` is `.` or `..` — the two entries a walk always skips.
    unsafe fn is_dot_entry(name: *const u8) -> bool {
        unsafe {
            if *name != b'.' {
                return false;
            }
            let second = *name.add(1);
            second == 0 || (second == b'.' && *name.add(2) == 0)
        }
    }
}

/// `now_nanos() -> u64`: wall-clock nanoseconds since the Unix epoch, carried on
/// the shared `i64` ABI (a `u64` occupies the same 8-byte slot, so the bit
/// pattern is the value). A clock set before the epoch — or an unsupported
/// platform — reads as 0. Takes nothing and owns nothing, so there is no
/// refcount traffic. Mirrors `aipl_now_nanos` in the JIT runtime.
#[no_mangle]
pub extern "C" fn aipl_now_nanos() -> i64 {
    count_builtin!(builtin_calls::AIPL_NOW_NANOS);
    unsafe { now_nanos_impl() }
}

#[cfg(unix)]
unsafe fn now_nanos_impl() -> i64 {
    unsafe { clock_nanos(CLOCK_REALTIME) }
}

#[cfg(not(unix))]
unsafe fn now_nanos_impl() -> i64 {
    0
}

/// `monotonic_now() -> u64`: nanoseconds from the system's monotonic clock,
/// which only counts up — so a difference between two readings is a real
/// elapsed duration. The origin is unspecified (the kernel's, typically boot),
/// so an absolute reading means nothing on its own. Reads as 0 on an
/// unsupported platform. Mirrors `aipl_monotonic_now` in the JIT runtime.
#[no_mangle]
pub extern "C" fn aipl_monotonic_now() -> i64 {
    count_builtin!(builtin_calls::AIPL_MONOTONIC_NOW);
    unsafe { monotonic_now_impl() }
}

#[cfg(unix)]
unsafe fn monotonic_now_impl() -> i64 {
    unsafe { clock_nanos(CLOCK_MONOTONIC) }
}

#[cfg(not(unix))]
unsafe fn monotonic_now_impl() -> i64 {
    0
}

// `struct timespec { tv_sec: time_t, tv_nsec: long }` is two 64-bit words on
// every 64-bit POSIX target, so one layout covers macOS and Linux alike — unlike
// `timeval`, whose `tv_usec` width differs between them. That's why the clocks
// are read through `clock_gettime` rather than `gettimeofday`.
#[cfg(unix)]
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[cfg(unix)]
extern "C" {
    fn clock_gettime(clk_id: c_int, tp: *mut Timespec) -> c_int;
}

/// Wall clock, seconds since the Unix epoch. Same id on every POSIX platform.
#[cfg(unix)]
const CLOCK_REALTIME: c_int = 0;
/// Monotonic clock — the id, unlike `CLOCK_REALTIME`'s, is per-platform.
#[cfg(all(unix, target_os = "macos"))]
const CLOCK_MONOTONIC: c_int = 6;
#[cfg(all(unix, not(target_os = "macos")))]
const CLOCK_MONOTONIC: c_int = 1;

/// Read clock `id` as a nanosecond count. 0 if the clock can't be read, or (for
/// the wall clock) is set before its epoch. The arithmetic wraps, like the JIT's
/// `as u64 as i64`: the count stays a valid *unsigned* nanosecond reading past
/// the point where it stops fitting in an `i64`.
#[cfg(unix)]
unsafe fn clock_nanos(id: c_int) -> i64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { clock_gettime(id, &mut ts) } != 0 || ts.tv_sec < 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64) as i64
}

// ---------- Effect shims ----------
//
// A `shim <effect> { op = f, .. } { body }` installs `f`'s address into the slot
// belonging to `op` for the dynamic extent of `body`. Every shimmable operation
// compiles to "load my slot; call it if non-zero, else call the real runtime
// fn", so a shim reaches every call at any depth without callees knowing about
// it. Save/restore is emitted by codegen, which is what makes shims nest.
// Mirrors the `SHIM_SLOTS` block in the JIT runtime.

/// One slot per shimmable operation, indexed as `aipl_syntax::shim_slot_index`
/// assigns. 0 means "no shim installed" — the state every slot starts in and
/// returns to. Sized by `aipl_syntax::SHIM_SLOT_COUNT`; `shim_slots_match_runtime`
/// in `tests/shims.rs` holds the two in sync.
static SHIM_SLOTS: [core::sync::atomic::AtomicI64; 2] =
    [const { core::sync::atomic::AtomicI64::new(0) }; 2];

/// The shim installed for slot `idx`, or 0 if none. An out-of-range index reads
/// as "no shim" rather than trapping (codegen only ever emits valid indices).
#[no_mangle]
pub extern "C" fn aipl_shim_get(idx: i64) -> i64 {
    count_builtin!(builtin_calls::AIPL_SHIM_GET);
    match SHIM_SLOTS.get(idx as usize) {
        Some(slot) => slot.load(core::sync::atomic::Ordering::Relaxed),
        None => 0,
    }
}

/// Install `ptr` (0 to clear) as the shim for slot `idx`.
#[no_mangle]
pub extern "C" fn aipl_shim_set(idx: i64, ptr: i64) {
    count_builtin!(builtin_calls::AIPL_SHIM_SET);
    if let Some(slot) = SHIM_SLOTS.get(idx as usize) {
        slot.store(ptr, core::sync::atomic::Ordering::Relaxed);
    }
}

/// `execute_program(program, args) -> ExecResult!Error` (sret ABI, mirroring
/// the JIT runtime): writes `Result<ExecResult, Error>` directly into `out`
/// (tag 1 = ok, 0 = err, matching the `ok`/`err` expression codegen; the
/// `ExecResult` fields go at their declared struct offsets 0/8/16 —
/// `stdout`/`stderr`/`exit_code`, must stay in sync with
/// `__builtin_ExecResult` in `BUILTIN_SIGNATURES`). Spawns `program` with
/// `args` and waits for it; POSIX-only for now (`cfg(unix)`). `ok(ExecResult)`
/// for any run that was actually launched, whatever it then exited with;
/// `err(message)` only if it couldn't be launched at all (bad UTF-8, not
/// found, embedded NUL in captured output, unsupported platform). Decrements
/// `program` and `args` per the refcount protocol.
#[no_mangle]
pub extern "C" fn aipl_execute_program(out: *mut i64, program: *const u8, args: *const u8) {
    count_builtin!(builtin_calls::AIPL_EXECUTE_PROGRAM);
    let a = aipl_arr_ensure_heap(args);
    unsafe { execute_program_impl(out, program, a) };
    aipl_dec(program);
    aipl_array_dec(a);
}

fn write_err(out: *mut i64, message: &[u8]) {
    let ptr = make_str(message);
    unsafe {
        *out = 0;
        *out.add(1) = ptr as i64;
    }
}

/// `stdout_ptr`/`stderr_ptr` are already-constructed `str` values (owned).
fn write_ok(out: *mut i64, stdout_ptr: *const u8, stderr_ptr: *const u8, exit_code: i64) {
    unsafe {
        *out = 1;
        *out.add(1) = stdout_ptr as i64;
        *out.add(2) = stderr_ptr as i64;
        *out.add(3) = exit_code;
    }
}

#[cfg(not(unix))]
unsafe fn execute_program_impl(out: *mut i64, _program: *const u8, _args: *const u8) {
    // Process spawning isn't implemented on this platform yet.
    write_err(out, b"could not execute program");
}

#[cfg(unix)]
unsafe fn execute_program_impl(out: *mut i64, program: *const u8, args: *const u8) {
    unsafe {
        if program.is_null() || args.is_null() {
            write_err(out, b"could not execute program");
            return;
        }
        exec_unix::run(out, program, args);
    }
}

// ---------- POSIX process spawning (`execute_program`) ----------
//
// Redirects the child's stdout/stderr to unique temp files (rather than pipes)
// so the parent can't deadlock waiting on one stream while the child blocks
// writing to a full buffer on the other — it just reads each file back after
// `waitpid`, reusing the same fopen/fread path as `read_file_to_string`.
#[cfg(unix)]
mod exec_unix {
    use super::{
        aipl_array_dec, aipl_dec, array_len, fail_msg, fclose, fmt_i64, fopen, fread, fseek,
        ftell, make_str, rt_alloc, rt_free, str_bytes, write_ok, ARR_ELEMS_OFFSET, SEEK_END,
        SEEK_SET,
    };
    use core::ffi::{c_char, c_int, c_void};
    use core::sync::atomic::{AtomicU64, Ordering};

    extern "C" {
        fn fork() -> c_int;
        fn execvp(path: *const c_char, argv: *const *const c_char) -> c_int;
        fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
        fn _exit(code: c_int) -> !;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn fileno(stream: *mut c_void) -> c_int;
        fn remove(path: *const c_char) -> c_int;
        fn getpid() -> c_int;
        fn pipe(fds: *mut c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    }

    // POSIX-standard values (`<fcntl.h>`), identical on Linux and macOS.
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh, `rt_alloc`'d, NUL-terminated copy of a str value's bytes (any
    /// representation), for building a C `argv` entry. Freed by the caller.
    unsafe fn dup_cstr(v: *const u8) -> *mut c_char {
        unsafe {
            let mut b = [0u8; 8];
            let bytes = str_bytes(v, &mut b);
            let n = bytes.len();
            let buf = rt_alloc(n + 1) as *mut u8;
            if buf.is_null() {
                super::abort();
            }
            if n > 0 {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
            }
            *buf.add(n) = 0;
            buf as *mut c_char
        }
    }

    /// Build a unique temp-file path `/tmp/aipl_exec_<pid>_<counter>_<suffix>`
    /// into `buf`, NUL-terminated, returning a `c_char` pointer into it.
    unsafe fn temp_path(buf: &mut [u8; 96], suffix: &[u8]) -> *const c_char {
        let mut digits = [0u8; 20];
        let mut pos = 0usize;
        let prefix = b"/tmp/aipl_exec_";
        buf[..prefix.len()].copy_from_slice(prefix);
        pos += prefix.len();
        let start = fmt_i64(&mut digits, unsafe { getpid() } as i64);
        buf[pos..pos + (20 - start)].copy_from_slice(&digits[start..]);
        pos += 20 - start;
        buf[pos] = b'_';
        pos += 1;
        let n = COUNTER.fetch_add(1, Ordering::Relaxed) as i64;
        let start = fmt_i64(&mut digits, n);
        buf[pos..pos + (20 - start)].copy_from_slice(&digits[start..]);
        pos += 20 - start;
        buf[pos] = b'_';
        pos += 1;
        buf[pos..pos + suffix.len()].copy_from_slice(suffix);
        pos += suffix.len();
        buf[pos] = 0;
        buf.as_ptr() as *const c_char
    }

    /// Read a temp file's full contents back as an owned `str`, or `None` on
    /// any failure — including an embedded NUL, which a NUL-terminated str
    /// can't hold (same constraint as `read_file_to_string`).
    unsafe fn read_temp_file(path: *const c_char) -> Option<*const u8> {
        unsafe {
            let f = fopen(path, b"rb\0".as_ptr() as *const c_char);
            if f.is_null() {
                return None;
            }
            if fseek(f, 0, SEEK_END) != 0 {
                fclose(f);
                return None;
            }
            let size = ftell(f);
            if size < 0 || fseek(f, 0, SEEK_SET) != 0 {
                fclose(f);
                return None;
            }
            let size = size as usize;
            if size == 0 {
                fclose(f);
                return Some(make_str(&[]));
            }
            let buf = rt_alloc(size) as *mut u8;
            if buf.is_null() {
                fclose(f);
                super::abort();
            }
            let n = fread(buf as *mut c_void, 1, size, f);
            fclose(f);
            let result = if n != size || core::slice::from_raw_parts(buf, size).contains(&0) {
                None
            } else {
                Some(make_str(core::slice::from_raw_parts(buf, size)))
            };
            rt_free(buf as *mut c_void);
            result
        }
    }

    /// Free `argv[0..count]` (each a `dup_cstr`'d buffer) and the `argv` array
    /// itself.
    unsafe fn free_argv(argv: *mut *mut c_char, count: usize) {
        unsafe {
            for i in 0..count {
                rt_free(*argv.add(i) as *mut c_void);
            }
            rt_free(argv as *mut c_void);
        }
    }

    /// `program`/`a` (a heap-representation array — the caller already
    /// materialized any reversed view) are each consumed exactly once, on
    /// every path.
    pub unsafe fn run(out: *mut i64, program: *const u8, a: *const u8) {
        unsafe {
            let len = array_len(a);
            let elems = a.add(ARR_ELEMS_OFFSET) as *const i64;

            // argv: program, each arg, then a NULL terminator.
            let argv =
                rt_alloc((len + 2) * core::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
            if argv.is_null() {
                super::abort();
            }
            *argv = dup_cstr(program);
            aipl_dec(program);
            for i in 0..len {
                let ep = core::ptr::read(elems.add(i)) as *const u8;
                *argv.add(1 + i) = dup_cstr(ep);
            }
            *argv.add(1 + len) = core::ptr::null_mut();
            aipl_array_dec(a);

            let mut outbuf = [0u8; 96];
            let mut errbuf = [0u8; 96];
            let out_path = temp_path(&mut outbuf, b"out");
            let err_path = temp_path(&mut errbuf, b"err");
            let out_f = fopen(out_path, b"wb\0".as_ptr() as *const c_char);
            let err_f = fopen(err_path, b"wb\0".as_ptr() as *const c_char);
            if out_f.is_null() || err_f.is_null() {
                if !out_f.is_null() {
                    fclose(out_f);
                    remove(out_path);
                }
                if !err_f.is_null() {
                    fclose(err_f);
                    remove(err_path);
                }
                free_argv(argv, 1 + len);
                return fail_msg(out);
            }
            let out_fd = fileno(out_f);
            let err_fd = fileno(err_f);

            // Self-pipe exec-error trick: the write end is marked close-on-exec,
            // so a *successful* `execvp` closes it for free (the exec'd image
            // never touches it) and the parent's `read` sees EOF; a *failed*
            // `execvp` leaves the child running our code, which writes one byte
            // before exiting. This is the only reliable way to tell "the child
            // ran and exited 127 on its own" from "execvp itself never ran the
            // program" — a real child could legitimately exit 127 too.
            let mut errpipe = [0 as c_int; 2];
            if pipe(errpipe.as_mut_ptr()) != 0 {
                fclose(out_f);
                fclose(err_f);
                remove(out_path);
                remove(err_path);
                free_argv(argv, 1 + len);
                return fail_msg(out);
            }
            let (err_r, err_w) = (errpipe[0], errpipe[1]);
            fcntl(err_w, F_SETFD, FD_CLOEXEC);

            let pid = fork();
            if pid < 0 {
                close(err_r);
                close(err_w);
                fclose(out_f);
                fclose(err_f);
                remove(out_path);
                remove(err_path);
                free_argv(argv, 1 + len);
                return fail_msg(out);
            }
            if pid == 0 {
                // Child: redirect stdout/stderr to the temp files, then exec.
                close(err_r);
                dup2(out_fd, 1);
                dup2(err_fd, 2);
                fclose(out_f);
                fclose(err_f);
                execvp(*argv, argv as *const *const c_char);
                // execvp failed: signal it via the pipe, then exit.
                let byte = 1u8;
                write(err_w, &byte as *const u8 as *const c_void, 1);
                _exit(127);
            }
            // Parent.
            close(err_w);
            fclose(out_f);
            fclose(err_f);
            free_argv(argv, 1 + len);

            let mut sentinel = 0u8;
            let exec_failed = read(err_r, &mut sentinel as *mut u8 as *mut c_void, 1) > 0;
            close(err_r);

            let mut status: c_int = 0;
            waitpid(pid, &mut status, 0);
            // Traditional System V wait-status encoding (shared by Linux and
            // macOS): low 7 bits 0 = exited normally, high byte = exit code;
            // otherwise the child was signal-terminated.
            let exit_code: i64 = if status & 0x7f == 0 {
                ((status >> 8) & 0xff) as i64
            } else {
                -1
            };

            if exec_failed {
                remove(out_path);
                remove(err_path);
                return fail_msg(out);
            }

            let stdout_ptr = read_temp_file(out_path);
            let stderr_ptr = read_temp_file(err_path);
            remove(out_path);
            remove(err_path);
            match (stdout_ptr, stderr_ptr) {
                (Some(so), Some(se)) => write_ok(out, so, se, exit_code),
                _ => fail_msg(out),
            }
        }
    }
}

fn fail_msg(out: *mut i64) {
    write_err(out, b"could not execute program");
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
    count_builtin!(builtin_calls::AIPL_STR_ALLOC);
    let len = if len < 0 { 0 } else { len as usize };
    unsafe { rt_str_buf(len).add(STR_HEADER_SIZE) }
}

/// Decimal byte length of `n` (matching `aipl_write_i64`).
#[no_mangle]
pub extern "C" fn aipl_i64_len(n: i64) -> i64 {
    count_builtin!(builtin_calls::AIPL_I64_LEN);
    let mut buf = [0u8; 20];
    (20 - fmt_i64(&mut buf, n)) as i64
}

/// Write `n`'s decimal representation at `dst`; return the advanced cursor.
#[no_mangle]
pub extern "C" fn aipl_write_i64(dst: *const u8, n: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_WRITE_I64);
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
    count_builtin!(builtin_calls::AIPL_U64_LEN);
    let mut buf = [0u8; 20];
    (20 - fmt_u64(&mut buf, n as u64)) as i64
}

/// Write `n` (interpreted as unsigned) in decimal at `dst`; return the cursor.
#[no_mangle]
pub extern "C" fn aipl_write_u64(dst: *const u8, n: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_WRITE_U64);
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
    count_builtin!(builtin_calls::AIPL_STR_LEN);
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
    count_builtin!(builtin_calls::AIPL_WRITE_BYTES);
    let n = if n < 0 { 0 } else { n as usize };
    unsafe {
        memcpy(dst as *mut c_void, src as *const c_void, n);
        dst.add(n)
    }
}

#[no_mangle]
pub extern "C" fn aipl_concat(a: *const u8, b: *const u8) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_CONCAT);
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
    count_builtin!(builtin_calls::AIPL_TRIM);
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
    count_builtin!(builtin_calls::AIPL_STR_REVERSE);
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
/// Consumes `s` (callers pre-inc).
///
/// `n` is unsigned at the language level but arrives in the same 64-bit register,
/// so it is read here as `i64`: the `n <= 0` guard yields `""` for zero and —
/// reading as negative — for any `u64` past `i64::MAX`, which could never be
/// allocated anyway.
#[no_mangle]
pub extern "C" fn aipl_str_repeat(s: *const u8, n: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_STR_REPEAT);
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

/// How [`aipl_arr_sort`] orders its elements. Every `ord` element type is an
/// 8-byte scalar, so the only question is how to read the word: as a signed
/// integer, as an unsigned one (`u8`..`u64`, and `char` — a byte value), or as a
/// `str` pointer to compare lexicographically by bytes.
const SORT_KIND_STR: i64 = 2;
const SORT_KIND_UNSIGNED: i64 = 1;

/// Order `words` in place. Mirrors the JIT runtime's `sort_words` byte-for-byte,
/// which is why it uses `sort_unstable_by` — the stable sorts need an allocation,
/// and this runtime is `#![no_std]`. Unstable is enough: `ord` elements are
/// compared by value, so equal ones are indistinguishable.
fn sort_words(words: &mut [i64], kind: i64) {
    match kind {
        SORT_KIND_STR => words.sort_unstable_by(|x, y| {
            let mut xb = [0u8; 8];
            let mut yb = [0u8; 8];
            let (xs, ys) = unsafe {
                (
                    str_bytes(*x as *const u8, &mut xb),
                    str_bytes(*y as *const u8, &mut yb),
                )
            };
            xs.cmp(ys)
        }),
        SORT_KIND_UNSIGNED => words.sort_unstable_by(|x, y| (*x as u64).cmp(&(*y as u64))),
        _ => words.sort_unstable(),
    }
}

/// `xs.sort() -> T[]` — a fresh array with the same elements in ascending order.
/// Consumes `a` (callers pre-inc) and co-owns the elements it copies. Unlike
/// `aipl_arr_reverse` this cannot be a lazy view: the order is not a function of
/// the index. Mirrors the JIT runtime's `aipl_arr_sort`.
#[no_mangle]
pub extern "C" fn aipl_arr_sort(
    a: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    kind: i64,
) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARR_SORT);
    let a = aipl_arr_ensure_heap(a);
    if a.is_null() {
        return a;
    }
    unsafe {
        let len = array_len(a);
        let raw = array_alloc(len, len, drop_fn, elem_size) as *const u8;
        if len > 0 {
            let es = elem_size.max(8) as usize;
            let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            let src = a.add(ARR_ELEMS_OFFSET);
            memcpy(dst as *mut c_void, src as *const c_void, len * es);
            // The new array co-owns every element. Done before the sort because
            // reordering words neither creates nor destroys a reference.
            elem_rc(retain_fn, dst, len);
            let words = core::slice::from_raw_parts_mut(dst as *mut i64, len);
            sort_words(words, kind);
        }
        aipl_array_dec(a);
        raw
    }
}

/// `s.sort() -> str` — a fresh `str` with the bytes in ascending order. The
/// str-shaped counterpart of `aipl_arr_sort`, for a `str` or `char[]` receiver
/// (the two share a representation, so neither is a real array block).
/// Consumes `s` (callers pre-inc). Mirrors the JIT runtime's `aipl_str_sort`.
#[no_mangle]
pub extern "C" fn aipl_str_sort(s: *const u8) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_STR_SORT);
    unsafe {
        let mut sbuf = [0u8; 8];
        let bytes = str_bytes(s, &mut sbuf);
        let n = bytes.len();
        // Copy into a buffer, sort it there, then build the result — the same
        // stack-for-short / heap-for-long split `aipl_str_reverse` uses.
        let result = if n == 0 {
            make_str(&[])
        } else if n <= 128 {
            let mut tmp = [0u8; 128];
            tmp[..n].copy_from_slice(bytes);
            tmp[..n].sort_unstable();
            make_str(&tmp[..n])
        } else {
            let raw = rt_str_buf(n);
            let dst = raw.add(STR_HEADER_SIZE) as *mut u8;
            memcpy(dst as *mut c_void, bytes.as_ptr() as *const c_void, n);
            core::slice::from_raw_parts_mut(dst, n).sort_unstable();
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
    count_builtin!(builtin_calls::AIPL_ARR_REVERSE);
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
    count_builtin!(builtin_calls::AIPL_STR_SLICE);
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

/// `xs[start..end]` — array slice. Both bounds are clamped to `[0, len]` (an
/// out-of-range end yields a shorter array; `start >= end` yields `[]`).
/// *Borrows* `xs` (does not drop it) and returns a fresh heap array holding
/// copies of the elements in `[start, end)`, each retained via `retain_fn`
/// (0 for scalar elements). Mirrors the JIT runtime's `aipl_arr_slice`.
#[no_mangle]
pub extern "C" fn aipl_arr_slice(
    a: *const u8,
    start: i64,
    end: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARR_SLICE);
    if a.is_null() {
        return a;
    }
    unsafe {
        let len = array_len(a) as i64;
        let lo = start.clamp(0, len) as usize;
        let hi = end.clamp(0, len) as usize;
        let n = hi.saturating_sub(lo);
        if elem_size == ELEM_BITPACKED {
            let raw = array_alloc(n, n, drop_fn, ELEM_BITPACKED) as *const u8;
            let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
            for i in 0..n {
                write_packed_bit(dst, i, arr_load_bit_rt(a, lo + i));
            }
            return raw;
        }
        let es = elem_size.max(8) as usize;
        let raw = array_alloc(n, n, drop_fn, elem_size) as *const u8;
        let dst_base = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
        for i in 0..n {
            let src = arr_elem_ptr_rt(a, lo + i, es);
            memcpy(dst_base.add(i * es) as *mut c_void, src as *const c_void, es);
        }
        elem_rc(retain_fn, dst_base, n);
        raw
    }
}

/// `split(self, sep) -> str[]` — parts of `self` between non-overlapping
/// occurrences of `sep`, each a slice of `self` (a buffer-sharing view for a long
/// part, else an inline/heap copy). An empty `sep` yields one part: the whole
/// string. Consumes both `self` and `sep`; the view parts hold their own refs on
/// `self`'s buffer. Mirrors the JIT runtime's `aipl_str_split`.
#[no_mangle]
pub extern "C" fn aipl_str_split(s: *const u8, sep: *const u8) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_STR_SPLIT);
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
    count_builtin!(builtin_calls::AIPL_STR_JOIN);
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
    count_builtin!(builtin_calls::AIPL_STR_DATA);
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
    count_builtin!(builtin_calls::AIPL_STR_ITER_INIT);
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
    count_builtin!(builtin_calls::AIPL_STR_ITER_NEXT);
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
    count_builtin!(builtin_calls::AIPL_TRIM_MUT);
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
    count_builtin!(builtin_calls::AIPL_CONCAT_MUT);
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
    count_builtin!(builtin_calls::AIPL_ARRAY_NEW);
    let len = if len < 0 { 0 } else { len as usize };
    // A fresh literal is allocated to exactly its length (cap == len).
    unsafe { array_alloc(len, len, drop_fn, elem_size) }
}

/// Allocate an empty array (len 0, refcount 1) reserved to `cap` slots of
/// `elem_size` bytes with the given element `drop_fn`. Used by `map`/`filter` to
/// pre-size their output.
#[no_mangle]
pub extern "C" fn aipl_array_with_cap(cap: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARRAY_WITH_CAP);
    let cap = if cap < 0 { 0 } else { cap as usize };
    unsafe { array_alloc(0, cap, drop_fn, elem_size) }
}

#[no_mangle]
pub extern "C" fn aipl_array_dec(ptr: *const u8) {
    count_builtin!(builtin_calls::AIPL_ARRAY_DEC);
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
    count_builtin!(builtin_calls::AIPL_ARR_INC);
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

// ---------- Recursive (boxed) type runtime ----------
//
// A value of a *recursive* struct/variant type is heap-allocated behind a
// pointer; the value points at the *payload* (the type's usual inline layout)
// and a four-word header sits just below it:
//
//   [strong: i64][weak: i64][drop_fn: ptr][payload size: i64][payload...]
//                                                             ^ value
//
// `strong` counts external references (locals, array elements, fields of
// non-recursive types); `weak` counts internal ones (a field of one boxed
// value of the recursion group pointing at another of the same group). A block
// is freed when both reach zero, releasing its payload via the stored
// `drop_fn` (a generated per-type function: `aipl_rec_dec_weak` for
// same-group children, normal drops for everything else) — so death cascades
// through contained values that become unreachable in turn. The cascade is
// iterative: dead blocks are pushed onto a pending list (the finished
// `strong` word doubles as the intrusive next link) drained only by the
// outermost release call, so a long list never needs deep native recursion.
// Mirrors the JIT runtime in `aipl-codegen`; see it for the full description.

const REC_HEADER_SIZE: usize = 32;
const REC_WEAK_WORD: usize = 1;
const REC_DROPFN_WORD: usize = 2;
const REC_SIZE_WORD: usize = 3;
type RecDropFn = extern "C" fn(*const u8);

/// The header words of the block behind payload pointer `p` (word 0 = strong).
fn rec_block(p: *const u8) -> *mut i64 {
    unsafe { p.sub(REC_HEADER_SIZE) as *mut i64 }
}

/// Allocate a boxed recursive-type value with a `size`-byte payload (strong 1,
/// weak 0), returning the payload pointer. Codegen stores the tag/fields
/// immediately after.
#[no_mangle]
pub extern "C" fn aipl_rec_alloc(size: i64, drop_fn: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_REC_ALLOC);
    let size = if size < 0 { 0 } else { size as usize };
    unsafe {
        let raw = rt_alloc(REC_HEADER_SIZE + size) as *mut u8;
        if raw.is_null() {
            abort();
        }
        let b = raw as *mut i64;
        *b = 1; // strong
        *b.add(REC_WEAK_WORD) = 0;
        *b.add(REC_DROPFN_WORD) = drop_fn;
        *b.add(REC_SIZE_WORD) = size as i64;
        raw.add(REC_HEADER_SIZE)
    }
}

#[no_mangle]
pub extern "C" fn aipl_rec_inc_strong(p: *const u8) {
    count_builtin!(builtin_calls::AIPL_REC_INC_STRONG);
    unsafe { *rec_block(p) += 1 }
}

#[no_mangle]
pub extern "C" fn aipl_rec_inc_weak(p: *const u8) {
    count_builtin!(builtin_calls::AIPL_REC_INC_WEAK);
    unsafe { *rec_block(p).add(REC_WEAK_WORD) += 1 }
}

#[no_mangle]
pub extern "C" fn aipl_rec_dec_strong(p: *const u8) {
    count_builtin!(builtin_calls::AIPL_REC_DEC_STRONG);
    let b = rec_block(p);
    unsafe {
        *b -= 1;
        if *b == 0 && *b.add(REC_WEAK_WORD) == 0 {
            rec_release(b);
        }
    }
}

#[no_mangle]
pub extern "C" fn aipl_rec_dec_weak(p: *const u8) {
    count_builtin!(builtin_calls::AIPL_REC_DEC_WEAK);
    let b = rec_block(p);
    unsafe {
        *b.add(REC_WEAK_WORD) -= 1;
        if *b == 0 && *b.add(REC_WEAK_WORD) == 0 {
            rec_release(b);
        }
    }
}

// Dead boxed blocks awaiting drop+free (intrusive list through the `strong`
// word), and whether a drain loop is already running below us on the native
// stack. The AOT runtime is single-threaded, so plain statics are fine (the
// JIT runtime, whose host process is multi-threaded, uses thread-locals).
static mut REC_PENDING: *mut i64 = core::ptr::null_mut();
static mut REC_DRAINING: bool = false;

/// Queue the dead block `b` (both counts zero) and, unless a drain is already
/// running further down the stack, drain the queue: call each block's
/// `drop_fn` on its payload (which may queue more dead blocks — that's the
/// cascade) and free it.
fn rec_release(b: *mut i64) {
    unsafe {
        *b = REC_PENDING as i64;
        REC_PENDING = b;
        if REC_DRAINING {
            return;
        }
        REC_DRAINING = true;
        while !REC_PENDING.is_null() {
            let head = REC_PENDING;
            REC_PENDING = *head as *mut i64;
            let drop_fn = *head.add(REC_DROPFN_WORD);
            if drop_fn != 0 {
                let f: RecDropFn = core::mem::transmute(drop_fn);
                f((head as *const u8).add(REC_HEADER_SIZE));
            }
            rt_free(head as *mut c_void);
        }
        REC_DRAINING = false;
    }
}

/// Copy-and-grow push (value semantics): returns a fresh array holding `a`'s
/// elements followed by the `elem_size`-byte element at `x`, then drops `a`.
/// Repr-aware element pointer for use from AOT-compiled code.
/// Returns a pointer to element `idx`, handling reversed views via recursion.
/// `elem_size` must be > 0 (not bit-packed — use `aipl_arr_load_bit` for bools).
#[no_mangle]
pub extern "C" fn aipl_arr_elem_ptr(a: *const u8, idx: i64, elem_size: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARR_ELEM_PTR);
    unsafe { arr_elem_ptr_rt(a, idx as usize, elem_size as usize) }
}

/// Repr-aware bit load for AOT-compiled code. Returns 0 or 1 as i64.
#[no_mangle]
pub extern "C" fn aipl_arr_load_bit(a: *const u8, idx: i64) -> i64 {
    count_builtin!(builtin_calls::AIPL_ARR_LOAD_BIT);
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
    count_builtin!(builtin_calls::AIPL_ARRAY_PUSH);
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
    count_builtin!(builtin_calls::AIPL_ARRAY_PUSH_MUT);
    let a = aipl_arr_ensure_heap(a);
    // A `STATIC_REFCOUNT` block is not ours to grow or write into, however
    // unaliased codegen proved the binding to be: it has no owner to answer to
    // and may live in read-only data. Copy instead, exactly as `aipl_concat_mut`
    // does for a static string literal. Mirrors the JIT runtime's guard, which
    // is what keeps a host-lent FFI argument array off this path.
    if !a.is_null() && unsafe { *header_of(a) } == STATIC_REFCOUNT {
        return aipl_array_push(a, x, drop_fn, retain_fn, elem_size);
    }
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

/// Lexicographic byte comparison of two `str`s: negative, zero, or positive
/// like `memcmp`. Backs the ordering operators (`<`, `<=`, `>`, `>=`) on `str`,
/// using the same byte order `sort` already imposes on a `str[]`. Unlike
/// `aipl_str_eq` this only *borrows* both inputs (no dec), so codegen emits no
/// compensating pre-inc. Mirrors the JIT `aipl_str_cmp`.
#[no_mangle]
pub extern "C" fn aipl_str_cmp(a: *const u8, b: *const u8) -> i64 {
    count_builtin!(builtin_calls::AIPL_STR_CMP);
    let mut ab = [0u8; 8];
    let mut bb = [0u8; 8];
    let (x, y) = unsafe { (str_bytes(a, &mut ab), str_bytes(b, &mut bb)) };
    let n = if x.len() < y.len() { x.len() } else { y.len() };
    let mut i = 0;
    while i < n {
        if x[i] != y[i] {
            return if x[i] < y[i] { -1 } else { 1 };
        }
        i += 1;
    }
    if x.len() == y.len() {
        0
    } else if x.len() < y.len() {
        -1
    } else {
        1
    }
}

/// `str == str`: content-compare, then decrement both inputs (consumes a ref
/// from each; callers pre-inc). Returns 1/0. Mirrors the JIT `aipl_str_eq`.
#[no_mangle]
pub extern "C" fn aipl_str_eq(a: *const u8, b: *const u8) -> i64 {
    count_builtin!(builtin_calls::AIPL_STR_EQ);
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
    count_builtin!(builtin_calls::AIPL_STR_STARTS_WITH);
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

/// `s.starts_with_at(prefix, at) -> bool` (1/0): whether `s`'s bytes from `at`
/// onward begin with `prefix`'s — `s[at..].starts_with(prefix)` without building
/// the slice. `at` clamps to `[0, len]` exactly as a slice bound does, so a start
/// past the end leaves nothing to match but the empty prefix. Consumes (decs)
/// both inputs; callers pre-inc. Mirrors the JIT `aipl_str_starts_with_at`.
#[no_mangle]
pub extern "C" fn aipl_str_starts_with_at(s: *const u8, prefix: *const u8, at: i64) -> i64 {
    count_builtin!(builtin_calls::AIPL_STR_STARTS_WITH_AT);
    let sl = aipl_str_len(s);
    let pl = aipl_str_len(prefix) as usize;
    let lo = if at < 0 {
        0
    } else if at > sl {
        sl as usize
    } else {
        at as usize
    };
    let starts = if (sl as usize) - lo < pl {
        false
    } else {
        // Stream `s`, skipping the chunks that end before `lo` and comparing from
        // there until the prefix is matched (or mismatches); `prefix` (usually a
        // short literal) is read contiguously.
        let mut pb = [0u8; 8];
        let pbytes = unsafe { str_bytes(prefix, &mut pb) };
        let mut pos = 0usize; // bytes of `s` behind us
        let mut off = 0usize; // bytes of `prefix` matched so far
        let mut ok = true;
        str_for_each_chunk(s, &mut |chunk| {
            if off >= pl {
                return false; // whole prefix matched — stop scanning `s`
            }
            let start = pos;
            pos += chunk.len();
            if pos <= lo {
                return true; // wholly before the offset — nothing to compare yet
            }
            let from = lo.saturating_sub(start);
            let take = core::cmp::min(chunk.len() - from, pl - off);
            if chunk[from..from + take] != pbytes[off..off + take] {
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
    count_builtin!(builtin_calls::AIPL_STR_ENDS_WITH);
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
/// contiguously anywhere in `s`'s. Consumes (decs) both inputs; callers
/// pre-inc. The empty needle always matches. Reads both strings via
/// `str_bytes` (a rope receiver materializes its memoized cache). Mirrors the
/// JIT `aipl_str_contains`.
#[no_mangle]
pub extern "C" fn aipl_str_contains(s: *const u8, needle: *const u8) -> i64 {
    count_builtin!(builtin_calls::AIPL_STR_CONTAINS);
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

/// FNV-1a content hash of a NUL-terminated string (consistent with `rt_str_eq`).
/// Borrows `a` (no refcount change). Mirrors the JIT `aipl_str_hash`.
#[no_mangle]
pub extern "C" fn aipl_str_hash(a: *const u8) -> i64 {
    count_builtin!(builtin_calls::AIPL_STR_HASH);
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
    count_builtin!(builtin_calls::AIPL_SET_CONTAINS);
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
    count_builtin!(builtin_calls::AIPL_SET_INSERT);
    if aipl_set_contains(a, x, elem_size, str_cmp) != 0 {
        return a;
    }
    aipl_array_push_mut(a, x, drop_fn, retain_fn, elem_size)
}

/// Make `a` uniquely owned with room for `extra` more elements, so the appends
/// an array-literal spread emits afterwards are plain in-place writes. Two
/// cases, mirroring `aipl_set_union_mut` vs `aipl_set_union`:
///
/// - refcount 1 (nothing else can observe the block): keep it. Already-large
///   enough capacity is returned untouched; otherwise `realloc` to exactly what
///   is needed — the header and elements move with the block, so no element is
///   re-retained and there is no old block to free.
/// - shared: allocate one right-sized block, copy the elements in and retain
///   each, then release the input.
///
/// Either way the result has refcount 1 and capacity for `old_len + extra`.
/// Sizing is exact rather than doubling: the spread knows its final length up
/// front. Mirrors `aipl_arr_reserve` in codegen.
#[no_mangle]
pub extern "C" fn aipl_arr_reserve(
    a: *const u8,
    extra: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARR_RESERVE);
    unsafe {
        let a = aipl_arr_ensure_heap(a);
        let elem_size = core::cmp::max(elem_size, 8) as usize;
        let old_len = if a.is_null() { 0 } else { array_len(a) };
        let want = old_len + if extra > 0 { extra as usize } else { 0 };
        let need_bytes = want * elem_size;
        if !a.is_null() {
            let cap_bytes = array_cap_bytes(a);
            let rc = *header_of(a);
            if rc == 1 {
                if need_bytes <= cap_bytes {
                    *(a.add(ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
                    return a;
                }
                let block = a.sub(HEADER_SIZE) as *mut c_void;
                let raw = rt_realloc(block, HEADER_SIZE + ARR_ELEMS_OFFSET + need_bytes) as *mut u8;
                if raw.is_null() {
                    abort();
                }
                let data = raw.add(HEADER_SIZE);
                *(data.add(ARR_CAP_OFFSET) as *mut i64) = need_bytes as i64;
                *(data.add(ARR_DROPFN_OFFSET) as *mut i64) = drop_fn;
                return data as *const u8;
            }
        }
        let raw = array_alloc(
            old_len,
            core::cmp::max(want, 1),
            drop_fn,
            elem_size as i64,
        );
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
        aipl_array_dec(a);
        raw as *const u8
    }
}

/// Append every element of `src` to `dst`, which `aipl_arr_reserve` has already
/// made uniquely owned and large enough — so this is a single `memcpy` plus one
/// retain pass, never a reallocation. Consumes (decs) `src`; `dst` keeps its
/// identity. Mirrors `aipl_arr_extend` in codegen.
#[no_mangle]
pub extern "C" fn aipl_arr_extend(
    dst: *const u8,
    src: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARR_EXTEND);
    unsafe {
        let src_heap = aipl_arr_ensure_heap(src);
        if elem_size == ELEM_BITPACKED {
            // Bit-packed `bool[]` opts out of the pre-sized path (`reserve` is a
            // no-op for it), so `dst` may be shared: append one bit at a time
            // with the *copying* push, which is correct either way.
            let add = if src_heap.is_null() {
                0
            } else {
                array_len(src_heap)
            };
            let mut out = dst;
            let mut i = 0;
            while i < add {
                let bit = i64::from(arr_load_bit_rt(src_heap, i));
                out = aipl_array_push(
                    out,
                    &bit as *const i64 as *const u8,
                    drop_fn,
                    retain_fn,
                    ELEM_BITPACKED,
                );
                i += 1;
            }
            aipl_array_dec(src_heap);
            return out;
        }
        let elem_size = core::cmp::max(elem_size, 8) as usize;
        let add = if src_heap.is_null() {
            0
        } else {
            array_len(src_heap)
        };
        if add == 0 || dst.is_null() {
            aipl_array_dec(src_heap);
            return dst;
        }
        let dst_len = array_len(dst);
        let at = dst.add(ARR_ELEMS_OFFSET).add(dst_len * elem_size) as *mut u8;
        let from = src_heap.add(ARR_ELEMS_OFFSET);
        memcpy(at as *mut c_void, from as *const c_void, add * elem_size);
        elem_rc(retain_fn, at, add);
        *(dst as *mut i64) = (dst_len + add) as i64;
        aipl_array_dec(src_heap);
        dst
    }
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
    count_builtin!(builtin_calls::AIPL_SET_UNION);
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
    count_builtin!(builtin_calls::AIPL_SET_UNION_MUT);
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
    count_builtin!(builtin_calls::AIPL_DICT_INSERT);
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
    count_builtin!(builtin_calls::AIPL_DICT_GET);
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
    count_builtin!(builtin_calls::AIPL_DICT_CONTAINS_KEY);
    (unsafe { dict_find(a, key_ptr, pair_size, str_cmp) } >= 0) as i64
}

/// Element drop-fn for `str[]`: dec each element string.
#[no_mangle]
pub extern "C" fn aipl_arr_drop_str(elems: *const u8, len: i64) {
    count_builtin!(builtin_calls::AIPL_ARR_DROP_STR);
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
    count_builtin!(builtin_calls::AIPL_ARR_DROP_ARR);
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
    count_builtin!(builtin_calls::AIPL_ARR_RETAIN_PTR);
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
    count_builtin!(builtin_calls::AIPL_ARR_DROP_OPT_STR);
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
    count_builtin!(builtin_calls::AIPL_ARR_DROP_OPT_ARR);
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
    count_builtin!(builtin_calls::AIPL_ARR_RETAIN_OPT);
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
// args parameter. When it's zero we skip building the array and pass null. The
// injected parameter is typed as an ignored word rather than `str[]` (codegen's
// `injected_cli_args_ty`) precisely so it owns nothing: `main` neither allocates
// nor drops anything for arguments it never asked for.
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
//   functions:
//     <name>:               (one entry per function that ran, AIPL or builtin)
//       calls: <n>
// The test harness reads this back to verify a case's `--- performance ---`
// section. What it writes is that section minus the two things only the harness
// can measure: the `binary size` block, and each function's `bytes:` line (read
// from the object's symbols, which is also where functions that were *emitted
// but never called* come from). The entries are written in no particular order —
// builtins first, then the pointer-hashed AIPL table — because the harness sorts
// the merged list by name anyway.
// Opened in binary mode so no `\n` -> `\r\n` translation occurs.

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

/// One `functions:` entry: the name as a heading, then its call count on its own
/// line. `bytes:` is the harness's to add — the runtime cannot see the object.
#[cfg(aipl_instrument)]
unsafe fn report_fn_calls(f: *mut c_void, name: *const c_char, n: u64) {
    unsafe {
        fputs(b"\n  \0".as_ptr() as *const c_char, f);
        fputs(name, f);
        fputs(b":\n    calls: \0".as_ptr() as *const c_char, f);
        fput_u64(f, n);
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
        // One `functions:` block covering both halves of the call story: the
        // runtime builtins (fixed list) and the compiled AIPL functions (the
        // pointer-keyed table). Only entries that actually ran are written — a
        // full 78-line builtin block per case would bury the few that matter,
        // and a function with no calls and no code is nothing to report.
        fputs(b"\nfunctions:\0".as_ptr() as *const c_char, f);
        for (i, name) in builtin_calls::NAMES.iter().enumerate() {
            let n = builtin_calls::COUNTS[i].load(Ordering::Relaxed);
            if n == 0 {
                continue;
            }
            report_fn_calls(f, name.as_ptr() as *const c_char, n);
        }
        for i in 0..fn_calls::CAP {
            let name = fn_calls::NAMES[i].load(Ordering::Relaxed);
            if name == 0 {
                continue;
            }
            let n = fn_calls::COUNTS[i].load(Ordering::Relaxed);
            report_fn_calls(f, name as *const c_char, n);
        }
        // Written only when it happened, and the harness treats its presence as
        // a hard failure: a silently dropped tally would otherwise read as a
        // real call-count change. See `fn_calls::OVERFLOW`.
        let dropped = fn_calls::OVERFLOW.load(Ordering::Relaxed);
        if dropped != 0 {
            fputs(b"\nuncounted calls: \0".as_ptr() as *const c_char, f);
            fput_u64(f, dropped);
        }
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
    count_builtin!(builtin_calls::AIPL_TEST_BEGIN);
    TEST_CUR_FAILED.store(false, TestOrd::Relaxed);
}

#[no_mangle]
pub extern "C" fn aipl_assert(cond: i64, _loc: *const u8) {
    count_builtin!(builtin_calls::AIPL_ASSERT);
    if cond == 0 {
        TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_fail(_msg: *const u8) {
    count_builtin!(builtin_calls::AIPL_TEST_FAIL);
    // `?` on an err inside a `.test`: mark the current test failed. The error
    // message (`_msg`) is only rendered by the JIT runtime under `aipl check`.
    TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
}

#[no_mangle]
pub extern "C" fn aipl_test_fail_none() {
    count_builtin!(builtin_calls::AIPL_TEST_FAIL_NONE);
    // `?` on a `none` inside a `.test`: mark the current test failed. The
    // report is only printed by the JIT runtime under `aipl check`.
    TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
}

#[no_mangle]
pub extern "C" fn aipl_test_end() {
    count_builtin!(builtin_calls::AIPL_TEST_END);
    if TEST_CUR_FAILED.load(TestOrd::Relaxed) {
        TEST_FAILED.fetch_add(1, TestOrd::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_summary() -> i64 {
    count_builtin!(builtin_calls::AIPL_TEST_SUMMARY);
    i64::from(TEST_FAILED.load(TestOrd::Relaxed) > 0)
}
