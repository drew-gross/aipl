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

extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn calloc(count: usize, size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn abort() -> !;
    fn strlen(s: *const c_char) -> usize;
    fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
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
// linked for `aipl report`, `measure` and the cases harness) they tally call
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
// Heap bytes live right now (requested sizes of blocks allocated and not yet
// freed), and the most that has been live since the peak was last reset. Both
// are only read through `measure`, which resets the peak when its window opens
// and reports it relative to what was live then.
//
// Knowing a block's size at `free` time is what costs: libc cannot be asked for
// the size that was *requested*, so the instrumented build prefixes every block
// with a header holding it (`ALLOC_HEADER` bytes, keeping the 16-byte alignment
// `malloc` promises). Only the instrumented build — never what ships — and
// `bytes allocated` is unaffected, since it counts the request, not the header.
#[cfg(aipl_instrument)]
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(aipl_instrument)]
static PEAK_LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(aipl_instrument)]
const ALLOC_HEADER: usize = 16;

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

    /// The entry points this table has a slot for, NUL-terminated for `fputs` and
    /// sorted so the reported breakdown has a stable order, with the `AIPL_*`
    /// constants below indexing it.
    ///
    /// Maintained by hand, and exactly the set that is counted: a slot is only
    /// ever bumped by a `count_builtin!` at the entry point itself. The shared
    /// modules (`str24`, `arr24`, `array_ops`) have no host-specific counter to
    /// reach, so their entry points are not here — nor do they show in a
    /// `aipl report` breakdown.
    pub const NAMES: &[&[u8]] = &[
        b"aipl_arr_inc\0",
        b"aipl_array_dec\0",
        b"aipl_array_new\0",
        b"aipl_array_with_cap\0",
        b"aipl_assert\0",
        b"aipl_execute_program\0",
        b"aipl_i64_len\0",
        b"aipl_list_files\0",
        b"aipl_monotonic_now\0",
        b"aipl_now_nanos\0",
        b"aipl_read_file_to_string\0",
        b"aipl_rec_alloc\0",
        b"aipl_rec_dec_strong\0",
        b"aipl_rec_dec_weak\0",
        b"aipl_rec_inc_strong\0",
        b"aipl_rec_inc_weak\0",
        b"aipl_shim_get\0",
        b"aipl_shim_set\0",
        b"aipl_test_begin\0",
        b"aipl_test_end\0",
        b"aipl_test_fail\0",
        b"aipl_test_fail_none\0",
        b"aipl_test_summary\0",
        b"aipl_u64_len\0",
        b"aipl_write_bytes\0",
        b"aipl_write_i64\0",
        b"aipl_write_string_to_file\0",
        b"aipl_write_u64\0",
    ];

    pub static COUNTS: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];

    pub const AIPL_ARR_INC: usize = 0;
    pub const AIPL_ARRAY_DEC: usize = 1;
    pub const AIPL_ARRAY_NEW: usize = 2;
    pub const AIPL_ARRAY_WITH_CAP: usize = 3;
    pub const AIPL_ASSERT: usize = 4;
    pub const AIPL_EXECUTE_PROGRAM: usize = 5;
    pub const AIPL_I64_LEN: usize = 6;
    pub const AIPL_LIST_FILES: usize = 7;
    pub const AIPL_MONOTONIC_NOW: usize = 8;
    pub const AIPL_NOW_NANOS: usize = 9;
    pub const AIPL_READ_FILE_TO_STRING: usize = 10;
    pub const AIPL_REC_ALLOC: usize = 11;
    pub const AIPL_REC_DEC_STRONG: usize = 12;
    pub const AIPL_REC_DEC_WEAK: usize = 13;
    pub const AIPL_REC_INC_STRONG: usize = 14;
    pub const AIPL_REC_INC_WEAK: usize = 15;
    pub const AIPL_SHIM_GET: usize = 16;
    pub const AIPL_SHIM_SET: usize = 17;
    pub const AIPL_TEST_BEGIN: usize = 18;
    pub const AIPL_TEST_END: usize = 19;
    pub const AIPL_TEST_FAIL: usize = 20;
    pub const AIPL_TEST_FAIL_NONE: usize = 21;
    pub const AIPL_TEST_SUMMARY: usize = 22;
    pub const AIPL_U64_LEN: usize = 23;
    pub const AIPL_WRITE_BYTES: usize = 24;
    pub const AIPL_WRITE_I64: usize = 25;
    pub const AIPL_WRITE_STRING_TO_FILE: usize = 26;
    pub const AIPL_WRITE_U64: usize = 27;
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

/// Whether `AIPL_INJECT_LEAK` is set: the instrumented runtime then stops counting
/// frees, so a run reads as one that leaked every block it allocated.
///
/// A test hook, and the reason it exists is that nothing in the language can leak
/// on purpose: every gate that refuses a leak (`aipl report`, the cases harness,
/// `aipl check` on `.performance` blocks) would otherwise have no way to be shown
/// still working, and a gate that quietly stops running looks exactly like a
/// clean codebase. Instrumented build only, so nothing that ships can be affected;
/// it changes only what is *counted*, never what is freed.
#[cfg(aipl_instrument)]
fn leak_injected() -> bool {
    use core::sync::atomic::AtomicU8;
    // 0: not looked at yet, 1: off, 2: on. Read once.
    static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        0 => {
            let on = unsafe { !getenv(b"AIPL_INJECT_LEAK\0".as_ptr() as *const c_char).is_null() };
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        state => state == 2,
    }
}

/// `size` more bytes are live: raise the live total and the peak with it.
#[cfg(aipl_instrument)]
#[inline]
fn note_live_up(size: usize) {
    let live = LIVE_BYTES.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
}

#[inline]
unsafe fn rt_alloc(size: usize) -> *mut c_void {
    #[cfg(aipl_instrument)]
    {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        let base = unsafe { malloc(size + ALLOC_HEADER) } as *mut u8;
        if base.is_null() {
            return base as *mut c_void;
        }
        unsafe { *(base as *mut usize) = size };
        note_live_up(size);
        return unsafe { base.add(ALLOC_HEADER) } as *mut c_void;
    }
    #[cfg(not(aipl_instrument))]
    unsafe {
        malloc(size)
    }
}

#[inline]
unsafe fn rt_free(ptr: *mut c_void) {
    #[cfg(aipl_instrument)]
    {
        if !leak_injected() {
            FREE_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        if ptr.is_null() {
            return;
        }
        let base = unsafe { (ptr as *mut u8).sub(ALLOC_HEADER) };
        let size = unsafe { *(base as *const usize) };
        LIVE_BYTES.fetch_sub(size as u64, Ordering::Relaxed);
        unsafe { free(base as *mut c_void) };
    }
    #[cfg(not(aipl_instrument))]
    unsafe {
        free(ptr)
    }
}

// The 24-byte `str` layout — the live representation — **shared verbatim** with
// the JIT runtime instead of mirrored by hand. This is the one piece where a
// divergence between the two runtimes would be silent memory corruption rather
// than a failing test, so it lives in one file that both compile — which is also
// why that file is `no_std`-safe and calls `super::rt_alloc`/`super::rt_free`
// (just above) rather than allocating for itself.
//
// `dead_code` is allowed because the file serves two hosts: a helper reached only
// from the JIT's side is genuinely unused *here*, and the warning would be about
// which host you are compiling rather than about the code.
#[allow(dead_code)]
mod str24 {
    include!("../../aipl-codegen/src/str24.rs");
}

// The 24-byte array value, shared verbatim rather than mirrored by hand, for the
// same reason. See the file's header. (`dead_code` is allowed on the same grounds
// as `str24`'s: a helper only the JIT reaches is unused here.)
#[allow(dead_code)]
mod arr24 {
    include!("../../aipl-codegen/src/arr24.rs");
}

// The 256-bit `#{char}` value, shared the same way — and cheaper here, because a
// char set has no allocator to ask about.
#[allow(dead_code)]
mod charset {
    include!("../../aipl-codegen/src/charset.rs");
}

// The array layout — block, representation tags, reversed view — shared the
// same way and for the same reason; see the file's own header.
//
// `dead_code` is allowed on the same grounds as `str24`'s and `arr24`'s: the file
// serves two hosts, and a helper only the JIT reaches (`arr_inline_len`) is
// genuinely unused here.
#[allow(dead_code)]
mod array_layout {
    include!("../../aipl-codegen/src/array_layout.rs");
}
use array_layout::*;

#[inline]
unsafe fn rt_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    // Tallied separately from alloc/free: an in-place grow reuses an existing
    // block rather than a fresh malloc/free pair, so it's neither an allocation
    // nor a deallocation, but it is worth tracking on its own.
    #[cfg(aipl_instrument)]
    {
        REALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // Resizing keeps the header: it moves with the block, and only the size
        // it records (and the live total with it) changes.
        if ptr.is_null() {
            let base = unsafe { malloc(size + ALLOC_HEADER) } as *mut u8;
            if base.is_null() {
                return base as *mut c_void;
            }
            unsafe { *(base as *mut usize) = size };
            note_live_up(size);
            return unsafe { base.add(ALLOC_HEADER) } as *mut c_void;
        }
        let old_base = unsafe { (ptr as *mut u8).sub(ALLOC_HEADER) };
        let old = unsafe { *(old_base as *const usize) };
        let base = unsafe { realloc(old_base as *mut c_void, size + ALLOC_HEADER) } as *mut u8;
        if base.is_null() {
            return base as *mut c_void;
        }
        unsafe { *(base as *mut usize) = size };
        if size >= old {
            note_live_up(size - old);
        } else {
            LIVE_BYTES.fetch_sub((old - size) as u64, Ordering::Relaxed);
        }
        return unsafe { base.add(ALLOC_HEADER) } as *mut c_void;
    }
    #[cfg(not(aipl_instrument))]
    unsafe {
        realloc(ptr, size)
    }
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

// ---------- The wide `str`'s I/O half (AOT) ----------
//
// `str24.rs` is shared verbatim between the two runtimes because the layout must
// not diverge, but I/O genuinely differs: the JIT half (`str24_host.rs`) writes
// through `std::io`, and this one writes through `libc` like every other entry
// point here. So these two are the AOT counterparts of `str24_host`'s
// `aipl_print` / `aipl_print_error`, and the reason every program that printed
// anything failed to *link* while one that printed nothing
// built fine.
//
// Unlike `aipl_print`, they do **not** release their argument: an `aipl_*` entry
// point borrows the caller's 24-byte value, which the caller is already keeping
// alive (see `active_sym` in `aipl-codegen`).

/// `print(s)` under the wide ABI. Streams leaf by leaf, so a rope prints without
/// being flattened first — the same property `aipl_print` has via
/// `str_for_each_chunk`.
#[no_mangle]
pub extern "C" fn aipl_print(s: *const str24::Str) {
    str24::for_each_chunk(unsafe { *s }, &mut |chunk| {
        unsafe { write(1, chunk.as_ptr() as *const c_void, chunk.len()) }; // stdout
        true
    });
    unsafe { write(1, b"\n".as_ptr() as *const c_void, 1) };
}

/// `put(s)` under the wide ABI: `print` without the newline. Streams leaf by
/// leaf, like `aipl_print`.
#[no_mangle]
pub extern "C" fn aipl_put(s: *const str24::Str) {
    str24::for_each_chunk(unsafe { *s }, &mut |chunk| {
        unsafe { write(1, chunk.as_ptr() as *const c_void, chunk.len()) };
        true
    });
}

/// `fn main() -> !Error`'s failure path under the wide ABI: `error: <msg>` on
/// stderr.
#[no_mangle]
pub extern "C" fn aipl_print_error(s: *const str24::Str) {
    let prefix = b"error: ";
    unsafe { write(2, prefix.as_ptr() as *const c_void, prefix.len()) };
    str24::for_each_chunk(unsafe { *s }, &mut |chunk| {
        unsafe { write(2, chunk.as_ptr() as *const c_void, chunk.len()) };
        true
    });
    unsafe { write(2, b"\n".as_ptr() as *const c_void, 1) };
}


// ---------- The wide `str`'s file I/O (AOT) ----------
//
// The JIT half (`str24_host.rs`) has `std::fs`; this one has `libc`, which wants
// NUL-terminated paths. A wide `str` is a *window* and carries no terminator —
// slicing is what makes it free — so one has to be made rather than assumed.
// That is the whole difference from the tagged versions, which could hand a heap
// str's own buffer straight to `fopen`.

/// A NUL-terminated copy of `s`'s bytes for the C file APIs. Allocated (not a
/// fixed stack buffer, so there is no path-length ceiling); free with
/// [`free_c_path`].
unsafe fn c_path_of(s: str24::Str) -> *mut u8 {
    let len = s.len();
    let raw = unsafe { rt_alloc(len + 1) } as *mut u8;
    if raw.is_null() {
        return raw;
    }
    let mut at = 0usize;
    str24::for_each_chunk(s, &mut |chunk| {
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), raw.add(at), chunk.len()) };
        at += chunk.len();
        true
    });
    unsafe { *raw.add(len) = 0 };
    raw
}

unsafe fn free_c_path(p: *mut u8) {
    unsafe { rt_free(p as *mut c_void) };
}

/// `read_file_to_string` under the wide ABI: contents through `out`, success as
/// the return value. See `str24_host::aipl_read_file_to_string` for why the
/// flag is explicit rather than a null result. Borrows `path`.
#[no_mangle]
pub extern "C" fn aipl_read_file_to_string(out: *mut str24::Str, path: *const str24::Str) -> i64 {
    count_builtin!(builtin_calls::AIPL_READ_FILE_TO_STRING);
    unsafe {
        *out = str24::Str::empty();
        let cpath = c_path_of(*path);
        if cpath.is_null() {
            return 0;
        }
        let f = fopen(cpath as *const c_char, b"rb\0".as_ptr() as *const c_char);
        free_c_path(cpath);
        if f.is_null() {
            return 0;
        }
        if fseek(f, 0, SEEK_END) != 0 {
            fclose(f);
            return 0;
        }
        let size = ftell(f);
        if size < 0 || fseek(f, 0, SEEK_SET) != 0 {
            fclose(f);
            return 0;
        }
        let size = size as usize;
        let buf = rt_alloc(size.max(1)) as *mut u8;
        if buf.is_null() {
            fclose(f);
            return 0;
        }
        let n = fread(buf as *mut c_void, 1, size, f);
        fclose(f);
        if n != size {
            rt_free(buf as *mut c_void);
            return 0;
        }
        *out = str24::from_bytes(core::slice::from_raw_parts(buf, size));
        rt_free(buf as *mut c_void);
        1
    }
}

/// Standard input read ahead of the line being asked for: `STDIN_BUFFER[pos..len]`
/// is bytes already read from fd 0 and not yet handed out. The program is single
/// threaded, so plain statics do.
static mut STDIN_BUFFER: [u8; 4096] = [0; 4096];
static mut STDIN_POS: usize = 0;
static mut STDIN_LEN: usize = 0;

/// The next byte of standard input, or `None` at end of input or on a read error.
unsafe fn stdin_byte() -> Option<u8> {
    unsafe {
        if STDIN_POS == STDIN_LEN {
            let n = read(
                0,
                core::ptr::addr_of_mut!(STDIN_BUFFER) as *mut c_void,
                STDIN_BUFFER.len(),
            );
            if n <= 0 {
                return None;
            }
            STDIN_POS = 0;
            STDIN_LEN = n as usize;
        }
        let byte = STDIN_BUFFER[STDIN_POS];
        STDIN_POS += 1;
        Some(byte)
    }
}

/// `read_line()` under the wide ABI: the next line of standard input, without its
/// `\n` or `\r\n`, through `out`, and 1/0 as the return value (0 at end of input,
/// with `out` left empty). See `str24_host::aipl_read_line`.
#[no_mangle]
pub extern "C" fn aipl_read_line(out: *mut str24::Str) -> i64 {
    unsafe {
        *out = str24::Str::empty();
        let mut line = rt_alloc(64) as *mut u8;
        let mut capacity = 64;
        let mut len = 0;
        let mut any = false;
        while let Some(byte) = stdin_byte() {
            any = true;
            if byte == b'\n' {
                break;
            }
            if len == capacity {
                let grown = rt_alloc(capacity * 2) as *mut u8;
                memcpy(grown as *mut c_void, line as *const c_void, len);
                rt_free(line as *mut c_void);
                line = grown;
                capacity *= 2;
            }
            *line.add(len) = byte;
            len += 1;
        }
        if any {
            if len > 0 && *line.add(len - 1) == b'\r' {
                len -= 1;
            }
            *out = str24::from_bytes(core::slice::from_raw_parts(line, len));
        }
        rt_free(line as *mut c_void);
        i64::from(any)
    }
}

/// `struct termios` and the constants this file needs from it, per platform.
#[cfg(target_os = "macos")]
mod terminal {
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        pub input_flags: u64,
        pub output_flags: u64,
        pub control_flags: u64,
        pub local_flags: u64,
        pub control_chars: [u8; 20],
        pub input_speed: u64,
        pub output_speed: u64,
    }
    pub const ICANON: u64 = 0x100;
    pub const ECHO: u64 = 0x8;
    pub const VMIN: usize = 16;
    pub const VTIME: usize = 17;
}
#[cfg(not(target_os = "macos"))]
mod terminal {
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        pub input_flags: u32,
        pub output_flags: u32,
        pub control_flags: u32,
        pub local_flags: u32,
        pub line: u8,
        pub control_chars: [u8; 32],
        pub input_speed: u32,
        pub output_speed: u32,
    }
    pub const ICANON: u32 = 0x2;
    pub const ECHO: u32 = 0x8;
    pub const VMIN: usize = 6;
    pub const VTIME: usize = 5;
}

extern "C" {
    fn tcgetattr(fd: c_int, settings: *mut terminal::Termios) -> c_int;
    fn tcsetattr(fd: c_int, when: c_int, settings: *const terminal::Termios) -> c_int;
}

/// `read_byte()`: the next byte of standard input as it is typed, or -1 at end of
/// input or on a read error. A terminal is taken out of line mode and stops echoing
/// while the call waits, and its settings are put back before it returns; input
/// that is not a terminal is read as it is. Reads through the buffer
/// `aipl_read_line` uses.
#[no_mangle]
pub extern "C" fn aipl_read_byte() -> i64 {
    unsafe {
        let mut saved = core::mem::zeroed::<terminal::Termios>();
        let is_terminal = tcgetattr(0, &mut saved) == 0;
        // Bytes already buffered are handed out without waiting, so the terminal
        // only needs its settings changed when a read is actually coming.
        let buffered = STDIN_POS != STDIN_LEN;
        if is_terminal && !buffered {
            let mut unbuffered = saved;
            unbuffered.local_flags &= !(terminal::ICANON | terminal::ECHO);
            unbuffered.control_chars[terminal::VMIN] = 1;
            unbuffered.control_chars[terminal::VTIME] = 0;
            tcsetattr(0, 0, &unbuffered);
        }
        let byte = stdin_byte();
        if is_terminal && !buffered {
            tcsetattr(0, 0, &saved);
        }
        match byte {
            Some(byte) => i64::from(byte),
            None => -1,
        }
    }
}

/// Sends the terminal a control sequence, straight to standard output.
fn write_terminal_sequence(sequence: &[u8]) {
    unsafe { write(1, sequence.as_ptr() as *const c_void, sequence.len()) };
}

/// `clear_terminal()`: erase the display (`ESC [ 2 J`), then home the cursor
/// (`ESC [ H`).
#[no_mangle]
pub extern "C" fn aipl_clear_terminal() {
    write_terminal_sequence(b"\x1b[2J\x1b[H");
}

/// `enter_alternate_screen()`: save the cursor and switch to the blank second
/// screen (`ESC [ ? 1049 h`).
#[no_mangle]
pub extern "C" fn aipl_enter_alternate_screen() {
    write_terminal_sequence(b"\x1b[?1049h");
}

/// `leave_alternate_screen()`: switch back to the first screen and put the cursor
/// where it was (`ESC [ ? 1049 l`).
#[no_mangle]
pub extern "C" fn aipl_leave_alternate_screen() {
    write_terminal_sequence(b"\x1b[?1049l");
}

/// `move_to(row, column)`: put the cursor at a cell counted from 0 at the top
/// left, which the terminal counts from 1 (`ESC [ row ; column H`).
#[no_mangle]
pub extern "C" fn aipl_move_to(row: u64, column: u64) {
    // `ESC [`, two 20-digit numbers (a u64 is at most 20 digits) with a `;`
    // between, and `H`.
    let mut sequence = [0u8; 44];
    let mut length = 0;
    sequence[length] = 0x1b;
    sequence[length + 1] = b'[';
    length += 2;
    for (index, cell) in [row, column].into_iter().enumerate() {
        // One past the cell so that 0 is the terminal's 1. Saturating keeps u64::MAX
        // an answer rather than a wrap to 0.
        let mut number = cell.saturating_add(1);
        let mut digits = [0u8; 20];
        let mut count = 0;
        while number > 0 {
            digits[count] = b'0' + (number % 10) as u8;
            number /= 10;
            count += 1;
        }
        while count > 0 {
            count -= 1;
            sequence[length] = digits[count];
            length += 1;
        }
        if index == 0 {
            sequence[length] = b';';
            length += 1;
        }
    }
    sequence[length] = b'H';
    length += 1;
    write_terminal_sequence(&sequence[..length]);
}

/// `write_string_to_file` under the wide ABI. Streams the contents chunk by
/// chunk, so a rope is written without being flattened. Borrows both arguments.
#[no_mangle]
pub extern "C" fn aipl_write_string_to_file(
    path: *const str24::Str,
    contents: *const str24::Str,
) -> i64 {
    count_builtin!(builtin_calls::AIPL_WRITE_STRING_TO_FILE);
    unsafe {
        let cpath = c_path_of(*path);
        if cpath.is_null() {
            return 0;
        }
        let f = fopen(cpath as *const c_char, b"wb\0".as_ptr() as *const c_char);
        free_c_path(cpath);
        if f.is_null() {
            return 0;
        }
        let mut ok = true;
        str24::for_each_chunk(*contents, &mut |chunk| {
            if fwrite(chunk.as_ptr() as *const c_void, 1, chunk.len(), f) != chunk.len() {
                ok = false;
                return false;
            }
            true
        });
        fclose(f);
        i64::from(ok)
    }
}

/// `list_files` under the wide ABI. Borrows `dir`. Writes the listing to `out`
/// and returns 1, or returns 0 and writes nothing.
#[no_mangle]
pub extern "C" fn aipl_list_files(out: *mut arr24::ArrValue, dir: *const str24::Str) -> i64 {
    count_builtin!(builtin_calls::AIPL_LIST_FILES);
    #[cfg(unix)]
    unsafe {
        match list_unix::list_files_wide(*dir) {
            Some(listing) => {
                core::ptr::write(out, listing);
                1
            }
            None => 0,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (out, dir);
        0
    }
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
    use super::{aipl_array_dec, aipl_array_with_cap, memcpy, str24, strlen};
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
        /// How many elements `out` holds.
        len: usize,
        /// Byte width of one element — 8 for a tagged `str` pointer,
        /// `str24::STR_SIZE` for a wide value stored inline. The walk itself is
        /// identical; only how a name becomes an element differs.
        elem_size: i64,
    }


    /// [`list_files`] for a wide `str` directory argument, producing 24-byte
    /// elements and the matching element drop-fn.
    pub unsafe fn list_files_wide(dir: str24::Str) -> Option<super::arr24::ArrValue> {
        let mut scratch = [0u8; str24::INLINE_CAP];
        let bytes = dir.bytes(&mut scratch);
        let drop_fn = str24::aipl_arr_drop_str as *const () as usize as i64;
        unsafe { walk_from(bytes, drop_fn, str24::STR_SIZE as i64) }
    }

    unsafe fn walk_from(
        bytes: &[u8],
        drop_fn: i64,
        elem_size: i64,
    ) -> Option<super::arr24::ArrValue> {
        if bytes.is_empty() || bytes.len() >= PATH_CAP {
            return None;
        }
        let mut w = Walk {
            path: [0u8; PATH_CAP],
            out: aipl_array_with_cap(0, drop_fn, elem_size),
            len: 0,
            elem_size,
        };
        unsafe {
            memcpy(
                w.path.as_mut_ptr() as *mut c_void,
                bytes.as_ptr() as *const c_void,
                bytes.len(),
            );
            if !walk(&mut w, bytes.len(), drop_fn) {
                aipl_array_dec(w.out);
                return None;
            }
        }
        Some(super::arr24::ArrValue::whole_of(w.out, w.len))
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
        // retain-fn 0 either way: the fresh string's one reference moves into
        // the array.
        w.out = {
            let v = str24::from_bytes(bytes);
            super::array_ops::push_mut(
                w.out,
                w.len,
                &v as *const str24::Str as *const u8,
                drop_fn,
                0,
                w.elem_size,
            )
        };
        w.len += 1;
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
static SHIM_SLOTS: [core::sync::atomic::AtomicI64; 15] =
    [const { core::sync::atomic::AtomicI64::new(0) }; 15];

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



// ---------- Cached constants ----------
//
// A top-level constant whose value is computed is built on its first use and
// shared by every use after. The cache is one zeroed block per constant, handed
// out by index: word 0 says whether the value has been built, and the value
// follows. Codegen decides the block's size, and does the building, copying and
// reference counting; this only owns the memory. Mirrors the JIT runtime's
// `aipl_const_slot`, which keeps a block per thread where this keeps one: a
// compiled program has a single thread.
//
// The blocks come from `calloc` directly, so the allocation tallies never see
// them — what a block *holds* is released by the teardown function codegen
// registers, before `main` returns to the tallies.

/// The most constants one program may cache. Sized by
/// `aipl_syntax::CONST_SLOT_COUNT`.
static CONST_SLOTS: [core::sync::atomic::AtomicI64; 256] =
    [const { core::sync::atomic::AtomicI64::new(0) }; 256];

/// The function registered by `aipl_const_on_exit`, or 0.
static CONST_TEARDOWN: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

/// The cache block for constant `idx`, allocating `bytes` zeroed bytes on the
/// first request.
#[no_mangle]
pub extern "C" fn aipl_const_slot(idx: i64, bytes: i64) -> i64 {
    let slot = &CONST_SLOTS[idx as usize];
    let block = slot.load(core::sync::atomic::Ordering::Relaxed);
    if block != 0 {
        return block;
    }
    let block = unsafe { calloc(1, bytes as usize) } as i64;
    slot.store(block, core::sync::atomic::Ordering::Relaxed);
    block
}

/// The cache block for constant `idx` if one was ever requested, else 0.
#[no_mangle]
pub extern "C" fn aipl_const_peek(idx: i64) -> i64 {
    CONST_SLOTS[idx as usize].load(core::sync::atomic::Ordering::Relaxed)
}

/// Register the function that releases every cached constant; `main` runs it
/// after the program does.
#[no_mangle]
pub extern "C" fn aipl_const_on_exit(teardown: i64) {
    CONST_TEARDOWN.store(teardown, core::sync::atomic::Ordering::Relaxed);
}

// ---------- The random generator ----------
//
// splitmix64, one word of state, as in the JIT runtime (`aipl_random_next` there):
// a seed gives the same sequence under both.

/// The generator's whole state. Mirrors `RANDOM_STATE` in the JIT runtime.
static RANDOM_STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The next number of the sequence: advance the state by the golden-ratio
/// increment, then scramble it.
#[no_mangle]
pub extern "C" fn aipl_random_next() -> i64 {
    let state = RANDOM_STATE
        .fetch_add(0x9E37_79B9_7F4A_7C15, core::sync::atomic::Ordering::Relaxed)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    let a = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let b = (a ^ (a >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (b ^ (b >> 31)) as i64
}

/// Start the sequence at `seed`. Also how a shim puts back the state it saved.
#[no_mangle]
pub extern "C" fn aipl_random_seed(seed: i64) {
    RANDOM_STATE.store(seed as u64, core::sync::atomic::Ordering::Relaxed);
}

/// The generator's state, for a shim to save.
#[no_mangle]
pub extern "C" fn aipl_random_state() -> i64 {
    RANDOM_STATE.load(core::sync::atomic::Ordering::Relaxed) as i64
}

/// `random_u64` called with no `shim random` installed: unreachable past the
/// checker, so it stops the program rather than hand out numbers from nowhere.
#[no_mangle]
pub extern "C" fn aipl_random_unshimmed() {
    let message = b"error: random_u64 called with no `shim random` installed\n";
    unsafe {
        write(2, message.as_ptr() as *const c_void, message.len());
        abort()
    }
}

/// The wide `ExecResult!Error` layout — tag, then either the `Error` message or
/// `{ stdout, stderr, exit_code }`. Every `str` is a whole value, so the offsets
/// step by `str24::STR_SIZE` rather than by a word. Mirrors codegen's
/// `write_err_wide` / `write_ok_wide`; both must agree with what `field_size`
/// computes, since that is what reads them back.
fn write_err_wide(out: *mut u8, message: &[u8]) {
    unsafe {
        *(out as *mut i64) = 0;
        core::ptr::write(out.add(8) as *mut str24::Str, str24::from_bytes(message));
    }
}

fn write_ok_wide(out: *mut u8, stdout: &[u8], stderr: &[u8], exit_code: i64) {
    unsafe {
        *(out as *mut i64) = 1;
        let base = out.add(8);
        core::ptr::write(base as *mut str24::Str, str24::from_bytes(stdout));
        core::ptr::write(
            base.add(str24::STR_SIZE) as *mut str24::Str,
            str24::from_bytes(stderr),
        );
        *(base.add(2 * str24::STR_SIZE) as *mut i64) = exit_code;
    }
}

/// `execute_program` under the wide ABI. Borrows both arguments, like every
/// `aipl_*` entry point; the spawn itself is shared with the tagged path, which
/// is why `run` takes the representation rather than being duplicated.
#[no_mangle]
pub extern "C" fn aipl_execute_program(
    out: *mut u8,
    program: *const str24::Str,
    args: *const arr24::ArrValue,
) {
    count_builtin!(builtin_calls::AIPL_EXECUTE_PROGRAM);
    #[cfg(unix)]
    unsafe {
        // The argument array is a value, read in place whatever its shape.
        let (first, step, len) = arr24::str_run(args);
        exec_unix::run(out as *mut i64, program as *const u8, first, step, len);
    }
    #[cfg(not(unix))]
    {
        let _ = (program, args);
        write_err_wide(out, b"could not execute program");
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
        fail_msg, fclose, fmt_i64, fopen, fread, fseek, ftell, rt_alloc, rt_free, str24, SEEK_END,
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
    /// [`dup_cstr`] for a wide `str`, which is a window carrying no terminator.
    unsafe fn dup_cstr(v: str24::Str) -> *mut c_char {
        let mut scratch = [0u8; str24::INLINE_CAP];
        let bytes = v.bytes(&mut scratch);
        let n = bytes.len();
        unsafe {
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
    /// The captured stream as an owned byte buffer and its length, or `None` if
    /// it could not be read (or holds a NUL, which a `str` cannot represent).
    /// The caller frees the buffer.
    unsafe fn read_temp_file(path: *const c_char) -> Option<(*mut u8, usize)> {
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
                return Some((core::ptr::null_mut(), 0));
            }
            let buf = rt_alloc(size) as *mut u8;
            if buf.is_null() {
                fclose(f);
                super::abort();
            }
            let n = fread(buf as *mut c_void, 1, size, f);
            fclose(f);
            if n != size || core::slice::from_raw_parts(buf, size).contains(&0) {
                rt_free(buf as *mut c_void);
                return None;
            }
            Some((buf, size))
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

    /// `program` and the argument run (`first`, `step` `Str`s apart, `len` of
    /// them — `arr24::str_run`) are borrowed: nothing is released here.
    pub unsafe fn run(
        out: *mut i64,
        program: *const u8,
        first: *const str24::Str,
        step: isize,
        len: usize,
    ) {
        unsafe {

            // argv: program, each arg, then a NULL terminator.
            let argv =
                rt_alloc((len + 2) * core::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
            if argv.is_null() {
                super::abort();
            }
            // Both arguments are borrowed: nothing is released here.
            *argv = dup_cstr(*(program as *const str24::Str));
            for i in 0..len {
                *argv.add(1 + i) = dup_cstr(core::ptr::read(first.offset(step * i as isize)));
            }
            *argv.add(1 + len) = core::ptr::null_mut();

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
                (Some((ob, ol)), Some((eb, el))) => {
                    let empty: &[u8] = &[];
                    let so = if ob.is_null() {
                        empty
                    } else {
                        core::slice::from_raw_parts(ob, ol)
                    };
                    let se = if eb.is_null() {
                        empty
                    } else {
                        core::slice::from_raw_parts(eb, el)
                    };
                    super::write_ok_wide(out as *mut u8, so, se, exit_code);
                    if !ob.is_null() {
                        rt_free(ob as *mut c_void);
                    }
                    if !eb.is_null() {
                        rt_free(eb as *mut c_void);
                    }
                }
                _ => fail_msg(out),
            }
        }
    }
}

fn fail_msg(out: *mut i64) {
    write_err_wide(out as *mut u8, b"could not execute program");
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

// One-allocation `to_str` cursor primitives (mirror the JIT runtime). `to_str`
// measures the total length, allocates once via `aipl_str_alloc`, then fills the
// buffer through a moving cursor with the write helpers below.


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


/// How [`aipl_arr_sort`] orders its elements. Every `ord` element type is a
/// scalar — a word, or a narrow integer packed at its own width — or a `str`, so
/// the only question is how to read one: as a signed integer, as an unsigned one
/// (`u8`..`u64`, and `char` — a byte value), or as a `str` to compare
/// lexicographically by bytes.
const SORT_KIND_STR: i64 = 2;
const SORT_KIND_UNSIGNED: i64 = 1;

/// Order `words` in place. Mirrors the JIT runtime's `sort_words` byte-for-byte,
/// which is why it uses `sort_unstable_by` — the stable sorts need an allocation,
/// and this runtime is `#![no_std]`. Unstable is enough: `ord` elements are
/// compared by value, so equal ones are indistinguishable.
fn sort_words(words: &mut [i64], kind: i64) {
    match kind {
        SORT_KIND_UNSIGNED => words.sort_unstable_by(|x, y| (*x as u64).cmp(&(*y as u64))),
        _ => words.sort_unstable(),
    }
}


// ---------- Refcounted array runtime ----------
//
// The block, the representation tags and the reversed view are `array_layout`
// (shared verbatim with the JIT runtime); this is the AOT side's allocation,
// accounting and element traffic over that layout, mirroring the JIT
// runtime's function for function.

type ArrDropFn = extern "C" fn(*const u8, i64);

// Element size is known at compile time, so codegen passes it to these fns as a
// constant rather than storing it in the header. The header keeps the element-
// region capacity in *bytes* so `aipl_array_dec` can free without it.

/// Retain/drop `count` elements at `at` via a helper-fn pointer, if non-null.
unsafe fn elem_rc(fn_ptr: i64, at: *const u8, count: usize) {
    if fn_ptr != 0 {
        let f: ArrDropFn = unsafe { core::mem::transmute(fn_ptr) };
        f(at, count as i64);
    }
}

// The operations over a block (push, reserve, the set and dict operations, the
// release at refcount zero) are `array_ops`, shared verbatim with the JIT runtime,
// over the `buffer` a `str` shares. What they ask of their host is the allocator
// (`rt_alloc`/`rt_realloc`/`rt_free`, with this runtime's accounting).
#[allow(dead_code)]
mod buffer {
    include!("../../aipl-codegen/src/buffer.rs");
}

#[allow(dead_code)]
mod array_ops {
    include!("../../aipl-codegen/src/array_ops.rs");
}

/// Allocate an array of `len` uninitialized elements, refcount 1, cap == len. A
/// fresh literal is allocated to exactly its length, even at zero.
#[no_mangle]
pub extern "C" fn aipl_array_new(len: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARRAY_NEW);
    let len = if len < 0 { 0 } else { len as usize };
    array_ops::alloc_block(len, len, drop_fn, elem_size)
}

/// Allocate an empty array (refcount 1) reserved to `cap` slots of `elem_size`
/// bytes with the given element `drop_fn`. Used by `map`/`filter` to pre-size
/// their output.
#[no_mangle]
pub extern "C" fn aipl_array_with_cap(cap: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    count_builtin!(builtin_calls::AIPL_ARRAY_WITH_CAP);
    let cap = if cap < 0 { 0 } else { cap as usize };
    array_ops::alloc_block(0, cap, drop_fn, elem_size)
}

#[no_mangle]
pub extern "C" fn aipl_array_dec(ptr: *const u8) {
    // The empty array owns no allocation, so there is no refcount to touch — the
    // same no-op `release` is for an inline `str`. Before anything is counted.
    if !ptr.is_null() && arr_repr(ptr) == ArrRepr::Empty {
        return;
    }
    count_builtin!(builtin_calls::AIPL_ARRAY_DEC);
    array_ops::release(ptr);
}

/// Retain an array value (any representation). Uses `arr_untag` to strip the
/// representation tag before touching the refcount.
#[no_mangle]
pub extern "C" fn aipl_arr_inc(ptr: *const u8) {
    if !ptr.is_null() && arr_repr(ptr) == ArrRepr::Empty {
        return;
    }
    count_builtin!(builtin_calls::AIPL_ARR_INC);
    array_ops::retain(ptr);
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
    // Null is the empty value a taken boxed field or binding is left holding;
    // nothing to release. Mirrors the JIT runtime.
    if p.is_null() {
        return;
    }
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

// ---------- Set runtime ----------
//
// A set reuses the array heap block verbatim; only construction differs
// (deduplicated insert). Elements are i64/bool/char (compared by value, a
// bit-compare for a packed `bool` set, no element drop/retain) or `str` (8-byte
// pointers compared by content, with the array `str` drop/retain helpers stored
// so the block frees/retains its strings). Mirrors codegen.


/// Build the CLI arguments as an AIPL `str[]`, excluding `argv[0]` (the program
/// name) so a program sees only the arguments a user passed. The array owns its
/// element strings via the `str[]` drop-fn; `main` releases the whole thing.
unsafe fn build_cli_args(argc: c_int, argv: *const *const c_char) -> *const u8 {
    unsafe {
        let n = if argc > 1 { (argc - 1) as i64 } else { 0 };
        // Built by the runtime and read by compiled code, so the two must agree
        // byte for byte — and this is one of the few places the runtime has no
        // type to consult.
        let drop_fn = str24::aipl_arr_drop_str as *const () as usize as i64;
        let arr = aipl_array_new(n, drop_fn, str24::STR_SIZE as i64);
        let elems = arr_untag(arr).add(ARR_ELEMS_OFFSET) as *mut str24::Str;
        for i in 0..n as usize {
            let c = *argv.add(i + 1);
            let bytes = core::slice::from_raw_parts(c as *const u8, strlen(c));
            core::ptr::write(elems.add(i), str24::from_bytes(bytes));
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
    // An array is passed by the address of its value, `{block, first element,
    // length}`.
    let mut value: [i64; 3] = [0; 3];
    let args = if unsafe { __aipl_main_wants_args } != 0 {
        let block = unsafe { build_cli_args(argc, argv) };
        value[0] = block as i64;
        value[1] = unsafe { arr_untag(block).add(ARR_ELEMS_OFFSET) } as i64;
        value[2] = if argc > 1 { (argc - 1) as i64 } else { 0 };
        value.as_ptr() as *const u8
    } else {
        core::ptr::null()
    };
    // Perfmon (env-gated): time only the user `main` so process spawn / static
    // init are excluded. `getenv` is the sole cost when the var is unset.
    let perfmon_path = unsafe { getenv(b"AIPL_PERFMON_STATS\0".as_ptr() as *const c_char) };
    let perfmon = !perfmon_path.is_null();
    let t0 = if perfmon { perfmon_os::now_ns() } else { 0 };
    let code = unsafe { __aipl_user_main(args) as c_int };
    let teardown = CONST_TEARDOWN.load(core::sync::atomic::Ordering::Relaxed);
    if teardown != 0 {
        let release: extern "C" fn() = unsafe { core::mem::transmute(teardown as usize) };
        release();
    }
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
//   peak live bytes: <P>
//   instructions executed: <I>
//   functions:
//     <name>:               (one entry per function that ran, AIPL or builtin)
//       calls: <n>
// `aipl report`, `measure` and the cases harness read this back. What it writes
// is the report minus the two things only the compiler side can measure: the
// `binary size` block, and each function's `bytes:` line (read from the
// object's symbols, which is also where functions that were *emitted
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
        fputs(b"\npeak live bytes: \0".as_ptr() as *const c_char, f);
        fput_u64(f, PEAK_LIVE_BYTES.load(Ordering::Relaxed));
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

// ---------- `measure`'s counting window ----------
//
// `measure(..)` opens a window, runs the work, closes it, and reads the counters
// of the closed window. Open snapshots the allocation, byte and instruction
// tallies and the live-byte total, and restarts the peak from where live bytes
// stand — so the peak it reports is what the work added on top of what was
// already held, not an absolute that depends on what ran before. Close takes the
// differences.
//
// All of it exists only in the instrumented build, the one that counts anything;
// elsewhere the window opens and closes on nothing and every counter reads zero.
// Windows do not nest: `measure` is not reentrant, and a second `begin` before an
// `end` simply restarts the first.
#[cfg(aipl_instrument)]
static MEASURE_BASE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
#[cfg(aipl_instrument)]
static MEASURE_RESULT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

#[no_mangle]
pub extern "C" fn aipl_measure_begin() {
    #[cfg(aipl_instrument)]
    {
        MEASURE_BASE[0].store(ALLOC_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
        MEASURE_BASE[1].store(ALLOC_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
        MEASURE_BASE[2].store(INSN_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
        let live = LIVE_BYTES.load(Ordering::Relaxed);
        MEASURE_BASE[3].store(live, Ordering::Relaxed);
        PEAK_LIVE_BYTES.store(live, Ordering::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_measure_end() {
    #[cfg(aipl_instrument)]
    {
        let since = |now: &AtomicU64, base: usize| {
            now.load(Ordering::Relaxed)
                .saturating_sub(MEASURE_BASE[base].load(Ordering::Relaxed))
        };
        // The index `aipl_measure_read` takes: allocations, bytes allocated, peak
        // live bytes, instructions.
        MEASURE_RESULT[0].store(since(&ALLOC_COUNT, 0), Ordering::Relaxed);
        MEASURE_RESULT[1].store(since(&ALLOC_BYTES, 1), Ordering::Relaxed);
        MEASURE_RESULT[2].store(since(&PEAK_LIVE_BYTES, 3), Ordering::Relaxed);
        MEASURE_RESULT[3].store(since(&INSN_COUNT, 2), Ordering::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_measure_read(which: i64) -> i64 {
    #[cfg(aipl_instrument)]
    if (0..4).contains(&which) {
        return MEASURE_RESULT[which as usize].load(Ordering::Relaxed) as i64;
    }
    let _ = which;
    0
}

// ---------- Test-runner hooks ----------
//
// The `check` command's JIT runner is the authoritative test reporter (see the
// JIT runtime in `aipl-codegen`). These hooks exist for two other uses of an AOT
// build of a synthesized test driver:
//
//   - a *library* case's driver, which the cases harness builds to measure
//     its run for the baseline table: it only needs the pass/fail tally, and failures were
//     already diagnosed by `aipl check`, so by default nothing is printed;
//   - a file's `.performance` blocks, which `aipl check` must run in an
//     instrumented build (their counts exist nowhere else) and so cannot hand to
//     the JIT. There the failures are the point, so `check` sets
//     `AIPL_TEST_REPORT` and they are printed exactly as the JIT prints them.
//
// `AIPL_TEST_REPORT` turns reporting on; its value, when not empty, is the file
// label a batch run qualifies each header with (`test <file>::<name> ... FAIL`).
// `AIPL_TEST_TALLY` names a file the final `total failed` counts are written to,
// so the caller can fold them into its own aggregate.
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering as TestOrd};
static TEST_CUR_FAILED: AtomicBool = AtomicBool::new(false);
static TEST_HEADER_PRINTED: AtomicBool = AtomicBool::new(false);
static TEST_TOTAL: AtomicI64 = AtomicI64::new(0);
static TEST_FAILED: AtomicI64 = AtomicI64::new(0);

/// The running test's name, copied at `test_begin`: the argument points at the
/// caller's stack slot, which does not outlive the call. A name past the buffer is
/// cut short, which only affects a failure header's text.
const TEST_NAME_CAP: usize = 256;
static mut TEST_NAME: [u8; TEST_NAME_CAP] = [0; TEST_NAME_CAP];
static TEST_NAME_LEN: AtomicUsize = AtomicUsize::new(0);

/// `AIPL_TEST_REPORT`'s value, or null when failures are not to be reported.
fn test_report_label() -> *const c_char {
    unsafe { getenv(b"AIPL_TEST_REPORT\0".as_ptr() as *const c_char) }
}

fn out_bytes(bytes: &[u8]) {
    unsafe { write(1, bytes.as_ptr() as *const c_void, bytes.len()) };
}

fn out_str(s: *const str24::Str) {
    str24::for_each_chunk(unsafe { *s }, &mut |chunk| {
        out_bytes(chunk);
        true
    });
}

/// The `test <name> ... FAIL` header, once per failing test, qualified by the file
/// label when there is one — the same line the JIT runner prints.
fn print_fail_header(label: *const c_char) {
    if TEST_HEADER_PRINTED.swap(true, TestOrd::Relaxed) {
        return;
    }
    out_bytes(b"test ");
    if !label.is_null() && unsafe { *label } != 0 {
        out_bytes(unsafe { core::slice::from_raw_parts(label as *const u8, strlen(label)) });
        out_bytes(b"::");
    }
    let len = TEST_NAME_LEN.load(TestOrd::Relaxed);
    out_bytes(unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(TEST_NAME) as *const u8, len) });
    out_bytes(b" ... FAIL\n");
}

// The wide-ABI entry points. They borrow their `str` argument for the reason the
// `aipl_*` versions always have: an `aipl_*` entry point is handed the caller's
// 24-byte value, which the caller keeps alive.
#[no_mangle]
pub extern "C" fn aipl_test_begin(name: *const str24::Str) {
    count_builtin!(builtin_calls::AIPL_TEST_BEGIN);
    TEST_CUR_FAILED.store(false, TestOrd::Relaxed);
    TEST_HEADER_PRINTED.store(false, TestOrd::Relaxed);
    let mut len = 0usize;
    str24::for_each_chunk(unsafe { *name }, &mut |chunk| {
        let take = chunk.len().min(TEST_NAME_CAP - len);
        unsafe {
            core::ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                (core::ptr::addr_of_mut!(TEST_NAME) as *mut u8).add(len),
                take,
            )
        };
        len += take;
        true
    });
    TEST_NAME_LEN.store(len, TestOrd::Relaxed);
}

#[no_mangle]
pub extern "C" fn aipl_assert(cond: i64, loc: *const str24::Str) {
    count_builtin!(builtin_calls::AIPL_ASSERT);
    if cond == 0 {
        TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
        let label = test_report_label();
        if !label.is_null() {
            print_fail_header(label);
            out_bytes(b"  assert failed at ");
            out_str(loc);
            out_bytes(b"\n");
        }
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_fail(msg: *const str24::Str) {
    count_builtin!(builtin_calls::AIPL_TEST_FAIL);
    TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
    let label = test_report_label();
    if !label.is_null() {
        print_fail_header(label);
        out_bytes(b"  `?` propagated an error: ");
        out_str(msg);
        out_bytes(b"\n");
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_fail_none() {
    count_builtin!(builtin_calls::AIPL_TEST_FAIL_NONE);
    // `?` on a `none` inside a `.test`: mark the current test failed.
    TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
    let label = test_report_label();
    if !label.is_null() {
        print_fail_header(label);
        out_bytes(b"  `?` propagated a `none`\n");
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_end() {
    count_builtin!(builtin_calls::AIPL_TEST_END);
    TEST_TOTAL.fetch_add(1, TestOrd::Relaxed);
    if TEST_CUR_FAILED.load(TestOrd::Relaxed) {
        TEST_FAILED.fetch_add(1, TestOrd::Relaxed);
    }
}

#[no_mangle]
pub extern "C" fn aipl_test_summary() -> i64 {
    count_builtin!(builtin_calls::AIPL_TEST_SUMMARY);
    let failed = TEST_FAILED.load(TestOrd::Relaxed);
    let tally = unsafe { getenv(b"AIPL_TEST_TALLY\0".as_ptr() as *const c_char) };
    if !tally.is_null() {
        unsafe {
            let f = fopen(tally, b"wb\0".as_ptr() as *const c_char);
            if !f.is_null() {
                fput_u64(f, TEST_TOTAL.load(TestOrd::Relaxed) as u64);
                fputs(b" \0".as_ptr() as *const c_char, f);
                fput_u64(f, failed as u64);
                fputs(b"\n\0".as_ptr() as *const c_char, f);
                fclose(f);
            }
        }
    }
    i64::from(failed > 0)
}
