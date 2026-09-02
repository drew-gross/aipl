//! Cranelift JIT codegen.
//!
//! Types:
//!   - `i64` is the primitive integer.
//!   - `bool` (encoded 0/1 in an i64 at the ABI level).
//!   - Declared struct names — stack-allocated, passed by pointer.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
    rc::Rc,
};

// TEMPORARY: 24-byte-`str` ABI spike (STR_REPR.md). Delete with the file.
#[cfg(test)]
mod abi_spike;

// STAGED: the 24-byte `str` layout (STR_REPR.md stage 1), proven on its own
// before the switch wires it up. `str24` is **shared verbatim** with the AOT
// runtime, which `include!`s the same file, so it is `no_std`-safe and asks its
// host for two things: the allocator below, and the I/O in `str24_host`.
#[allow(dead_code)] // staged: wired up by the Stage 1 switch
mod str24;
mod str24_host;

/// Allocation for the shared `str` layout. Each runtime supplies its own, so
/// each keeps its own accounting: this side forwards to libc, and the AOT copy
/// tallies counts and bytes for `--- performance ---` under
/// `--cfg aipl_instrument`. Same names and signatures on both sides — that is
/// what lets `str24.rs` call them without knowing which runtime it is in.
unsafe fn rt_alloc(size: usize) -> *mut core::ffi::c_void {
    unsafe { libc_malloc(size) }
}

unsafe fn rt_free(ptr: *mut core::ffi::c_void) {
    unsafe { libc_free(ptr) }
}

unsafe fn rt_realloc(ptr: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void {
    unsafe { libc_realloc(ptr, size) }
}

extern "C" {
    #[link_name = "malloc"]
    fn libc_malloc(size: usize) -> *mut core::ffi::c_void;
    #[link_name = "free"]
    fn libc_free(ptr: *mut core::ffi::c_void);
    #[link_name = "realloc"]
    fn libc_realloc(ptr: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void;
}

use cranelift::{
    codegen::{
        cursor::{Cursor, FuncCursor},
        ir::{
            Block, BlockArg, ExternalName, FuncRef, Function, Inst, InstructionData, Signature,
            StackSlot, UserFuncName,
        },
        isa::{CallConv, TargetIsa},
        Context,
    },
    prelude::{
        settings, types, AbiParam, Configurable, FunctionBuilder, FunctionBuilderContext,
        InstBuilder, IntCC, MemFlagsData, StackSlotData, StackSlotKind, Value,
    },
};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

use aipl_syntax::{
    ast::{
        ConcreteType,
        Expr,
        ExprKind,
        Function as AstFn,
        Item,
        MatchArm,
        Param,
        Pattern,
        Primitive,
        Program,
        Signature as AstSignature,
        StructDecl,
        // The abstract representation, for the pre-monomorphization plumbing
        // codegen still owns: the checker's inputs, and `main`'s CLI-args
        // rewrite — both of which edit source signatures.
        Type,
    },
    concrete::{
        error_ty, flex_int_ty, is_array_elem, is_dict_key, is_error, is_int_ty, is_none_inner,
        is_set_elem, is_str_repr, is_unit, type_name,
    },
    IMPORTABLE_BUILTINS,
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

/// The AIPL `FmtError` struct, rebuilt as a spanned [`Error`]. Shared by every
/// formatter entry that can fail, so they decode the same shape the same way.
fn fmt_error_of(v: FfiValue) -> Error {
    let FfiValue::Struct(fields) = v else {
        panic!("dogfooded formatter: err side is not a struct: {v:?}");
    };
    let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    let msg = match get("message") {
        Some(FfiValue::Str(m)) => m.clone(),
        other => panic!("dogfooded formatter: FmtError.message: {other:?}"),
    };
    match get("span") {
        Some(FfiValue::Struct(sp)) => {
            let at = |k: &str| match sp.iter().find(|(n, _)| n == k) {
                Some((_, FfiValue::Int(v))) => *v as usize,
                other => panic!("dogfooded formatter: Span.{k}: {other:?}"),
            };
            Error::at(msg, at("start")..at("end"))
        }
        other => panic!("dogfooded formatter: FmtError.span: {other:?}"),
    }
}

/// The refcount cell of any heap block (string or array): the word at `-8`.
unsafe fn header_of(ptr: *const u8) -> *mut i64 {
    unsafe { ptr.sub(HEADER_SIZE) as *mut i64 }
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
const INLINE_TAG: usize = 0b01;

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

/// Pack <= 7 content bytes into an inline str value (`bytes.len()` must be <= 7).
fn pack_inline(bytes: &[u8]) -> *const u8 {
    debug_assert!(bytes.len() <= 7);
    let mut val: u64 = ((bytes.len() as u64) << 2) | 1;
    for (i, &b) in bytes.iter().enumerate() {
        val |= (b as u64) << (8 * (i + 1));
    }
    val as usize as *const u8
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

/// `list_files(dir) -> str[]?` runtime: every file at or below `dir`, walked
/// recursively, as a fresh `str[]` of `dir`-prefixed paths — or null on
/// any failure (unreadable directory, non-UTF-8 name, unknowable entry kind),
/// which codegen wraps into `err`. Order is whatever the filesystem yields;
/// callers that need determinism sort. A directory is descended into rather than
/// listed, and a symlink counts as a file (`file_type` doesn't follow it), so the
/// walk can't cycle. Decrements `dir` per the refcount protocol (callers pre-inc,
/// as with any str-taking fn). Mirrors `aipl_list_files` in the linker runtime.
/// `list_files(dir)` under the wide ABI: a `str[]` of 24-byte elements, or null
/// on failure. Only the array half is per-runtime — the directory walk is shared
/// with the tagged version. Borrows `dir`.
#[no_mangle]
extern "C" fn aipl_list_files(dir: *const str24::Str) -> *const u8 {
    let mut scratch = [0u8; str24::INLINE_CAP];
    let dv = unsafe { *dir };
    let Ok(root) = std::str::from_utf8(dv.bytes(&mut scratch)) else {
        return std::ptr::null();
    };
    let mut files: Vec<String> = Vec::new();
    if walk_dir(root, &mut files).is_err() {
        return std::ptr::null();
    }
    let drop_fn = str24::aipl_arr_drop_str as *const () as usize as i64;
    let arr = aipl_array_new(files.len() as i64, drop_fn, str24::STR_SIZE as i64);
    unsafe {
        let elems = arr.add(ARR_ELEMS_OFFSET) as *mut str24::Str;
        for (i, f) in files.iter().enumerate() {
            std::ptr::write(elems.add(i), str24::from_bytes(f.as_bytes()));
        }
    }
    arr
}

/// Append every file under `dir` (recursively) to `out` as `dir`-prefixed paths.
/// `Err(())` for anything that leaves the walk incomplete rather than silently
/// short: an unreadable directory, a name that isn't UTF-8, or an entry whose
/// kind the filesystem won't report (`file_type` falls back to `lstat` when the
/// directory entry itself says "unknown", and only fails if that fails too).
fn walk_dir(dir: &str, out: &mut Vec<String>) -> Result<(), ()> {
    for entry in std::fs::read_dir(dir).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(())?;
        let path = join_path(dir, name);
        if entry.file_type().map_err(|_| ())?.is_dir() {
            walk_dir(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// `dir` and `name` joined with a single `/` (a `dir` that already ends in one
/// isn't given a second). Mirrors the linker runtime's path building.
fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `now_nanos() -> u64` runtime: wall-clock nanoseconds since the Unix epoch,
/// carried on the shared `i64` ABI (a `u64` occupies the same 8-byte slot, so
/// the bit pattern is the value — it stays positive until the year 2554). A
/// clock set before the epoch reads as 0 rather than wrapping. Takes nothing and
/// owns nothing, so there is no refcount traffic. Mirrors `aipl_now_nanos` in
/// the linker runtime.
#[no_mangle]
extern "C" fn aipl_now_nanos() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as u64 as i64,
        Err(_) => 0, // clock is before the epoch
    }
}

/// `monotonic_now() -> u64` runtime: nanoseconds from the system's monotonic
/// clock, which only counts up — so a difference between two readings is a real
/// elapsed duration. The origin is unspecified (the kernel's, typically boot),
/// so an absolute reading means nothing on its own. Read through `clock_gettime`
/// rather than `std::time::Instant` because `Instant` has no way to yield an
/// absolute count, and because it puts this runtime and the linker's on the
/// exact same clock. Mirrors `aipl_monotonic_now` in the linker runtime.
#[no_mangle]
extern "C" fn aipl_monotonic_now() -> i64 {
    monotonic_nanos()
}

#[cfg(unix)]
fn monotonic_nanos() -> i64 {
    // See the linker runtime's `now_nanos_impl` for why this is `clock_gettime`
    // (a `timespec` is two 64-bit words everywhere) rather than `gettimeofday`.
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    extern "C" {
        fn clock_gettime(clk_id: std::ffi::c_int, tp: *mut Timespec) -> std::ffi::c_int;
    }
    // `CLOCK_MONOTONIC`'s value is per-platform: 6 on Darwin, 1 on Linux.
    #[cfg(target_os = "macos")]
    const CLOCK_MONOTONIC: std::ffi::c_int = 6;
    #[cfg(not(target_os = "macos"))]
    const CLOCK_MONOTONIC: std::ffi::c_int = 1;

    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0; // unreadable clock
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64) as i64
}

#[cfg(not(unix))]
fn monotonic_nanos() -> i64 {
    0
}

// ---------- Effect shims ----------
//
// A `shim <effect> { op = f, .. } { body }` installs `f`'s address into the slot
// belonging to `op` for the dynamic extent of `body`, restoring the previous
// occupant afterwards. Every shimmable operation compiles to "load my slot; call
// it if non-zero, else call the real runtime fn", so a shim reaches every call
// at any depth — including through function values and recursion — without the
// callees knowing anything about it.
//
// One flat slot array indexed by `aipl_syntax::shim_slot_index` is the whole
// mechanism; nothing here names a particular effect. Save/restore is the
// caller's job (codegen emits it), which is what makes shims nest.

/// Installed shim addresses, one slot per shimmable operation. 0 means "no shim
/// installed" — the state every slot starts in and returns to. Mirrors
/// `SHIM_SLOTS` in the linker runtime.
static SHIM_SLOTS: [std::sync::atomic::AtomicI64; aipl_syntax::SHIM_SLOT_COUNT] =
    [const { std::sync::atomic::AtomicI64::new(0) }; aipl_syntax::SHIM_SLOT_COUNT];

/// The shim installed for slot `idx`, or 0 if none. An out-of-range index reads
/// as "no shim" rather than trapping (codegen only ever emits valid indices).
#[no_mangle]
extern "C" fn aipl_shim_get(idx: i64) -> i64 {
    SHIM_SLOTS
        .get(idx as usize)
        .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
}

/// Install `ptr` (0 to clear) as the shim for slot `idx`.
#[no_mangle]
extern "C" fn aipl_shim_set(idx: i64, ptr: i64) {
    if let Some(slot) = SHIM_SLOTS.get(idx as usize) {
        slot.store(ptr, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The wide `ExecResult!Error` layout, written by hand because the runtime has
/// no layout table: tag, then either the `Error` message or the `ExecResult`
/// struct `{ stdout, stderr, exit_code }`. Both start at `OPT_VALUE_OFFSET` and
/// every `str` is a whole value, so the offsets are multiples of
/// `str24::STR_SIZE` rather than of a word. Must agree with what `field_size`
/// computes for the same types — that is what codegen reads it back with.
fn write_err_wide(out: *mut u8, message: &[u8]) {
    unsafe {
        *(out as *mut i64) = 0;
        std::ptr::write(
            out.add(OPT_VALUE_OFFSET as usize) as *mut str24::Str,
            str24::from_bytes(message),
        );
    }
}

fn write_ok_wide(out: *mut u8, stdout: &[u8], stderr: &[u8], exit_code: i64) {
    unsafe {
        *(out as *mut i64) = 1;
        let base = out.add(OPT_VALUE_OFFSET as usize);
        std::ptr::write(base as *mut str24::Str, str24::from_bytes(stdout));
        std::ptr::write(
            base.add(str24::STR_SIZE) as *mut str24::Str,
            str24::from_bytes(stderr),
        );
        *(base.add(2 * str24::STR_SIZE) as *mut i64) = exit_code;
    }
}

/// `execute_program` under the wide ABI. Only the marshalling differs from
/// `execute_program_impl` — the program name and each argument are whole `str`
/// values rather than words, and the result's `str` fields likewise — so the
/// spawn itself is shared through `run_program`.
#[no_mangle]
extern "C" fn aipl_execute_program(out: *mut u8, program: *const str24::Str, args: *const u8) {
    let mut scratch = [0u8; str24::INLINE_CAP];
    let pv = unsafe { *program };
    let Ok(program) = std::str::from_utf8(pv.bytes(&mut scratch)) else {
        return write_err_wide(out, b"could not execute program");
    };
    let program = program.to_string();
    let mut arg_strings: Vec<String> = Vec::new();
    if !args.is_null() {
        let len = unsafe { array_len_of(args) };
        let elems = unsafe { args.add(ARR_ELEMS_OFFSET) as *const str24::Str };
        for i in 0..len {
            let ev = unsafe { std::ptr::read(elems.add(i)) };
            let mut buf = [0u8; str24::INLINE_CAP];
            let Ok(s) = std::str::from_utf8(ev.bytes(&mut buf)) else {
                return write_err_wide(out, b"could not execute program");
            };
            arg_strings.push(s.to_string());
        }
    }
    match run_program(&program, &arg_strings) {
        Some((stdout, stderr, code)) => write_ok_wide(out, &stdout, &stderr, code),
        None => write_err_wide(out, b"could not execute program"),
    }
}

/// Spawn `program` with `args` and collect its output. `None` if it could not be
/// launched, or if either stream holds a NUL byte (which a NUL-terminated `str`
/// cannot represent — the same constraint `read_file_to_string` has).
fn run_program(program: &str, args: &[String]) -> Option<(Vec<u8>, Vec<u8>, i64)> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if output.stdout.contains(&0) || output.stderr.contains(&0) {
        return None;
    }
    let code = i64::from(output.status.code().unwrap_or(-1));
    Some((output.stdout, output.stderr, code))
}

// ---------- One-allocation `to_str` primitives ----------
//
// `to_str` renders in two passes: a measure pass sums the total byte length, the
// result buffer is allocated once (`aipl_str_alloc`), then a write pass fills it
// via a moving cursor. These are the cursor primitives; everything structural
// (brackets, separators, labels) is emitted in IR.

/// Decimal byte length of `n` (with a leading `-` for negatives). Must agree,
/// byte-for-byte, with `aipl_write_i64`.
#[no_mangle]
extern "C" fn aipl_i64_len(n: i64) -> i64 {
    let mut buf = [0u8; 24];
    fmt_i64(&mut buf, n) as i64
}

/// Write `n`'s decimal representation at `dst`; return the advanced cursor.
#[no_mangle]
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
#[no_mangle]
extern "C" fn aipl_u64_len(n: i64) -> i64 {
    let mut buf = [0u8; 24];
    fmt_u64(&mut buf, n as u64) as i64
}

/// Write `n` (interpreted as unsigned) in decimal at `dst`; return the cursor.
#[no_mangle]
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

/// Copy `n` bytes `src` → `dst`; return the advanced cursor.
#[no_mangle]
extern "C" fn aipl_write_bytes(dst: *const u8, src: *const u8, n: i64) -> *const u8 {
    let n = n.max(0) as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(src, dst as *mut u8, n);
        dst.add(n)
    }
}

/// How [`aipl_arr_sort`] orders its elements. Every `ord` element type is an
/// 8-byte scalar, so the only question is how to read the word: as a signed
/// integer, as an unsigned one (`u8`..`u64`, and `char` — a byte value), or as a
/// `str` pointer to compare lexicographically by bytes.
const SORT_KIND_SIGNED: i64 = 0;
const SORT_KIND_UNSIGNED: i64 = 1;
const SORT_KIND_STR: i64 = 2;

/// `xs.sort() -> T[]` — a fresh array with the same elements in ascending order.
/// Consumes `xs` (callers pre-inc) and co-owns the elements it copies.
///
/// Unlike [`aipl_arr_reverse`] this cannot be a lazy view: the order isn't a
/// function of the index, so the result has to be materialized. Only `ord`
/// element types reach here and all of them are 8-byte scalars, so the block is
/// a flat run of words and sorting is a permutation of those words — which moves
/// no ownership, hence the single blanket retain before sorting.
///
/// Mirrors `aipl_arr_sort` in the linker runtime.
#[no_mangle]
extern "C" fn aipl_arr_sort(
    a: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
    kind: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    if a.is_null() {
        return a;
    }
    let len = unsafe { array_len_of(a) };
    let raw = alloc_array(len, len, drop_fn, elem_size);
    unsafe {
        let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
        if len > 0 {
            let src = a.add(ARR_ELEMS_OFFSET);
            std::ptr::copy_nonoverlapping(src, dst, len * elem_size.max(8) as usize);
            // The new array co-owns every element. Done before the sort because
            // reordering elements neither creates nor destroys a reference.
            elem_rc(retain_fn, dst, len);
            // A wide `str` element is the whole 24-byte value, not a word, so it
            // is both moved and compared differently — `sort_words` would
            // reorder thirds of values and compare their first fields.
            if kind == SORT_KIND_STR && elem_size == str24::STR_SIZE as i64 {
                let vals = std::slice::from_raw_parts_mut(dst as *mut str24::Str, len);
                str24::sort_values(vals);
            } else {
                let words = std::slice::from_raw_parts_mut(dst as *mut i64, len);
                sort_words(words, kind);
            }
        }
    }
    aipl_array_dec(a);
    raw
}

/// Order `words` in place by [`SORT_KIND_SIGNED`]/`_UNSIGNED`/`_STR`. Shared by
/// both runtimes' `aipl_arr_sort`; kept byte-for-byte identical, which is why it
/// uses `sort_unstable_by` — the stable sorts need an allocation, and the linker
/// runtime is `#![no_std]`. Unstable is enough here: `ord` elements are compared
/// by value, so equal ones are indistinguishable.
fn sort_words(words: &mut [i64], kind: i64) {
    match kind {
        SORT_KIND_UNSIGNED => words.sort_unstable_by(|x, y| (*x as u64).cmp(&(*y as u64))),
        // SORT_KIND_SIGNED, and anything unexpected (codegen only ever emits the
        // three above).
        _ => words.sort_unstable(),
    }
}

/// `xs.reverse() -> T[]` — O(1): returns a reversed-view repr wrapping `xs`.
/// Transfers ownership of `xs` into the view (no drop, no retain).
/// `drop_fn`, `retain_fn`, `elem_size` describe the element type.
#[no_mangle]
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

/// `xs[start..end]` — array slice. Both bounds are clamped to `[0, len]` (an
/// out-of-range end yields a shorter array; `start >= end` yields `[]`).
/// *Borrows* `xs` (does not drop it) and returns a fresh heap array holding
/// copies of the elements in `[start, end)`, each retained via `retain_fn`
/// (0 for scalar elements).
#[no_mangle]
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

/// `join(parts: str[], sep: str) -> str` — concatenate the parts with `sep`
/// between consecutive elements (`[]` -> `""`, `[x]` -> `x`). Two passes: measure
/// the total length, then fill a single fresh buffer (inline when <= 7 bytes).
/// Consumes both args (the array drop releases its element strings), like the
/// other str builtins.
/// `split` under the wide ABI. Only the array half lives here — where the cuts
/// fall is `str24::for_each_split`, shared with the linker runtime, because that
/// is the part that would diverge silently. Two passes over the same shared
/// splitter: count, allocate, fill.
///
/// Each part is a window into `s`, retained because the array owns it. Borrows
/// both arguments, like every `aipl_*` entry point.
#[no_mangle]
extern "C" fn aipl_str_split(s: *const str24::Str, sep: *const str24::Str) -> *const u8 {
    let (sv, sepv) = unsafe { (*s, *sep) };
    let mut count = 0usize;
    str24::for_each_split(sv, sepv, &mut |_| count += 1);
    let drop_fn = str24::aipl_arr_drop_str as *const () as usize as i64;
    let arr = aipl_array_new(count as i64, drop_fn, str24::STR_SIZE as i64);
    let elems = unsafe { arr.add(ARR_ELEMS_OFFSET) as *mut str24::Str };
    let mut k = 0usize;
    str24::for_each_split(sv, sepv, &mut |part| {
        part.retain();
        unsafe { std::ptr::write(elems.add(k), part) };
        k += 1;
    });
    arr
}

/// `join` under the wide ABI — the array half only; the streaming is
/// `str24::join_from`. Borrows the array and the separator.
#[no_mangle]
extern "C" fn aipl_str_join(out: *mut str24::Str, arr: *const u8, sep: *const str24::Str) {
    let sepv = unsafe { *sep };
    unsafe {
        let len = array_len_of(arr);
        let elems = arr_untag(arr).add(ARR_ELEMS_OFFSET) as *const str24::Str;
        *out = str24::join_from(elems, len, sepv);
    }
}

/// Bytes codegen reserves for a `for (let c : s)` cursor, for whichever
/// representation is active. The two iterators carry different state — the wide
/// one caches a whole 24-byte leaf where the tagged one caches a pointer plus an
/// 8-byte spill — so the slot follows the ABI rather than a fixed constant.
fn iter_state_size() -> u32 {
    str24::ITER_SIZE as u32
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
#[no_mangle]
extern "C" fn aipl_arr_elem_ptr(a: *const u8, idx: i64, elem_size: i64) -> *const u8 {
    unsafe { arr_elem_ptr(a, idx as usize, elem_size as usize) }
}

/// Repr-aware bit load for JIT-compiled code. Returns 0 or 1.
#[no_mangle]
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
#[no_mangle]
extern "C" fn aipl_array_new(len: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    let len = len.max(0) as usize;
    alloc_array(len, len, drop_fn, elem_size)
}

/// Allocate an empty array (len 0, refcount 1) reserved to `cap` slots of
/// `elem_size` bytes with the given element `drop_fn`. Used by `map`/`filter` to
/// pre-size their output. Mirrors `aipl_array_with_cap` in the linker runtime.
#[no_mangle]
extern "C" fn aipl_array_with_cap(cap: i64, drop_fn: i64, elem_size: i64) -> *const u8 {
    alloc_array(0, cap.max(0) as usize, drop_fn, elem_size)
}

/// Decrement an array's refcount; at zero, release each element via the
/// stored `drop_fn` (if any) and free the block (sized by its byte-capacity).
#[no_mangle]
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
#[no_mangle]
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

// ---------- Recursive (boxed) type runtime ----------
//
// A value of a *recursive* struct/variant type (one whose declaration reaches
// itself through struct fields, variant payloads, or optional/result cores) is
// heap-allocated behind a pointer — stored inline it would have infinite size.
// The value points at the *payload*, which has exactly the type's usual inline
// layout (so every payload read is shared with the inline path); a four-word
// header sits just below it:
//
//   [strong: i64][weak: i64][drop_fn: ptr][payload size: i64][payload...]
//                                                             ^ value
//
// Two counts, split by who holds the reference:
//   - `strong` counts *external* references: locals, array elements, fields of
//     non-recursive types — every owner outside the value's own recursion
//     group.
//   - `weak` counts *internal* references: a field of one boxed value of the
//     group pointing at another of the same group (a Cons node's tail).
//
// Dropping an external reference decrements `strong`; a block whose counts are
// both zero is unreachable and is freed, releasing its payload via the stored
// `drop_fn` (a generated per-type function that calls `aipl_rec_dec_weak` on
// same-group children and the normal drops on everything else) — so death
// cascades through any contained values that become unreachable in turn. A
// value still held by a live parent (weak > 0) survives the death of its last
// external reference and dies when the parent releases it. All AIPL mutation
// is copy-and-modify, so the reference graph is acyclic and this cascade
// reclaims every unreachable node.
//
// The cascade is iterative, not natively recursive (a million-node list must
// not need a million stack frames): a dead block is pushed onto a thread-local
// pending list — its `strong` word, finished at that point, is reused as the
// intrusive next link — and only the outermost release call drains the list.
// Mirrors the linker runtime.

const REC_HEADER_SIZE: usize = 32;
const REC_WEAK_WORD: usize = 1;
const REC_DROPFN_WORD: usize = 2;
const REC_SIZE_WORD: usize = 3;
type RecDropFn = extern "C" fn(*const u8);

/// The header words of the block behind payload pointer `p` (word 0 = strong).
fn rec_block(p: *const u8) -> *mut i64 {
    unsafe { p.sub(REC_HEADER_SIZE) as *mut i64 }
}

fn rec_layout(payload_size: usize) -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(REC_HEADER_SIZE + payload_size, std::mem::align_of::<i64>())
        .expect("rec block layout")
}

/// Allocate a boxed recursive-type value with a `size`-byte payload (strong 1,
/// weak 0), returning the payload pointer. Codegen stores the tag/fields
/// immediately after.
#[no_mangle]
extern "C" fn aipl_rec_alloc(size: i64, drop_fn: i64) -> *const u8 {
    let size = size.max(0) as usize;
    let layout = rec_layout(size);
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    let b = raw as *mut i64;
    unsafe {
        std::ptr::write(b, 1); // strong
        std::ptr::write(b.add(REC_WEAK_WORD), 0);
        std::ptr::write(b.add(REC_DROPFN_WORD), drop_fn);
        std::ptr::write(b.add(REC_SIZE_WORD), size as i64);
        raw.add(REC_HEADER_SIZE)
    }
}

#[no_mangle]
extern "C" fn aipl_rec_inc_strong(p: *const u8) {
    unsafe { *rec_block(p) += 1 }
}

#[no_mangle]
extern "C" fn aipl_rec_inc_weak(p: *const u8) {
    unsafe { *rec_block(p).add(REC_WEAK_WORD) += 1 }
}

#[no_mangle]
extern "C" fn aipl_rec_dec_strong(p: *const u8) {
    let b = rec_block(p);
    unsafe {
        *b -= 1;
        if *b == 0 && *b.add(REC_WEAK_WORD) == 0 {
            rec_release(b);
        }
    }
}

#[no_mangle]
extern "C" fn aipl_rec_dec_weak(p: *const u8) {
    let b = rec_block(p);
    unsafe {
        *b.add(REC_WEAK_WORD) -= 1;
        if *b == 0 && *b.add(REC_WEAK_WORD) == 0 {
            rec_release(b);
        }
    }
}

thread_local! {
    /// Dead boxed blocks awaiting drop+free (intrusive list through the
    /// `strong` word), and whether a drain loop is already running below us on
    /// the native stack.
    static REC_PENDING: std::cell::Cell<*mut i64> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    static REC_DRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Queue the dead block `b` (both counts zero) and, unless a drain is already
/// running further down the stack, drain the queue: call each block's
/// `drop_fn` on its payload (which may queue more dead blocks — that's the
/// cascade) and free it.
fn rec_release(b: *mut i64) {
    REC_PENDING.with(|l| {
        unsafe { *b = l.get() as i64 };
        l.set(b);
    });
    if REC_DRAINING.with(std::cell::Cell::get) {
        return;
    }
    REC_DRAINING.with(|d| d.set(true));
    loop {
        let head = REC_PENDING.with(|l| {
            let h = l.get();
            if !h.is_null() {
                l.set(unsafe { *h } as *mut i64);
            }
            h
        });
        if head.is_null() {
            break;
        }
        unsafe {
            let drop_fn = *head.add(REC_DROPFN_WORD);
            if drop_fn != 0 {
                let f: RecDropFn = std::mem::transmute(drop_fn);
                f((head as *const u8).add(REC_HEADER_SIZE));
            }
            let size = *head.add(REC_SIZE_WORD) as usize;
            std::alloc::dealloc(head as *mut u8, rec_layout(size));
        }
    }
    REC_DRAINING.with(|d| d.set(false));
}

/// Copy-and-grow push (value semantics): a fresh array of `a`'s elements plus
/// the element at `x` (`elem_size` bytes), then drop `a`. Used when the array
/// may be aliased. `retain_fn` retains the copied elements (the new array
/// co-owns them).
#[no_mangle]
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
#[no_mangle]
extern "C" fn aipl_array_push_mut(
    a: *const u8,
    x: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    // A `STATIC_REFCOUNT` block belongs to someone else — an array the *host*
    // lent us as an FFI argument (see [`ArgBufs::array_block`]) and frees itself
    // after the call — so it can neither be written into nor `realloc`ed here.
    // Copy instead, exactly as `aipl_concat_mut` does for a static string
    // literal.
    //
    // Reaching this needs a host array to become an exclusive `mut` binding,
    // which today it can't: `exclusive` wants a fresh array literal or a
    // *moved-in* parameter, and a moved-in parameter is a separate `$own`
    // instance that the host never calls (an FFI entry is always the borrow
    // form). The check keeps that a property of the runtime rather than of an
    // invariant maintained two crates away in the monomorphizer.
    if !a.is_null() && unsafe { *header_of(a) } == STATIC_REFCOUNT {
        return aipl_array_push(a, x, drop_fn, retain_fn, elem_size);
    }
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

/// Make `a` uniquely owned with room for `extra` more elements, so the appends
/// an array-literal spread emits afterwards are plain in-place writes. Two
/// cases, mirroring `aipl_set_union_mut` vs `aipl_set_union`:
///
/// - refcount 1 (nothing else can observe the block): keep it. Already-large
///   enough capacity is returned untouched; otherwise `realloc` to exactly what
///   is needed — the header and elements move with the block, so no element is
///   re-retained and there is no old block to free.
/// - shared: allocate one right-sized block, copy the elements in and retain
///   each (the source keeps its own refs), then release the input.
///
/// Either way the result has refcount 1 and capacity for `old_len + extra`, so
/// `aipl_array_push_mut` / `aipl_arr_extend` can write straight into it. Sizing
/// is exact rather than doubling: the spread knows its final length up front.
/// Mirrors `aipl_arr_reserve` in the linker runtime.
#[no_mangle]
extern "C" fn aipl_arr_reserve(
    a: *const u8,
    extra: i64,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let a = aipl_arr_ensure_heap(a);
    let elem_size = elem_size.max(8) as usize;
    let old_len = if a.is_null() {
        0
    } else {
        unsafe { array_len_of(a) }
    };
    let want = old_len + extra.max(0) as usize;
    let need_bytes = want * elem_size;
    if !a.is_null() {
        let (cap_bytes, rc) = unsafe { (array_cap_bytes_of(a), *header_of(a)) };
        if rc == 1 {
            if need_bytes <= cap_bytes {
                unsafe { std::ptr::write(a.add(ARR_DROPFN_OFFSET) as *mut i64, drop_fn) };
                return a;
            }
            unsafe {
                let block = a.sub(HEADER_SIZE) as *mut u8;
                let old_layout = std::alloc::Layout::from_size_align(
                    array_block_size(cap_bytes),
                    std::mem::align_of::<i64>(),
                )
                .expect("array layout");
                let raw = std::alloc::realloc(block, old_layout, array_block_size(need_bytes));
                if raw.is_null() {
                    std::alloc::handle_alloc_error(old_layout);
                }
                let data = raw.add(HEADER_SIZE);
                std::ptr::write(data.add(ARR_CAP_OFFSET) as *mut i64, need_bytes as i64);
                std::ptr::write(data.add(ARR_DROPFN_OFFSET) as *mut i64, drop_fn);
                return data as *const u8;
            }
        }
    }
    let raw = alloc_array(old_len, want.max(1), drop_fn, elem_size as i64);
    unsafe {
        let dst = raw.add(ARR_ELEMS_OFFSET) as *mut u8;
        if old_len > 0 && !a.is_null() {
            let src = a.add(ARR_ELEMS_OFFSET);
            std::ptr::copy_nonoverlapping(src, dst, old_len * elem_size);
            elem_rc(retain_fn, dst, old_len);
        }
    }
    aipl_array_dec(a);
    raw
}

/// Append every element of `src` to `dst`, which `aipl_arr_reserve` has already
/// made uniquely owned and large enough — so this is a single `memcpy` plus one
/// retain pass, never a reallocation. Consumes (decs) `src`; `dst` keeps its
/// identity. Mirrors `aipl_arr_extend` in the linker runtime.
#[no_mangle]
extern "C" fn aipl_arr_extend(
    dst: *const u8,
    src: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let src_heap = aipl_arr_ensure_heap(src);
    if elem_size == ELEM_BITPACKED {
        // Bit-packed `bool[]` opts out of the pre-sized path (`reserve` is a
        // no-op for it), so `dst` may be shared: append one bit at a time with
        // the *copying* push, which is correct either way.
        let add = if src_heap.is_null() {
            0
        } else {
            unsafe { array_len_of(src_heap) }
        };
        let mut out = dst;
        for i in 0..add {
            let bit = i64::from(aipl_arr_load_bit(src_heap, i as i64) != 0);
            out = aipl_array_push(
                out,
                &bit as *const i64 as *const u8,
                drop_fn,
                retain_fn,
                ELEM_BITPACKED,
            );
        }
        aipl_array_dec(src_heap);
        return out;
    }
    let elem_size = elem_size.max(8) as usize;
    let add = if src_heap.is_null() {
        0
    } else {
        unsafe { array_len_of(src_heap) }
    };
    if add == 0 || dst.is_null() {
        aipl_array_dec(src_heap);
        return dst;
    }
    unsafe {
        let dst_len = array_len_of(dst);
        let elems = dst.add(ARR_ELEMS_OFFSET) as *mut u8;
        let at = elems.add(dst_len * elem_size);
        let from = src_heap.add(ARR_ELEMS_OFFSET);
        std::ptr::copy_nonoverlapping(from, at, add * elem_size);
        elem_rc(retain_fn, at, add);
        std::ptr::write(dst as *mut i64, (dst_len + add) as i64);
    }
    aipl_array_dec(src_heap);
    dst
}

/// Join a `T[][]` into a `T[]`, placing `sep`'s elements between consecutive
/// parts. The array counterpart of `aipl_str_join`, and native for the same
/// reason: the output length is known before anything is written — every part's
/// length is O(1) and the separator appears exactly `n - 1` times — so the whole
/// result is *one* exact-size allocation with no growth chain, and each part
/// moves as a single block copy.
///
/// `drop_fn`/`retain_fn`/`elem_size` describe the *inner* element `T`, not the
/// parts. Consumes (decs) both `parts` and `sep`, as `aipl_str_join` does.
/// Mirrors `aipl_arr_join` in the linker runtime.
#[no_mangle]
extern "C" fn aipl_arr_join(
    parts: *const u8,
    sep: *const u8,
    drop_fn: i64,
    retain_fn: i64,
    elem_size: i64,
) -> *const u8 {
    let parts_heap = aipl_arr_ensure_heap(parts);
    let sep_heap = aipl_arr_ensure_heap(sep);
    let n = if parts_heap.is_null() {
        0
    } else {
        unsafe { array_len_of(parts_heap) }
    };
    let sep_len = if sep_heap.is_null() {
        0
    } else {
        unsafe { array_len_of(sep_heap) }
    };
    // A bit-packed `bool[]` has no byte-addressable elements to memcpy, so it
    // falls back to appending one bit at a time through the copying push. Rare
    // enough not to be worth a packed fast path; correct either way.
    if elem_size == ELEM_BITPACKED {
        let mut out = aipl_array_new(0, drop_fn, ELEM_BITPACKED);
        for i in 0..n {
            if i > 0 {
                for k in 0..sep_len {
                    let bit = i64::from(aipl_arr_load_bit(sep_heap, k as i64) != 0);
                    out = aipl_array_push(
                        out,
                        &bit as *const i64 as *const u8,
                        drop_fn,
                        retain_fn,
                        ELEM_BITPACKED,
                    );
                }
            }
            let part = unsafe { part_at(parts_heap, i) };
            let plen = if part.is_null() {
                0
            } else {
                unsafe { array_len_of(part) }
            };
            for k in 0..plen {
                let bit = i64::from(aipl_arr_load_bit(part, k as i64) != 0);
                out = aipl_array_push(
                    out,
                    &bit as *const i64 as *const u8,
                    drop_fn,
                    retain_fn,
                    ELEM_BITPACKED,
                );
            }
            aipl_array_dec(part);
        }
        aipl_array_dec(parts_heap);
        aipl_array_dec(sep_heap);
        return out;
    }
    let esz = elem_size.max(8) as usize;
    // Measure first — this is the whole point of the builtin being native.
    let mut total = sep_len * n.saturating_sub(1);
    let mut lens: Vec<(usize, *const u8)> = Vec::with_capacity(n);
    for i in 0..n {
        let part = unsafe { part_at(parts_heap, i) };
        let plen = if part.is_null() {
            0
        } else {
            unsafe { array_len_of(part) }
        };
        total += plen;
        lens.push((plen, part));
    }
    // One allocation, sized exactly.
    let out = alloc_array(total, total.max(1), drop_fn, elem_size);
    unsafe {
        let dst = out.add(ARR_ELEMS_OFFSET) as *mut u8;
        let mut pos = 0usize;
        for (i, (plen, part)) in lens.iter().enumerate() {
            if i > 0 && sep_len > 0 {
                let from = sep_heap.add(ARR_ELEMS_OFFSET);
                std::ptr::copy_nonoverlapping(from, dst.add(pos * esz), sep_len * esz);
                pos += sep_len;
            }
            if *plen > 0 {
                let from = part.add(ARR_ELEMS_OFFSET);
                std::ptr::copy_nonoverlapping(from, dst.add(pos * esz), plen * esz);
                pos += plen;
            }
        }
        // Every element was copied by value; the result co-owns each one.
        elem_rc(retain_fn, dst, total);
    }
    for (_, part) in lens {
        aipl_array_dec(part);
    }
    aipl_array_dec(parts_heap);
    aipl_array_dec(sep_heap);
    out
}

/// Part `i` of a `T[][]`, materialized to a heap array the caller owns. The
/// extra `inc` before `aipl_arr_ensure_heap` is what makes both representations
/// balance: a heap part comes back as itself with the borrowed reference turned
/// into an owned one, and a reversed view is consumed and replaced by a fresh
/// array — either way the caller releases exactly one reference.
unsafe fn part_at(parts: *const u8, i: usize) -> *const u8 {
    let elems = unsafe { parts.add(ARR_ELEMS_OFFSET) as *const i64 };
    let p = unsafe { std::ptr::read(elems.add(i)) } as *const u8;
    if p.is_null() {
        return p;
    }
    aipl_arr_inc(p);
    aipl_arr_ensure_heap(p)
}

/// Executed-instruction counter hook. Codegen emits one call per basic block
/// (arg = the block's instruction count). The JIT path never reports perf
/// counts (those come from the AOT instrumented runtime), so this is a no-op
/// here — it exists only so the symbol resolves for JIT-run programs.
#[no_mangle]
extern "C" fn aipl_count_insns(_n: i64) {}

/// Per-function call counter hook (arg = a pointer to the function's
/// NUL-terminated symbol name). A no-op here for the same reason as
/// `aipl_count_insns`: the JIT never instruments, so this only has to resolve.
#[no_mangle]
extern "C" fn aipl_count_call(_name: i64) {}

// ---------- Test-runner runtime ----------
//
// The `check` command JIT-runs a synthesized `__test_main` driver: for each
// function with a `.test({ .. })` body it emits `__test_begin(name)` /
// `__test$<fn>()` / `__test_end()`, then `__test_summary()` as the driver's
// (exit-code) result. `__assert(cond, loc)` records and reports each failure.
// State is process-global (the driver runs single-threaded in-process). Only
// failures print: a failing test gets one `test <name> ... FAIL` header line
// followed by an indented line per failed assertion; passing tests are silent.
//
// `check`'s batch mode runs one such driver per file in a single process, so the
// counters here deliberately accumulate across drivers: [`set_test_file`] adds
// the file to each FAIL header and [`set_quiet_summary`] suppresses the per-file
// summary line, leaving one aggregate that [`test_totals`] reports at the end.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering as TestOrd};
use std::sync::Mutex;

static TEST_CUR_FAILED: AtomicBool = AtomicBool::new(false);
static TEST_HEADER_PRINTED: AtomicBool = AtomicBool::new(false);
/// The running test's name, held as *text* rather than as a pointer to the
/// `str` the caller passed: that argument points at the caller's 24-byte stack
/// slot, and outliving the call is exactly what a stack slot does not promise.
/// So the name is materialized at `test_begin` instead of read back at report
/// time.
static TEST_CUR_NAME: Mutex<Option<String>> = Mutex::new(None);
static TEST_TOTAL: AtomicI64 = AtomicI64::new(0);
static TEST_PASSED: AtomicI64 = AtomicI64::new(0);
static TEST_FAILED: AtomicI64 = AtomicI64::new(0);
/// Set by `check`'s batch mode (see [`set_test_file`]); `None` in single-file
/// mode, where the file is already obvious from the command line.
static TEST_CUR_FILE: Mutex<Option<String>> = Mutex::new(None);
/// Set by `check`'s batch mode (see [`set_quiet_summary`]).
static TEST_QUIET_SUMMARY: AtomicBool = AtomicBool::new(false);

/// Name the file whose tests are running now, so a failure in a many-file run
/// says which file it came from (`test <file>::<name> ... FAIL`). Passing `None`
/// restores the bare `test <name> ... FAIL` used when checking a single file.
///
/// The counters this module keeps are cumulative across every program run in the
/// process, which is exactly what batch mode wants — one aggregate at the end.
pub fn set_test_file(file: Option<&str>) {
    let mut cur = TEST_CUR_FILE.lock().unwrap_or_else(|e| e.into_inner());
    *cur = file.map(str::to_string);
}

/// Silence the per-program summary line that [`aipl_test_summary`] otherwise
/// prints. Batch mode runs one program per file and prints its own aggregate,
/// so the per-file lines would just be a running total repeated N times.
pub fn set_quiet_summary(quiet: bool) {
    TEST_QUIET_SUMMARY.store(quiet, TestOrd::Relaxed);
}

/// Cumulative `(total, passed, failed)` test counts across every program run in
/// this process, for batch mode's aggregate report.
pub fn test_totals() -> (i64, i64, i64) {
    (
        TEST_TOTAL.load(TestOrd::Relaxed),
        TEST_PASSED.load(TestOrd::Relaxed),
        TEST_FAILED.load(TestOrd::Relaxed),
    )
}

/// Print the `test <name> ... FAIL` header once per failing test, qualified by
/// the current file in batch mode. Shared by every failure path below so the
/// header appears exactly once no matter which of them reports first.
fn print_fail_header() {
    if TEST_HEADER_PRINTED.swap(true, TestOrd::Relaxed) {
        return;
    }
    let held = TEST_CUR_NAME.lock().unwrap_or_else(|e| e.into_inner());
    let name = held.as_deref().unwrap_or("<unknown>");
    match &*TEST_CUR_FILE.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(file) => println!("test {file}::{name} ... FAIL"),
        None => println!("test {name} ... FAIL"),
    }
}

// `?` used inside a `.test` body, applied to a `none`: the optional analog of
// [`aipl_test_fail`]. There is no payload to render — `none` carries nothing —
// so the report just names what happened. Same failure bookkeeping.
#[no_mangle]
extern "C" fn aipl_test_fail_none() {
    print_fail_header();
    println!("  `?` propagated a `none`");
    TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
}

/// Read a wide `str` argument as an owned `String` for the report. Materializes
/// rather than borrowing, since the caller's slot is not guaranteed to outlive
/// the report — see [`TEST_CUR_NAME`].
fn wide_text(s: *const str24::Str) -> String {
    let mut scratch = [0u8; str24::INLINE_CAP];
    let v = unsafe { *s };
    String::from_utf8_lossy(v.bytes(&mut scratch)).into_owned()
}

/// [`aipl_test_begin`] under the wide ABI.
#[no_mangle]
extern "C" fn aipl_test_begin(name: *const str24::Str) {
    *TEST_CUR_NAME.lock().unwrap_or_else(|e| e.into_inner()) = Some(wide_text(name));
    TEST_CUR_FAILED.store(false, TestOrd::Relaxed);
    TEST_HEADER_PRINTED.store(false, TestOrd::Relaxed);
}

/// [`aipl_assert`] under the wide ABI.
#[no_mangle]
extern "C" fn aipl_assert(cond: i64, loc: *const str24::Str) {
    if cond == 0 {
        print_fail_header();
        println!("  assert failed at {}", wide_text(loc));
        TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
    }
}

/// [`aipl_test_fail`] under the wide ABI.
#[no_mangle]
extern "C" fn aipl_test_fail(msg: *const str24::Str) {
    print_fail_header();
    println!("  `?` propagated an error: {}", wide_text(msg));
    TEST_CUR_FAILED.store(true, TestOrd::Relaxed);
}

#[no_mangle]
extern "C" fn aipl_test_end() {
    TEST_TOTAL.fetch_add(1, TestOrd::Relaxed);
    if TEST_CUR_FAILED.load(TestOrd::Relaxed) {
        TEST_FAILED.fetch_add(1, TestOrd::Relaxed);
    } else {
        TEST_PASSED.fetch_add(1, TestOrd::Relaxed);
    }
}

#[no_mangle]
extern "C" fn aipl_test_summary() -> i64 {
    let (total, passed, failed) = test_totals();
    if !TEST_QUIET_SUMMARY.load(TestOrd::Relaxed) {
        println!("{total} tests: {passed} passed, {failed} failed");
    }
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

/// Whether `a` already contains the element at `x`. `str_cmp != 0` compares
/// `str` elements by content and carries their width (see `str_cmp_width`);
/// otherwise a bit-packed `bool` set (`elem_size == 0`) compares unpacked bits
/// and every other scalar set compares the 8-byte value. Returns 1 (present) or
/// 0 (absent); a null/empty set is never a member.
#[no_mangle]
extern "C" fn aipl_set_contains(a: *const u8, x: *const u8, elem_size: i64, str_cmp: i64) -> i64 {
    if a.is_null() {
        return 0;
    }
    let len = unsafe { array_len_of(a) };
    if str_cmp == str24::STR_SIZE as i64 {
        // Wide `str` elements: the element *is* the 24-byte value, so there is
        // no pointer to load — compare the values in place.
        let target = unsafe { std::ptr::read(x as *const str24::Str) };
        for i in 0..len {
            let sp = unsafe { arr_elem_ptr(a, i, str24::STR_SIZE) };
            let s = unsafe { std::ptr::read(sp as *const str24::Str) };
            if str24::eq(s, target) {
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
#[no_mangle]
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

/// The address to hand `aipl_set_insert` for element `i` of `src`, and a
/// one-word scratch backing it.
///
/// Every element wider than a word — a wide `str` among them — must be passed
/// *in place*: copying it into a local `i64` first keeps only its first word,
/// which is then read back as a whole value. Only the bit-packed `bool` case has
/// no address to give (its element is one bit), so that one is materialized into
/// the scratch and its address returned.
unsafe fn set_elem_ptr(src: *const u8, i: usize, elem_size: i64, scratch: &mut i64) -> *const u8 {
    if elem_size == ELEM_BITPACKED {
        *scratch = i64::from(unsafe { arr_load_bit(src, i) });
        return scratch as *const i64 as *const u8;
    }
    unsafe { arr_elem_ptr(src, i, elem_size.max(8) as usize) }
}

/// `a.union(b)` (copy): a fresh set with every distinct element of `a` then `b`.
/// Consumes (decs) both inputs, like `aipl_concat`. Inserted elements are
/// retained by `aipl_set_insert`, so they outlive the inputs' release.
#[no_mangle]
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
        let mut scratch = 0i64;
        let vp = unsafe { set_elem_ptr(a, i, elem_size, &mut scratch) };
        dest = aipl_set_insert(dest, vp, drop_fn, retain_fn, elem_size, str_cmp);
    }
    for i in 0..b_len {
        let mut scratch = 0i64;
        let vp = unsafe { set_elem_ptr(b, i, elem_size, &mut scratch) };
        dest = aipl_set_insert(dest, vp, drop_fn, retain_fn, elem_size, str_cmp);
    }
    aipl_array_dec(a);
    aipl_array_dec(b);
    dest
}

/// `set a = a.union(b)` for an exclusive `a`: extend `a` in place with `b`'s
/// distinct elements (reusing `a`'s allocation) and return the (possibly
/// relocated) set. Consumes (decs) `b`; `a` is reused, not dec'd.
#[no_mangle]
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
        let mut scratch = 0i64;
        let vp = unsafe { set_elem_ptr(b, i, elem_size, &mut scratch) };
        a = aipl_set_insert(a, vp, drop_fn, retain_fn, elem_size, str_cmp);
    }
    aipl_array_dec(b);
    a
}

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

/// Index of the pair in dict `a` whose key matches the key at `pair_ptr`, or -1.
/// Keys compare by `str_cmp`: a 24-byte value compare for a `str` key, else the
/// raw 8-byte value.
unsafe fn dict_find(a: *const u8, pair_ptr: *const u8, pair_size: i64, str_cmp: i64) -> i64 {
    if a.is_null() {
        return -1;
    }
    let len = unsafe { array_len_of(a) };
    let stride = pair_size as usize;
    let wide = str_cmp == str24::STR_SIZE as i64;
    let want_wide = if wide {
        unsafe { std::ptr::read(pair_ptr as *const str24::Str) }
    } else {
        str24::Str::empty()
    };
    let want = unsafe { std::ptr::read(pair_ptr as *const i64) };
    for i in 0..len {
        let ep = unsafe { arr_elem_ptr(a, i, stride) };
        let eq = if wide {
            str24::eq(
                unsafe { std::ptr::read(ep as *const str24::Str) },
                want_wide,
            )
        } else {
            let k: i64 = unsafe { std::ptr::read(ep as *const i64) };
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
#[no_mangle]
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
#[no_mangle]
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
    // The key occupies the first `dict_key_width` bytes of the pair; the value
    // follows immediately.
    unsafe { arr_elem_ptr(a, idx as usize, pair_size as usize).add(dict_key_width(str_cmp)) }
}

/// `d.contains_key(k)`: whether `key_ptr` is a key of dict `a`. Borrows `a`.
#[no_mangle]
extern "C" fn aipl_dict_contains_key(
    a: *const u8,
    key_ptr: *const u8,
    pair_size: i64,
    str_cmp: i64,
) -> i64 {
    i64::from(unsafe { dict_find(a, key_ptr, pair_size, str_cmp) } >= 0)
}

/// Element drop-fn for an array of arrays (`T[][]`): release each element
/// array, which recursively releases its own elements via its own drop-fn.
#[no_mangle]
extern "C" fn aipl_arr_drop_arr(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_array_dec(std::ptr::read(elems.add(i)) as *const u8);
        }
    }
}

/// Element retain-fn for `T[][]`: inc each element array (the outer array's
/// co-ownership). `str` elements have their own helper — they are 24-byte values
/// now, not pointers — so this is the array case alone.
#[no_mangle]
extern "C" fn aipl_arr_retain_ptr(elems: *const u8, len: i64) {
    unsafe {
        let elems = elems as *const i64;
        for i in 0..len as usize {
            aipl_arr_inc(std::ptr::read(elems.add(i)) as *const u8);
        }
    }
}

/// Element drop-fn for `T[]?[]`: release the inner array of each present element.
#[no_mangle]
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
#[no_mangle]
extern "C" fn aipl_arr_retain_opt(elems: *const u8, len: i64) {
    unsafe {
        for i in 0..len as usize {
            let e = elems.add(i * 16);
            let tag = std::ptr::read(e as *const i64);
            if tag != 0 {
                aipl_arr_inc(std::ptr::read(e.add(8) as *const i64) as *const u8);
            }
        }
    }
}

/// Build a `str[]` from `args` using the JIT's own allocators (so the JIT'd
/// callee can free it through the matching runtime). Mirrors what the AOT
/// runtime's `build_cli_args` does for a native binary.
fn build_cli_array(args: &[String]) -> *const u8 {
    // Built by the host and read by compiled code, so it is one of the few places
    // the two must agree byte for byte — which means it follows the switch.
    let drop_fn = str24::aipl_arr_drop_str as *const () as usize as i64;
    let arr = aipl_array_new(args.len() as i64, drop_fn, str24::STR_SIZE as i64);
    unsafe {
        let elems = arr.add(ARR_ELEMS_OFFSET) as *mut str24::Str;
        for (i, a) in args.iter().enumerate() {
            core::ptr::write(elems.add(i), str24::from_bytes(a.as_bytes()));
        }
    }
    return arr;
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

/// One parameter of a `funcs`-map entry: its type together with the two
/// per-parameter calling-convention decisions the call site and the callee body
/// must both honour. They live on the parameter rather than as index lists on
/// [`FuncInfo`] because that is what they are — properties of a parameter,
/// decided once monomorphization knows the instance (the same reasoning that
/// puts them on [`aipl_mono::ConcreteParam`]).
#[derive(Clone)]
struct ParamInfo {
    ty: ConcreteType,
    /// This instance takes ownership of the parameter (set by monomorphization).
    /// At a call site the corresponding argument — always a fresh, uniquely-owned
    /// heap value — is *moved* in (no retain) rather than borrowed, and the
    /// callee won't drop it on entry-scope exit.
    owned: bool,
    /// The instance only *inspects* this heap argument, so the borrow protocol's
    /// retain/release pair cancels — the call site skips the retain and the
    /// callee skips the entry-scope release (see
    /// `aipl_mono::inspect_only_params`, which decides both halves so they cannot
    /// disagree).
    ///
    /// For a *user* function that answer is computed from the body. A **builtin**
    /// has no AIPL body to analyse, but it does have a Rust or cranelift one that
    /// can be read once and the answer written down — which is what
    /// [`Args`] records, beside each symbol in `SIG_REGS`. Still false for the
    /// FFI metadata rebuilt from checked-in IR, which has neither.
    inspect_only: bool,
    /// Extend the caller-retains/callee-releases protocol to a parameter that
    /// would otherwise be a *pure borrow* — a boxed (recursive) value, which is
    /// refcounted but not [`is_heap`], so by default the caller's own reference
    /// is what keeps it alive across the call and neither side touches the
    /// count.
    ///
    /// Set only for the parameters of a tail-call participant (see
    /// [`tail_call_plan`]), and it is what makes a tail call to one *possible*:
    /// a tail call releases the caller's scope before transferring control, so
    /// an argument the caller was lending would die under the callee's feet.
    /// Paying the retain/release pair buys the callee a reference of its own.
    tail_owned: bool,
}

impl ParamInfo {
    /// A parameter the callee only reads: the caller keeps its own reference and
    /// neither side touches the count. For a builtin this is a claim about its
    /// native implementation — see [`Args`].
    fn inspected(ty: ConcreteType) -> Self {
        Self {
            ty,
            owned: false,
            inspect_only: true,
            tail_owned: false,
        }
    }

    /// A plainly borrowed parameter — the caller retains, the callee releases.
    fn borrowed(ty: ConcreteType) -> Self {
        Self {
            ty,
            owned: false,
            inspect_only: false,
            tail_owned: false,
        }
    }

    /// Whether the borrow protocol applies to this parameter: the caller
    /// retains the argument and the callee releases it on entry-scope exit.
    /// Both halves read this one answer, so they cannot disagree.
    ///
    /// False for a moved-in `owned` parameter (its reference transfers), for an
    /// `inspect_only` heap parameter (the pair cancels), and for anything that
    /// owns no heap at all.
    fn retained(&self) -> bool {
        if self.owned {
            return false;
        }
        if is_heap(&self.ty) {
            !self.inspect_only
        } else {
            self.tail_owned
        }
    }
}

#[derive(Clone)]
struct FuncInfo {
    link: FuncLink,
    /// The `<name>$tail` body, when this function participates in a tail call
    /// (see the "Tail calls" section): a second declaration with the same ABI
    /// but `CallConv::Tail`, module-local, holding the real body — `link` then
    /// names the exported C-convention trampoline that forwards to it. Every
    /// AIPL call site calls this one directly when it is present, tail or not;
    /// only the FFI and `func_addr` go through the trampoline.
    tail_id: Option<FuncId>,
    params: Vec<ParamInfo>,
    return_ty: ConcreteType,
    effects: Vec<String>,
    /// `true` for a mutating method (`fn f(mut self: T, ...)`): it returns
    /// nothing to the user, mutates its receiver, and must be called as
    /// `v.f(...)`. At the ABI level it returns the mutated `self` (its
    /// `return_ty`), which the call site stores back into `v`.
    is_mutating: bool,
}

impl FuncInfo {
    /// Just the parameter types, in order — for the places that describe the
    /// function's *type* rather than its calling convention (a function value's
    /// `ConcreteType::Fn`, the dogfood-IR manifest, FFI argument checking).
    fn param_types(&self) -> impl Iterator<Item = &ConcreteType> {
        self.params.iter().map(|p| &p.ty)
    }
}

struct StructLayout {
    fields: Vec<FieldLayout>,
    size: u32,
    /// True when this type is *recursive* (its declaration reaches itself
    /// through struct fields, variant payloads, or optional/result cores).
    /// Recursive types are heap-allocated ("boxed") behind an 8-byte pointer —
    /// see the "Recursive (boxed) type runtime" section — so `size` is the
    /// *payload* size, and everything that stores/copies a value of this type
    /// handles a pointer instead of `size` inline bytes.
    boxed: bool,
    /// Which recursion group (strongly-connected component of the type
    /// reference graph) this type belongs to. Only meaningful when `boxed`:
    /// a reference between boxed values of the *same* group is an internal
    /// (weak-counted) reference; everything else is external (strong).
    scc: u32,
}

#[derive(Clone)]
struct FieldLayout {
    name: String,
    ty: ConcreteType,
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
    /// Recursive-type flags, exactly as on [`StructLayout`].
    boxed: bool,
    scc: u32,
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
    /// Whether values of this type are heap-allocated behind a pointer (a
    /// recursive type) rather than stored inline. See [`StructLayout::boxed`].
    fn boxed(&self) -> bool {
        match self {
            TypeDef::Struct(s) => s.boxed,
            TypeDef::Variant(v) => v.boxed,
        }
    }
    /// This type's recursion group id (meaningful only when [`Self::boxed`]).
    fn scc(&self) -> u32 {
        match self {
            TypeDef::Struct(s) => s.scc,
            TypeDef::Variant(v) => v.scc,
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
/// the builtin `Error`) is `Str`; an optional is `Opt`, a `Result` is `Res`, a
/// `struct` is `Struct`, a `variant` is `Variant`, and an array is `Array`.
///
/// Every variant marshals in *both* directions, and by the same rule in each:
/// whatever [`check_ffi_return`] accepts as a return type can also be passed as
/// an argument (built by [`ffi_arg_abi`]), to any nesting depth — a `Note[]` of
/// `{ message: str, span: Span }` goes in as readily as it comes out. The two
/// exceptions are a *recursive* (boxed) type, which the host has no way to
/// allocate, and sets/dicts, which aren't marshalable in either direction.
#[derive(Debug, Clone, PartialEq)]
pub enum FfiValue {
    /// A scalar AIPL value at its `i64` ABI.
    Int(i64),
    /// An AIPL `str` (or the builtin `Error`, which shares its representation).
    Str(String),
    /// An AIPL optional: `Opt(None)` is `none`; `Opt(Some(v))` is `some(v)`
    /// (nested for `T??`). The inner core is an `Int`, `Str`, `Struct`, or
    /// `Variant`.
    Opt(Option<Box<FfiValue>>),
    /// An AIPL `Result<ok, err>`: `Res(Ok(v))` for `ok(v)`, `Res(Err(e))` for
    /// `err(e)`. Each side's payload is an `Int` (also standing in for a
    /// `Unit` side, e.g. `!Error`'s ok case), `Str`, `Struct`, or `Variant` —
    /// whatever [`check_ffi_return`] accepted for that side.
    Res(Result<Box<FfiValue>, Box<FfiValue>>),
    /// An AIPL `struct`: its fields in declaration order, each a `(name, value)`.
    /// Passed and returned through a hidden sret pointer (like an optional). A
    /// field may itself be any marshalable value, composites included.
    Struct(Vec<(String, FfiValue)>),
    /// An AIPL `variant` (sum type): the active case's constructor name plus its
    /// payload values in positional order (empty for a nullary case). Passed and
    /// returned through a hidden sret pointer, like a struct. A payload value may
    /// itself be any marshalable value.
    Variant(String, Vec<FfiValue>),
    /// An AIPL array `T[]`: its elements in order. Each element is marshaled by
    /// the array's element type — an `Int`/`Str`/`Struct`/`Variant`/`Array`/… as
    /// appropriate (a `char[]` is an `Array` of `Int` codepoints, and a `bool[]`
    /// an `Array` of `Int` `0`/`1`, both directions).
    Array(Vec<FfiValue>),
}

pub struct Compilation {
    code: Code,
    funcs: HashMap<String, FuncInfo>,
    /// Declared struct/variant layouts, retained so [`Compilation::call_values`]
    /// can marshal a struct return value back to the host (read each field at its
    /// offset). Populated from the frontend on [`Compilation::new`], and from the
    /// `; struct` manifest lines on the dogfood [`Compilation::from_artifact`] path.
    structs: HashMap<String, TypeDef>,
    /// Which `str` representation this compilation's code speaks. Freshly
    /// compiled code speaks the wide `str`; a checked-in artifact was built
    /// before the switch and does not. The FFI marshals with this.
    ///
    /// An [`Abi`] rather than a bare flag so that the *next* per-type
    /// representation choice is a field there, not a second parameter threaded
    /// through the whole marshaling layer.
    abi: Abi,
    ir: String,
}

/// Parse [`aipl_syntax::BUILTIN_SIGNATURES`] into the checker's builtin
/// declarations. The source is a fixed constant, so a parse failure is a
/// compiler bug. Each is marked `pub` to match the hand-built originals
/// (visibility is irrelevant to the checker, the only consumer). The
/// AIPL-implemented builtins aren't in that constant — their signatures come
/// straight from their `.aipl` source via [`aipl_mono::aipl_builtin_sig_decls`],
/// so a builtin's signature lives in exactly one place.
fn builtin_decls(needed: &BTreeSet<&'static str>) -> Vec<Item> {
    native_builtin_decls()
        .iter()
        .cloned()
        .chain(aipl_mono::aipl_builtin_sig_decls(needed))
        .collect()
}

/// The [`BUILTIN_SIGNATURES`] half of [`builtin_decls`], parsed once. Every
/// compile reads it three times (checker decls, the call-site signature
/// registry, the struct decls), and it is a fixed constant, so the parse is
/// cached rather than repeated.
fn native_builtin_decls() -> &'static [Item] {
    static DECLS: std::sync::OnceLock<Vec<Item>> = std::sync::OnceLock::new();
    DECLS.get_or_init(|| {
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
            .collect()
    })
}

/// The `struct` declarations among [`BUILTIN_SIGNATURES`] (e.g. `__builtin_Span`,
/// `__builtin_ExecResult`) — the builtin *types*, as opposed to the builtin
/// function signatures that make up the rest of that constant.
fn builtin_struct_decls() -> Vec<StructDecl> {
    native_builtin_decls()
        .iter()
        .filter_map(|item| match item {
            Item::Struct(s) => Some(s.clone()),
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
    // `!list_files`/`!execute_program`/`!clock` functions), so the synthesized
    // test fns / driver declare every known effect.
    let all_effects = || {
        vec![
            "prints".to_string(),
            "read_files".to_string(),
            "write_files".to_string(),
            "list_files".to_string(),
            "execute_program".to_string(),
            "clock".to_string(),
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
// ([`DOGFOOD_SOURCE_FILES`]) and compiled together as a single program with one
// checked-in artifact ([`dogfood.clif`]) and one set of FFI entry points
// ([`DOGFOOD_ENTRIES`]). Adding a newly-dogfooded function is then: write the
// `.aipl` file and add it to those two lists, not a new bundle of duplicated
// dependency sources.
/// Where the dogfooded `.aipl` sources live: this crate's `src/` directory,
/// resolved at compile time from the manifest dir but *read at run time*, by
/// [`read_dogfood_sources`].
///
/// Reading rather than `include_str!`ing them is the same trade
/// [`DOGFOOD_CLIF_PATH`] makes, for the same reason and with more force.
/// Embedding made every `.aipl` edit a source change to this crate, and so a
/// rebuild of it, everything downstream, and all 12 test binaries — ~730s
/// against a 1s no-op build — even though nothing in an ordinary compile reads
/// these sources: the compiler links the checked-in `.clif`, and only the
/// IR-regeneration helpers and their tests ever want the text. Reading at run
/// time leaves no crate's fingerprint depending on the corpus, so editing a
/// dogfooded `.aipl` now costs nothing to rebuild.
///
/// The price is the one the artifact already pays: a compiler binary is tied to
/// the repo it was built from, and a missing or unreadable source is a run-time
/// panic rather than a compile error. Only the author helpers can reach it.
const DOGFOOD_SRC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// Every `.aipl` file the compiler dogfoods, by module name — the name a
/// `from "./..."` import resolves against. The set is closed under imports and
/// each file appears exactly once (unlike the old per-function engines, which
/// each separately bundled copies of their shared dependencies — `lines.aipl`,
/// `parse_test_section_header.aipl`, etc.).
///
/// This list is the single source of truth shared by [`DOGFOOD_ENGINE`] (which
/// runs the artifact these compile to), the author helper that regenerates it,
/// and the test that verifies it's current; [`read_dogfood_sources`] reads them.
/// The **first** entry is the root: `aipl_loader::load_program_sources` leaves
/// only its top-level names unmangled (every other file's become
/// `__m<index>__<name>`), but [`resolve_dogfood_entry`] resolves an entry name
/// against either form, so any dogfooded function can be an entry regardless of
/// which file declares it — no aggregator/re-export file required. Adding a
/// newly-dogfooded function is then: write the `.aipl` file and add it to this
/// list and [`DOGFOOD_ENTRIES`].
pub const DOGFOOD_SOURCE_FILES: &[&str] = &[
    "./process_raw_string.aipl",
    "./dedent.aipl",
    "./lines.aipl",
    "./trim_prefix.aipl",
    "./trim_end_while.aipl",
    "./trim_suffix.aipl",
    "./parse_test_section_header.aipl",
    "./strip_test_sections.aipl",
    "./split_test_sections.aipl",
    "./find_trailing_whitespace.aipl",
    "./assert_loc.aipl",
    "./line_at.aipl",
    "./caret_block.aipl",
    "./fill_or_add_section.aipl",
    "./fill_or_add_section_file.aipl",
    "./normalize_output.aipl",
    "./int_fits.aipl",
    "./is_operator_name.aipl",
    "./lexer.aipl",
    "./lex_aipl.aipl",
    "./unescape.aipl",
    "./reindent_block.aipl",
    "./indent.aipl",
    "./find_files.aipl",
    "./companion_files.aipl",
    "./parse_spec.aipl",
];

/// Every `.aipl` the *formatter* engine needs: the walker and its `Doc` printer,
/// plus everything they import — which includes the lexer, since
/// `format_program` tokenizes for itself. That overlaps [`DOGFOOD_SOURCE_FILES`]
/// heavily, and deliberately so: the two artifacts are linked independently, and
/// keeping each self-contained is what lets an ordinary compile link only the
/// parser half.
///
/// Splitting them matters because re-linking is not free — it is ~2.4s for the
/// combined artifact, paid once per process, and the formatter is over two
/// thirds of it. An ordinary compile never formats, so it should never pay for
/// the walker; `aipl fmt` links this one on top and is an explicit user action.
pub const FMT_SOURCE_FILES: &[&str] = &[
    "./process_raw_string.aipl",
    "./clean_trailing_whitespace.aipl",
    "./format_source.aipl",
    "./dedent.aipl",
    "./lines.aipl",
    "./trim_prefix.aipl",
    "./trim_end_while.aipl",
    "./trim_suffix.aipl",
    "./parse_test_section_header.aipl",
    "./strip_test_sections.aipl",
    "./split_test_sections.aipl",
    "./is_operator_name.aipl",
    "./lexer.aipl",
    "./lex_aipl.aipl",
    "./unescape.aipl",
    "./reindent_block.aipl",
    "./indent.aipl",
    "./doc.aipl",
    "./walker.aipl",
];

/// Read `files` — module names as spelled in [`DOGFOOD_SOURCE_FILES`] — out of
/// [`DOGFOOD_SRC_DIR`], as the `(name, source)` pairs the loader compiles as
/// in-memory modules. The name is kept verbatim so `from "./x.aipl"` imports
/// still resolve against it; only the path read from disk strips the `./`.
///
/// An unreadable file is a loud panic: these are checked-in sources the caller
/// has just asked to compile, so a missing one is a broken checkout or a stale
/// entry in the list, not something to recover from.
pub fn read_dogfood_sources(files: &[&str]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|name| {
            let path = std::path::Path::new(DOGFOOD_SRC_DIR).join(name.trim_start_matches("./"));
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("dogfood source {path:?}: {e}"));
            ((*name).to_string(), src)
        })
        .collect()
}

/// Borrow a read-in source set as the `(name, source)` slice the loader and
/// [`generate_dogfood_artifact`] take.
pub fn source_refs(sources: &[(String, String)]) -> Vec<(&str, &str)> {
    sources
        .iter()
        .map(|(name, src)| (name.as_str(), src.as_str()))
        .collect()
}

/// The functions Rust calls via the FFI (need `; entry` metadata in the
/// checked-in artifact) — the real name each is declared with in its own file
/// under [`DOGFOOD_SOURCE_FILES`]. Resolved through mangling by
/// [`resolve_dogfood_entry`], so a function need not live in the root file to
/// be listed here.
///
/// *Every* entry must have a Rust caller: this list is the FFI surface, not an
/// index of what is dogfooded. A dogfooded function whose caller has since been
/// ported to AIPL is reached in-engine by that AIPL caller and belongs only in
/// [`DOGFOOD_SOURCE_FILES`] — `process_raw_string` (called by `lex_aipl`'s
/// emit), `line_at` (by `caret_block`), and `fill_or_add_section` (by
/// `fill_or_add_section_file`) are all in that position. Listing one here anyway
/// costs entry metadata in the artifact and advertises a Rust-facing API that
/// nothing calls.
pub const DOGFOOD_ENTRIES: &[&str] = &[
    "parse_test_section_header",
    "strip_test_sections",
    "split_test_sections",
    "find_trailing_whitespace",
    "assert_loc",
    "caret_block",
    "fill_or_add_section_file",
    "normalize_output",
    "int_fits",
    "is_operator_name",
    "lex_aipl",
    "lex_aipl_stripped",
    "find_files",
    "companion_files",
    "parse_spec",
];

/// The formatter engine's single FFI entry.
pub const FMT_ENTRIES: &[&str] = &["format_program", "fmt_prepare", "fmt_layout"];

/// Where the checked-in formatter IR lives, and its bare filename for the
/// `dogfood_ir` test. See [`DOGFOOD_CLIF_PATH`] for why this is a path read at
/// run time rather than the artifact text baked in with `include_str!`.
pub const FMT_CLIF_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/fmt.clif");
pub const FMT_CLIF_FILE: &str = "fmt.clif";

/// Env var naming an alternate *formatter* artifact — the [`DOGFOOD_IR_ENV`]
/// twin. A staged-IR validation run sets both, since the two artifacts are
/// regenerated and promoted together.
pub const FMT_IR_ENV: &str = "AIPL_FMT_IR";

/// Where the checked-in dogfood IR for the whole of [`DOGFOOD_SOURCE_FILES`]/
/// [`DOGFOOD_ENTRIES`] lives — re-linked at run time instead of recompiling
/// from source. Kept current by the `dogfood_ir` test (regenerate with
/// `fill_dogfood_ir`).
///
/// This is a *path*, resolved at compile time from the manifest directory, and
/// the file is read at run time — by the author helpers that regenerate and
/// verify it, and by `build.rs`, which lowers it into the prebuilt object.
/// Nothing reads it on the ordinary run path any more; the machine code it
/// describes is already in the binary.
///
/// It is not `include_str!`d into this crate. Doing that made every `.clif`
/// regeneration a source change to a 16k-line crate, rebuilding it, everything
/// downstream, and all 12 test binaries. `build.rs` does re-run when the
/// artifact changes — that is what keeps the prebuilt object honest — so a
/// regeneration is no longer free, but it costs a build-script run and a relink
/// (~55s measured) rather than a recompile of the world.
pub const DOGFOOD_CLIF_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/dogfood.clif");
/// The checked-in artifact's filename, for the `dogfood_ir` test.
pub const DOGFOOD_CLIF_FILE: &str = "dogfood.clif";

/// Env var overriding [`aipl_mono::DEFAULT_INLINE_MAX_EXPRS`], the body-size
/// threshold for small-function inlining. Set it to a number to try a different
/// one — `AIPL_INLINE_MAX_EXPRS=4 cargo test --test cases -- cases_strings` —
/// without rebuilding the compiler for each value. An unparseable value falls
/// back to the default rather than failing a compile over a stray env var.
pub const INLINE_MAX_EXPRS_ENV: &str = "AIPL_INLINE_MAX_EXPRS";

/// The small-function inlining threshold: [`INLINE_MAX_EXPRS_ENV`] if it parses,
/// else [`aipl_mono::DEFAULT_INLINE_MAX_EXPRS`].
fn inline_max_exprs() -> usize {
    std::env::var(INLINE_MAX_EXPRS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(aipl_mono::DEFAULT_INLINE_MAX_EXPRS)
}

/// Env var naming an alternate dogfood-IR artifact to run the compiler against
/// (see [`dogfood_artifact_text`]). Set it to a `.clif` path — typically
/// `dogfood.clif.staged` — to validate *candidate* IR: every parse the compiler
/// does (including its own source) then runs off that file instead of the
/// checked-in [`DOGFOOD_CLIF_PATH`], so `AIPL_DOGFOOD_IR=…staged cargo test`
/// exercises staged IR across the whole corpus before it's promoted to live.
pub const DOGFOOD_IR_ENV: &str = "AIPL_DOGFOOD_IR";

/// The manifest headers of the two prebuilt artifacts, emitted alongside their
/// object code by `build.rs`. Just the `;`-comment block — the FFI signatures
/// and struct layouts the runtime marshals against — not the megabytes of IR
/// bodies, which are already machine code by the time this crate compiles.
const DOGFOOD_MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/dogfood.manifest"));
const FMT_MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/fmt.manifest"));

// Defines `DOGFOOD_PREBUILT` / `FMT_PREBUILT`: each artifact's entry points, as
// (FFI name, address) pairs resolved by the system linker. See `build.rs`.
include!(concat!(env!("OUT_DIR"), "/prebuilt.rs"));

pub use aipl_artifact::fingerprint as artifact_fingerprint;

/// The fingerprint of the artifact `<file>` (e.g. `"dogfood.clif"`) that this
/// binary's prebuilt object was built from, or `None` if there is no such
/// artifact. Compared against the checked-in file by a test — see
/// [`artifact_fingerprint`].
pub fn prebuilt_fingerprint(clif_file: &str) -> Option<u64> {
    PREBUILT_FINGERPRINTS
        .iter()
        .find(|(f, _)| *f == clif_file)
        .map(|(_, h)| *h)
}

/// The dogfood engine for this process: the prebuilt object normally, or a
/// fresh JIT link of the file named by [`DOGFOOD_IR_ENV`] when that override is
/// set.
fn dogfood_engine() -> Compilation {
    engine(
        DOGFOOD_IR_ENV,
        DOGFOOD_MANIFEST,
        DOGFOOD_PREBUILT,
        "dogfood engine",
    )
}

/// The formatter engine, on the same rules.
fn fmt_engine() -> Compilation {
    engine(FMT_IR_ENV, FMT_MANIFEST, FMT_PREBUILT, "formatter engine")
}

/// Build one dogfood engine.
///
/// The default is the prebuilt object: the same artifact, already lowered to
/// machine code inside this binary, so there is no IR to parse and nothing to
/// compile. Setting `$env` to a `.clif` path swaps in the run-time JIT link of
/// *that* file instead, which is how the staged-IR workflow validates candidate
/// IR across the whole corpus before promoting it — the object in the binary
/// was built from the live artifact and cannot speak for a staged one.
///
/// Either way a failure is a panic: the compiler cannot parse its own source
/// without this engine, and there is deliberately no fallback (see "No native
/// fallbacks for dogfooded functions").
fn engine(
    env: &str,
    manifest: &'static str,
    prebuilt: &'static [(&'static str, PrebuiltFn)],
    what: &str,
) -> Compilation {
    match std::env::var(env) {
        Ok(path) if !path.is_empty() => {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{env}: could not read IR at {path:?}: {e}"));
            Compilation::from_artifact(&text)
                .unwrap_or_else(|e| panic!("{what} builds from {path:?}: {e:?}"))
        }
        _ => Compilation::from_prebuilt(manifest, prebuilt)
            .unwrap_or_else(|e| panic!("{what} builds from the prebuilt object: {e:?}")),
    }
}

thread_local! {
    /// The one dogfood engine, built lazily on first use per thread. A
    /// `Compilation` isn't `Sync`, hence one per thread. Building it runs no
    /// AIPL frontend, so it works even when the frontend can't currently
    /// compile the dogfooded sources, and never recurses even though several of
    /// these hooks are themselves invoked from the parser.
    static DOGFOOD_ENGINE: Compilation = dogfood_engine();

    /// The formatter engine, built lazily and separately — so a compile that
    /// never formats never links the walker. See [`FMT_SOURCE_FILES`].
    static FMT_ENGINE: Compilation = fmt_engine();
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

/// The formatter's split-test-sections hook (see [`install_parser_hooks`]):
/// splits `src` into `(main, sections)` at the first `--- name ---` marker line
/// — computed by the dogfooded AIPL `split_test_sections` via the FFI. The AIPL
/// returns a `(str, str)` tuple, which lowers to a two-field struct marshaled as
/// [`FfiValue::Struct`] with fields `_0` (the main source) and `_1` (the
/// sections). Both are byte-substrings of `src`. No native fallback; panics if it
/// can't be built or called.
fn split_test_sections(src: &str) -> (String, String) {
    fn field(fields: &[(String, FfiValue)], name: &str) -> String {
        match fields.iter().find(|(n, _)| n == name) {
            Some((_, FfiValue::Str(s))) => s.clone(),
            other => panic!("dogfooded split_test_sections() field {name:?}: {other:?}"),
        }
    }
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("split_test_sections", &[FfiValue::Str(src.to_string())]) {
            Ok(FfiValue::Struct(fields)) => (field(&fields, "_0"), field(&fields, "_1")),
            other => panic!("dogfooded split_test_sections() call: {other:?}"),
        }
    })
}

/// The parser's companion-files hook (see [`install_parser_hooks`]): the
/// `--- file: <path> ---` sections of a source, computed by the dogfooded AIPL
/// `companion_files` (`str -> Companion[]!str`) and marshaled back as an
/// [`FfiValue::Res`] of an [`FfiValue::Array`] of `Companion` structs. No native fallback; panics if it
/// can't be built or called.
fn companion_files(src: &str) -> Result<Vec<(String, String)>, String> {
    fn field(fields: &[(String, FfiValue)], name: &str) -> String {
        match fields.iter().find(|(n, _)| n == name) {
            Some((_, FfiValue::Str(s))) => s.clone(),
            other => panic!("dogfooded companion_files() Companion.{name}: {other:?}"),
        }
    }
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("companion_files", &[FfiValue::Str(src.to_string())]) {
            Ok(FfiValue::Res(Ok(ok))) => match *ok {
                FfiValue::Array(items) => Ok(items
                    .iter()
                    .map(|item| match item {
                        FfiValue::Struct(fields) => {
                            (field(fields, "path"), field(fields, "contents"))
                        }
                        other => panic!("dogfooded companion_files() element: {other:?}"),
                    })
                    .collect()),
                other => panic!("dogfooded companion_files() ok payload: {other:?}"),
            },
            Ok(FfiValue::Res(Err(e))) => match *e {
                FfiValue::Str(msg) => Err(msg),
                other => panic!("dogfooded companion_files() err payload: {other:?}"),
            },
            other => panic!("dogfooded companion_files() call: {other:?}"),
        }
    })
}

/// One `--- name ---`-delimited case file, parsed by the dogfooded AIPL
/// `parse_spec` (`str -> Spec!str`) and marshaled back as an [`FfiValue::Res`]
/// of an [`FfiValue::Struct`]. Field order follows the AIPL declaration; each is
/// looked up by name so a reordering there cannot silently shift a field here.
/// No native fallback; panics if it can't be built or called.
pub fn parse_spec(src: &str) -> Result<SpecFields, String> {
    fn fields(v: &FfiValue, what: &str) -> Vec<(String, FfiValue)> {
        match v {
            FfiValue::Struct(f) => f.clone(),
            other => panic!("dogfooded parse_spec() {what}: {other:?}"),
        }
    }
    fn get<'a>(f: &'a [(String, FfiValue)], name: &str) -> &'a FfiValue {
        match f.iter().find(|(n, _)| n == name) {
            Some((_, v)) => v,
            None => panic!("dogfooded parse_spec() Spec has no field {name:?}"),
        }
    }
    fn as_str(f: &[(String, FfiValue)], name: &str) -> String {
        match get(f, name) {
            FfiValue::Str(s) => s.clone(),
            other => panic!("dogfooded parse_spec() Spec.{name}: {other:?}"),
        }
    }
    fn opt_str(f: &[(String, FfiValue)], name: &str) -> Option<String> {
        match get(f, name) {
            FfiValue::Opt(None) => None,
            FfiValue::Opt(Some(inner)) => match &**inner {
                FfiValue::Str(s) => Some(s.clone()),
                other => panic!("dogfooded parse_spec() Spec.{name} payload: {other:?}"),
            },
            other => panic!("dogfooded parse_spec() Spec.{name}: {other:?}"),
        }
    }
    fn str_array(f: &[(String, FfiValue)], name: &str) -> Vec<String> {
        match get(f, name) {
            FfiValue::Array(items) => items
                .iter()
                .map(|i| match i {
                    FfiValue::Str(s) => s.clone(),
                    other => panic!("dogfooded parse_spec() Spec.{name} element: {other:?}"),
                })
                .collect(),
            other => panic!("dogfooded parse_spec() Spec.{name}: {other:?}"),
        }
    }
    fn entries(f: &[(String, FfiValue)], name: &str) -> Vec<(String, String)> {
        match get(f, name) {
            FfiValue::Array(items) => items
                .iter()
                .map(|i| {
                    let e = fields(i, "Entry");
                    (as_str(&e, "path"), as_str(&e, "contents"))
                })
                .collect(),
            other => panic!("dogfooded parse_spec() Spec.{name}: {other:?}"),
        }
    }
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values("parse_spec", &[FfiValue::Str(src.to_string())]) {
            Ok(FfiValue::Res(Ok(ok))) => {
                let f = fields(&ok, "Spec");
                Ok(SpecFields {
                    source: as_str(&f, "source"),
                    extra_files: entries(&f, "extra_files"),
                    expect_files: entries(&f, "expect_files"),
                    stdout: opt_str(&f, "stdout"),
                    stderr: opt_str(&f, "stderr"),
                    exit_code: match get(&f, "exit_code") {
                        FfiValue::Opt(None) => None,
                        FfiValue::Opt(Some(inner)) => match &**inner {
                            FfiValue::Int(n) => Some(*n as i32),
                            other => panic!("dogfooded parse_spec() Spec.exit_code: {other:?}"),
                        },
                        other => panic!("dogfooded parse_spec() Spec.exit_code: {other:?}"),
                    },
                    errors: opt_str(&f, "errors"),
                    performance: opt_str(&f, "performance"),
                    cli: str_array(&f, "cli"),
                    check: opt_str(&f, "check"),
                })
            }
            Ok(FfiValue::Res(Err(e))) => match *e {
                FfiValue::Str(msg) => Err(msg),
                other => panic!("dogfooded parse_spec() err payload: {other:?}"),
            },
            other => panic!("dogfooded parse_spec() call: {other:?}"),
        }
    })
}

/// The marshaled result of [`parse_spec`] — the AIPL `Spec`, field for field.
/// The cases harness wraps this in its own richer `Spec`.
#[derive(Debug, Default)]
pub struct SpecFields {
    pub source: String,
    pub extra_files: Vec<(String, String)>,
    pub expect_files: Vec<(String, String)>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: Option<i32>,
    pub errors: Option<String>,
    pub performance: Option<String>,
    pub cli: Vec<String>,
    pub check: Option<String>,
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

/// Lay `src` out at `width` — the dogfooded AIPL formatter (`walker.aipl`'s
/// `format_program`) via the FFI. `src` must already be the *code* half of a
/// file, with trailing `--- section ---` blocks split off and per-line trailing
/// whitespace removed; the caller re-attaches the sections and normalizes the
/// final newline. Errors come back as the AIPL `FmtError` struct and are
/// rebuilt as a spanned [`Error`]. No native fallback; panics if the engine
/// can't be built or called.
pub fn format_program(src: &str, width: usize) -> Result<String, Error> {
    FMT_ENGINE.with(|comp| {
        match comp.call_values(
            "format_program",
            &[FfiValue::Str(src.to_string()), FfiValue::Int(width as i64)],
        ) {
            Ok(FfiValue::Res(Ok(v))) => match *v {
                FfiValue::Str(out) => Ok(out),
                other => panic!("dogfooded format_program(): ok side is not a str: {other:?}"),
            },
            Ok(FfiValue::Res(Err(e))) => Err(fmt_error_of(*e)),
            other => panic!("dogfooded format_program() call: {other:?}"),
        }
    })
}

/// `format_source`'s input, as [`fmt_prepare`] splits it: the code to lay out
/// (trailing whitespace already stripped) and the trailing `--- section ---`
/// blocks, which are copied to the output verbatim.
pub struct FmtInput {
    pub cleaned: String,
    pub sections: String,
}

/// The first half of the formatter pipeline, dogfooded — see the AIPL
/// `fmt_prepare`. No native fallback; panics if it can't be built or called.
pub fn fmt_prepare(src: &str) -> FmtInput {
    fn field(fields: &[(String, FfiValue)], name: &str) -> String {
        match fields.iter().find(|(n, _)| n == name) {
            Some((_, FfiValue::Str(s))) => s.clone(),
            other => panic!("dogfooded fmt_prepare() FmtInput.{name}: {other:?}"),
        }
    }
    FMT_ENGINE.with(|comp| {
        match comp.call_values("fmt_prepare", &[FfiValue::Str(src.to_string())]) {
            Ok(FfiValue::Struct(fields)) => FmtInput {
                cleaned: field(&fields, "cleaned"),
                sections: field(&fields, "sections"),
            },
            other => panic!("dogfooded fmt_prepare() call: {other:?}"),
        }
    })
}

/// The second half of the formatter pipeline, dogfooded — see the AIPL
/// `fmt_layout`. `cleaned` must already have been parsed by the caller. No
/// native fallback; panics if it can't be built or called.
pub fn fmt_layout(cleaned: &str, width: usize) -> Result<String, Error> {
    FMT_ENGINE.with(|comp| {
        match comp.call_values(
            "fmt_layout",
            &[
                FfiValue::Str(cleaned.to_string()),
                FfiValue::Int(width as i64),
            ],
        ) {
            Ok(FfiValue::Res(Ok(v))) => match *v {
                FfiValue::Str(out) => Ok(out),
                other => panic!("dogfooded fmt_layout(): ok side is not a str: {other:?}"),
            },
            Ok(FfiValue::Res(Err(e))) => Err(fmt_error_of(*e)),
            other => panic!("dogfooded fmt_layout() call: {other:?}"),
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

/// Every file at or below `dir` whose path ends with `ext`, sorted — computed by
/// the dogfooded AIPL `find_files` via the FFI (itself doing the directory walk;
/// nothing here touches `std::fs`). Not a parser hook — only `tests/fmt.rs`'s
/// formatting enforcement calls this. Returns the walked paths, or the builtin
/// `Error`'s message if the tree couldn't be walked. No native fallback; panics
/// if it can't be built or called.
pub fn find_files(dir: &str, ext: &str) -> Result<Vec<String>, String> {
    DOGFOOD_ENGINE.with(|comp| {
        match comp.call_values(
            "find_files",
            &[
                FfiValue::Str(dir.to_string()),
                FfiValue::Str(ext.to_string()),
            ],
        ) {
            Ok(FfiValue::Res(Ok(v))) => match *v {
                FfiValue::Array(paths) => Ok(paths
                    .into_iter()
                    .map(|p| match p {
                        FfiValue::Str(s) => s,
                        other => panic!("dogfooded find_files() path is not a str: {other:?}"),
                    })
                    .collect()),
                other => panic!("dogfooded find_files() ok side is not an array: {other:?}"),
            },
            Ok(FfiValue::Res(Err(e))) => match *e {
                FfiValue::Str(s) => Err(s),
                other => panic!("dogfooded find_files() err payload: {other:?}"),
            },
            other => panic!("dogfooded find_files() call: {other:?}"),
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

/// Marshal a dogfooded lexer entry — one returning `LexResult<AiplTok>!LexError`
/// — from the dogfood engine: call `entry` on `src` and mirror the returned
/// `LexResult<AiplTok>` (the token stream plus the trivia side-channel) into
/// the parser's [`aipl_parser::LexedOutput`] arm-for-arm, and a `LexError`
/// into [`aipl_parser::LexedError`]. One FFI crossing per source, not one per
/// token. No native fallback; panics if the engine can't be built or called,
/// or if a marshaled shape doesn't match `lex_aipl.aipl`'s types. Shared by
/// the raw ([`lex_aipl`]) and section-stripping ([`lex_aipl_stripped`]) hooks.
fn marshal_lex(
    entry: &str,
    src: &str,
) -> Result<aipl_parser::LexedOutput, aipl_parser::LexedError> {
    use aipl_parser::{LexedError, LexedOutput, LexedStrStyle, LexedToken, LexedTokenKind as K};

    // The `(String, FfiValue)` field named `name` of a marshaled struct.
    fn field(fields: Vec<(String, FfiValue)>, name: &str) -> FfiValue {
        fields
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("dogfooded lex_aipl(): struct has no field {name:?}"))
    }

    // A `Span` struct value as a Rust `Span`.
    fn span_of(v: FfiValue) -> Span {
        let FfiValue::Struct(fields) = v else {
            panic!("dogfooded lex_aipl(): expected a Span struct, got {v:?}");
        };
        let bound = |name: &str| match fields.iter().find(|(n, _)| n == name) {
            Some((_, FfiValue::Int(i))) => *i as usize,
            other => panic!("dogfooded lex_aipl(): Span.{name}: {other:?}"),
        };
        bound("start")..bound("end")
    }

    // An `AiplTok` variant value as the mirrored kind. Value-carrying arms
    // check their payload shape; everything else must be nullary.
    fn kind_of(v: FfiValue) -> K {
        let FfiValue::Variant(case, payload) = v else {
            panic!("dogfooded lex_aipl(): token kind not a variant: {v:?}");
        };
        // The single `str` payload of a value-carrying case.
        let str_payload = |payload: Vec<FfiValue>| match <[FfiValue; 1]>::try_from(payload) {
            Ok([FfiValue::Str(s)]) => s,
            other => panic!("dogfooded lex_aipl(): {case} payload: {other:?}"),
        };
        // The single scalar payload of `IntLit`/`CharTok`.
        let int_payload = |payload: Vec<FfiValue>| match <[FfiValue; 1]>::try_from(payload) {
            Ok([FfiValue::Int(i)]) => i,
            other => panic!("dogfooded lex_aipl(): {case} payload: {other:?}"),
        };
        // `StrLit`'s `(str, StrStyle)` payload: the decoded value plus a nested
        // `StrStyle` variant (nullary), marshaled as `Variant(style_name, [])`.
        let str_lit_payload = |payload: Vec<FfiValue>| match <[FfiValue; 2]>::try_from(payload) {
            Ok([FfiValue::Str(s), FfiValue::Variant(style, style_payload)]) => {
                assert!(
                    style_payload.is_empty(),
                    "dogfooded lex_aipl(): StrStyle {style} carries an unexpected payload"
                );
                let style = match style.as_str() {
                    "Quoted" => LexedStrStyle::Quoted,
                    "TripleQuoted" => LexedStrStyle::TripleQuoted,
                    "Backtick" => LexedStrStyle::Backtick,
                    "TripleBacktick" => LexedStrStyle::TripleBacktick,
                    other => panic!("dogfooded lex_aipl(): unknown StrStyle {other:?}"),
                };
                (s, style)
            }
            other => panic!("dogfooded lex_aipl(): StrLit payload: {other:?}"),
        };
        match case.as_str() {
            "Name" => return K::Name(str_payload(payload)),
            "IntLit" => return K::IntLit(int_payload(payload)),
            "StrLit" => {
                let (s, style) = str_lit_payload(payload);
                return K::StrLit(s, style);
            }
            "CharTok" => return K::CharTok(int_payload(payload) as u8),
            "TemplateHead" => return K::TemplateHead(str_payload(payload)),
            "TemplateMid" => return K::TemplateMid(str_payload(payload)),
            "TemplateTail" => return K::TemplateTail(str_payload(payload)),
            "RawTemplateHead" => return K::RawTemplateHead(str_payload(payload)),
            "RawTemplateMid" => return K::RawTemplateMid(str_payload(payload)),
            "RawTemplateTail" => return K::RawTemplateTail(str_payload(payload)),
            _ => {}
        }
        assert!(
            payload.is_empty(),
            "dogfooded lex_aipl(): {case} carries an unexpected payload"
        );
        match case.as_str() {
            "Space" => K::Space,
            "LineComment" => K::LineComment,
            "BlockComment" => K::BlockComment,
            "AllowMarker" => K::AllowMarker,
            "True" => K::True,
            "False" => K::False,
            "None" => K::None,
            "Fn" => K::Fn,
            "Let" => K::Let,
            "Mut" => K::Mut,
            "Set" => K::Set,
            "Pub" => K::Pub,
            "Import" => K::Import,
            "From" => K::From,
            "As" => K::As,
            "For" => K::For,
            "While" => K::While,
            "Match" => K::Match,
            "Return" => K::Return,
            "Shim" => K::Shim,
            "Struct" => K::Struct,
            "Variant" => K::Variant,
            "If" => K::If,
            "Else" => K::Else,
            "Builtins" => K::Builtins,
            "EqEq" => K::EqEq,
            "Ne" => K::Ne,
            "Arrow" => K::Arrow,
            "FatArrow" => K::FatArrow,
            "AndAnd" => K::AndAnd,
            "OrOr" => K::OrOr,
            "Pipe" => K::Pipe,
            "DotDot" => K::DotDot,
            "PlusPlusPlus" => K::PlusPlusPlus,
            "PlusPlus" => K::PlusPlus,
            "Eq" => K::Eq,
            "Lt" => K::Lt,
            "Le" => K::Le,
            "Gt" => K::Gt,
            "Ge" => K::Ge,
            "Bang" => K::Bang,
            "Plus" => K::Plus,
            "Minus" => K::Minus,
            "Star" => K::Star,
            "Slash" => K::Slash,
            "Percent" => K::Percent,
            "Period" => K::Period,
            "Comma" => K::Comma,
            "Colon" => K::Colon,
            "Semi" => K::Semi,
            "Question" => K::Question,
            "Hash" => K::Hash,
            "LParen" => K::LParen,
            "RParen" => K::RParen,
            "LBrace" => K::LBrace,
            "RBrace" => K::RBrace,
            "LBracket" => K::LBracket,
            "RBracket" => K::RBracket,
            other => panic!("dogfooded lex_aipl(): unknown AiplTok case {other:?}"),
        }
    }

    // A `Token<AiplTok>[]` array value as mirrored tokens.
    fn tokens_of(v: FfiValue) -> Vec<LexedToken> {
        let FfiValue::Array(elems) = v else {
            panic!("dogfooded lex_aipl(): expected a token array, got {v:?}");
        };
        elems
            .into_iter()
            .map(|t| {
                let FfiValue::Struct(fields) = t else {
                    panic!("dogfooded lex_aipl(): token not a struct: {t:?}");
                };
                // Move both fields out (kind first — field consumes the vec).
                let mut kind = None;
                let mut span = None;
                for (n, v) in fields {
                    match n.as_str() {
                        "kind" => kind = Some(v),
                        "span" => span = Some(v),
                        other => panic!("dogfooded lex_aipl(): unexpected Token field {other:?}"),
                    }
                }
                LexedToken {
                    kind: kind_of(kind.expect("dogfooded lex_aipl(): Token missing kind")),
                    span: span_of(span.expect("dogfooded lex_aipl(): Token missing span")),
                }
            })
            .collect()
    }

    DOGFOOD_ENGINE.with(
        |comp| match comp.call_values(entry, &[FfiValue::Str(src.to_string())]) {
            Ok(FfiValue::Res(Ok(res))) => {
                let FfiValue::Struct(fields) = *res else {
                    panic!("dogfooded {entry}(): ok payload not a LexResult struct: {res:?}");
                };
                let mut tokens = None;
                let mut trivia = None;
                for (n, v) in fields {
                    match n.as_str() {
                        "tokens" => tokens = Some(v),
                        "trivia" => trivia = Some(v),
                        other => {
                            panic!("dogfooded {entry}(): unexpected LexResult field {other:?}")
                        }
                    }
                }
                Ok(LexedOutput {
                    tokens: tokens_of(tokens.expect("dogfooded lex entry: missing tokens")),
                    trivia: tokens_of(trivia.expect("dogfooded lex entry: missing trivia")),
                })
            }
            Ok(FfiValue::Res(Err(e))) => {
                let FfiValue::Struct(fields) = *e else {
                    panic!("dogfooded {entry}(): err payload not a LexError struct: {e:?}");
                };
                let message = match field(fields.clone(), "message") {
                    FfiValue::Str(s) => s,
                    other => panic!("dogfooded {entry}(): LexError.message: {other:?}"),
                };
                let span = span_of(field(fields, "span"));
                Err(LexedError { message, span })
            }
            other => panic!("dogfooded {entry}() call: {other:?}"),
        },
    )
}

/// The parser's raw lexer hook (see [`install_parser_hooks`]): lex `src`
/// as-is through the dogfooded AIPL `lex_aipl`. Used by the formatter, which
/// accounts for every byte and must not strip test sections.
fn lex_aipl(src: &str) -> Result<aipl_parser::LexedOutput, aipl_parser::LexedError> {
    marshal_lex("lex_aipl", src)
}

/// The parser's section-stripping lexer hook (see [`install_parser_hooks`]):
/// strip trailing `--- section ---` cases-harness blocks, then lex — through
/// the dogfooded AIPL `lex_aipl_stripped`, which composes both dogfooded steps
/// in one FFI crossing. Used by the highlighter and the parser.
fn lex_aipl_stripped(src: &str) -> Result<aipl_parser::LexedOutput, aipl_parser::LexedError> {
    marshal_lex("lex_aipl_stripped", src)
}

/// Point the parser's hooks at the dogfooded AIPL implementations: the
/// test-section-header parser at
/// [`parse_test_section_header`], the section stripper at [`strip_test_sections`],
/// the trailing-whitespace finder at [`find_trailing_whitespace`], the
/// assertion-location formatter at [`assert_loc`], the error-renderer's
/// caret-block formatter at [`caret_block`], the checker's flexible-literal
/// range check at [`int_fits`], the loader's operator-import gate at
/// [`is_operator_name`], and the lexer at [`lex_aipl`] (which de-dents `"""` raw
/// strings itself, in its emit, so there is no separate raw-string hook).
/// (The formatter needs no hook: it is dogfooded end to end through
/// [`format_program`], and its printer imports `reindent_block.aipl` directly.)
/// Idempotent (first install wins). The compiler's entry points (the CLI and the
/// embedding [`Compilation`] API's callers) install them; there are **no native
/// fallbacks**, so any in-process parse (or error render, literal
/// range-check, or operator-import resolution) must install them first.
pub fn install_parser_hooks() {
    aipl_parser::set_test_section_header_hook(parse_test_section_header);
    aipl_parser::set_strip_test_sections_hook(strip_test_sections);
    aipl_parser::set_split_test_sections_hook(split_test_sections);
    aipl_parser::set_companion_files_hook(companion_files);
    aipl_parser::set_find_trailing_whitespace_hook(find_trailing_whitespace);
    aipl_parser::set_assert_loc_hook(assert_loc);
    aipl_parser::set_lex_hook(lex_aipl);
    aipl_parser::set_lex_stripped_hook(lex_aipl_stripped);
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
) -> Result<(HashMap<String, FuncInfo>, HashMap<String, TypeDef>, String), Vec<Error>> {
    // Builtin *types* (e.g. `__builtin_Span`) are real struct declarations,
    // unlike builtin functions: a call to a builtin function is intercepted by
    // reserved name in codegen and never needs to reach monomorphization as a
    // real item, but a builtin-typed value is constructed/laid out/passed
    // around exactly like a user struct, so it must flow through mono and
    // codegen as one. Prepend them to the actual compiled program (not just
    // the checker-only view below) so `build_struct_layouts` and mono's own
    // struct table see them regardless of which file (if any) imports them.
    //
    // The AIPL-implemented builtins this program can reach, computed once here
    // and passed to everything that needs their declarations. Each one costs a
    // parse of its `.aipl` source, so the ones the program never mentions are
    // never loaded. Taken from the program as handed in — the lowering passes
    // below rewrite expressions but never introduce a new builtin call.
    let needed = aipl_mono::aipl_builtin_demand(program);
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
    // Resolve generic struct/variant type annotations (`Foo<i64>`) to synthetic
    // monomorphic named structs/variants before anything else, so tuple lowering
    // and the rest of the pipeline only see ordinary named types.
    let program = &aipl_mono::lower_generics(program)?;

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
        items: builtin_decls(&needed),
    });
    let check_program = Program {
        items: lowered_builtins
            .items
            .into_iter()
            .chain(program.items.iter().cloned())
            .collect(),
    };
    // `check` hands back the program with the types of context-dependent
    // expressions stamped in (`Expr::ty`) — a bare `none`, an empty `[]`, a
    // generic constructor. Everything downstream runs on *that*, because the
    // passes below move expressions away from the context those types came from.
    let checked = aipl_mono::check(&check_program)?;
    // `check_program` is the synthesized builtin decls followed by the real
    // program; take back the second half, now stamped. `check` neither reorders
    // nor adds items, so the tail lines up by construction.
    let program = &Program {
        items: checked.items[checked.items.len() - program.items.len()..].to_vec(),
    };

    // Optimization: inline single-use private functions (a no-op unless the
    // program has a `main` — see `inline_single_use`). Runs on the checked source
    // before monomorphization; mono's reachability then drops the inlined-away
    // definitions.
    let inlined = aipl_mono::inline_single_use(program);

    // Optimization: inline every function small enough to be worth duplicating,
    // at all of its call sites. After `inline_single_use` (which is strictly
    // cheaper — it moves a body rather than copying it) so a single-use function
    // is never duplicated here first.
    let inlined = aipl_mono::inline_small(&inlined, inline_max_exprs());

    // Optimization: substitute a binding read exactly once into its use site.
    // This removes no work by itself — codegen emits the same instructions
    // either way — but the passes below match on the *shape* of an expression,
    // and a binding hides the shape from them: `let ys = xs.filter(p);
    // ys.map(f)` is the same computation as `xs.filter(p).map(f)`, and only the
    // second is a chain the fusion pass can see. So it runs first, and after
    // inlining, which is what creates most single-use bindings.
    let inlined = aipl_mono::inline_single_use_bindings(
        &inlined,
        &aipl_mono::effectful_fns(&check_program.items),
    );

    // Optimization: fuse a composite expression into one builtin that computes
    // the same answer with less work (`xs.count(x) < 4` stops at the fourth
    // match). After inlining, so a comparison that only became visible by
    // inlining is fused too; before folding, so a fused call's constant
    // arguments still fold. The effect set comes from `check_program`, which is
    // where the builtin signatures (and their `!prints`) live.
    let inlined =
        aipl_mono::fuse_operations(&inlined, &aipl_mono::effectful_fns(&check_program.items));

    // Optimization: fold constant subexpressions (`2 + 3` → `5`). Runs after
    // `check` so diagnostics always report against the unfolded source, and
    // after inlining so bodies folded here are the ones actually emitted.
    let folded = aipl_mono::fold_constants(&inlined);

    // Optimization: sink a binding only one branch uses into that branch, so the
    // arms that ignore its value stop computing it. After inlining, which is
    // what creates most of them (a call's arguments become bindings ahead of the
    // inlined body, and that body often branches), and after folding, so a
    // binding folded down to a literal is already gone rather than sunk.
    let folded = aipl_mono::sink_bindings(&folded, &aipl_mono::effectful_fns(&check_program.items));

    // Resolve generic `any[]` functions into concrete instances first, so the
    // rest of codegen only ever sees concrete types.
    let monomorphized = aipl_mono::monomorphize(&folded, dbg)?;
    // A second inlining pass now that monomorphization has lifted each lambda to
    // its own function and split higher-order callees into per-lambda
    // specializations: those lifted lambdas (and any other now-single-use
    // instance) are each called from exactly one site, so fold them back in. Only
    // possible post-mono — the lambdas don't exist until mono creates them.
    // Inline single-use instances. The functions an *external* caller reaches by
    // name — the FFI engines' entries, and the `check` driver's `__test_main` —
    // are named here so they are never elided; they can still be inlined into
    // any AIPL caller, since keeping the definition is all an external call
    // needs. `DOGFOOD_ENTRIES`/`FMT_ENTRIES` are the same lists the engines are
    // built and validated against, so this can't drift from what FFI actually
    // calls.
    let externally_called: std::collections::HashSet<String> = DOGFOOD_ENTRIES
        .iter()
        .chain(FMT_ENTRIES)
        .map(|s| (*s).to_string())
        .chain(["main".to_string(), "__test_main".to_string()])
        .collect();
    let program = aipl_mono::inline_single_use_post_mono(&monomorphized, &externally_called);
    // Then the small bodies, at *every* call site. This is what the sinking pass
    // below needs to see through a call: `value_or`'s `match` has to be in the
    // caller before a binding can be moved into one of its arms.
    let program =
        aipl_mono::inline_small_post_mono(&program, inline_max_exprs(), &externally_called);
    // Sink again over the monomorphized program: mono instantiates the
    // AIPL-implemented builtins the pre-mono run could not see, and post-mono
    // inlining folds each lifted lambda and single-use instance into its caller
    // — so bindings that only one branch reads become visible here that were not
    // visible there. `value_or`'s default is the motivating case.
    let program = &aipl_mono::sink_bindings_post_mono(
        &program,
        &aipl_mono::effectful_fns(&check_program.items),
    );

    let mut ctx = module.make_context();
    let mut fbc = FunctionBuilderContext::new();
    let mut funcs: HashMap<String, FuncInfo> = HashMap::new();
    let mut ir = String::new();

    let structs = build_struct_layouts(program)?;

    // Register builtin signatures/types so user code can `print("...")`. The
    // actual `aipl_*` imports are declared lazily on first use (see `Builtins`).
    let builtins = register_builtins(&mut funcs, &needed);

    // Monomorphization has already split each function into the instances the
    // program reaches — borrow and owned forms alike, each its own `ConcreteFn`
    // with each parameter's `owned` set. Codegen just declares and defines each
    // one.
    let mut decls: Vec<(FuncId, &aipl_mono::ConcreteFn)> = Vec::new();
    // Which heap parameters each instance only *inspects*, so its call sites can
    // skip the borrow retain and its body the matching entry-scope release. One
    // shared answer for both halves — see `aipl_mono::inspect_only_params`.
    let mut inspect_only = aipl_mono::inspect_only_params(program);
    // Which functions carry `CallConv::Tail` (and so split into a `$tail` body
    // plus an exported trampoline) — see the "Tail calls" section.
    let tail_plan = tail_call_plan(program, &structs);
    for f in &program.fns {
        // Signature/effect/mutating validity is checked up front by
        // `aipl_mono::check`; codegen trusts it and goes straight to lowering.
        let participant = tail_plan.contains(&f.name);

        let mut sig = module.make_signature();
        build_signature(&mut sig, f, &structs, false);

        let export_name = match main_export_name {
            Some(rename) if f.name == "main" => rename,
            _ => &f.name,
        };
        let id = module
            .declare_function(export_name, Linkage::Export, &sig)
            .map_err(|e| Error::msg(format!("declare {}: {e}", f.name)))?;
        dbg.trace("codegen", format_args!("declare `{}`", f.name));
        // A participant's real body: same ABI, `tail` convention, module-local.
        // `export_name` (not `f.name`) so a renamed `main` — which is never a
        // participant today, but would be silently mis-declared if it became one
        // — keeps one consistent symbol stem.
        let tail_id = if participant {
            let mut tail_sig = module.make_signature();
            build_signature(&mut tail_sig, f, &structs, true);
            let sym = format!("{export_name}{TAIL_SUFFIX}");
            Some(
                module
                    .declare_function(&sym, Linkage::Local, &tail_sig)
                    .map_err(|e| Error::msg(format!("declare {sym}: {e}")))?,
            )
        } else {
            None
        };
        let inspects = inspect_only.remove(&f.name).unwrap_or_default();
        let info = FuncInfo {
            link: FuncLink::User(id),
            tail_id,
            params: f
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| ParamInfo {
                    // Widened at the boundary: monomorphization now hands
                    // codegen `ConcreteType`, while codegen's own machinery
                    // still speaks the abstract representation. Migrating it is
                    // the next slice of the split.
                    ty: p.ty.clone(),
                    owned: p.owned,
                    // A participant gives up retain elision on its heap
                    // parameters: an inspect-only one is a borrow on the
                    // caller's reference, which a tail call would release before
                    // transferring control. Paying the pair is what makes the
                    // call site eligible at all.
                    inspect_only: !participant && inspects.get(i).copied().unwrap_or(false),
                    // …and for the same reason its boxed parameters, borrows by
                    // default, join the retain/release protocol.
                    tail_owned: participant && is_boxed(&p.ty, &structs),
                })
                .collect(),
            return_ty: f.abi_return_type(),
            effects: f.effects.clone(),
            is_mutating: f.is_mutating(),
        };
        funcs.insert(f.name.clone(), info);
        decls.push((id, f));
    }

    // One counter across all functions so synthesized literal names are unique.
    let lit_ctr = Cell::new(0u32);
    // Static string literals interned by content across the whole compilation.
    let str_data = RefCell::new(StrLiterals::default());
    // Per-element-type array drop/retain helpers, generated on demand while
    // compiling and defined afterward (below).
    let elem_rc = RefCell::new(ElemRc::default());
    for (id, f) in decls {
        dbg.trace("codegen", format_args!("define `{}`", f.name));
        define_fn(
            module, &mut ctx, &mut fbc, id, f, &funcs, &structs, &builtins, &lit_ctr, &str_data,
            &elem_rc, &mut ir, instrument, dbg,
        )?;
    }

    // Per-boxed-type payload drop helpers (`__rec_drop_<n>`), requested by boxed
    // value construction in the bodies above. A body drops the payload's
    // contained values; a same-group boxed child just weak-decs (no further
    // helper), but a non-recursive heap field (a `List[]`) can request an array
    // element helper — so drain these *before* the element `pending` drain below.
    let rec_drop_pending = std::mem::take(&mut elem_rc.borrow_mut().rec_drop_pending);
    for (name, id) in rec_drop_pending {
        define_rec_drop_fn(
            module, &mut ctx, &mut fbc, &builtins, &structs, id, &name, &mut ir, instrument,
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
            instrument,
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
            instrument,
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
            instrument,
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
            instrument,
        )?;
    }

    // Per-err-type test-fail helpers (`?` inside a `.test`). Each body renders
    // the err via `emit_to_str`, which requests a `to_str` helper — so drain this
    // *before* `tostr_pending` below, so those requests are then satisfied.
    let test_fail_pending = std::mem::take(&mut elem_rc.borrow_mut().test_fail_pending);
    for (ty, id) in test_fail_pending {
        define_test_fail_fn(
            module, &mut ctx, &mut fbc, &funcs, &structs, &builtins, &lit_ctr, &str_data, &elem_rc,
            id, &ty, &mut ir, instrument,
        )?;
    }

    // Per-type structural-equality helpers. A body compares borrowed values and,
    // for a composite field/element, calls *that* type's eq helper — so defining
    // one can enqueue further eq helpers (and nothing else). Drain until empty.
    loop {
        let batch = std::mem::take(&mut elem_rc.borrow_mut().eq_pending);
        if batch.is_empty() {
            break;
        }
        for (ty, id) in batch {
            define_eq_fn(
                module, &mut ctx, &mut fbc, &funcs, &structs, &builtins, &lit_ctr, &str_data,
                &elem_rc, id, &ty, &mut ir, instrument,
            )?;
        }
    }

    // Per-type `to_str` rendering helpers. A non-recursive type's helper renders
    // structurally *inline* and requests no further helpers. A *boxed*
    // (recursive) type's helper instead calls the helper of each nested boxed
    // child (so it terminates by recursion through calls, not inlining), which
    // for a *different* boxed type (mutual recursion) enqueues a fresh helper —
    // so drain until empty, like the eq helpers.
    loop {
        let batch = std::mem::take(&mut elem_rc.borrow_mut().tostr_pending);
        if batch.is_empty() {
            break;
        }
        for (ty, id) in batch {
            define_tostr_fn(
                module, &mut ctx, &mut fbc, &funcs, &structs, &builtins, &lit_ctr, &str_data,
                &elem_rc, id, &ty, &mut ir, instrument,
            )?;
        }
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
///
/// This table is for code compiled *in this process*. The prebuilt dogfood
/// object (see `build.rs`) resolves the same builtins the ordinary way, through
/// the system linker — which is why every one of them carries `#[no_mangle]`:
/// an address registered here is invisible to a linker, so the object has to be
/// able to find them by symbol name instead.
fn new_jit_module() -> Result<JITModule, Error> {
    let isa = host_isa()?;
    let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    // Expose builtins to the JIT linker.
    // The second ABI (`STR_REPR.md` stage 1): 24-byte `str` values passed by
    // pointer. Registered alongside the originals — an artifact links whichever
    // it names, which is what lets the switch be tested one case at a time.
    jit_builder.symbol("aipl_print", str24_host::aipl_print as *const u8);
    jit_builder.symbol(
        "aipl_print_error",
        str24_host::aipl_print_error as *const u8,
    );
    jit_builder.symbol("aipl_inc", str24::aipl_inc as *const u8);
    jit_builder.symbol("aipl_dec", str24::aipl_dec as *const u8);
    jit_builder.symbol("aipl_str_len", str24::aipl_str_len as *const u8);
    jit_builder.symbol("aipl_str_eq", str24::aipl_str_eq as *const u8);
    jit_builder.symbol("aipl_str_cmp", str24::aipl_str_cmp as *const u8);
    jit_builder.symbol("aipl_str_hash", str24::aipl_str_hash as *const u8);
    jit_builder.symbol("aipl_char_at", str24::aipl_char_at as *const u8);
    jit_builder.symbol("aipl_str_slice", str24::aipl_str_slice as *const u8);
    jit_builder.symbol("aipl_concat", str24::aipl_concat as *const u8);
    jit_builder.symbol("aipl_trim", str24::aipl_trim as *const u8);
    jit_builder.symbol("aipl_str_data", str24::aipl_str_data as *const u8);
    jit_builder.symbol("aipl_str_contains", str24::aipl_str_contains as *const u8);
    jit_builder.symbol(
        "aipl_str_starts_with",
        str24::aipl_str_starts_with as *const u8,
    );
    jit_builder.symbol(
        "aipl_str_starts_with_at",
        str24::aipl_str_starts_with_at as *const u8,
    );
    jit_builder.symbol("aipl_str_ends_with", str24::aipl_str_ends_with as *const u8);
    jit_builder.symbol("aipl_str_reverse", str24::aipl_str_reverse as *const u8);
    jit_builder.symbol("aipl_str_sort", str24::aipl_str_sort as *const u8);
    jit_builder.symbol("aipl_str_repeat", str24::aipl_str_repeat as *const u8);
    jit_builder.symbol("aipl_char_to_str", str24::aipl_char_to_str as *const u8);
    jit_builder.symbol("aipl_str_alloc", str24::aipl_str_alloc as *const u8);
    jit_builder.symbol("aipl_str_write_ptr", str24::aipl_str_write_ptr as *const u8);
    jit_builder.symbol("aipl_str_grew", str24::aipl_str_grew as *const u8);
    jit_builder.symbol("aipl_str_push_byte", str24::aipl_str_push_byte as *const u8);
    jit_builder.symbol("aipl_str_append", str24::aipl_str_append as *const u8);
    jit_builder.symbol("aipl_str_iter_init", str24::aipl_str_iter_init as *const u8);
    jit_builder.symbol("aipl_str_iter_next", str24::aipl_str_iter_next as *const u8);
    jit_builder.symbol(
        "aipl_arr_drop_opt_str",
        str24::aipl_arr_drop_opt_str as *const u8,
    );
    jit_builder.symbol(
        "aipl_arr_retain_opt_str",
        str24::aipl_arr_retain_opt_str as *const u8,
    );
    jit_builder.symbol(
        "aipl_read_file_to_string",
        str24_host::aipl_read_file_to_string as *const u8,
    );
    jit_builder.symbol(
        "aipl_write_string_to_file",
        str24_host::aipl_write_string_to_file as *const u8,
    );
    jit_builder.symbol("aipl_list_files", aipl_list_files as *const u8);
    jit_builder.symbol("aipl_execute_program", aipl_execute_program as *const u8);
    jit_builder.symbol("aipl_str_split", aipl_str_split as *const u8);
    jit_builder.symbol("aipl_str_join", aipl_str_join as *const u8);
    jit_builder.symbol("aipl_arr_drop_str", str24::aipl_arr_drop_str as *const u8);
    jit_builder.symbol(
        "aipl_arr_retain_str",
        str24::aipl_arr_retain_str as *const u8,
    );
    jit_builder.symbol("aipl_now_nanos", aipl_now_nanos as *const u8);
    jit_builder.symbol("aipl_monotonic_now", aipl_monotonic_now as *const u8);
    jit_builder.symbol("aipl_shim_get", aipl_shim_get as *const u8);
    jit_builder.symbol("aipl_shim_set", aipl_shim_set as *const u8);
    jit_builder.symbol("aipl_arr_reverse", aipl_arr_reverse as *const u8);
    jit_builder.symbol("aipl_arr_sort", aipl_arr_sort as *const u8);
    jit_builder.symbol("aipl_arr_slice", aipl_arr_slice as *const u8);
    jit_builder.symbol("aipl_array_new", aipl_array_new as *const u8);
    jit_builder.symbol("aipl_array_with_cap", aipl_array_with_cap as *const u8);
    jit_builder.symbol("aipl_array_push", aipl_array_push as *const u8);
    jit_builder.symbol("aipl_array_push_mut", aipl_array_push_mut as *const u8);
    jit_builder.symbol("aipl_arr_reserve", aipl_arr_reserve as *const u8);
    jit_builder.symbol("aipl_arr_extend", aipl_arr_extend as *const u8);
    jit_builder.symbol("aipl_arr_join", aipl_arr_join as *const u8);
    jit_builder.symbol("aipl_array_dec", aipl_array_dec as *const u8);
    jit_builder.symbol("aipl_arr_inc", aipl_arr_inc as *const u8);
    jit_builder.symbol("aipl_rec_alloc", aipl_rec_alloc as *const u8);
    jit_builder.symbol("aipl_rec_inc_strong", aipl_rec_inc_strong as *const u8);
    jit_builder.symbol("aipl_rec_dec_strong", aipl_rec_dec_strong as *const u8);
    jit_builder.symbol("aipl_rec_inc_weak", aipl_rec_inc_weak as *const u8);
    jit_builder.symbol("aipl_rec_dec_weak", aipl_rec_dec_weak as *const u8);
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
    jit_builder.symbol("aipl_count_call", aipl_count_call as *const u8);
    jit_builder.symbol("aipl_test_begin", aipl_test_begin as *const u8);
    jit_builder.symbol("aipl_assert", aipl_assert as *const u8);
    jit_builder.symbol("aipl_test_fail", aipl_test_fail as *const u8);
    jit_builder.symbol("aipl_test_fail_none", aipl_test_fail_none as *const u8);
    jit_builder.symbol("aipl_test_end", aipl_test_end as *const u8);
    jit_builder.symbol("aipl_test_summary", aipl_test_summary as *const u8);
    jit_builder.symbol("aipl_arr_drop_arr", aipl_arr_drop_arr as *const u8);
    jit_builder.symbol("aipl_arr_retain_ptr", aipl_arr_retain_ptr as *const u8);
    jit_builder.symbol("aipl_arr_drop_opt_arr", aipl_arr_drop_opt_arr as *const u8);
    jit_builder.symbol("aipl_arr_retain_opt", aipl_arr_retain_opt as *const u8);
    jit_builder.symbol("aipl_i64_len", aipl_i64_len as *const u8);
    jit_builder.symbol("aipl_write_i64", aipl_write_i64 as *const u8);
    jit_builder.symbol("aipl_u64_len", aipl_u64_len as *const u8);
    jit_builder.symbol("aipl_write_u64", aipl_write_u64 as *const u8);
    jit_builder.symbol("aipl_write_bytes", aipl_write_bytes as *const u8);
    Ok(JITModule::new(jit_builder))
}

/// The dogfood-IR tag for an FFI-marshalable entry type. The FFI marshals
/// scalars (any integer width, `bool`, `char`), `str`, `Unit` (the empty-payload side of a `Result`), optionals of
/// those (a trailing `?` per `Optional` layer, e.g. `str?`), results of those
/// (`{ok}!{err}`, e.g. `unit!Error`), arrays (a trailing `[]`, e.g.
/// `Token[]`), and structs/variants (the bare type name, e.g. `Span`, whose
/// layout is carried separately on a `; struct`/`; variant` manifest line).
/// Anything else can't cross the FFI and is rejected here.
fn ffi_type_tag(t: &ConcreteType) -> Result<String, Error> {
    Ok(match t {
        // Every scalar the FFI marshals — any integer width (`i64`, `u64`, `u8`,
        // …), `bool`, `char` — plus `str`. The tag is the type's own spelling,
        // which `ffi_type_from_tag` reads back with `Primitive::from_name`.
        ConcreteType::Primitive(p) if is_ffi_scalar(t) || is_str_repr(t) => p.name().to_string(),
        ConcreteType::Unit => "unit".to_string(),
        ConcreteType::Optional(inner) => format!("{}?", ffi_type_tag(inner)?),
        ConcreteType::Result(ok, err) => format!("{}!{}", ffi_type_tag(ok)?, ffi_type_tag(err)?),
        // An array of results would read back ambiguously (`A!B[]` already
        // means a result whose err side is an array), so it's rejected; no
        // other element type contains `!`.
        ConcreteType::Array(elem) if matches!(**elem, ConcreteType::Result(_, _)) => {
            return Err(Error::msg(
                "dogfood entry type is an array of results; that can't be tagged unambiguously"
                    .to_string(),
            ))
        }
        ConcreteType::Array(elem) => format!("{}[]", ffi_type_tag(elem)?),
        ConcreteType::Named(n) => n.clone(),
        _ => {
            return Err(Error::msg(format!(
                "dogfood entry type {} is not FFI-serializable (only i64/bool/char/str, \
                 optionals/results/arrays of those, and structs/variants)",
                type_name(t)
            )))
        }
    })
}

/// Collect the distinct struct/variant type names a type references — itself
/// if `Named` and not the builtin `Error` (which is str-repr, not a struct),
/// the core of an `Optional`, either side of a `Result`, an array's element —
/// and, transitively, everything a collected type's own fields (or case
/// payloads) reference, appending any not already in `out`. The presence
/// check doubles as the visited set, so a (hypothetical) type cycle can't
/// recurse forever. Used to gather the layouts a set of dogfood entries needs
/// serialized.
fn collect_named_types(
    t: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
    out: &mut Vec<String>,
) {
    match t {
        ConcreteType::Named(n) if !is_error(t) => {
            if out.iter().any(|s| s == n) {
                return;
            }
            out.push(n.clone());
            match structs.get(n) {
                Some(TypeDef::Struct(s)) => {
                    for f in &s.fields {
                        collect_named_types(&f.ty, structs, out);
                    }
                }
                Some(TypeDef::Variant(v)) => {
                    for case in &v.cases {
                        for f in &case.fields {
                            collect_named_types(&f.ty, structs, out);
                        }
                    }
                }
                // Unknown name: leave it for the emission loop to report.
                None => {}
            }
        }
        ConcreteType::Optional(inner) => collect_named_types(inner, structs, out),
        ConcreteType::Result(ok, err) => {
            collect_named_types(ok, structs, out);
            collect_named_types(err, structs, out);
        }
        ConcreteType::Array(elem) => collect_named_types(elem, structs, out),
        _ => {}
    }
}

/// Inverse of [`ffi_type_tag`]: parse a manifest type tag back into a `ConcreteType`. A
/// trailing `?` is an `Optional` layer over the rest; an unsuffixed tag
/// containing `!` is a `Result` (`{ok}!{err}`, each side parsed the same way —
/// `!` can't appear in a bare tag otherwise, since identifiers don't carry it);
/// then a trailing `[]` is an `Array` layer (after the `!` split, so `A!B[]`
/// is a result whose err side is an array — arrays of results are never
/// emitted); a non-keyword tag is a struct/variant type name ([`ConcreteType::Named`])
/// whose layout the `; struct`/`; variant` lines supply.
fn ffi_type_from_tag(tag: &str) -> Result<ConcreteType, Error> {
    if let Some(base) = tag.strip_suffix('?') {
        return Ok(ConcreteType::Optional(Box::new(ffi_type_from_tag(base)?)));
    }
    if let Some((ok, err)) = tag.split_once('!') {
        return Ok(ConcreteType::Result(
            Box::new(ffi_type_from_tag(ok)?),
            Box::new(ffi_type_from_tag(err)?),
        ));
    }
    if let Some(base) = tag.strip_suffix("[]") {
        return Ok(ConcreteType::Array(Box::new(ffi_type_from_tag(base)?)));
    }
    if tag == "unit" {
        return Ok(ConcreteType::Unit);
    }
    // A primitive spelling (`i64`, `u32`, `bool`, `char`, `str`, …) is that
    // primitive; anything else names a struct/variant. A user type can't collide
    // here: in type position the parser already resolves a primitive spelling to
    // the primitive, so a declaration of that name is unreachable as a type.
    Ok(match Primitive::from_name(tag) {
        Some(p) => ConcreteType::Primitive(p),
        None => ConcreteType::Named(tag.to_string()),
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
) -> Result<String, Vec<Error>> {
    let dbg = DebugOptions::new(false);
    // Propagate a load/parse failure (don't `unwrap`) so the caller can pin the
    // offending source file and report how to test just it.
    let program = aipl_loader::load_program_sources(sources, dbg)?;
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
    out.push_str(";   cargo test --test dogfood -- --ignored dogfood_ir::fill_dogfood_ir\n");
    // Struct types any entry references (param or return), so the inverse can
    // rebuild their layouts and marshal a struct return — collected here, emitted
    // as `; struct` lines after the entries.
    let mut referenced_structs: Vec<String> = Vec::new();
    for name in entries {
        let info = resolve_dogfood_entry(&funcs, name)?;
        let id = match info.link {
            FuncLink::User(id) => remap[&id.as_u32()],
            FuncLink::Builtin(_) => {
                return Err(
                    Error::msg(format!("dogfood entry {name:?} is a builtin, not a fn")).into(),
                )
            }
        };
        if info.is_mutating {
            return Err(Error::msg(format!(
                "dogfood entry {name:?} is a mutating method; not FFI-callable"
            ))
            .into());
        }
        let params = info
            .param_types()
            .map(ffi_type_tag)
            .collect::<Result<Vec<_>, _>>()?
            .join(" ");
        let sep = if params.is_empty() { "" } else { " " };
        let ret = ffi_type_tag(&info.return_ty)?;
        out.push_str(&format!("; entry {name} {id}{sep}{params} -> {ret}\n"));
        for t in info.param_types().chain(std::iter::once(&info.return_ty)) {
            collect_named_types(t, &structs, &mut referenced_structs);
        }
    }
    for sname in &referenced_structs {
        match structs.get(sname) {
            Some(TypeDef::Struct(layout)) => {
                let mut fields = String::new();
                for f in &layout.fields {
                    fields.push_str(&format!(
                        " {}@{}:{}",
                        f.name,
                        f.offset,
                        ffi_type_tag(&f.ty)?
                    ));
                }
                out.push_str(&format!("; struct {sname} {}{fields}\n", layout.size));
            }
            // `; variant <name> <size> <Case> <Case>(<off>:<tag>,...) ...` —
            // one whitespace-free token per case (payload fields are
            // positional, so they carry offset:tag only, comma-separated
            // inside the parens; a nullary case is the bare name).
            Some(TypeDef::Variant(layout)) => {
                let mut cases = String::new();
                for case in &layout.cases {
                    if case.fields.is_empty() {
                        cases.push_str(&format!(" {}", case.name));
                        continue;
                    }
                    let fields = case
                        .fields
                        .iter()
                        .map(|f| Ok(format!("{}:{}", f.offset, ffi_type_tag(&f.ty)?)))
                        .collect::<Result<Vec<_>, Error>>()?
                        .join(",");
                    cases.push_str(&format!(" {}({fields})", case.name));
                }
                out.push_str(&format!("; variant {sname} {}{cases}\n", layout.size));
            }
            None => {
                return Err(
                    Error::msg(format!("dogfood entry references unknown type {sname:?}")).into(),
                )
            }
        }
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

/// A prebuilt entry point: a function compiled into this binary by `build.rs`,
/// held as a bare `fn()` because that is the one code-pointer shape Rust lets a
/// `static` hold (a raw pointer is not `Sync`). Every call site transmutes it to
/// the real signature, exactly as it does a JIT-finalized address.
pub type PrebuiltFn = unsafe extern "C" fn();

/// Where a [`Compilation`]'s machine code lives.
enum Code {
    /// Compiled in this process, and owned by this module — source compilation,
    /// and the `AIPL_DOGFOOD_IR` / `AIPL_FMT_IR` staging overrides.
    Jit(JITModule),
    /// Compiled at build time into the binary, addressed by a generated
    /// name→address table. See [`Compilation::from_prebuilt`].
    Prebuilt(&'static [(&'static str, PrebuiltFn)]),
}

/// The FFI-callable metadata for an artifact's `; entry` functions, keyed by
/// name — what `call` / `call_values` marshal against. `link` says how to reach
/// the code, and differs between the JIT and prebuilt paths.
fn entry_funcs(
    manifest: &aipl_artifact::Manifest,
    link: impl Fn(u32) -> FuncLink,
) -> Result<HashMap<String, FuncInfo>, Error> {
    manifest
        .entries
        .iter()
        .map(|e| {
            let params = e
                .params
                .iter()
                .map(|t| ffi_type_from_tag(t).map(ParamInfo::borrowed))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                e.name.clone(),
                FuncInfo {
                    link: link(e.id),
                    tail_id: None,
                    params,
                    return_ty: ffi_type_from_tag(&e.ret)?,
                    effects: Vec::new(),
                    is_mutating: false,
                },
            ))
        })
        .collect()
}

/// Struct and variant layouts recovered from the artifact's `; struct` /
/// `; variant` manifest lines, so a struct-returning entry can be marshaled back
/// to the host field by field.
fn manifest_structs(manifest: &aipl_artifact::Manifest) -> Result<HashMap<String, TypeDef>, Error> {
    let mut out = HashMap::new();
    for line in &manifest.types {
        let (name, def) = match line {
            // `<name> <size> <field>@<offset>:<tag> ...`
            aipl_artifact::TypeLine::Struct(body) => {
                let toks: Vec<&str> = body.split_whitespace().collect();
                let (name, size) = manifest_type_head(&toks, "struct")?;
                let mut fields = Vec::new();
                for ft in &toks[2..] {
                    let (fname, rest) = ft
                        .split_once('@')
                        .ok_or_else(|| Error::msg(format!("malformed `; struct` field {ft:?}")))?;
                    let (off, tag) = rest
                        .split_once(':')
                        .ok_or_else(|| Error::msg(format!("malformed `; struct` field {ft:?}")))?;
                    fields.push(FieldLayout {
                        name: fname.to_string(),
                        ty: ffi_type_from_tag(tag)?,
                        offset: off.parse().map_err(|_| {
                            Error::msg(format!("bad `; struct` field offset {ft:?}"))
                        })?,
                    });
                }
                (
                    name,
                    // FFI-marshalable types are never recursive (`check_ffi_return`
                    // rejects boxed types), so the manifest carries no flags.
                    TypeDef::Struct(StructLayout {
                        fields,
                        size,
                        boxed: false,
                        scc: 0,
                    }),
                )
            }
            // `<name> <size> <Case> <Case>(<off>:<tag>,...) ...`
            // (payload fields are positional: offset + type tag, no name).
            aipl_artifact::TypeLine::Variant(body) => {
                let toks: Vec<&str> = body.split_whitespace().collect();
                let (name, size) = manifest_type_head(&toks, "variant")?;
                let mut cases = Vec::new();
                for ct in &toks[2..] {
                    let (cname, fields) = match ct.split_once('(') {
                        None => (ct.to_string(), Vec::new()),
                        Some((cname, rest)) => {
                            let inner = rest.strip_suffix(')').ok_or_else(|| {
                                Error::msg(format!("malformed `; variant` case {ct:?}"))
                            })?;
                            let mut fields = Vec::new();
                            for ft in inner.split(',') {
                                let (off, tag) = ft.split_once(':').ok_or_else(|| {
                                    Error::msg(format!("malformed `; variant` field {ft:?}"))
                                })?;
                                fields.push(FieldLayout {
                                    name: String::new(),
                                    ty: ffi_type_from_tag(tag)?,
                                    offset: off.parse().map_err(|_| {
                                        Error::msg(format!("bad `; variant` field offset {ft:?}"))
                                    })?,
                                });
                            }
                            (cname.to_string(), fields)
                        }
                    };
                    cases.push(VariantCaseLayout {
                        name: cname,
                        fields,
                    });
                }
                (
                    name,
                    TypeDef::Variant(VariantLayout {
                        cases,
                        size,
                        boxed: false,
                        scc: 0,
                    }),
                )
            }
        };
        out.insert(name, def);
    }
    Ok(out)
}

/// The leading `<name> <size>` both type-manifest lines start with.
fn manifest_type_head(toks: &[&str], kind: &str) -> Result<(String, u32), Error> {
    let name = toks
        .first()
        .ok_or_else(|| Error::msg(format!("`; {kind}` line missing name")))?;
    let size = toks
        .get(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::msg(format!("`; {kind}` line missing/invalid size")))?;
    Ok((name.to_string(), size))
}
impl Compilation {
    pub fn new(program: &Program, dbg: DebugOptions) -> Result<Self, Vec<Error>> {
        let mut module = new_jit_module()?;

        // The JIT never reports perf counters, so it's never instrumented.
        let (funcs, structs, ir) = compile_program(&mut module, program, None, dbg, false)?;

        module
            .finalize_definitions()
            .map_err(|e| Error::msg(format!("finalize: {e}")))?;

        Ok(Self {
            code: Code::Jit(module),
            funcs,
            structs,
            // Whichever representation this codegen just emitted.
            abi: Abi::active(),
            ir,
        })
    }

    /// Build a `Compilation` by linking dogfood IR (the artifact produced by
    /// [`generate_dogfood_artifact`]) into a fresh `JITModule`, *without*
    /// running the AIPL frontend — so it works even when the frontend can't
    /// currently compile the dogfooded `.aipl` sources.
    ///
    /// Ordinary runs don't come through here: they use
    /// [`Compilation::from_prebuilt`], which reads the same artifact already
    /// lowered to machine code by `build.rs`. This path exists for the artifact
    /// the binary *wasn't* built against — the `AIPL_DOGFOOD_IR` /
    /// `AIPL_FMT_IR` staging overrides, which run candidate IR across the whole
    /// corpus before it is promoted, and the author helpers that validate a
    /// freshly generated artifact.
    ///
    /// The linking itself — the id↔name mapping that makes the artifact's
    /// `u0:<id>` references resolve — lives in [`aipl_artifact::link_artifact`],
    /// shared with `build.rs` so the two can't drift.
    pub fn from_artifact(text: &str) -> Result<Self, Error> {
        let manifest = aipl_artifact::parse_manifest(text).map_err(Error::msg)?;
        let mut module = new_jit_module()?;
        let ids = aipl_artifact::link_artifact(
            text,
            &manifest,
            &mut module,
            &aipl_artifact::LinkNames::local("__dogfood_fn"),
        )
        .map_err(Error::msg)?;
        module
            .finalize_definitions()
            .map_err(|e| Error::msg(format!("finalize dogfood IR: {e}")))?;
        Ok(Self {
            funcs: entry_funcs(&manifest, |id| FuncLink::User(ids[id as usize]))?,
            // Struct layouts recovered from the `; struct` manifest lines, so a
            // struct-returning entry marshals back through `call_values`.
            structs: manifest_structs(&manifest)?,
            abi: Abi::active(),
            code: Code::Jit(module),
            ir: text.to_string(),
        })
    }

    /// Build a `Compilation` over machine code that is *already in this binary*
    /// — the same artifact, linked into a prebuilt object by `build.rs` instead
    /// of JIT-compiled here. `manifest_text` is the artifact's `;`-comment
    /// header, carried along by the build script; `entries` is the generated
    /// name→address table for its `; entry` functions.
    ///
    /// This is the normal path, and it is the whole point of the prebuilt
    /// object: it does no Cranelift work whatsoever, so a process that only
    /// needs to parse (or format) starts in single-digit milliseconds instead of
    /// re-lowering several hundred functions it lowered identically last time.
    /// The manifest still gets parsed, because the FFI metadata and struct
    /// layouts live there — but that is a scan of a few dozen header lines, not
    /// of the megabytes of IR behind them.
    pub fn from_prebuilt(
        manifest_text: &'static str,
        entries: &'static [(&'static str, PrebuiltFn)],
    ) -> Result<Self, Error> {
        let manifest = aipl_artifact::parse_manifest(manifest_text).map_err(Error::msg)?;
        Ok(Self {
            // The artifact's own id for the entry. Nothing dereferences it on
            // this path — `code_ptr` resolves prebuilt entries by name — but it
            // is the honest value, and it keeps `FuncInfo` uniform across both.
            funcs: entry_funcs(&manifest, |id| FuncLink::User(FuncId::from_u32(id)))?,
            structs: manifest_structs(&manifest)?,
            abi: Abi::active(),
            code: Code::Prebuilt(entries),
            ir: manifest_text.to_string(),
        })
    }

    pub fn ir(&self) -> &str {
        &self.ir
    }

    pub fn run_0(&self, name: &str) -> Result<i64, Error> {
        let ptr = self.code_ptr(name)?;
        let f: extern "C" fn() -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f())
    }

    pub fn run_1(&self, name: &str, a: i64) -> Result<i64, Error> {
        let ptr = self.code_ptr(name)?;
        let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f(a))
    }

    pub fn run_2(&self, name: &str, a: i64, b: i64) -> Result<i64, Error> {
        let ptr = self.code_ptr(name)?;
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(ptr) };
        Ok(f(a, b))
    }

    /// Whether `name` takes exactly one `str[]` parameter — i.e. it wants the
    /// CLI arguments. Used by the driver to choose its `run_cli` path over the
    /// integer-argument `run_*` forms.
    pub fn takes_cli_args(&self, name: &str) -> bool {
        self.funcs
            .get(name)
            .is_some_and(|i| i.params.len() == 1 && i.params[0].ty == registered_cli_args_ty())
    }

    /// Run a function taking a single `str[]`, passing `args` as that array.
    /// The callee owns the array (and its strings) and frees it on return.
    pub fn run_cli(&self, name: &str, args: &[String]) -> Result<i64, Error> {
        let ptr = self.code_ptr(name)?;
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
        for (i, p) in info.param_types().enumerate() {
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
        let ptr = self.code_ptr(name)?;
        // SAFETY: every scalar AIPL param/return lowers to a single `i64`
        // (`cl_type_of`), and we verified arity (<= 6) + scalar types above, so
        // the finalized code matches the transmuted signature.
        Ok(unsafe { invoke(ptr, args) })
    }

    /// Call AIPL function `name` from Rust, marshaling composites as well as
    /// scalars (see [`FfiValue`]). Each argument's [`FfiValue`] variant must
    /// match the parameter type — `Int` for `i64`/`bool`/`char`, `Str` for `str`,
    /// `Array` for an array, and so on, to any nesting depth — and the return is
    /// marshaled by the function's declared return type. Like [`call`], it
    /// rejects a missing/mutating/wrong-arity function, and it rejects any
    /// parameter or return type the FFI can't marshal (a set, a dict, a recursive
    /// type) or an argument whose shape doesn't match its parameter.
    ///
    /// Every heap value the host builds for an argument — a `str`, an array block,
    /// the strings inside one — is *borrowed* for the duration of the call: the
    /// host owns the buffer (passed with a static refcount, so the callee neither
    /// frees nor mutates it in place) and releases it on return. Don't write an
    /// AIPL function that stashes an argument somewhere outliving the call.
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
        let abi_kind = self.abi;
        // An optional `T?` (possibly nested) — scalar, `str`, or struct core — is
        // returned through a hidden sret pointer (see the sret path below).
        let ret_is_opt = matches!(info.return_ty, ConcreteType::Optional(_));
        // A `Result<ok, err>` — each side a scalar/`str`/`Unit`/struct — is also
        // sret-returned, same shape as an optional but tagged/sized differently.
        let ret_is_result = matches!(info.return_ty, ConcreteType::Result(_, _));
        // A `struct` returned directly (not under an optional); read back field by
        // field from the sret buffer.
        let ret_struct = ffi_struct_layout(&info.return_ty, &self.structs);
        // A `variant` returned directly; read back tag + payload from the sret
        // buffer (same sret shape as a struct return).
        let ret_variant = ffi_variant_layout(&info.return_ty, &self.structs);

        // Marshal each argument to its `i64` ABI, validating the value against
        // the parameter type. Every buffer this allocates — heap `str`s, array
        // blocks, and the composite buffers the ABI passes by pointer — lives in
        // `bufs` until it drops at the end of this function, which is after
        // `invoke` and after the return value has been copied out.
        let mut bufs = ArgBufs::default();
        let mut abi: Vec<i64> = Vec::with_capacity(args.len());
        for (i, (p, a)) in info.param_types().zip(args).enumerate() {
            match ffi_arg_abi(abi_kind, p, a, &self.structs, &mut bufs) {
                Ok(word) => abi.push(word),
                Err(why) => {
                    return Err(Error::msg(format!("fn {name:?} parameter {i} {why}")));
                }
            }
        }

        let ptr = self.code_ptr(name)?;

        if ret_is_opt {
            // Composite (optional) return: the callee writes it through a hidden
            // sret pointer — a normal *leading* i64 param — and returns nothing.
            // The flattened layout is `{ i64 tag, core }`; `elem_size_of` gives the
            // total (16 for a scalar/str core, more for a struct core like `Span?`).
            let words = (abi_elem_size(abi_kind, &info.return_ty, &self.structs) as usize)
                .div_ceil(8)
                .max(1);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for an optional return; the FFI \
                     supports up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: the function takes `(sret_ptr, <= 5 scalar/str args)` and
            // returns nothing; we transmute through the i64-returning `invoke` and
            // ignore the (unset) return register.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result = unsafe {
                read_ffi_optional(
                    abi_kind,
                    sret_buf.as_ptr(),
                    &info.return_ty,
                    true,
                    &self.structs,
                )
            };
            return Ok(result);
        }

        if ret_is_result {
            // Composite (result) return: same sret shape as an optional (a
            // leading pointer, no register return), but the buffer is sized to
            // the wider of the two sides and the tag means `1` = Ok / `0` = Err
            // rather than a nesting depth.
            let words = (abi_elem_size(abi_kind, &info.return_ty, &self.structs) as usize)
                .div_ceil(8)
                .max(1);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for a result return; the FFI \
                     supports up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: as the optional path above.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result = unsafe {
                read_ffi_result(
                    abi_kind,
                    sret_buf.as_ptr(),
                    &info.return_ty,
                    true,
                    &self.structs,
                )
            };
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
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for a struct return; the FFI supports \
                     up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: as the optional path, but the buffer is the struct's size.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result = unsafe {
                read_ffi_struct(
                    abi_kind,
                    sret_buf.as_ptr() as *const u8,
                    layout,
                    true,
                    &self.structs,
                )
            };
            return Ok(result);
        }

        if let Some(layout) = ret_variant {
            // Composite (variant) return: like the struct path — a hidden leading
            // sret pointer, sized to the variant (widest case), read back as a
            // `{ tag, payload }`.
            let words = (layout.size as usize).div_ceil(8).max(1);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for a variant return; the FFI supports \
                     up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: as the struct path, but the buffer is the variant's size.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let result = unsafe {
                read_ffi_variant(
                    abi_kind,
                    sret_buf.as_ptr() as *const u8,
                    layout,
                    true,
                    &self.structs,
                )
            };
            return Ok(result);
        }

        // A wide `str` return is a composite like any other: the callee writes it
        // through a hidden sret pointer and returns nothing. (Under the tagged
        // ABI it comes back in a register — the branch further down.)
        //
        // `is_str_shaped`, so `char[]` comes here too: it shares the
        // representation, so it is equally composite under this ABI. Reading it
        // from the word return further down gets the value's first field, which
        // decodes as an empty array. Only the decoding differs — codepoints
        // rather than text.
        if is_str_shaped(&info.return_ty) && true {
            let words = str24::STR_SIZE.div_ceil(8);
            let mut sret_buf = vec![0i64; words];
            let mut sret_abi = Vec::with_capacity(1 + abi.len());
            sret_abi.push(sret_buf.as_mut_ptr() as i64);
            sret_abi.extend_from_slice(&abi);
            if sret_abi.len() > 6 {
                return Err(Error::msg(format!(
                    "fn {name:?} has too many parameters for a `str` return; the FFI supports \
                     up to 5 (plus the hidden return pointer)"
                )));
            }
            // SAFETY: the function takes `(sret_ptr, <= 5 scalar/str args)` and
            // returns nothing; the buffer is `str` sized.
            let _ = unsafe { invoke(ptr, &sret_abi) };
            let value = unsafe { core::ptr::read(sret_buf.as_ptr() as *const str24::Str) };
            // Copy the bytes out while the argument buffers are still alive — an
            // identity `fn(s) -> s` hands one of them straight back — then
            // release the reference the callee gave us.
            let mut scratch = [0u8; str24::INLINE_CAP];
            let text = String::from_utf8_lossy(value.bytes(&mut scratch)).into_owned();
            value.release();
            if is_char_array(&info.return_ty) {
                return Ok(FfiValue::Array(
                    text.chars().map(|c| FfiValue::Int(c as i64)).collect(),
                ));
            }
            return Ok(FfiValue::Str(text));
        }

        // SAFETY: arity (<= 6) and per-argument types are validated above; every
        // scalar and `str` lowers to one `i64`, so the finalized code matches the
        // transmuted signature.
        let r = unsafe { invoke(ptr, &abi) };

        // An array is pointer-like (a single `i64` return, not sret): the callee
        // handed us one reference on the block, which `read_ffi_array` releases.
        if let ConcreteType::Array(elem) = &info.return_ty {
            let result = unsafe { read_ffi_array(abi_kind, r, elem, true, &self.structs) };
            return Ok(result);
        }

        // A `str` return took the sret path above, so anything reaching here
        // came back in a register.
        Ok(FfiValue::Int(r))
    }

    /// The address of `name`'s machine code, ready to transmute to its real
    /// signature. The two [`Code`] backings resolve it differently — a JIT
    /// module by `FuncId`, a prebuilt object by exported symbol name — and this
    /// is the only place that distinction is visible.
    fn code_ptr(&self, name: &str) -> Result<*const u8, Error> {
        match &self.code {
            Code::Jit(module) => Ok(module.get_finalized_function(self.lookup(name)?)),
            Code::Prebuilt(entries) => entries
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, f)| *f as *const u8)
                .ok_or_else(|| {
                    // Reaching this means the generated table and the manifest
                    // disagree, which `build.rs` builds them from together.
                    Error::msg(format!("no prebuilt entry {name:?}"))
                }),
        }
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

/// Whether `t` is a scalar the embedding FFI can marshal as a bare `i64` — any
/// integer width (each already canonicalized into an `i64` register at the ABI),
/// `bool`, or `char`. A `u64` past `i64::MAX` round-trips by bit pattern, so the
/// host sees it as a negative [`FfiValue::Int`].
fn is_ffi_scalar(t: &ConcreteType) -> bool {
    is_int_ty(t)
        || matches!(
            t,
            ConcreteType::Primitive(Primitive::Bool | Primitive::Char)
        )
}

/// The [`StructLayout`] for `t` if it names a `struct` (not a variant), else
/// `None` — the gate the FFI uses to decide whether to marshal a value field by
/// field.
fn ffi_struct_layout<'a>(
    t: &ConcreteType,
    structs: &'a HashMap<String, TypeDef>,
) -> Option<&'a StructLayout> {
    match t {
        ConcreteType::Named(n) => structs.get(n).and_then(TypeDef::as_struct),
        _ => None,
    }
}

/// The [`VariantLayout`] for `t` if it names a `variant` (not a struct), else
/// `None` — the FFI's gate for marshaling a variant by tag + payload.
fn ffi_variant_layout<'a>(
    t: &ConcreteType,
    structs: &'a HashMap<String, TypeDef>,
) -> Option<&'a VariantLayout> {
    match t {
        ConcreteType::Named(n) => structs.get(n).and_then(TypeDef::as_variant),
        _ => None,
    }
}

/// Validate that `ty` can be marshaled back across the embedding FFI as a return
/// value. The rule is recursive: a scalar, `str`, or `Unit`; a `struct` each of
/// whose fields is marshalable; a `variant` each of whose case payloads is
/// marshalable; an array whose element type is marshalable; an optional (possibly
/// nested) whose core is; or a `Result` whose `ok`/`err` sides each independently
/// are (so `!Error`, i.e. `Result<Unit, Error>`, is fine: `Unit` on the ok side,
/// `Error` — a `str`-repr type — on the err side). This lets arbitrarily nested
/// shapes marshal — e.g. `Token<K>[]` where `Token` is `{ kind: K, span: Span }`
/// (a variant field plus a struct field). Errors name the offending type/field.
/// (`call_values` then dispatches on the type's shape.)
fn check_ffi_return(
    name: &str,
    ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) -> Result<(), Error> {
    if is_ffi_scalar(ty) || is_str_repr(ty) || is_unit(ty) {
        return Ok(());
    }
    match ty {
        // Peel optional layers down to the (shared, flattened) core.
        ConcreteType::Optional(inner) => check_ffi_return(name, inner, structs),
        // Each side independently: same rules as a bare return.
        ConcreteType::Result(ok, err) => {
            check_ffi_return(name, ok, structs)?;
            check_ffi_return(name, err, structs)
        }
        // An array marshals if its element type does (recursively). `char[]` —
        // whose element is a scalar — is read specially (str-shaped) but validates
        // the same way.
        ConcreteType::Array(elem) => check_ffi_return(name, elem, structs),
        ConcreteType::Named(_) => {
            if let Some(layout) = ffi_struct_layout(ty, structs) {
                // Each field must itself be marshalable — a field may be a nested
                // struct/variant/array/optional (stored inline), so recurse.
                for f in &layout.fields {
                    if check_ffi_return(name, &f.ty, structs).is_err() {
                        return Err(Error::msg(format!(
                            "fn {name:?} returns struct {} whose field {:?} is {}; the FFI can't \
                             marshal that field type",
                            type_name(ty),
                            f.name,
                            type_name(&f.ty)
                        )));
                    }
                }
                return Ok(());
            }
            if let Some(layout) = ffi_variant_layout(ty, structs) {
                // Every case's payload must be marshalable, since the active case
                // is only known at runtime — recurse (a payload may be composite).
                for case in &layout.cases {
                    for f in &case.fields {
                        if check_ffi_return(name, &f.ty, structs).is_err() {
                            return Err(Error::msg(format!(
                                "fn {name:?} returns variant {} whose case {:?} has a payload of \
                                 type {}; the FFI can't marshal that payload type",
                                type_name(ty),
                                case.name,
                                type_name(&f.ty)
                            )));
                        }
                    }
                }
                return Ok(());
            }
            Err(Error::msg(format!(
                "fn {name:?} returns {}; the FFI supports only i64/bool/char, str, structs and \
                 variants of those, arrays of those, and optionals/results of those",
                type_name(ty)
            )))
        }
        _ => Err(Error::msg(format!(
            "fn {name:?} returns {}; the FFI supports only i64/bool/char, str, structs and \
             variants of those, arrays of those, and optionals/results of those",
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

// ---------------------------------------------------------------------------
// FFI argument writers.
//
// The mirror of the return-value readers below: given a host [`FfiValue`] and
// the parameter's declared `ConcreteType`, produce the value the callee expects, in the
// callee's own representation. The rule matches `check_ffi_return`'s, so an
// argument can be any shape a return value can be.
//
// Everything the host builds is *borrowed*: a heap `str` or array block it
// allocates carries `STATIC_REFCOUNT`, so every retain/release the callee
// performs on it is a no-op and nothing the callee does can free it. The host
// keeps ownership and frees the lot when [`ArgBufs`] drops — after the call, and
// after the return value has been copied out (an identity `fn(s) -> s` hands
// back an argument). The one thing a static block forbids is *in-place*
// mutation, which the runtime's `aipl_array_push_mut` / `aipl_concat_mut` /
// `aipl_trim_mut` guards already redirect to their copying counterparts.
// ---------------------------------------------------------------------------

/// One host-owned buffer backing a borrowed FFI argument.
enum ArgBuf {
    /// A heap `str` block from [`build_borrowed_str`]: its base and content length.
    Str(*mut u8, usize),
    /// An array block from [`ArgBufs::array_block`]: its *data* pointer (the base
    /// plus [`HEADER_SIZE`]) and the byte capacity that sizes the deallocation.
    Array(*mut u8, usize),
    /// An inline composite (struct/variant/optional/result) the callee receives a
    /// pointer to. Owned by the `Vec`, so it just has to stay alive.
    Composite(Vec<u8>),
    /// A wide (24-byte) `str` argument: the value was copied into the callee's
    /// buffer, and this holds the reference that keeps its allocation alive.
    WideStr(str24::Str),
}

/// The host-owned buffers behind one call's borrowed arguments, freed on drop.
/// Dropping is the *only* release path, which is what makes the early `return
/// Err(..)`s in [`Compilation::call_values`] leak-free without a cleanup block
/// each.
#[derive(Default)]
struct ArgBufs(Vec<ArgBuf>);

impl ArgBufs {
    /// A wide (24-byte) `str` argument written into a caller buffer: its own
    /// allocation has to outlive the call, so the release is deferred to here.
    fn wide_str(&mut self, value: str24::Str) {
        self.0.push(ArgBuf::WideStr(value));
    }

    /// A borrowed `str` value (see [`build_borrowed_str`]), keeping the heap case
    /// alive for the call.
    fn str_value(&mut self, s: &str) -> i64 {
        let (val, heap) = build_borrowed_str(s.as_bytes());
        if let Some((block, content_len)) = heap {
            self.0.push(ArgBuf::Str(block, content_len));
        }
        val
    }

    /// Keep `buf` — an inline composite — alive for the call, and return the
    /// address the callee receives. The `Vec`'s own buffer doesn't move when the
    /// enclosing `Vec<ArgBuf>` grows, so the address stays valid.
    fn composite(&mut self, buf: Vec<u8>) -> i64 {
        self.0.push(ArgBuf::Composite(buf));
        match self.0.last() {
            Some(ArgBuf::Composite(b)) => b.as_ptr() as i64,
            _ => unreachable!("just pushed a composite buffer"),
        }
    }

    /// Allocate a zeroed, borrowed array block of `len` elements of `elem_size`
    /// bytes (0 = bit-packed) and return its data pointer — the array value
    /// itself, since a fresh block is untagged (`ArrRepr::Heap`).
    ///
    /// The element drop-fn is 0, and unused: the header's drop-fn only runs when
    /// `aipl_array_dec` frees the block, which `STATIC_REFCOUNT` prevents, and
    /// every path that *copies* an array (`push`, `slice`, `sort`, `reserve`)
    /// takes the drop-fn for the copy from codegen rather than from this header.
    fn array_block(&mut self, len: usize, elem_size: i64) -> *mut u8 {
        let data = alloc_array(len, len, 0, elem_size) as *mut u8;
        let cap_bytes = cap_bytes_for(elem_size, len);
        unsafe {
            // Zero the element region: a partially-written inline composite
            // element (a `none`'s slack, a nullary variant case's payload) must
            // read back as zeroes, and `alloc_array` leaves it uninitialized.
            std::ptr::write_bytes(data.add(ARR_ELEMS_OFFSET), 0, cap_bytes);
            *header_of(data) = STATIC_REFCOUNT;
        }
        self.0.push(ArgBuf::Array(data, cap_bytes));
        data
    }
}

impl Drop for ArgBufs {
    fn drop(&mut self) {
        for buf in &self.0 {
            match buf {
                ArgBuf::Str(block, content_len) => unsafe {
                    free_dynamic_string(*block, *content_len)
                },
                ArgBuf::Array(data, cap_bytes) => unsafe {
                    let layout = std::alloc::Layout::from_size_align(
                        array_block_size(*cap_bytes),
                        std::mem::align_of::<i64>(),
                    )
                    .expect("array layout");
                    std::alloc::dealloc(data.sub(HEADER_SIZE), layout);
                },
                // Freed by the `Vec`'s own drop.
                ArgBuf::Composite(_) => {}
                // The callee borrowed the value; releasing here is what balances
                // the allocation `write_ffi_arg` made for it.
                ArgBuf::WideStr(value) => value.release(),
            }
        }
    }
}

/// Marshal `v` into the single `i64` the ABI passes for a parameter of type
/// `ty`. An inline composite (struct, variant, optional, result) is written into
/// a host buffer and passed by *pointer*, exactly as codegen passes one from a
/// stack slot; everything else — scalars, `str`, arrays — is the value word
/// itself. Any buffer this needs is recorded in `bufs` and outlives the call.
///
/// The `Err` describes the mismatch without naming the function or parameter
/// index; [`Compilation::call_values`] prefixes those.
fn ffi_arg_abi(
    abi: Abi,
    ty: &ConcreteType,
    v: &FfiValue,
    structs: &HashMap<String, TypeDef>,
    bufs: &mut ArgBufs,
) -> Result<i64, String> {
    if abi_is_composite(abi, ty, structs) {
        let size =
            abi_sret_size(abi, ty, structs).expect("a composite has an inline size") as usize;
        let mut buf = vec![0u8; size.max(8)];
        // SAFETY: `buf` is `size` zeroed bytes — the type's own inline layout.
        unsafe { write_ffi_arg(abi, buf.as_mut_ptr(), ty, v, structs, bufs)? };
        return Ok(bufs.composite(buf));
    }
    ffi_arg_word(abi, ty, v, structs, bufs)
}

/// Marshal `v` into the 8-byte word a value of the *pointer-like* type `ty` — a
/// scalar, a `str`, or an array — is represented by. Composites don't come here;
/// see [`ffi_arg_abi`].
fn ffi_arg_word(
    abi: Abi,
    ty: &ConcreteType,
    v: &FfiValue,
    structs: &HashMap<String, TypeDef>,
    bufs: &mut ArgBufs,
) -> Result<i64, String> {
    debug_assert!(
        !abi_is_composite(abi, ty, structs),
        "a composite goes through ffi_arg_abi"
    );
    match v {
        FfiValue::Int(n) if is_ffi_scalar(ty) => Ok(*n),
        FfiValue::Str(s) if is_str_repr(ty) => Ok(bufs.str_value(s)),
        FfiValue::Array(elems) => match ty {
            ConcreteType::Array(elem) => build_borrowed_array(abi, elem, elems, structs, bufs),
            _ => Err(mismatch(ty, v)),
        },
        _ => Err(mismatch(ty, v)),
    }
}

/// The error for an [`FfiValue`] that doesn't match the parameter type it was
/// passed for — naming both sides, since the pairing is the whole mistake.
fn mismatch(ty: &ConcreteType, v: &FfiValue) -> String {
    format!(
        "is {} but was given an FfiValue::{}; pass the matching variant (Int for \
         i64/bool/char, Str for str, Array for an array, Struct for a struct, Variant for a \
         variant, Opt for an optional, Res for a result)",
        type_name(ty),
        ffi_value_kind(v)
    )
}

/// An [`FfiValue`]'s variant name, for error messages.
fn ffi_value_kind(v: &FfiValue) -> &'static str {
    match v {
        FfiValue::Int(_) => "Int",
        FfiValue::Str(_) => "Str",
        FfiValue::Opt(_) => "Opt",
        FfiValue::Res(_) => "Res",
        FfiValue::Struct(_) => "Struct",
        FfiValue::Variant(_, _) => "Variant",
        FfiValue::Array(_) => "Array",
    }
}

/// Build a borrowed `T[]` value from `elems`: an array block the callee reads
/// like any other, tagged `STATIC_REFCOUNT` so its retains and releases no-op
/// (the `str` treatment in [`build_borrowed_str`], one level up). Each element is
/// written in its inline representation, so an element may itself be a struct, a
/// variant, an optional, or another array.
///
/// `char[]` is the one element type that shares `str`'s representation rather
/// than using a block (see [`is_char_array`]), so it's built as packed bytes;
/// `bool[]` is bit-packed, one bit per element.
/// A `char[]`'s host spelling — a list of codepoints — as the UTF-8 text the
/// runtime actually stores. Shared by both ABIs' array marshalling.
fn chars_to_utf8(elems: &[FfiValue]) -> Result<String, String> {
    let mut s = String::with_capacity(elems.len());
    for e in elems {
        match e {
            FfiValue::Int(c) => s.push(
                char::from_u32(*c as u32)
                    .ok_or_else(|| format!("char[] element {c} is not a valid codepoint"))?,
            ),
            other => {
                return Err(format!(
                    "char[] element must be an FfiValue::Int codepoint, got FfiValue::{}",
                    ffi_value_kind(other)
                ))
            }
        }
    }
    Ok(s)
}

fn build_borrowed_array(
    abi: Abi,
    elem: &ConcreteType,
    elems: &[FfiValue],
    structs: &HashMap<String, TypeDef>,
    bufs: &mut ArgBufs,
) -> Result<i64, String> {
    if matches!(elem, ConcreteType::Primitive(Primitive::Char)) {
        let s = chars_to_utf8(elems)?;
        return Ok(bufs.str_value(&s));
    }
    let elem_size = if is_str_repr(elem) && true {
        // A wide `str` element is the value itself, 24 bytes in the block.
        abi.str_size()
    } else {
        runtime_elem_size(elem, structs)
    };
    let data = bufs.array_block(elems.len(), elem_size);
    let base = unsafe { data.add(ARR_ELEMS_OFFSET) };
    if elem_size == ELEM_BITPACKED {
        for (i, e) in elems.iter().enumerate() {
            match e {
                FfiValue::Int(b) => unsafe { write_packed_bit(base, i, *b != 0) },
                other => {
                    return Err(format!(
                        "bool[] element must be an FfiValue::Int 0/1, got FfiValue::{}",
                        ffi_value_kind(other)
                    ))
                }
            }
        }
    } else {
        let stride = elem_size.max(8) as usize;
        for (i, e) in elems.iter().enumerate() {
            // SAFETY: the block was sized for `elems.len()` elements of `stride`
            // bytes and zeroed, so this slot is in bounds and initialized.
            unsafe { write_ffi_arg(abi, base.add(i * stride), elem, e, structs, bufs)? };
        }
    }
    Ok(data as i64)
}

/// Write `v`, a value of type `ty`, at `dst` in the callee's inline
/// representation — the mirror of [`read_ffi_borrowed`]. A struct/variant/
/// optional/result is written in place, component by component; a scalar, `str`,
/// or array is the single word at `dst`.
///
/// SAFETY: `dst` must point at `elem_size_of(ty)` writable bytes, zeroed (the
/// slack past a `none`'s tag or a nullary variant case's payload is never
/// written here, but the callee may still copy it around).
unsafe fn write_ffi_arg(
    abi: Abi,
    dst: *mut u8,
    ty: &ConcreteType,
    v: &FfiValue,
    structs: &HashMap<String, TypeDef>,
    bufs: &mut ArgBufs,
) -> Result<(), String> {
    // A `str` under the wide ABI is the value itself, written in place — there is
    // no word to pass.
    //
    // `is_str_shaped`, not `is_str_repr`: `char[]` shares the representation, so
    // under this ABI it is composite too (`abi_is_composite` says so) and must be
    // written here rather than falling through to `ffi_arg_word`, whose own
    // assertion catches exactly that mistake. The two differ only in how the host
    // spells the value — a `str` arrives as text, a `char[]` as codepoints.
    if is_str_shaped(ty) && true {
        let text = match v {
            FfiValue::Str(text) if is_str_repr(ty) => text.clone(),
            FfiValue::Array(elems) if is_char_array(ty) => chars_to_utf8(elems)?,
            _ => return Err(mismatch(ty, v)),
        };
        let value = str24::from_bytes(text.as_bytes());
        unsafe { core::ptr::write(dst as *mut str24::Str, value) };
        bufs.wide_str(value);
        return Ok(());
    }
    // A recursive type's value is a pointer to a refcounted heap block the host
    // has no way to build — and passing an inline payload where one is expected
    // would have the callee read a header off the front of our buffer.
    if is_boxed(ty, structs) {
        return Err(format!(
            "is {}, a recursive type held behind a refcounted heap pointer; the FFI can't \
             build one",
            type_name(ty)
        ));
    }
    if let Some(layout) = ffi_struct_layout(ty, structs) {
        let FfiValue::Struct(fields) = v else {
            return Err(mismatch(ty, v));
        };
        if fields.len() != layout.fields.len() {
            return Err(format!(
                "is struct {} with {} field(s), got {}",
                type_name(ty),
                layout.fields.len(),
                fields.len()
            ));
        }
        for ((name, val), f) in fields.iter().zip(layout.fields.iter()) {
            if name != &f.name {
                return Err(format!("expected field {:?}, got {:?}", f.name, name));
            }
            unsafe { write_ffi_arg(abi, dst.add(f.offset as usize), &f.ty, val, structs, bufs)? };
        }
        return Ok(());
    }
    if let Some(layout) = ffi_variant_layout(ty, structs) {
        let FfiValue::Variant(case_name, payload) = v else {
            return Err(mismatch(ty, v));
        };
        let Some((tag, case)) = layout.case(case_name) else {
            return Err(format!(
                "is variant {}, which has no case {case_name:?}",
                type_name(ty)
            ));
        };
        if payload.len() != case.fields.len() {
            return Err(format!(
                "is variant {} whose case {case_name:?} takes {} payload value(s), got {}",
                type_name(ty),
                case.fields.len(),
                payload.len()
            ));
        }
        // The tag (case index) at offset 0, then each payload at its offset.
        unsafe { std::ptr::write(dst as *mut i64, tag as i64) };
        for (val, f) in payload.iter().zip(case.fields.iter()) {
            unsafe { write_ffi_arg(abi, dst.add(f.offset as usize), &f.ty, val, structs, bufs)? };
        }
        return Ok(());
    }
    if matches!(ty, ConcreteType::Optional(_)) {
        // Flattened `{ i64 tag, core }`: the tag counts the `some` layers (`0` =
        // `none`) and every layer shares the one core slot, so peel `Opt`s and
        // `Optional`s together and write the depth reached. See
        // `read_ffi_optional_tag`, which reconstructs the nesting from it.
        let (mut cur_ty, mut cur_v, mut depth) = (ty, v, 0i64);
        while let ConcreteType::Optional(inner) = cur_ty {
            let FfiValue::Opt(o) = cur_v else {
                return Err(mismatch(cur_ty, cur_v));
            };
            match o {
                // A `none` at this depth: the tag alone, no core.
                None => {
                    unsafe { std::ptr::write(dst as *mut i64, depth) };
                    return Ok(());
                }
                Some(inner_v) => {
                    depth += 1;
                    cur_ty = inner;
                    cur_v = inner_v;
                }
            }
        }
        unsafe {
            std::ptr::write(dst as *mut i64, depth);
            return write_ffi_arg(
                abi,
                dst.add(OPT_VALUE_OFFSET as usize),
                cur_ty,
                cur_v,
                structs,
                bufs,
            );
        }
    }
    if let ConcreteType::Result(ok, err) = ty {
        // `{ i64 tag, value }` like an optional, but the tag is `1` = Ok / `0` =
        // Err and the slot is sized to the wider side. A `Unit` side (`!Error`'s
        // ok case) has no value to write — the zeroed slot stands in for it.
        let FfiValue::Res(r) = v else {
            return Err(mismatch(ty, v));
        };
        let (tag, side, payload) = match r {
            Ok(p) => (1i64, ok.as_ref(), p),
            Err(p) => (0i64, err.as_ref(), p),
        };
        unsafe { std::ptr::write(dst as *mut i64, tag) };
        if is_unit(side) {
            return Ok(());
        }
        return unsafe {
            write_ffi_arg(
                abi,
                dst.add(OPT_VALUE_OFFSET as usize),
                side,
                payload,
                structs,
                bufs,
            )
        };
    }
    let word = ffi_arg_word(abi, ty, v, structs, bufs)?;
    unsafe { std::ptr::write(dst as *mut i64, word) };
    Ok(())
}

// ---------------------------------------------------------------------------
// FFI return-value readers.
//
// Every reader threads an `owned` flag. A value read at the *top level* of a
// return carries the one reference the callee retained on return (see the
// composite-return path in `define_fn` / the `Return` path in `compile_expr`):
// `owned = true` means "this read consumes that reference" — so a `str` read
// releases it, and an array read `dec`s the block. A value read as a *component
// of a larger owned value* (an array element, whose reference the block owns) is
// *borrowed*: `owned = false`, no release — the enclosing value's single
// top-level release cascades to it.
// ---------------------------------------------------------------------------

/// Read the value of type `ty` whose inline representation begins exactly at
/// `at`. This is the one place that dispatches an arbitrary marshalable type on
/// its representation: an inline composite (struct/variant/optional/result) is
/// read in place; a pointer-like value (scalar/`str`/array) is the 8-byte word at
/// `at`. Used for array elements and, at byte offset 8, for optional/result
/// cores (see [`read_ffi_core`]).
unsafe fn read_ffi_borrowed(
    abi: Abi,
    at: *const u8,
    ty: &ConcreteType,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    if let Some(layout) = ffi_struct_layout(ty, structs) {
        return unsafe { read_ffi_struct(abi, at, layout, owned, structs) };
    }
    if let Some(layout) = ffi_variant_layout(ty, structs) {
        return unsafe { read_ffi_variant(abi, at, layout, owned, structs) };
    }
    match ty {
        ConcreteType::Optional(_) => unsafe {
            read_ffi_optional(abi, at as *const i64, ty, owned, structs)
        },
        ConcreteType::Result(_, _) => unsafe {
            read_ffi_result(abi, at as *const i64, ty, owned, structs)
        },
        // A wide `char[]` is a whole `str` value at `at`, not a word pointing at
        // one — the same shape the `str` arm below reads, differing only in how
        // the text is handed back (codepoints rather than a string). Reading it
        // as a word yields the value's first field and decodes as empty.
        ConcreteType::Array(_) if is_char_array(ty) && true => {
            let value = unsafe { core::ptr::read(at as *const str24::Str) };
            let mut scratch = [0u8; str24::INLINE_CAP];
            let text = String::from_utf8_lossy(value.bytes(&mut scratch)).into_owned();
            if owned {
                value.release();
            }
            FfiValue::Array(text.chars().map(|c| FfiValue::Int(c as i64)).collect())
        }
        ConcreteType::Array(elem) => {
            // The 8-byte word is the array value (a tagged block pointer, or — for
            // `char[]`, which shares `str`'s representation — an inline/heap `str`).
            let raw = unsafe { *(at as *const i64) };
            unsafe { read_ffi_array(abi, raw, elem, owned, structs) }
        }
        _ if is_str_repr(ty) => {
            // The value *is* the 24 bytes at `at`.
            let value = unsafe { core::ptr::read(at as *const str24::Str) };
            let mut scratch = [0u8; str24::INLINE_CAP];
            let text = String::from_utf8_lossy(value.bytes(&mut scratch)).into_owned();
            if owned {
                value.release();
            }
            FfiValue::Str(text)
        }
        _ => FfiValue::Int(unsafe { *(at as *const i64) }),
    }
}

/// Read an array value `raw` (of element type `elem`) into an [`FfiValue::Array`]
/// of its elements in order. `char[]` is special-cased: it shares `str`'s
/// representation (packed UTF-8 bytes, not an array block), so it's read as its
/// bytes decoded to codepoints. Every other array is a heap/reversed block —
/// elements read by *borrowing* (the block owns their references); when `owned`,
/// the block's own reference is released after the read (`dec` cascades to the
/// elements), balancing the retain the callee did on return.
///
/// SAFETY: `raw` must be a valid array value of element type `elem` (a `char[]`
/// str value, or a heap/reversed array block whose elements are `elem`-shaped),
/// carrying one reference when `owned`.
unsafe fn read_ffi_array(
    abi: Abi,
    raw: i64,
    elem: &ConcreteType,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    // `char[]` never arrives here: it is str-shaped, so it is a 24-byte value
    // rather than the `i64` this takes, and both callers (`read_ffi_borrowed`
    // and the `call_values` return path) peel it off first.
    debug_assert!(
        !matches!(elem, ConcreteType::Primitive(Primitive::Char)),
        "a `char[]` is read through the str-shaped path, not as an array block"
    );
    let a = raw as *const u8;
    // A null pointer stands for an empty array (never allocated).
    if a.is_null() {
        return FfiValue::Array(Vec::new());
    }
    let len = unsafe { array_len_of(a) };
    let mut out = Vec::with_capacity(len);
    if is_bit_packed(elem) {
        // `bool[]` packs one bit per element.
        for i in 0..len {
            out.push(FfiValue::Int(i64::from(unsafe { arr_load_bit(a, i) })));
        }
    } else {
        let stride = abi_elem_size(abi, elem, structs) as usize;
        for i in 0..len {
            let ep = unsafe { arr_elem_ptr(a, i, stride) };
            // Elements are borrowed: the block owns their references.
            out.push(unsafe { read_ffi_borrowed(abi, ep, elem, false, structs) });
        }
    }
    if owned {
        aipl_array_dec(a);
    }
    FfiValue::Array(out)
}

/// Read a flattened optional of type `ty` into an [`FfiValue::Opt`]. The layout
/// mirrors codegen: an `i64` tag at offset 0 (`0` = `none`; `k` = `k` nested
/// `some`s) and the core value at [`OPT_VALUE_OFFSET`]. A present heap core (a
/// `str` or array) carries the callee's reference when `owned` (see
/// [`read_ffi_borrowed`]).
///
/// SAFETY: `buf` must point at a `{ i64 tag, core }` the callee filled for an
/// optional whose core is a marshalable type.
unsafe fn read_ffi_optional(
    abi: Abi,
    buf: *const i64,
    ty: &ConcreteType,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let tag = unsafe { *buf };
    unsafe { read_ffi_optional_tag(abi, buf, ty, tag, owned, structs) }
}

/// Read a flattened `Result<ok, err>` from `buf` into an [`FfiValue::Res`]. The
/// layout mirrors codegen: an `i64` tag at offset 0 (`1` = `Ok`, `0` = `Err`) and
/// the active side's payload at [`OPT_VALUE_OFFSET`] — read with [`read_ffi_core`],
/// which reads a `Unit` side (e.g. `!Error`'s ok case) back as a harmless `Int(0)`.
///
/// SAFETY: `buf` must point at a `{ i64 tag, value }` a `Result`-returning callee
/// filled, with `ok`/`err` each a marshalable type or `Unit`.
unsafe fn read_ffi_result(
    abi: Abi,
    buf: *const i64,
    ty: &ConcreteType,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let (ok_ty, err_ty) = match ty {
        ConcreteType::Result(ok, err) => (ok.as_ref(), err.as_ref()),
        _ => unreachable!("read_ffi_result on a non-result type"),
    };
    let tag = unsafe { *buf };
    if tag == 1 {
        FfiValue::Res(Ok(Box::new(unsafe {
            read_ffi_core(abi, buf, ok_ty, owned, structs)
        })))
    } else {
        FfiValue::Res(Err(Box::new(unsafe {
            read_ffi_core(abi, buf, err_ty, owned, structs)
        })))
    }
}

/// Reconstruct the nested `Opt` for `ty` given the flattened `tag`. Because the
/// representation is flattened, every nesting level shares the same `buf`; we
/// peel one `Optional` layer per recursion, decrementing the tag, until either a
/// `none` (tag `0`) or the non-optional core.
unsafe fn read_ffi_optional_tag(
    abi: Abi,
    buf: *const i64,
    ty: &ConcreteType,
    tag: i64,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let inner = match ty {
        ConcreteType::Optional(inner) => inner.as_ref(),
        _ => unreachable!("read_ffi_optional_tag on a non-optional type"),
    };
    if tag == 0 {
        return FfiValue::Opt(None);
    }
    let value = if matches!(inner, ConcreteType::Optional(_)) {
        unsafe { read_ffi_optional_tag(abi, buf, inner, tag - 1, owned, structs) }
    } else {
        unsafe { read_ffi_core(abi, buf, inner, owned, structs) }
    };
    FfiValue::Opt(Some(Box::new(value)))
}

/// Read the present core value at [`OPT_VALUE_OFFSET`] of an optional/result
/// buffer — every marshalable core sits inline there, so this is just
/// [`read_ffi_borrowed`] at byte offset 8.
unsafe fn read_ffi_core(
    abi: Abi,
    buf: *const i64,
    ty: &ConcreteType,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let at = unsafe { (buf as *const u8).add(OPT_VALUE_OFFSET as usize) };
    unsafe { read_ffi_borrowed(abi, at, ty, owned, structs) }
}

/// Read a struct (`layout`) at `base` into an [`FfiValue::Struct`] — one
/// `(name, value)` per field, in declaration order, each read (via
/// [`read_ffi_borrowed`]) at its byte offset. A field may itself be a composite
/// (a nested struct/variant/array/optional stored inline), so the shape is
/// arbitrarily deep; heap constituents are borrowed/released per `owned`.
/// `check_ffi_return` has already rejected any field whose type isn't marshalable.
///
/// SAFETY: `base` must point at a `layout`-shaped struct, each heap constituent
/// carrying one reference when `owned`.
unsafe fn read_ffi_struct(
    abi: Abi,
    base: *const u8,
    layout: &StructLayout,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let fields = layout
        .fields
        .iter()
        .map(|f| {
            let at = unsafe { base.add(f.offset as usize) };
            (f.name.clone(), unsafe {
                read_ffi_borrowed(abi, at, &f.ty, owned, structs)
            })
        })
        .collect();
    FfiValue::Struct(fields)
}

/// Read a variant (`layout`) at `base` into an [`FfiValue::Variant`]. The tag
/// (case index) sits at offset 0; the active case's payload fields follow from
/// [`VARIANT_PAYLOAD_OFFSET`], each read (via [`read_ffi_borrowed`]) at its byte
/// offset — a payload may itself be a composite stored inline. Heap constituents
/// are borrowed/released per `owned`. `check_ffi_return` has already ensured every
/// case's payload is marshalable.
///
/// SAFETY: `base` must point at a `layout`-shaped variant, its tag a valid case
/// index and each heap constituent carrying one reference when `owned`.
unsafe fn read_ffi_variant(
    abi: Abi,
    base: *const u8,
    layout: &VariantLayout,
    owned: bool,
    structs: &HashMap<String, TypeDef>,
) -> FfiValue {
    let tag = unsafe { *(base as *const i64) } as usize;
    let case = layout
        .cases
        .get(tag)
        .expect("variant tag out of range — callee wrote an invalid case index");
    let fields = case
        .fields
        .iter()
        .map(|f| {
            let at = unsafe { base.add(f.offset as usize) };
            unsafe { read_ffi_borrowed(abi, at, &f.ty, owned, structs) }
        })
        .collect();
    FfiValue::Variant(case.name.clone(), fields)
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

/// [`cli_args_ty`] as it appears in a *registered* signature — the same `str[]`,
/// after monomorphization.
///
/// Spelled out rather than converted from its source twin: this is a two-node
/// type, and stating each side plainly beats a conversion whose only job is to
/// cross a boundary that isn't really being crossed.
fn registered_cli_args_ty() -> ConcreteType {
    ConcreteType::Array(Box::new(ConcreteType::Primitive(Primitive::Str)))
}

/// The type of the *injected* CLI-arguments parameter — the one added to a
/// `main` that declares none, purely to keep the entry ABI uniform.
///
/// Deliberately not [`cli_args_ty`]. A `main` that ignores the arguments is
/// exactly the case where the runtime skips building the array and passes null
/// (see [`MAIN_WANTS_ARGS_SYMBOL`]), so this parameter never receives an array:
/// typing it `str[]` would make the callee drop an owned heap parameter it was
/// never given, emitting one `aipl_array_dec(null)` per program run. An
/// ignored pointer-sized word lowers to the same ABI and owns nothing, so no
/// drop is registered and no call is emitted.
fn injected_cli_args_ty() -> Type {
    Type::Primitive(Primitive::I64)
}

/// Fill `sig`'s params and returns for `f`'s ABI: a hidden sret pointer when
/// the (ABI) return is a struct, one i64 per declared parameter, then the
/// result — nothing for unit/struct(sret), `(tag, value)` for an optional, a
/// single i64 otherwise. Used by both the declaration and the definition so
/// they can't drift.
/// Whether parameter `ty` is passed as its three words rather than as a pointer,
/// which is what the `tail` convention needs for a wide `str`.
///
/// `return_call` hands the caller's frame over to the callee, so any argument
/// that *points into* that frame dangles the moment the transfer happens — which
/// is why `tail_safe_param` excludes composites in general. A wide `str` is 24
/// bytes of value with its content on the heap, so it has a way out the other
/// composites do not: pass the value itself. Three words in registers is exactly
/// what the ABI spike measured as workable (`abi_spike::q1`).
///
/// Only the `$tail` signature changes. The exported trampoline keeps the
/// one-pointer host shape, so the FFI, artifacts and function values are
/// untouched.
fn tail_passes_str_by_value(tail: bool, ty: &ConcreteType) -> bool {
    tail && is_str_shaped(ty)
}

/// The words a wide `str` at `addr` lowers to, for a by-value tail argument.
fn str_value_words(builder: &mut FunctionBuilder, addr: Value) -> Vec<Value> {
    let flags = MemFlagsData::trusted();
    (0..str24::STR_SIZE as i32)
        .step_by(8)
        .map(|off| builder.ins().load(types::I64, flags, addr, off))
        .collect()
}

/// The inverse: spill three words back into a slot and return its address, so
/// the body sees the same handle every other `str` has.
fn str_words_to_value(builder: &mut FunctionBuilder, words: &[Value]) -> Value {
    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        str24::STR_SIZE as u32,
        3,
    ));
    let addr = builder.ins().stack_addr(types::I64, slot, 0);
    let flags = MemFlagsData::trusted();
    for (i, w) in words.iter().enumerate() {
        builder.ins().store(flags, *w, addr, (i * 8) as i32);
    }
    addr
}

fn build_signature(
    sig: &mut Signature,
    f: &aipl_mono::ConcreteFn,
    structs: &HashMap<String, TypeDef>,
    tail: bool,
) {
    // The `$tail` body differs from the exported trampoline only here: cranelift
    // will not lower a `return_call` unless the callee's convention supports tail
    // calls *and* matches the caller's, and the host convention does neither.
    if tail {
        sig.call_conv = CallConv::Tail;
    }
    let abi = f.abi_return_type();
    // Composites — structs and optionals (possibly nested) — are returned
    // through a hidden caller-provided pointer (sret), uniformly.
    let returns_composite = sret_size(&abi, structs).is_some();
    if returns_composite {
        sig.params.push(AbiParam::new(types::I64));
    }
    for p in &f.params {
        if tail_passes_str_by_value(tail, &p.ty) {
            for _ in 0..str24::STR_SIZE / 8 {
                sig.params.push(AbiParam::new(types::I64));
            }
        } else {
            sig.params.push(AbiParam::new(cl_type_of(&p.ty)));
        }
    }
    if is_unit(&abi) || returns_composite {
        // Unit yields no result; a composite is written through the sret pointer.
    } else {
        sig.returns.push(AbiParam::new(types::I64));
    }
}

// ---------------------------------------------------------------------------
// Tail calls
//
// A call in *tail position* — one whose value is the enclosing function's value,
// with nothing left to run but the scope cleanup — is emitted as cranelift's
// `return_call`, which reuses the caller's frame instead of stacking a new one.
// That turns a recursive walk into a loop, so depth stops being bounded by the
// native stack (8 MB in a binary from `aipl build`, which is where this bites).
//
// Three things have to line up, and all three are decided *before* any function
// is declared, by `tail_call_plan`:
//
//   1. **Calling convention.** `return_call` requires `CallConv::Tail` on the
//      callee *and* an identical convention on the caller (cranelift's verifier
//      enforces both). The host convention doesn't support tail calls, so a
//      participant's body moves to a second, `Linkage::Local` function named
//      `<name>$tail` carrying `CallConv::Tail`, and the exported `<name>` becomes
//      a C-convention trampoline that forwards to it. The trampoline is what
//      keeps `Engine::call_values`, the `; entry` FFI surface and function values
//      (`func_addr`) working unconditionally — none of them can call a `tail`
//      function, and none of them can know whether some AIPL function three
//      edges away happens to tail-recurse. AIPL call sites skip the trampoline
//      and call `<name>$tail` directly; an ordinary `call` may cross conventions
//      freely, so only the `return_call` sites care.
//
//   2. **Nothing may point into the caller's frame.** The frame is gone the
//      moment control transfers, so a participant may not take or return a
//      composite (struct/optional/result): those are passed and returned by the
//      address of caller storage.
//
//   3. **Every refcounted argument must be the callee's own.** This is the real
//      constraint, and the reason "eliminate only where no drops are pending"
//      is a no-op in practice: a function that destructures an owned recursive
//      value and recurses on a field releases those payload bindings on scope
//      exit — *after* the call — so there is always a drop pending. A tail call
//      runs that cleanup *before* transferring control, which is safe only if
//      each argument still holds a reference the cleanup cannot release.
//      Refcounting gives exactly that guarantee for a retained argument (the
//      caller's `+1` is the callee's, and releasing other references can never
//      consume it), so the rule is: every argument that owns heap must be
//      `ParamInfo::retained` or moved in. Boxed parameters are pure borrows by
//      default, which would fail the rule, so a participant's boxed parameters
//      are marked `tail_owned` and pay the retain/release pair.
// ---------------------------------------------------------------------------

/// The suffix distinguishing a participant's real, `tail`-convention body from
/// the exported C-convention trampoline that forwards to it. `$` cannot appear
/// in an AIPL identifier, so this can't collide with a user function.
const TAIL_SUFFIX: &str = "$tail";

/// The callees `body` can reach from *tail position*.
///
/// Tail position propagates through the forms that only sequence or choose a
/// value — `Seq`/`Let`/`LetMut`/`Assign` bodies, both `if` branches, every
/// `match` arm — and is *created* by `return e`, whose operand is in tail
/// position however deep the `return` sits. It deliberately does not propagate
/// through `Shim` (which restores the previous bindings after its body runs) or
/// `Try` (which inspects the result), nor into any sub-expression whose value is
/// consumed rather than yielded.
///
/// Under-approximating is safe: a missed edge only costs an optimization, since
/// codegen independently re-derives tail position and emits a `return_call` only
/// when both ends carry the convention this plan gave them.
fn tail_callees(body: &Expr) -> Vec<String> {
    fn walk(e: &Expr, tail: bool, out: &mut Vec<String>) {
        match &e.kind {
            ExprKind::Call(name, args, _) => {
                if tail {
                    out.push(name.clone());
                }
                for a in args {
                    walk(a, false, out);
                }
            }
            ExprKind::Return(v) => walk(v, true, out),
            ExprKind::Seq(a, b) => {
                walk(a, false, out);
                walk(b, tail, out);
            }
            ExprKind::Let(_, _, v, b)
            | ExprKind::LetMut(_, _, v, b)
            | ExprKind::Assign(_, v, b) => {
                walk(v, false, out);
                walk(b, tail, out);
            }
            ExprKind::If(c, t, f) => {
                walk(c, false, out);
                walk(t, tail, out);
                walk(f, tail, out);
            }
            ExprKind::Match(s, arms) => {
                walk(s, false, out);
                for a in arms {
                    walk(&a.body, tail, out);
                }
            }
            _ => {
                for c in aipl_mono::children(e) {
                    walk(c, false, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(body, true, &mut out);
    out
}

/// Whether a parameter of this type can cross a tail call. A composite is
/// handed over as the *address* of caller storage, which the transfer
/// invalidates; anything else that owns heap has to be refcountable so the
/// callee can hold a reference of its own (`str`/array/set directly, a boxed
/// value via `tail_owned`). A dict — refcounted but neither — is left out
/// rather than reasoned about.
fn tail_safe_param(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    // A wide `str` is composite but still tail-safe, because the `tail`
    // signature passes it by value — see `tail_passes_str_by_value`. Without
    // this every `str`-taking function silently lost tail-call elimination the
    // moment the representation widened, turning a trampolined mutual cycle back
    // into real recursion (`cases/tail_calls/mutual_cycle`, which overflowed the
    // 8 MB stack of an `aipl build` binary at ~50k deep while the JIT, on a
    // bigger stack, still passed).
    if is_str_shaped(ty) {
        return true;
    }
    !is_composite(ty, structs) && (!needs_drop(ty, structs) || is_heap(ty) || is_boxed(ty, structs))
}

/// Whether `f` can carry `CallConv::Tail` at all — independent of whether any
/// tail call actually reaches or leaves it. Rules out the entry-style functions
/// whose ABI return is *derived* from the body (a unit or `!Error` `main`, a
/// mutating method), anything returning through an sret pointer, and any
/// parameter that fails [`tail_safe_param`].
///
/// Excluding `main` by name covers both entry-style mains on its own — each of
/// `ConcreteFn::is_unit_main` and `is_error_main` requires that name — so only
/// the mutating case needs asking about separately.
fn tail_eligible(f: &aipl_mono::ConcreteFn, structs: &HashMap<String, TypeDef>) -> bool {
    f.name != "main"
        && !f.is_mutating()
        && sret_size(&f.abi_return_type(), structs).is_none()
        && f.params.iter().all(|p| tail_safe_param(&p.ty, structs))
}

/// The functions that get `CallConv::Tail` (and so a `$tail` body plus an
/// exported trampoline): both endpoints of every tail-call edge whose two ends
/// are eligible and agree on their ABI return shape — cranelift requires a
/// `return_call`'s results to match the caller's signature exactly, which here
/// means both unit or both scalar.
fn tail_call_plan(
    program: &aipl_mono::MonoProgram,
    structs: &HashMap<String, TypeDef>,
) -> HashSet<String> {
    let by_name: HashMap<&str, &aipl_mono::ConcreteFn> =
        program.fns.iter().map(|f| (f.name.as_str(), f)).collect();
    let eligible: HashMap<&str, bool> = program
        .fns
        .iter()
        .map(|f| (f.name.as_str(), tail_eligible(f, structs)))
        .collect();
    let mut plan = HashSet::new();
    for f in &program.fns {
        if !eligible[f.name.as_str()] {
            continue;
        }
        let caller_unit = is_unit(&f.abi_return_type());
        for callee in tail_callees(&f.body) {
            // A name with no instance is a builtin or a codegen intrinsic; both
            // are reached through paths that never become a tail call.
            let Some(g) = by_name.get(callee.as_str()) else {
                continue;
            };
            if !eligible[callee.as_str()] || is_unit(&g.abi_return_type()) != caller_unit {
                continue;
            }
            plan.insert(f.name.clone());
            plan.insert(g.name.clone());
        }
    }
    plan
}

/// The declared type of an environment binding.
fn env_binding_type(b: &EnvBinding) -> ConcreteType {
    match b {
        EnvBinding::Immut(_, t) => t.clone(),
        EnvBinding::Mut(_, t, _) => t.borrow().clone(),
    }
}

/// A CLIF signature for an *indirect* call through a `(ptys) -> ret` function
/// value. Must match `build_signature`'s ABI exactly, since the callee was
/// emitted with that signature: a composite result is returned through a leading
/// sret pointer, and every parameter (and the scalar result) is an i64.
fn fn_value_signature<M: Module>(
    module: &M,
    ptys: &[ConcreteType],
    ret: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) -> Signature {
    let mut sig = module.make_signature();
    let returns_composite = sret_size(ret, structs).is_some();
    if returns_composite {
        sig.params.push(AbiParam::new(types::I64));
    }
    for _ in ptys {
        sig.params.push(AbiParam::new(types::I64));
    }
    if !is_unit(ret) && !returns_composite {
        sig.returns.push(AbiParam::new(types::I64));
    }
    sig
}

/// Prepare `program`'s `main` for the AOT entry, which always receives the CLI
/// arguments as a `str[]` (the runtime passes one). If the user's `main`
/// declares no parameters, inject a synthetic, ignored one so the entry ABI is
/// uniform — typed as an ignored word rather than `str[]`, because that is
/// exactly the case the runtime passes null for (see [`injected_cli_args_ty`]).
/// A declared `str[]` is owned by `main` and freed by the normal heap-parameter
/// drop. Errors if `main` declares anything other than a single `str[]`
/// parameter.
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
                ty: injected_cli_args_ty(),
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
    ) -> Result<Self, Vec<Error>> {
        if !program
            .items
            .iter()
            .any(|i| matches!(i, Item::Fn(f) if f.name == "main"))
        {
            return Err(Error::msg("binary build requires a \"main\" function".to_string()).into());
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
        // `none` is Cranelift's default; it is set here explicitly because it was
        // measured rather than inherited.
        //
        // `speed` looks obviously right for the AOT path — a `build` compiles
        // once and the code then runs indefinitely, unlike the JIT (`host_isa`),
        // where compile time *is* the latency. It measured as a wash. Two
        // benchmarks, 300M iterations of scalar arithmetic and 6M
        // struct-plus-`str`-concat allocations, each ran within noise of the
        // `none` build (0.19s vs 0.19s, 0.58s vs 0.57s) — for 1.5-2% smaller
        // objects and a 44% slower build.
        //
        // The reason is the shape of what this frontend emits: the generated code
        // is dominated by extern calls into the runtime — 88% of the calls in the
        // largest dogfooded function are `aipl_inc`/`aipl_dec` — and an optimizer
        // cannot see through a call. There is little straight-line code for it to
        // work on. Worth revisiting once the refcount and `str`-iteration fast
        // paths are inlined the way `load_array_elem` already inlines the array
        // one; until then it would cost a corpus-wide `binary size` refill and
        // half again the build time for no measured gain.
        flag_builder
            .set("opt_level", "none")
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

    /// Maps each defined function's *object symbol* to the AIPL-level name it
    /// was compiled from, for the per-function `code:` breakdown in the test
    /// harness's `--- performance ---` section (the object knows only symbols;
    /// this is what turns them back into names a reader recognizes).
    ///
    /// The two differ wherever codegen renames on the way out — notably `main`,
    /// which is emitted as [`BINARY_USER_MAIN`] so the linked runtime can own
    /// the real `main`. Covers only functions defined here: each generic
    /// specialization and owned/borrow/`str`-kept form is its own mangled
    /// instance (see the monomorphizer's `enqueue_full`), while builtin imports
    /// are runtime externs linked in separately and carry no code in this
    /// object. Codegen's *synthesized* helpers (`__to_str_<n>`, …) are absent
    /// too — they are declared straight on the module rather than through
    /// `funcs`, so the breakdown shows them under their symbol name.
    pub fn code_symbol_names(&self) -> HashMap<String, String> {
        let decls = self.module.declarations();
        self.funcs
            .iter()
            .filter_map(|(name, info)| match info.link {
                FuncLink::User(id) => Some((
                    decls.get_function_decl(id).linkage_name(id).into_owned(),
                    name.clone(),
                )),
                FuncLink::Builtin(_) => None,
            })
            .collect()
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
    Struct(&'a aipl_syntax::ast::ConcreteStructDecl),
    Variant(&'a aipl_syntax::ast::ConcreteVariantDecl),
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

    // Detect recursive types: a type that reaches itself through the reference
    // graph is heap-allocated ("boxed") rather than inline, so its layout can
    // treat every reference back into its own group as an 8-byte pointer.
    let rec = recursion_groups(&decls);

    let mut layouts: HashMap<String, TypeDef> = HashMap::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    // Sorted so resolution order — and therefore which type a cycle error is
    // reported against — is deterministic across runs.
    let mut names: Vec<&str> = decls.keys().copied().collect();
    names.sort_unstable();
    for name in names {
        resolve_type_layout(name, &decls, &mut layouts, &mut on_stack, &rec)?;
    }
    Ok(layouts)
}

/// The named types `ty` *contains inline* — reached directly or through
/// optional/result layers, which store their core inline. Arrays/sets/dicts
/// are excluded: they are separately refcounted heap blocks, so a reference
/// they hold is an *external*, strong reference (see the recursive-type
/// runtime). That distinction is what this answers, so it is the walk
/// `contains_scc_ref` wants — for "does this type reach that one at all",
/// which is what boxing is decided from, see `referenced_named_types`.
fn contained_named_types<'a>(ty: &'a ConcreteType, out: &mut Vec<&'a str>) {
    match ty {
        ConcreteType::Named(n) => out.push(n),
        ConcreteType::Optional(inner) => contained_named_types(inner, out),
        ConcreteType::Result(ok, err) => {
            contained_named_types(ok, out);
            contained_named_types(err, out);
        }
        _ => {}
    }
}

/// Every named type `ty` *reaches*, through any number of layers — including
/// array elements, set elements and dict keys/values, which
/// `contained_named_types` stops at.
///
/// Being behind a heap block bounds a type's *size*, but it does not break the
/// cycle for anything that walks a value's structure: rendering, dropping,
/// retaining and comparing a `Tree { kids: Tree[] }` all have to recurse into
/// the array's elements, which are `Tree`s again. Inline (unboxed) values are
/// walked by inlining that structure into the generated code, so such a type
/// must still be boxed — otherwise codegen recurses forever (`emit_render`) and
/// a rebind copies a live value byte-wise instead of transferring a refcounted
/// pointer. Function types are excluded: a function value is a bare code
/// address that owns nothing, so it reaches no value at all.
/// The largest value of integer type `p`, as the i64 bit pattern a canonicalized
/// value of that type holds. `u64::MAX` is all-ones, which reads as `-1` in an
/// i64 register — that is the representation, not a sign.
fn int_max_bits(p: Primitive) -> i64 {
    let bits = p.int_bits().expect("integer type");
    if p.int_signed() {
        // Shifted down from `i64::MAX` rather than built up from a shifted `1`:
        // at 64 bits the latter is `i64::MIN`, and subtracting one from it
        // overflows.
        i64::MAX >> (64 - bits)
    } else if bits == 64 {
        -1
    } else {
        (1i64 << bits) - 1
    }
}

/// The smallest value of signed integer type `p`, as an i64 bit pattern. Shifted
/// down from `i64::MIN` for the same reason [`int_max_bits`] shifts.
fn int_min_bits(p: Primitive) -> i64 {
    i64::MIN >> (64 - p.int_bits().expect("integer type"))
}

/// `a / b`, saturating instead of trapping.
///
/// Cranelift's `sdiv`/`udiv` trap on a zero divisor, and `sdiv` traps again on
/// `MIN / -1` (whose true quotient is one past `MAX`). AIPL has no aborts, so
/// both answer `MAX` for the operand type — the same "clamp at the end of the
/// range" rule `saturating_add` follows, which is why the builtin is spelled
/// `saturating_divide`.
///
/// Written branch-free: the divisor is replaced by `1` whenever it would trap,
/// so the division instruction never sees a case it cannot answer, and a
/// `select` swaps in `MAX` afterwards. That keeps the whole thing three
/// instructions and straight-line, with no block to split the caller's flow.
fn saturating_div(builder: &mut FunctionBuilder, a: Value, b: Value, p: Primitive) -> Value {
    let zero = builder.ins().iconst(types::I64, 0);
    let one = builder.ins().iconst(types::I64, 1);
    let max = builder.ins().iconst(types::I64, int_max_bits(p));
    let div_by_zero = builder.ins().icmp(IntCC::Equal, b, zero);
    let overflows = if p.int_signed() {
        // `MIN / -1` overflows the range; every other signed pair is fine.
        let min = builder.ins().iconst(types::I64, int_min_bits(p));
        let neg_one = builder.ins().iconst(types::I64, -1);
        let a_min = builder.ins().icmp(IntCC::Equal, a, min);
        let b_neg = builder.ins().icmp(IntCC::Equal, b, neg_one);
        let both = builder.ins().band(a_min, b_neg);
        builder.ins().bor(div_by_zero, both)
    } else {
        div_by_zero
    };
    let safe_b = builder.ins().select(overflows, one, b);
    let quotient = if p.int_signed() {
        builder.ins().sdiv(a, safe_b)
    } else {
        builder.ins().udiv(a, safe_b)
    };
    builder.ins().select(overflows, max, quotient)
}

/// Every named type `ty` reaches. The names are *borrowed* from `ty`, which is
/// why this walks the concrete representation directly rather than widening at
/// the call site: a widened temporary would not outlive the collection.
fn referenced_named_types<'a>(ty: &'a aipl_syntax::ast::ConcreteType, out: &mut Vec<&'a str>) {
    use aipl_syntax::ast::ConcreteType as C;
    match ty {
        C::Named(n) => out.push(n),
        C::Optional(inner) | C::Array(inner) | C::Set(inner) => referenced_named_types(inner, out),
        C::Result(a, b) | C::Dict(a, b) => {
            referenced_named_types(a, out);
            referenced_named_types(b, out);
        }
        _ => {}
    }
}

/// Compute the *recursion groups*: for every type that is part of a
/// reference cycle (it reaches itself through struct fields, variant payloads,
/// optional/result cores, or container elements — directly or mutually), map
/// its name to its cycle's group id (the strongly-connected component of the containment
/// graph). Types not in any cycle are absent. Members of a group are boxed,
/// and a reference between boxed values of the same group is an internal
/// (weak-counted) reference.
fn recursion_groups(decls: &HashMap<&str, TypeDeclRef>) -> HashMap<String, u32> {
    // Deterministic node order so group ids are stable across runs.
    let mut names: Vec<&str> = decls.keys().copied().collect();
    names.sort_unstable();
    let index: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    let edges: Vec<Vec<usize>> = names
        .iter()
        .map(|n| {
            let mut refs: Vec<&str> = Vec::new();
            match decls[n] {
                TypeDeclRef::Struct(s) => {
                    for f in &s.fields {
                        referenced_named_types(&f.ty, &mut refs);
                    }
                }
                TypeDeclRef::Variant(v) => {
                    for c in &v.cases {
                        for ty in &c.payload {
                            referenced_named_types(ty, &mut refs);
                        }
                    }
                }
            }
            // An unknown name (reported later during layout resolution) has no
            // node; skip it here.
            refs.iter().filter_map(|r| index.get(r).copied()).collect()
        })
        .collect();

    // Tarjan's SCC algorithm (recursive — type graphs are small).
    struct Scc<'a> {
        edges: &'a [Vec<usize>],
        index: Vec<Option<u32>>,
        lowlink: Vec<u32>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next_index: u32,
        components: Vec<Vec<usize>>,
    }
    impl Scc<'_> {
        fn visit(&mut self, v: usize) {
            self.index[v] = Some(self.next_index);
            self.lowlink[v] = self.next_index;
            self.next_index += 1;
            self.stack.push(v);
            self.on_stack[v] = true;
            for &w in &self.edges[v] {
                if self.index[w].is_none() {
                    self.visit(w);
                    self.lowlink[v] = self.lowlink[v].min(self.lowlink[w]);
                } else if self.on_stack[w] {
                    self.lowlink[v] = self.lowlink[v].min(self.index[w].expect("visited"));
                }
            }
            if self.lowlink[v] == self.index[v].expect("visited") {
                let mut comp = Vec::new();
                loop {
                    let w = self.stack.pop().expect("scc stack");
                    self.on_stack[w] = false;
                    comp.push(w);
                    if w == v {
                        break;
                    }
                }
                self.components.push(comp);
            }
        }
    }
    let n = names.len();
    let mut scc = Scc {
        edges: &edges,
        index: vec![None; n],
        lowlink: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next_index: 0,
        components: Vec::new(),
    };
    for v in 0..n {
        if scc.index[v].is_none() {
            scc.visit(v);
        }
    }

    // A type is recursive iff its component has more than one member, or it
    // contains itself directly (a self-edge).
    let mut groups: HashMap<String, u32> = HashMap::new();
    let mut next_group = 0u32;
    for comp in &scc.components {
        let cyclic = comp.len() > 1 || edges[comp[0]].contains(&comp[0]);
        if cyclic {
            for &v in comp {
                groups.insert(names[v].to_string(), next_group);
            }
            next_group += 1;
        }
    }
    groups
}

/// Compute (and memoize into `layouts`) the layout of the struct or variant
/// `name`, recursing into any struct- or variant-typed components first —
/// except components of *boxed* (recursive) types, which are 8-byte pointers
/// regardless of their layout, so they don't need resolving first (they get
/// their own top-level resolution pass). That skip is what breaks every
/// containment cycle: `on_stack` is a backstop only, since any remaining
/// cycle would have been classified boxed by `recursion_groups`.
fn resolve_type_layout(
    name: &str,
    decls: &HashMap<&str, TypeDeclRef>,
    layouts: &mut HashMap<String, TypeDef>,
    on_stack: &mut HashSet<String>,
    rec: &HashMap<String, u32>,
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
            "{kind} {name}: containment cycle not classified as recursive \
             (compiler bug in recursion_groups)"
        )));
    }
    let def = match decl {
        TypeDeclRef::Struct(s) => {
            TypeDef::Struct(build_struct_layout(s, decls, layouts, on_stack, rec)?)
        }
        TypeDeclRef::Variant(v) => {
            TypeDef::Variant(build_variant_layout(v, decls, layouts, on_stack, rec)?)
        }
    };
    on_stack.remove(name);
    layouts.insert(name.to_string(), def);
    Ok(())
}

/// Lay out a `struct`: fields are stored sequentially (no padding — every
/// field size is a multiple of 8), nested composites inline.
fn build_struct_layout(
    decl: &aipl_syntax::ast::ConcreteStructDecl,
    decls: &HashMap<&str, TypeDeclRef>,
    layouts: &mut HashMap<String, TypeDef>,
    on_stack: &mut HashSet<String>,
    rec: &HashMap<String, u32>,
) -> Result<StructLayout, Error> {
    let mut fields = Vec::with_capacity(decl.fields.len());
    let mut offset: u32 = 0;
    for f in &decl.fields {
        // Allowed field types: i64/bool/char (by value), `str` or an array
        // (8-byte refcounted heap pointers), another declared struct or a
        // variant (stored inline — resolve it here so its size is known —
        // unless it's boxed, in which case the field is an 8-byte pointer and
        // needs no size), or an optional of a scalar/str/array (a 16-byte
        // inline `{tag, value}` composite).
        match &f.ty {
            // Every scalar: any integer width, `bool`, `char`, `str`.
            _ if is_set_elem(&f.ty) => {}
            ConcreteType::Named(n) if decls.contains_key(n.as_str()) => {
                if !rec.contains_key(n.as_str()) {
                    resolve_type_layout(n, decls, layouts, on_stack, rec)?;
                }
            }
            ConcreteType::Array(_) => {}
            // A function value is stored as its 8-byte code address (an i64);
            // it owns nothing, so like a scalar it needs no drop.
            ConcreteType::Fn(_, _) => {}
            // An optional of a scalar/str/array, or of a *boxed* (recursive)
            // type — the latter is an 8-byte pointer core, so `Tree?` is a
            // 16-byte `{tag, ptr}` inline composite (this is how a recursive
            // struct spells "maybe a child": `left: Tree?`).
            ConcreteType::Optional(inner)
                if is_set_elem(inner)
                    || matches!(inner.as_ref(), ConcreteType::Array(_))
                    || matches!(inner.as_ref(), ConcreteType::Named(n) if rec.contains_key(n.as_str())) =>
                {}
            _ => {
                return Err(Error::msg(format!(
                    "struct {}: field {} has type {}, but struct fields must be an integer (i8..i64, u8..u64), bool, char, str, a function, a struct, a variant, an array, or an optional of (an integer, bool, char, str, an array, or a recursive type)",
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
        boxed: rec.contains_key(&decl.name),
        scc: rec.get(&decl.name).copied().unwrap_or(0),
    })
}

/// Lay out a `variant`: the tag occupies offset 0, each case's payload is laid
/// out (like a struct) from `VARIANT_PAYLOAD_OFFSET`, and the whole value is
/// sized to the widest case so all cases share one payload region.
fn build_variant_layout(
    v: &aipl_syntax::ast::ConcreteVariantDecl,
    decls: &HashMap<&str, TypeDeclRef>,
    layouts: &mut HashMap<String, TypeDef>,
    on_stack: &mut HashSet<String>,
    rec: &HashMap<String, u32>,
) -> Result<VariantLayout, Error> {
    let mut cases = Vec::with_capacity(v.cases.len());
    let mut max_payload: u32 = 0;
    for c in &v.cases {
        let mut fields = Vec::with_capacity(c.payload.len());
        let mut offset = VARIANT_PAYLOAD_OFFSET;
        for ty in &c.payload {
            // A payload field is an array element / inline composite: a scalar,
            // `str`, an array, an optional, or a struct/variant (resolved here
            // so its size is known — unless boxed, in which case the field is
            // an 8-byte pointer; that's how a recursive sum type like a list
            // gets its indirection).
            let ty = ty;
            let ok = match ty {
                _ if is_set_elem(ty) => true, // i64/bool/char/str
                ConcreteType::Array(_) | ConcreteType::Optional(_) => true,
                // A function value is an 8-byte code address, stored inline like
                // a scalar; it owns no heap, so it needs no drop.
                ConcreteType::Fn(_, _) => true,
                ConcreteType::Named(n) if decls.contains_key(n.as_str()) => {
                    if !rec.contains_key(n.as_str()) {
                        resolve_type_layout(n, decls, layouts, on_stack, rec)?;
                    }
                    true
                }
                _ => false,
            };
            if !ok {
                return Err(Error::msg(format!(
                    "variant {} case {}: payload type {} is not supported (use an integer \
                     (i8..i64, u8..u64), bool, char, str, a function, an array, an optional, a \
                     struct, or a variant)",
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
        boxed: rec.contains_key(&v.name),
        scc: rec.get(&v.name).copied().unwrap_or(0),
    })
}

fn cl_type_of(_t: &ConcreteType) -> types::Type {
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
        let l = builder.ins().ishl_imm_u(v, shift);
        builder.ins().sshr_imm_u(l, shift)
    } else {
        let mask = (1i64 << bits) - 1;
        builder.ins().band_imm_u(v, mask)
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
        let overflowed = builder.ins().icmp_imm_s(IntCC::SignedLessThan, both, 0);
        let is_neg = builder.ins().icmp_imm_s(IntCC::SignedLessThan, lv, 0);
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

/// What one runtime-call parameter or result is, in ABI terms.
///
/// [`ArgKind::Word`] is anything that travels in a single `i64` register: a pointer,
/// an integer, a `bool`/`char`, a function-pointer constant, a raw byte cursor,
/// an array/set/dict handle, or a pointer *to* an element rather than the
/// element itself (`aipl_array_push`'s `x`, `aipl_dict_get`'s `key_ptr`).
/// [`ArgKind::Str`] is a `str` *value*.
///
/// The two are the same width today, which is why this table used to count
/// parameters instead of naming them — and exactly why it can't any more. A
/// 24-byte `str` (`STR_REPR.md`) passes as three scalar words and returns
/// through a leading out pointer, because multi-value returns of three words are
/// refused on x86-64 (`abi_spike::q2_three_word_returns_are_refused_on_x86_64`;
/// the working shape is pinned by `q2b`). Naming the kinds now makes that flip a
/// change to [`lower_import_sig`] rather than a re-derivation of seventy
/// signatures under time pressure.
///
/// Two classifications worth knowing, because they are not guessable from the
/// Rust signature (every pointer is `*const u8` there):
///
/// - **Refcount ops take a `str`.** `aipl_inc`/`aipl_dec` branch on the
///   representation tag, so they are `Str`, not a bare pointer — though after
///   the flip they need only the base and meta words, never `data`, which is an
///   optimization to take once it can be measured.
/// - **The `to_str` builder writes through raw cursors.** `aipl_str_alloc`
///   returns the writable content pointer, and `aipl_write_i64`/`_u64`/`_bytes`
///   advance it; none of those is a `str` value. Today a heap `str` value *is*
///   its content pointer, so the distinction is invisible; after the flip that
///   idiom becomes "allocate a buffer, then build a value over it", and this is
///   the table that says which is which.

/// What a runtime import gives back. The argument half is just a count — every
/// argument is one `i64` — so only the return needs naming, because a `str`
/// result is not returned at all but written through an out pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ret {
    None,
    Word,
    Str,
}

/// Lower a runtime import's kinds to a cranelift signature. **The one place the
/// runtime ABI's shape is decided**: today every kind is one `i64`, so this is
/// the identity the old arity table encoded; at the flip, `ArgKind::Str` expands to
/// three params and `Ret::Str` becomes a leading out-pointer param with no
/// return value.
fn lower_import_sig<M: Module>(
    module: &mut M,
    params: usize,
    ret: Ret,
    out_pointer: bool,
) -> Signature {
    let mut s = module.make_signature();
    // A `str` result is written through a leading out pointer, so the callee
    // returns nothing and the caller supplies the buffer. `Ret::Str` says
    // "yields a `str`"; whether to lower it that way is the caller's to say —
    // see `builtin_import_sig`.
    if out_pointer {
        s.params.push(AbiParam::new(types::I64));
    }
    for _ in 0..params {
        s.params.push(AbiParam::new(types::I64));
    }
    match ret {
        Ret::None => {}
        Ret::Str if out_pointer => {}
        Ret::Word | Ret::Str => s.returns.push(AbiParam::new(types::I64)),
    }
    s
}

/// Whether `sym` returns its `str` through a leading out pointer — true for the
/// new ABI's entry points, false for the originals.
fn returns_via_out_pointer(ret: Ret) -> bool {
    ret == Ret::Str
}

/// Signature of a runtime import `sym`.
fn builtin_import_sig<M: Module>(module: &mut M, sym: &str) -> Signature {
    let (params, ret) = import_abi(sym);
    lower_import_sig(module, params, ret, returns_via_out_pointer(ret))
}

/// How many `i64` argument words `params` lowers to — the arity every call site
/// must supply. One per kind today; an [`ArgKind::Str`] becomes three at the flip.
fn import_arg_words(params: usize) -> usize {
    params
}

/// What runtime import `sym` takes and gives back. See [`ArgKind`] for what the
/// kinds mean and why they are spelled out rather than counted.
fn import_abi(sym: &str) -> (usize, Ret) {
    // `(argument words, what comes back)`. Every argument is one `i64` — a
    // scalar, a pointer, or the address of a `str` value — so counting them is
    // enough; what a word *means* is documented at each entry point. A `str`
    // result is the one thing the count cannot express, hence [`Ret::Str`]: it
    // travels through a leading out pointer, so the callee returns nothing and
    // the caller supplies the buffer (`Builtins::call`).
    match sym {
        // ---- nothing back ----
        "aipl_print"
        | "aipl_print_error"
        | "aipl_inc"
        | "aipl_dec"
        | "aipl_test_begin"
        | "aipl_test_fail"
        | "aipl_array_dec"
        | "aipl_arr_inc"
        | "aipl_rec_inc_strong"
        | "aipl_rec_dec_strong"
        | "aipl_rec_inc_weak"
        | "aipl_rec_dec_weak"
        | "aipl_count_insns"
        | "aipl_count_call" => (1, Ret::None),
        "aipl_test_end" | "aipl_test_fail_none" => (0, Ret::None),
        "aipl_assert"
        | "aipl_shim_set"
        | "aipl_str_iter_init"
        | "aipl_str_grew"
        | "aipl_str_push_byte"
        | "aipl_str_append"
        | "aipl_arr_drop_str"
        | "aipl_arr_drop_arr"
        | "aipl_arr_retain_ptr"
        | "aipl_arr_retain_str"
        | "aipl_arr_drop_opt_str"
        | "aipl_arr_drop_opt_arr"
        | "aipl_arr_retain_opt"
        | "aipl_arr_retain_opt_str" => (2, Ret::None),
        "aipl_execute_program" => (3, Ret::None),
        // ---- a scalar back ----
        "aipl_test_summary" | "aipl_now_nanos" | "aipl_monotonic_now" => (0, Ret::Word),
        "aipl_shim_get" | "aipl_str_len" | "aipl_str_hash" | "aipl_str_iter_next"
        | "aipl_str_write_ptr" | "aipl_i64_len" | "aipl_u64_len" | "aipl_list_files" => {
            (1, Ret::Word)
        }
        "aipl_char_at"
        | "aipl_str_cmp"
        | "aipl_str_eq"
        | "aipl_str_starts_with"
        | "aipl_str_ends_with"
        | "aipl_str_contains"
        | "aipl_str_data"
        | "aipl_arr_load_bit"
        | "aipl_write_string_to_file"
        | "aipl_str_split"
        | "aipl_read_file_to_string"
        | "aipl_rec_alloc"
        | "aipl_write_i64"
        | "aipl_write_u64" => (2, Ret::Word),
        "aipl_str_starts_with_at"
        | "aipl_arr_elem_ptr"
        | "aipl_write_bytes"
        | "aipl_array_new"
        | "aipl_array_with_cap" => (3, Ret::Word),
        "aipl_set_contains" | "aipl_dict_get" | "aipl_dict_contains_key" | "aipl_arr_reverse" => {
            (4, Ret::Word)
        }
        "aipl_array_push"
        | "aipl_array_push_mut"
        | "aipl_arr_sort"
        | "aipl_arr_reserve"
        | "aipl_arr_extend" => (5, Ret::Word),
        "aipl_set_insert" | "aipl_set_union" | "aipl_set_union_mut" | "aipl_dict_insert"
        | "aipl_arr_slice" => (6, Ret::Word),
        // ---- a `str` back, through the out pointer ----
        "aipl_trim" | "aipl_str_reverse" | "aipl_str_sort" | "aipl_str_alloc"
        | "aipl_char_to_str" => (1, Ret::Str),
        "aipl_concat" | "aipl_str_repeat" | "aipl_str_join" => (2, Ret::Str),
        "aipl_str_slice" => (3, Ret::Str),
        // Joins `T[][]` into a `T[]` — an *array* back in a register, not a
        // `str`. It was labelled `Ret::Str` while every result was one word and
        // the two were indistinguishable; they no longer are, and a `Ret::Str`
        // here would have codegen prepend an out pointer for a value that comes
        // back in a register.
        "aipl_arr_join" => (5, Ret::Word),
        other => panic!("unknown builtin import symbol {other:?}"),
    }
}

impl Builtins {
    /// The `FuncId` for runtime import `sym`, declaring it (once) on first use.
    /// The `FuncId` for `sym` **if it has already been declared**, without
    /// declaring it. An IR pass keyed on a runtime symbol uses this rather than
    /// [`Builtins::id`]: declaring one here would add an unused import to every
    /// program that never calls it, moving its `binary size` for nothing.
    fn declared<M: Module>(&self, _module: &mut M, sym: &'static str) -> Option<FuncId> {
        self.imports.borrow().get(sym).copied()
    }

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

    /// Emit a call to runtime import `sym` and return its result.
    ///
    /// **Every runtime call should come through here or [`Builtins::call_void`].**
    /// Today the helper is only boilerplate removal — import, call, read the one
    /// result — but it is also the seam a 24-byte `str` needs (`STR_REPR.md`):
    /// when an [`ArgKind::Str`] argument becomes three words and an [`Ret::Str`]
    /// result becomes a leading out pointer, this is the function that expands
    /// them. The arity assertion is what keeps a call site honest in the
    /// meantime; a wrong count is otherwise a silent ABI mismatch that corrupts
    /// memory rather than failing to compile.
    fn call<M: Module>(
        &self,
        module: &mut M,
        builder: &mut FunctionBuilder,
        sym: &'static str,
        args: &[Value],
    ) -> Value {
        let (params, ret) = import_abi(sym);
        debug_assert_eq!(
            args.len(),
            import_arg_words(params),
            "runtime call {sym}: wrong argument count"
        );
        debug_assert!(ret != Ret::None, "runtime call {sym} returns nothing");
        // A `str` result travels through a leading out pointer under the *new*
        // ABI only (`abi_spike::q2b`, and it is what a 24-byte value needs). The
        // old entry points return their tagged pointer in a register, so the
        // protocol keys off the convention the symbol belongs to, not off
        // `Ret::Str` alone — which both ABIs use to mean "this yields a `str`".
        if returns_via_out_pointer(ret) {
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                str24::STR_SIZE as u32,
                3,
            ));
            let out = builder.ins().stack_addr(types::I64, slot, 0);
            let mut with_out = Vec::with_capacity(args.len() + 1);
            with_out.push(out);
            with_out.extend_from_slice(args);
            let f = self.import(module, builder.func, sym);
            builder.ins().call(f, &with_out);
            return out;
        }
        let f = self.import(module, builder.func, sym);
        let call = builder.ins().call(f, args);
        builder.inst_results(call)[0]
    }

    /// [`Builtins::call`] for an import that returns nothing.
    fn call_void<M: Module>(
        &self,
        module: &mut M,
        builder: &mut FunctionBuilder,
        sym: &'static str,
        args: &[Value],
    ) {
        let (params, ret) = import_abi(sym);
        debug_assert_eq!(
            args.len(),
            import_arg_words(params),
            "runtime call {sym}: wrong argument count"
        );
        debug_assert!(ret == Ret::None, "runtime call {sym} returns a value");
        let f = self.import(module, builder.func, sym);
        builder.ins().call(f, args);
    }

    /// Clear the per-function `FuncRef` cache. Must be called at the start of
    /// every `define_*` function so stale refs from the previous `Function` are
    /// never reused (Cranelift clears `ctx.func` via `module.clear_context`).
    fn clear_func_cache(&self) {
        self.func_refs.borrow_mut().clear();
    }
}

/// Every runtime entry point **borrows** its heap arguments: it reads them and
/// releases nothing, so the caller keeps its own reference and the borrow
/// protocol's retain/release pair cancels.
///
/// This used to be a choice recorded per symbol. The old `str` convention had
/// str-taking builtins *consume* their argument, with the call site pre-inc'ing
/// to balance — a protocol that existed because a `str` was a bare pointer whose
/// lifetime had to be managed across the call. A 24-byte value is passed by
/// address to something the caller is already keeping alive, so there is nothing
/// to hand over, and the answer became the same for every symbol.
///
/// Kept as a named function rather than inlined at each `reg` call because it is
/// the *statement of the protocol*, and the place a second answer would go if a
/// future entry point ever needed one.
fn borrowed_param(ty: ConcreteType) -> ParamInfo {
    ParamInfo::inspected(ty)
}

fn register_builtins(
    funcs: &mut HashMap<String, FuncInfo>,
    needed: &BTreeSet<&'static str>,
) -> Builtins {
    // Record the call-site-resolved builtins in `funcs` for type-checking and
    // method resolution. None are *declared* here — each `aipl_*` import is
    // declared lazily by `Builtins::id` on first reference (see the `Builtins`
    // doc), so a program carries only the builtin symbols it actually uses, and
    // adding a builtin doesn't shift every program's `binary size`.
    fn reg(
        funcs: &mut HashMap<String, FuncInfo>,
        name: &str,
        sym: &'static str,
        params: Vec<ParamInfo>,
        return_ty: ConcreteType,
        effects: &[&str],
    ) {
        funcs.insert(
            name.to_string(),
            FuncInfo {
                link: FuncLink::Builtin(sym),
                tail_id: None,
                params,
                return_ty,
                effects: effects.iter().map(|s| s.to_string()).collect(),
                is_mutating: false,
            },
        );
    }
    // The user-facing builtins' call-site signatures come straight from the parsed
    // `BUILTIN_SIGNATURES` (the single source of truth) — only the mapping to each
    // runtime symbol is given here (mostly `__builtin_X` -> `aipl_X`, but e.g.
    // `split` -> `aipl_str_split`). Each is intercepted by a custom codegen arm, so
    // this `funcs` entry is only for call-site arg type-checking / method resolution.
    let decls = builtin_decls(needed);
    let sig: HashMap<&str, &AstFn> = decls
        .iter()
        .filter_map(|it| match it {
            Item::Fn(f) => Some((f.name.as_str(), f)),
            _ => None,
        })
        .collect();
    // (canonical builtin name, runtime symbol). Every entry point borrows its
    // arguments — see `borrowed_param` — so there is nothing else to record.
    const SIG_REGS: &[(&str, &str)] = &[
        ("__builtin_print", "aipl_print"),
        ("__builtin_split", "aipl_str_split"),
        ("__builtin_read_file_to_string", "aipl_read_file_to_string"),
        (
            "__builtin_write_string_to_file",
            "aipl_write_string_to_file",
        ),
        ("__builtin_list_files", "aipl_list_files"),
        ("__builtin_now_nanos", "aipl_now_nanos"),
        ("__builtin_monotonic_now", "aipl_monotonic_now"),
        ("__builtin_execute_program", "aipl_execute_program"),
        ("__builtin_trim", "aipl_trim"),
        ("__builtin_repeat", "aipl_str_repeat"),
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
        // Every `SIG_REGS` entry names a builtin with a direct runtime symbol,
        // and those are monomorphic — a generic builtin (`map`, `filter`) is
        // resolved at its call site and never registered here. So the signature
        // is concrete, and the `expect` states that rather than assuming it.
        let concretize = |t: &aipl_syntax::ast::Type| {
            t.to_concrete().unwrap_or_else(|| {
                panic!("builtin {name:?} has a generic signature but a direct runtime symbol")
            })
        };
        let params: Vec<ParamInfo> = f
            .sig
            .param_types()
            .iter()
            .map(|t| borrowed_param(concretize(t)))
            .collect();
        let return_ty = concretize(&f.sig.return_type());
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
            borrowed_param(ConcreteType::Primitive(Primitive::Str)),
            borrowed_param(ConcreteType::Primitive(Primitive::Str)),
        ],
        ConcreteType::Primitive(Primitive::Str),
        &[],
    );
    reg(
        funcs,
        "__aipl_concat_lazy",
        "aipl_concat",
        vec![
            borrowed_param(ConcreteType::Primitive(Primitive::Str)),
            borrowed_param(ConcreteType::Primitive(Primitive::Str)),
        ],
        ConcreteType::Primitive(Primitive::Str),
        &[],
    );
    reg(
        funcs,
        "__aipl_concat_mut",
        "aipl_concat",
        vec![
            borrowed_param(ConcreteType::Primitive(Primitive::Str)),
            borrowed_param(ConcreteType::Primitive(Primitive::Str)),
        ],
        ConcreteType::Primitive(Primitive::Str),
        &[],
    );
    reg(
        funcs,
        "__aipl_trim_mut",
        "aipl_trim",
        vec![borrowed_param(ConcreteType::Primitive(Primitive::Str))],
        ConcreteType::Primitive(Primitive::Str),
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
    Immut(Value, ConcreteType),
    /// A mutable binding in a stack slot. The `bool` is the *exclusive* flag:
    /// set when static analysis proved the binding's array is never aliased, so
    /// `push` may mutate it in place. Always false for non-array bindings.
    Mut(StackSlot, Rc<RefCell<ConcreteType>>, bool),
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
) -> Result<(Value, ConcreteType), Error> {
    let binding = env
        .get(name)
        .ok_or_else(|| Error::at(format!("unknown identifier {name:?}"), span.clone()))?;
    Ok(match binding {
        EnvBinding::Immut(v, t) => (*v, t.clone()),
        EnvBinding::Mut(slot, t, _) => {
            let ty = t.borrow().clone();
            // Reading a `mut` binding yields a **snapshot**, and under the wide
            // representation that has to be made explicitly.
            //
            // A tagged read copies a pointer out of the slot, so `let snap = cs;`
            // kept naming the old buffer even after `cs` was reassigned — value
            // semantics for free, because a rebuild allocates a fresh buffer and
            // only the slot is repointed. A wide value *is* the slot's 24 bytes,
            // so handing back the slot's address would make `snap` an alias that
            // changes underneath the next `set`. Copying restores the tagged
            // behaviour exactly; the refcount is untouched (this is a borrow,
            // like the load it replaces).
            let v = if is_str_shaped(&ty) {
                let src = builder.ins().stack_addr(types::I64, *slot, 0);
                copy_str_value(builder, src)
            } else {
                builder.ins().stack_load(types::I64, types::I64, *slot, 0)
            };
            (v, ty)
        }
    })
}

/// Whether a value of type `actual` may flow where `expected` is wanted.
/// `none` (`Optional(__none__)`) and the empty array literal `[]`
/// (`Array(__none__)`) carry the placeholder element `__none__`, which unifies
/// with any concrete element in either direction — recursively through matching
/// optional/array layers, so e.g. `some(some(none))` (`__none__???`) fits
/// `i64???` and `[[]]` fits `i64[][]`.
fn coercible(actual: &ConcreteType, expected: &ConcreteType) -> bool {
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
        (ConcreteType::Optional(a), ConcreteType::Optional(b)) => coercible(a, b),
        (ConcreteType::Array(a), ConcreteType::Array(b)) => coercible(a, b),
        (ConcreteType::Set(a), ConcreteType::Set(b)) => coercible(a, b),
        (ConcreteType::Dict(ak, av), ConcreteType::Dict(bk, bv)) => {
            coercible(ak, bk) && coercible(av, bv)
        }
        (ConcreteType::Result(ao, ae), ConcreteType::Result(bo, be)) => {
            coercible(ao, bo) && coercible(ae, be)
        }
        _ => false,
    }
}

/// Merge two branch/arm types (an `if`'s arms, a `match`'s arms): the common
/// type both coerce to, or `None` if they're incompatible. A `__none__` element
/// on either side takes the other's, recursively through matching layers (the
/// type-level counterpart of `coercible`).
fn merge_types(a: &ConcreteType, b: &ConcreteType) -> Option<ConcreteType> {
    if a == b || is_none_inner(b) {
        return Some(a.clone());
    }
    if is_none_inner(a) {
        return Some(b.clone());
    }
    // `Error` and `str` share a representation; their common type is a plain str.
    if (is_error(a) && *b == ConcreteType::Primitive(Primitive::Str))
        || (*a == ConcreteType::Primitive(Primitive::Str) && is_error(b))
    {
        return Some(ConcreteType::Primitive(Primitive::Str));
    }
    // `char[]` and `str` share a representation too (see `is_char_array`);
    // their common type is a plain str (`emit_eq` dispatches identically for
    // either — see `is_str_shaped` — so the choice is just a label).
    if (is_char_array(a) && *b == ConcreteType::Primitive(Primitive::Str))
        || (*a == ConcreteType::Primitive(Primitive::Str) && is_char_array(b))
    {
        return Some(ConcreteType::Primitive(Primitive::Str));
    }
    match (a, b) {
        (ConcreteType::Optional(x), ConcreteType::Optional(y)) => {
            Some(ConcreteType::Optional(Box::new(merge_types(x, y)?)))
        }
        (ConcreteType::Array(x), ConcreteType::Array(y)) => {
            Some(ConcreteType::Array(Box::new(merge_types(x, y)?)))
        }
        (ConcreteType::Set(x), ConcreteType::Set(y)) => {
            Some(ConcreteType::Set(Box::new(merge_types(x, y)?)))
        }
        (ConcreteType::Dict(xk, xv), ConcreteType::Dict(yk, yv)) => Some(ConcreteType::Dict(
            Box::new(merge_types(xk, yk)?),
            Box::new(merge_types(xv, yv)?),
        )),
        (ConcreteType::Result(xo, xe), ConcreteType::Result(yo, ye)) => Some(ConcreteType::Result(
            Box::new(merge_types(xo, yo)?),
            Box::new(merge_types(xe, ye)?),
        )),
        _ => None,
    }
}

fn expect_type(
    actual: &ConcreteType,
    expected: &ConcreteType,
    context: &str,
    span: Span,
) -> Result<(), Error> {
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

/// A length-like operand — an index, a slice bound, a `Span` field, or a
/// capacity — accepted as `i64` *or* `u64`. Mirrors the checker's
/// `expect_len_operand`: an integer literal or loop counter is `i64` while
/// `len()` is `u64`, and requiring one signedness would force a conversion on
/// every `xs[xs.len() - 1]` or every `xs[i]`. Both occupy the same 64-bit
/// register and bounds are clamped to `[0, len]` either way, so a negative
/// `i64` and an out-of-range `u64` already behave identically.
fn expect_len_operand(actual: &ConcreteType, context: &str, span: Span) -> Result<(), Error> {
    if matches!(
        actual,
        ConcreteType::Primitive(Primitive::I64) | ConcreteType::Primitive(Primitive::U64)
    ) {
        return Ok(());
    }
    expect_type(
        actual,
        &ConcreteType::Primitive(Primitive::I64),
        context,
        span,
    )
}

/// The type a `let`/`mut` binding takes, given its initializer's compiled type
/// `actual` and the optional `let x: T = ..` annotation. Unannotated, that's
/// just `actual`. Annotated, the annotation wins, and a bare integer literal
/// initializer flexes to it — the same `flex_int_ty` retype a literal gets
/// flowing into a narrow parameter or return type, and correct for the same
/// reason: a literal that fits is already canonical in its i64 register, so
/// only the static type changes. The checker has verified all of this; this is
/// the codegen-side mirror.
fn binding_ty(
    builder: &mut FunctionBuilder,
    value: &Expr,
    v: Value,
    actual: &ConcreteType,
    declared: Option<&ConcreteType>,
    name: &str,
) -> Result<(Value, ConcreteType), Error> {
    let Some(declared) = declared else {
        return Ok((v, actual.clone()));
    };
    let actual = flex_int_ty(value, actual, declared);
    // Widths that genuinely differ: the annotation *converts*, which is what
    // replaced the `u8(..)` form — re-canonicalize the i64-register value to the
    // declared width (wrapping for a narrowing, extending for a widening),
    // exactly as the old conversion builtin did. Skipped when flexing already
    // settled the type, so a literal initializer emits no extra instructions.
    if actual != *declared {
        if let (ConcreteType::Primitive(pa), ConcreteType::Primitive(pd)) = (&actual, declared) {
            if pa.is_int() && pd.is_int() {
                return Ok((canon_int(builder, v, *pd), declared.clone()));
            }
        }
    }
    expect_type(
        &actual,
        declared,
        &format!("binding {name:?}"),
        value.span.clone(),
    )?;
    Ok((v, declared.clone()))
}

/// Reject binding a unit value (the result of a function that returns
/// nothing) to a name. Such a value can't be stored or used; the call
/// belongs in statement position instead (`print(x);`, not
/// `let _ = print(x);`).
fn reject_unit_binding(ty: &ConcreteType, name: &str, span: Span) -> Result<(), Error> {
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
    str_data: &RefCell<StrLiterals>,
    elem_rc: &RefCell<ElemRc>,
    ir_out: &mut String,
    instrument: bool,
    dbg: DebugOptions,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    // This instance's own `funcs` entry — the very one every call site reads, so
    // the caller's skipped retain and the release skipped below can only ever be
    // the same decision.
    let self_info: Option<&FuncInfo> = funcs.get(&func.name);
    let call_params: &[ParamInfo] = self_info.map_or(&[], |info| info.params.as_slice());
    // A tail-call participant's body is defined into its `$tail` declaration;
    // `id` then receives the C-convention trampoline, emitted after this. Only a
    // `tail`-convention body may itself contain a `return_call`.
    let tail_id = self_info.and_then(|info| info.tail_id);
    let body_id = tail_id.unwrap_or(id);
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
    let declared_ret = func.return_type();
    let abi_ret = func.abi_return_type();
    let ret_composite = sret_size(&abi_ret, structs).is_some();
    let mutating = func.is_mutating();
    let unit_main = func.is_unit_main();
    let error_main = func.is_error_main();
    // Synthesized `.test` bodies are named `__test$<fn>` (see `synthesize_test_program`).
    let in_test = func.name.starts_with("__test$");
    build_signature(&mut ctx.func.signature, func, structs, tail_id.is_some());

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
        // A by-value `str` parameter occupies three block params, so the walk
        // cannot zip one-to-one; it reassembles those into a slot and binds its
        // address, which is the handle every other `str` has.
        let mut bound: Vec<Value> = Vec::with_capacity(func.params.len());
        {
            let mut at = 0usize;
            for p in &func.params {
                if tail_passes_str_by_value(tail_id.is_some(), &p.ty) {
                    let n = str24::STR_SIZE / 8;
                    let words = user_params[at..at + n].to_vec();
                    bound.push(str_words_to_value(&mut builder, &words));
                    at += n;
                } else {
                    bound.push(user_params[at]);
                    at += 1;
                }
            }
        }
        for (idx, (p, v)) in func.params.iter().zip(bound.iter()).enumerate() {
            if p.mutable {
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                builder.ins().stack_store(types::I64, *v, slot, 0);
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
                if mut_binding_owns_slot_ref(&p.ty, structs) {
                    emit_retain(&mut builder, module, builtins, structs, *v, &p.ty);
                    scopes[0].push(Tracked::slot(slot, &p.ty));
                }
            } else {
                env.insert(p.name.clone(), EnvBinding::Immut(*v, p.ty.clone()));
                bindings
                    .borrow_mut()
                    .push((p.name.clone(), format!("v{}", v.as_u32())));
            }
            // A retained parameter is owned by the callee and dropped at exit.
            // `ParamInfo::retained` is the *shared* answer — the very entry each
            // call site reads — so the caller's retain and the release here are
            // one decision: a moved-in owned param transfers its reference (so
            // dropping it here would double-free), an inspect-only one is a pure
            // borrow alive by the caller's own reference (releasing it here would
            // free a value the caller still holds), and a `tail_owned` boxed
            // param is retained by the caller precisely so it *can* be released
            // here. Without a `funcs` entry (nothing calls this instance), fall
            // back to the plain heap-parameter rule.
            let retained = match call_params.get(idx) {
                Some(cp) => cp.retained(),
                None => is_heap(&p.ty) && !p.owned,
            };
            if retained {
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
            str_data,
            elem_rc,
            ret_ty: &abi_ret,
            sret: sret_val,
            error_main,
            in_test,
            bindings: &bindings,
            // The body of a `$tail` declaration is in tail position, and its ABI
            // return is its own value (`tail_eligible` ruled out the entry-style
            // functions whose return is derived from the body). Both halves of
            // `can_tail`, in one flag.
            can_tail: tail_id.is_some(),
            tail: tail_id.is_some(),
        };
        let body_mark = scope_depth(&scopes);
        let (body_ret, body_ty) = compile_expr(module, &mut builder, cx, &mut scopes, &func.body)?;
        // Enforce the declared return type. For a mutating method or unit
        // `main` that's unit, so a trailing value (an attempt to return one) is
        // an error. For a normal fn this also guards struct returns: the sret
        // copy below uses the declared layout.
        // A bare-literal body flexes to a narrow-int return type.
        let body_ty = flex_int_ty(&func.body, &body_ty, &declared_ret);
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
                .stack_load(types::I64, types::I64, self_slot.expect("mut self slot"), 0)
        } else if unit_main {
            builder.ins().iconst(types::I64, 0)
        } else if error_main {
            // Derive the exit code from the `!Error` result: ok() → 0; err(msg) →
            // print `error: <msg>` and 1. Read before the scope drop frees it.
            emit_error_main_exit_code(&mut builder, module, builtins, body_ret)
        } else {
            body_ret
        };

        // Hand the caller a ref on the returned value (retaining nested heap for
        // composites), then release this scope. When the body's value is a fresh
        // temporary we own (the common `fn f() -> T { g() }` / `{ T { .. } }`
        // shape), move it instead: skip the retain and untrack it so `drop_scope`
        // below won't free it. A `mut self` method, unit `main`, or `error_main`
        // returns a value other than `body_ret` (its `self` slot / exit code),
        // which is never the innermost scope's top entry, so the move never fires
        // there and `body_ret` is still dropped by `drop_scope`.
        if needs_drop(&abi_ret, structs) && !move_owned_temp(&mut scopes, body_mark, ret_val) {
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
        builder.finalize(module.target_config());
    }

    // Tag the function with its real `FuncId` so the printed IR header reads
    // `function u0:<id>` instead of the default `u0:0`. This makes each function
    // self-identify its id, which the dogfood-IR loader relies on to re-link the
    // checked-in CLIF (see `from_artifact`).
    ctx.func.name = UserFuncName::user(0, body_id.as_u32());
    // Optimize before the dump: the dumped text is what `aipl ir` shows *and*
    // what the dogfood `.clif` artifacts carry, so a pass run after it would
    // never reach the compiler's own re-linked engine.
    run_ir_passes(module, builtins, &mut ctx.func, dbg);
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
        let call_fn = builtins.id(module, "aipl_count_call");
        instrument_call_count(module, &mut ctx.func, body_id, call_fn)?;
    }

    module
        .define_function(body_id, ctx)
        .map_err(|e| Error::msg(format!("define {}: {e:?}", func.name)))?;
    module.clear_context(ctx);
    // A participant's exported symbol is still C-convention: give it the
    // forwarding body now that the real one is defined.
    if tail_id.is_some() {
        define_tail_trampoline(
            module, ctx, fbc, id, body_id, func, structs, ir_out, instrument,
        )?;
    }
    Ok(())
}

/// Define the exported C-convention trampoline for a tail-call participant:
/// forward every parameter to the `$tail` body and hand its result straight
/// back. Nothing else happens here — no refcount work, because the forwarded
/// arguments and result keep exactly the ownership the ABI already gave them.
///
/// It exists because a `tail`-convention function cannot be called from outside
/// the module: not by `Engine::call_values`, not through a `; entry` in a
/// checked-in artifact, and not through a `func_addr` function value (whose
/// signature is built by `fn_value_signature` at the default convention). None
/// of those can know whether the function they name happens to tail-recurse, so
/// the exported symbol keeps the convention they expect and this forwards.
#[allow(clippy::too_many_arguments)]
fn define_tail_trampoline<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    id: FuncId,
    tail_id: FuncId,
    func: &aipl_mono::ConcreteFn,
    structs: &HashMap<String, TypeDef>,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    build_signature(&mut ctx.func.signature, func, structs, false);
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let host_args: Vec<Value> = builder.block_params(entry).to_vec();
        // The trampoline speaks the host shape (one pointer per `str`); the
        // `$tail` body it forwards to wants the three words. An sret pointer, if
        // any, leads and passes straight through.
        let lead = host_args.len() - func.params.len();
        let mut args: Vec<Value> = host_args[..lead].to_vec();
        for (p, a) in func.params.iter().zip(&host_args[lead..]) {
            if tail_passes_str_by_value(true, &p.ty) {
                args.extend(str_value_words(&mut builder, *a));
            } else {
                args.push(*a);
            }
        }
        let callee = module.declare_func_in_func(tail_id, builder.func);
        let inst = builder.ins().call(callee, &args);
        let results: Vec<Value> = builder.inst_results(inst).to_vec();
        builder.ins().return_(&results);
        builder.finalize(module.target_config());
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&format!("{}\n", ctx.func.display()));
    if instrument {
        // The trampoline is a real frame the program executes, so it counts like
        // any other — and separately from the `$tail` body it forwards to, which
        // is how the object's symbols count it too. Both hooks are declared by
        // the body's own instrumentation, so look them up rather than declaring
        // them again.
        let count_fn = declared_import(module, "aipl_count_insns")?;
        instrument_insn_count(module, &mut ctx.func, count_fn);
        let call_fn = declared_import(module, "aipl_count_call")?;
        instrument_call_count(module, &mut ctx.func, id, call_fn)?;
    }
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define {} trampoline: {e:?}", func.name)))?;
    module.clear_context(ctx);
    Ok(())
}

// ---------------------------------------------------------------------------
// IR passes
//
// Small rewrites over a function's finished CLIF. They run after the body is
// built and *before* the IR is dumped, which matters more than it looks: the
// dump is what `aipl ir` prints and what the checked-in dogfood `.clif`
// artifacts are made of, so a pass running after it would optimize every
// compiled program while leaving the compiler's own re-linked engine on
// unoptimized IR.
//
// A pass sees one function at a time, rewrites it in place, and reports how many
// rewrites it made — the count drives the `--debug` trace and tells you at a
// glance whether a pass fires at all on real code. Adding one is: implement
// [`IrPass`], add it to [`IR_PASSES`]. They run in list order, once each; a pass
// that wants a fixpoint should iterate internally rather than rely on the driver
// re-running it, so its cost stays its own.
// ---------------------------------------------------------------------------

/// What a pass needs besides the function: the `FuncId`s of the runtime symbols
/// it recognizes, so a `call` can be matched against them.
///
/// Each is `None` when the symbol was never declared in this module, which means
/// nothing calls it and any pass keyed on it can return immediately. Resolved by
/// *lookup only* — declaring a symbol here would add an unused import to programs
/// that never use it, and so move their `binary size`.
struct PassCx {
    inc: Option<FuncId>,
    dec: Option<FuncId>,
}

/// One rewrite over a finished function. See the section comment above.
trait IrPass {
    /// Stable identifier, used in the `--debug` trace.
    fn name(&self) -> &'static str;

    /// Rewrite `func` in place; return the number of rewrites made (0 = the pass
    /// did not apply).
    fn run(&self, func: &mut Function, cx: &PassCx) -> usize;
}

/// Every pass, in the order they run.
const IR_PASSES: &[&dyn IrPass] = &[&ElideRcPairs];

/// Run [`IR_PASSES`] over `func`.
fn run_ir_passes<M: Module>(
    module: &mut M,
    builtins: &Builtins,
    func: &mut Function,
    dbg: DebugOptions,
) {
    let cx = PassCx {
        inc: builtins.declared(module, "aipl_inc"),
        dec: builtins.declared(module, "aipl_dec"),
    };
    for pass in IR_PASSES {
        let n = pass.run(func, &cx);
        if n > 0 {
            dbg.trace(
                "ir-pass",
                format_args!("{}: {n} rewrite(s) in {}", pass.name(), func.name),
            );
        }
    }
}

/// Whether `inst` calls the module function `want`, and if so its first argument.
/// `None` for any other instruction — including a call to something else, which
/// callers still have to treat as opaque.
fn calls_builtin(func: &Function, inst: Inst, want: Option<FuncId>) -> Option<Value> {
    let want = want?;
    let InstructionData::Call { func_ref, .. } = func.dfg.insts[inst] else {
        return None;
    };
    let ExternalName::User(name_ref) = func.dfg.ext_funcs[func_ref].name else {
        return None;
    };
    let user = &func.params.user_named_funcs()[name_ref];
    // Namespace 0 is `cranelift-module`'s: the index is the `FuncId`.
    (user.namespace == 0 && user.index == want.as_u32())
        .then(|| func.dfg.inst_args(inst).first().copied())
        .flatten()
}

/// Whether `inst` is a call of any kind — the conservative barrier every pass
/// here reasons against, since a call is the only thing that can free memory,
/// re-enter compiled code, or otherwise observe a refcount mid-block.
fn is_any_call(func: &Function, inst: Inst) -> bool {
    matches!(
        func.dfg.insts[inst],
        InstructionData::Call { .. } | InstructionData::CallIndirect { .. }
    )
}

/// `aipl_inc(v)` … `aipl_dec(v)` within one block, with no call between them:
/// both are removed.
///
/// The pair cancels. Nothing between the two can observe the raised count —
/// only a call could free the value, re-enter compiled code, or read the
/// counter, and a call between them is exactly what disqualifies the pair.
/// Loads and stores in between are fine: a store may hand `v` to a new owner,
/// which is the common shape (retain, store into a struct or array, then drop
/// the temporary at scope exit), and eliding both leaves that owner holding the
/// reference the temporary used to hold — the same end state, two runtime calls
/// cheaper.
///
/// Deliberately not matched across blocks. A retain in one block and a release
/// in another is only safe to cancel if every path between them agrees, which is
/// a dataflow question rather than a peephole one — worth its own pass, not a
/// widening of this one.
struct ElideRcPairs;

impl IrPass for ElideRcPairs {
    fn name(&self) -> &'static str {
        "elide-rc-pairs"
    }

    fn run(&self, func: &mut Function, cx: &PassCx) -> usize {
        if cx.inc.is_none() || cx.dec.is_none() {
            return 0;
        }
        let mut doomed: Vec<Inst> = Vec::new();
        let blocks: Vec<Block> = func.layout.blocks().collect();
        for block in blocks {
            // Retains seen since the last call, innermost last.
            let mut pending: Vec<(Value, Inst)> = Vec::new();
            let mut inst = func.layout.first_inst(block);
            while let Some(i) = inst {
                inst = func.layout.next_inst(i);
                if let Some(v) = calls_builtin(func, i, cx.inc) {
                    // A retain only bumps a counter: it can't free or re-enter,
                    // so it doesn't invalidate the retains already pending.
                    pending.push((v, i));
                    continue;
                }
                if let Some(v) = calls_builtin(func, i, cx.dec) {
                    // Pair with the *nearest* unmatched retain of the same value,
                    // so `inc(v); inc(v); dec(v)` cancels one and leaves one.
                    if let Some(pos) = pending.iter().rposition(|(pv, _)| *pv == v) {
                        doomed.push(pending[pos].1);
                        doomed.push(i);
                    }
                    // A release *is* a call — it can free and cascade — so every
                    // remaining retain stops being pairable across it.
                    pending.clear();
                    continue;
                }
                if is_any_call(func, i) {
                    pending.clear();
                }
            }
        }
        for i in &doomed {
            func.layout.remove_inst(*i);
        }
        doomed.len() / 2
    }
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

/// Instrument `func` to tally how many times it is *entered*: at the head of the
/// entry block, insert `aipl_count_call(<name>)`, where the argument points at a
/// freshly-minted static copy of the function's own object-symbol name. The
/// runtime keys its table on that pointer, so the name travels with the count
/// and the compiler needs no side-channel to say what it compiled.
///
/// Runs *after* [`instrument_insn_count`] at every site, which is what keeps the
/// two measurements independent: the block counts are read before these
/// instructions exist, so turning call counting on doesn't move
/// `instructions executed`.
///
/// The name is the *linkage* name (`__aipl_user_main`, `__to_str_3`,
/// `foo$i64$tail`), not the AIPL-level one — the same key the object's symbols
/// use, so the harness maps both back to display names through one table.
fn instrument_call_count<M: Module>(
    module: &mut M,
    func: &mut Function,
    id: FuncId,
    count_fn: FuncId,
) -> Result<(), Error> {
    let Some(entry) = func.layout.entry_block() else {
        return Ok(()); // unreachable: every defined function has an entry block
    };
    let Some(first) = func.layout.first_inst(entry) else {
        return Ok(()); // unreachable: every block ends in a terminator
    };
    let sym = module
        .declarations()
        .get_function_decl(id)
        .linkage_name(id)
        .into_owned();
    // One name object per `FuncId`, so the pointer the runtime sees identifies
    // the function even when two of them would render the same text.
    let data_id = module
        .declare_data(
            &format!("__aipl_fnname_{}", id.as_u32()),
            Linkage::Local,
            false,
            false,
        )
        .map_err(|e| Error::msg(format!("declare fn name: {e}")))?;
    let mut bytes = sym.into_bytes();
    bytes.push(0);
    let mut desc = DataDescription::new();
    desc.define(bytes.into_boxed_slice());
    module
        .define_data(data_id, &desc)
        .map_err(|e| Error::msg(format!("define fn name: {e}")))?;
    let gv = module.declare_data_in_func(data_id, func);
    let fref = module.declare_func_in_func(count_fn, func);
    let mut pos = FuncCursor::new(func);
    pos.goto_inst(first);
    let name_val = pos.ins().symbol_value(types::I64, gv);
    pos.ins().call(fref, &[name_val]);
    Ok(())
}

/// The `FuncId` a runtime import was declared under, for the instrumentation
/// paths that run without a [`Builtins`] in hand (a trampoline, a synthesized
/// helper). Every such site runs after the import is already declared, so this
/// looks it up rather than declaring a second one.
fn declared_import<M: Module>(module: &M, sym: &str) -> Result<FuncId, Error> {
    module
        .declarations()
        .get_name(sym)
        .and_then(|d| match d {
            cranelift_module::FuncOrDataId::Func(f) => Some(f),
            cranelift_module::FuncOrDataId::Data(_) => None,
        })
        .ok_or_else(|| Error::msg(format!("instrumented build without `{sym}`")))
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
        // `iadd_imm` no longer lowers to a single `BinaryImm64`/`IaddImm`
        // instruction: it materializes the immediate as an `iconst` and emits a
        // plain `iadd`. So `symbol_value + STR_HEADER_SIZE` now appears as
        // `iadd(symbol_value, iconst STR_HEADER_SIZE)`.
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => {
            let is_symbol = |val: Value| {
                matches!(
                    func.dfg.value_def(val),
                    ValueDef::Result(def, _)
                        if func.dfg.insts[def].opcode() == Opcode::SymbolValue
                )
            };
            let is_header_const = |val: Value| {
                matches!(
                    func.dfg.value_def(val),
                    ValueDef::Result(def, _)
                        if matches!(
                            func.dfg.insts[def],
                            InstructionData::UnaryImm {
                                opcode: Opcode::Iconst,
                                imm,
                            } if imm.bits() == STR_HEADER_SIZE as i64
                        )
                )
            };
            let [a, b] = args;
            (is_symbol(a) && is_header_const(b)) || (is_symbol(b) && is_header_const(a))
        }
        _ => false,
    }
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
    // No retain: `char_at` is a pure observer. It reads one byte and stores
    // nothing, and the caller provably holds a reference for the whole call —
    // which is exactly the condition under which the borrow protocol's
    // retain/release pair cancels (see `inspect_only_params` in `aipl-mono`). So
    // both halves are dropped rather than paid and immediately refunded, which
    // also means the two ABIs need no different treatment here.
    let raw = builtins.call(module, builder, "aipl_char_at", &[s_v, i_v]);
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
    let is_some_b = builder.ins().icmp_imm_s(IntCC::NotEqual, raw, -1);
    let tag = builder.ins().uextend(types::I64, is_some_b);
    builder.ins().stack_store(types::I64, tag, slot, 0);
    builder.ins().stack_store(types::I64, raw, slot, 8);
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
    ty: ConcreteType,
}

impl Tracked {
    fn new(val: Value, ty: &ConcreteType) -> Self {
        Tracked {
            owned: Owned::Value(val),
            ty: ty.clone(),
        }
    }
    fn slot(slot: StackSlot, ty: &ConcreteType) -> Self {
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
            Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
        };
        emit_drop(builder, module, builtins, structs, v, &t.ty);
    }
}

/// Snapshot of the innermost scope's tracking depth, taken *before* evaluating a
/// subexpression, so a later `owned_temp_since` can tell whether that evaluation
/// produced a fresh temporary we exclusively own.
fn scope_depth(scopes: &[Vec<Tracked>]) -> usize {
    scopes.last().map_or(0, |s| s.len())
}

/// The shared recognizer behind every retain-elision site. True when `v` is a
/// fresh temporary we exclusively own: evaluating the subexpression that produced
/// it grew the innermost scope past `mark` and left `v` itself as the last-tracked
/// entry (a call result, constructor, `if`/`match` merge, retained element read,
/// …). A borrowed place — a bare variable, or a component read that tracked
/// nothing new — yields false. When true, a value about to be handed to a new
/// owner can be *moved* (its existing ref transfers) instead of being retained now
/// and dropped later; those two ops cancel, so eliding them is a pure win.
fn owned_temp_since(scopes: &[Vec<Tracked>], mark: usize, v: Value) -> bool {
    scopes.last().is_some_and(|s| {
        s.len() > mark
            && matches!(s.last(), Some(Tracked { owned: Owned::Value(x), .. }) if *x == v)
    })
}

/// If `v` is a fresh owned temporary produced since `mark` (see
/// `owned_temp_since`), consume its innermost-scope tracking entry and return
/// true: the caller is moving `v`'s reference into a new owner, so it must skip
/// the co-owning retain and scope exit will not drop it again. Returns false for
/// a borrowed `v`, which the caller must retain to co-own as before. Use this at
/// a single-value hand-off (a `return`, a `?` unwrap); the array-literal store,
/// which evaluates several elements before moving them, instead captures
/// `owned_temp_since` per element and batch-removes the moved entries.
fn move_owned_temp(scopes: &mut [Vec<Tracked>], mark: usize, v: Value) -> bool {
    if owned_temp_since(scopes, mark, v) {
        scopes.last_mut().expect("scope").pop();
        true
    } else {
        false
    }
}

/// Hand a refcounted call argument `v` (of type `ty`) to a callee. When `moved`
/// — the arg is a fresh temporary we exclusively own, or a param the callee takes
/// ownership of — move it: drop its tracking entry so the callee's own accounting
/// frees it exactly once (a borrowed param decs it on return; an owned param
/// drops the local it's moved into). Otherwise retain, so the caller keeps its
/// ref and the callee's return-dec balances the inc. Unlike `move_owned_temp`,
/// the arg's tracking entry need not be on top (later args are evaluated before
/// the hand-off), so it is located by value; a fresh temp that somehow isn't
/// tracked is retained to stay balanced.
///
/// The retain goes through [`emit_retain`] rather than the bare `aipl_inc`,
/// because `ty` is no longer always [`is_heap`]: a tail-call participant's boxed
/// parameters are retained here too (see `ParamInfo::tail_owned`), and a boxed
/// value counts on its own block header via `aipl_rec_inc_strong` — `aipl_inc`
/// dispatches on `str` tag bits and would read the wrong word.
#[allow(clippy::too_many_arguments)]
fn hand_off_arg<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    scopes: &mut [Vec<Tracked>],
    v: Value,
    ty: &ConcreteType,
    moved: bool,
) {
    if moved {
        let scope = scopes.last_mut().expect("scope");
        match scope
            .iter()
            .rposition(|t| matches!(t.owned, Owned::Value(tv) if tv == v))
        {
            Some(pos) => {
                scope.remove(pos);
            }
            None => emit_retain(builder, module, builtins, structs, v, ty),
        }
    } else {
        emit_retain(builder, module, builtins, structs, v, ty);
    }
}

/// Whether a value of type `ty` owns any heap references that must be released
/// when it dies. Strings and arrays are heap; a struct/optional needs a drop
/// iff a component does.
fn needs_drop(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    match ty {
        // `str` (and `Error`, which shares its heap representation) is dropped
        // like a heap pointer; the other primitives own no heap.
        _ if is_str_repr(ty) => true,
        // An untyped `[]`/`none` element/core carries nothing to drop — it's
        // never actually a live value (an empty array holds no elements; a
        // bare `none` optional's payload is garbage), so this reaches codegen
        // (e.g. picking an empty array literal's element drop-fn) but is
        // always vacuously drop-free.
        ConcreteType::Primitive(_) | ConcreteType::Unit | ConcreteType::NoneInner => false,
        // Already handled by the `is_str_repr` guard above.
        ConcreteType::ConcatStr => unreachable!(),
        ConcreteType::Named(n) => match structs.get(n) {
            // A boxed (recursive) value owns its heap block, so it always
            // drops — and answering without recursing into the fields is what
            // keeps this terminating on a recursive type.
            Some(d) if d.boxed() => true,
            Some(TypeDef::Struct(s)) => s.fields.iter().any(|f| needs_drop(&f.ty, structs)),
            // A variant needs cleanup if any case's payload field does.
            Some(TypeDef::Variant(v)) => v
                .cases
                .iter()
                .any(|c| c.fields.iter().any(|f| needs_drop(&f.ty, structs))),
            None => false,
        },
        ConcreteType::Array(_) | ConcreteType::Set(_) | ConcreteType::Dict(_, _) => true,
        ConcreteType::Optional(inner) => needs_drop(inner, structs),
        // A result needs cleanup if either payload does (only the active one is
        // released, dispatched on the tag — see `emit_rc`).
        ConcreteType::Result(ok, err) => needs_drop(ok, structs) || needs_drop(err, structs),
        // Function types are erased by monomorphization; never a runtime value.
        ConcreteType::Fn(_, _) => false,
        // Tuple type annotations are lowered to Named by lower_tuples before codegen.
        // Generic applications are lowered to Named by lower_generics before codegen.
        // `Any`/`EmptyArrayArg`/`NoneLiteralArg` are resolved away by
        // monomorphization (the latter two collapse to `Array`/`Optional` of
        // `NoneInner` — see `subst_vars`) — codegen never sees them directly.
        ConcreteType::EmptyArrayArg | ConcreteType::NoneLiteralArg => {
            unreachable!("compiler pseudo-type reached codegen")
        }
    }
}

/// A composite is stored *inline* and handled by the address of its storage
/// (struct, optional); scalars / `str` / arrays are 8-byte values. A *boxed*
/// (recursive) struct/variant is not a composite in this sense: its value is
/// an 8-byte heap pointer, stored and copied like an array's — though since
/// that pointer addresses the type's usual payload layout, every layout read
/// (fields, tag, equality, rendering) is shared with the inline path.
fn is_composite(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    abi_is_composite(Abi::active(), ty, structs)
}

/// Whether `ty` is a *boxed* (recursive) declared type — its values are 8-byte
/// pointers to a refcounted heap payload. See the "Recursive (boxed) type
/// runtime" section.
fn is_boxed(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    matches!(ty, ConcreteType::Named(n) if structs.get(n).is_some_and(TypeDef::boxed))
}

/// Whether `ty` *contains* (directly or through optional/result layers, which
/// store their core inline) a reference to a boxed type of recursion group
/// `scc` — i.e. whether storing a value of `ty` into a boxed value of that
/// group creates an internal (weak-counted) reference.
fn contains_scc_ref(ty: &ConcreteType, scc: u32, structs: &HashMap<String, TypeDef>) -> bool {
    let mut refs = Vec::new();
    contained_named_types(ty, &mut refs);
    refs.iter()
        .any(|n| structs.get(*n).is_some_and(|d| d.boxed() && d.scc() == scc))
}

/// Read the component of type `ty` at `base + offset`: an inline composite is
/// addressed (`base + offset`); a scalar/str/array is loaded as an i64.
fn component(
    builder: &mut FunctionBuilder,
    base: Value,
    offset: u32,
    ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) -> Value {
    if is_composite(ty, structs) {
        builder.ins().iadd_imm_s(base, offset as i64)
    } else {
        builder
            .ins()
            .load(types::I64, MemFlagsData::trusted(), base, offset as i32)
    }
}

/// Store a value `v` of static type `src_ty` into the slot at address `slot`. A
/// composite (an optional) is addressed by `v`, so copy its bytes; a scalar or
/// pointer is a single 8-byte value. Mirrors how `component` reads it back.
fn store_array_elem(
    builder: &mut FunctionBuilder,
    slot: Value,
    v: Value,
    src_ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) {
    if is_composite(src_ty, structs) {
        copy_composite(builder, slot, v, src_ty, structs);
    } else {
        builder.ins().store(MemFlagsData::trusted(), v, slot, 0);
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
    x_ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) {
    let core = opt_core(x_ty);
    let (tag, core_val) = match x_ty {
        // Wrapping an optional: bump its tag, reuse its core value (at offset 8).
        ConcreteType::Optional(_) => {
            let inner_tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), x_val, 0);
            let tag = builder.ins().iadd_imm_s(inner_tag, 1);
            let cv = component(builder, x_val, OPT_VALUE_OFFSET, core, structs);
            (tag, cv)
        }
        // Wrapping a non-optional: it is the core, tag 1.
        _ => (builder.ins().iconst(types::I64, 1), x_val),
    };
    builder.ins().store(MemFlagsData::trusted(), tag, slot, 0);
    let val_addr = builder.ins().iadd_imm_s(slot, OPT_VALUE_OFFSET as i64);
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
fn is_bit_packed(elem: &ConcreteType) -> bool {
    matches!(elem, ConcreteType::Primitive(Primitive::Bool))
}

/// Whether `ty` is `char[]` — the one array element type that shares `str`'s
/// runtime representation entirely (tag scheme, heap layout, refcounting)
/// rather than the generic array block, since a `char` is a single byte and
/// `str`'s content is just packed bytes (see the "Representation dispatch"
/// doc above `StrRepr`). `char[]` stays a distinct compile-time `ConcreteType` (it
/// still displays and type-checks as `char[]`, not `str`) — only its runtime
/// construction/access/refcounting is redirected to the `str` runtime, at
/// every site that would otherwise use the generic array runtime.
fn is_char_array(ty: &ConcreteType) -> bool {
    matches!(ty, ConcreteType::Array(elem) if **elem == ConcreteType::Primitive(Primitive::Char))
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
/// Non-`char[]` arrays and boxed (recursive) declared types: both are plain
/// refcounted pointers, and both can be replaced from a nested scope
/// (`set acc = Cons(x, acc)` in a loop body), where the value-track model alone
/// would free the new value at the inner scope's exit and leave the slot
/// dangling. `char[]` is str-shaped (different rc entry points and an inline
/// representation that isn't a pointer), `str` has its own established slot
/// model, and sets/dicts keep the value-track model until a case demands
/// otherwise.
fn mut_binding_owns_slot_ref(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    (matches!(ty, ConcreteType::Array(_)) && !is_char_array(ty)) || is_boxed(ty, structs)
}

/// Whether `ty`'s runtime value is str-shaped: a real `str`/`Error`/concat-str
/// (`is_str_repr`), or `char[]` (`is_char_array`). Deliberately kept separate
/// from `is_str_repr` itself (rather than folding `char[]` into that shared
/// helper) so this only widens the specific array-construction/access/
/// refcounting dispatch sites that need it — not every one of `is_str_repr`'s
/// many other consumers (FFI marshaling, monomorphization's generic-instance
/// naming, the concat-str pseudo-type, etc.), which weren't audited for a
/// `char[]` value flowing through them.
fn is_str_shaped(ty: &ConcreteType) -> bool {
    is_str_repr(ty) || is_char_array(ty)
}

/// A bare `[]` literal has no element type to infer from, so it's built as the
/// generic (array-shaped) empty collection — `ConcreteType::Array(NoneInner)` — same
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
    scopes: &mut [Vec<Tracked>],
    v: Value,
    actual: &ConcreteType,
    expected: &ConcreteType,
) -> Value {
    let is_empty_placeholder = matches!(actual, ConcreteType::Array(inner) if is_none_inner(inner));
    if is_char_array(expected) && is_empty_placeholder {
        builtins.call_void(module, builder, "aipl_array_dec", &[v]);
        // The empty `[]` was a freshly allocated, tracked temporary; we've just
        // consumed (dec'd) it in favor of the inline empty-char sentinel, so drop
        // its tracking entry — otherwise scope exit decs it a second time, a
        // double-free once its now-freed block is reused.
        if let Some(scope) = scopes.last_mut() {
            if let Some(pos) = scope
                .iter()
                .rposition(|t| matches!(t.owned, Owned::Value(x) if x == v))
            {
                scope.remove(pos);
            }
        }
        builder.ins().iconst(types::I64, 1) // pack_inline(&[]): tag (0 << 2) | 1
    } else {
        v
    }
}

/// Advance a `str` cursor and return its next byte as `0..=255`, or `-1` at the
/// end — the inlined form of `aipl_str_iter_next`, which it still calls for the
/// case that needs real work.
///
/// This is `load_array_elem`'s pattern one level down: a fast path in IR, an
/// extern call only when the fast path doesn't apply. It matters more here,
/// because the call was *per byte* — every `for (let c : s)` paid a full call
/// plus its loads and branches for each character, and walking source text a
/// character at a time is what the dogfooded lexer does for a living.
///
/// The fast path is exactly the runtime's: still inside the string, and still
/// inside the cached leaf, so the byte is a load at `leaf_ptr + (pos -
/// leaf_start)` and the only write is the bumped position. Everything else —
/// descending a rope to the leaf containing `pos`, and caching it — stays in the
/// runtime, reached by the same call as before.
///
/// The leaf test is one unsigned compare rather than two signed ones: `pos <
/// leaf_start` wraps `rel` negative, which as a `u64` is enormous and so fails
/// `rel < leaf_len` too. A freshly-initialized cursor has `leaf_len == 0`, so it
/// fails that test for any position and takes the call — which is what fills the
/// cache in the first place, and why the null `leaf_ptr` is never loaded from.
fn emit_str_iter_next<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    cur_addr: Value,
) -> Value {
    // The fast path below reads the *tagged* cursor's field layout (`ITER_*`).
    // The wide cursor is a different struct — two whole `Str` values plus two
    // positions — so this is not a matter of substituting offsets: a wide leaf
    // may be inline, in which case there is no `leaf_ptr` to load and the byte
    // lives in the cursor's own words. Until that version is written, the wide
    // ABI takes the call per byte, exactly as this code did before the fast path
    // existed.
    return builtins.call(module, builder, "aipl_str_iter_next", &[cur_addr]);
}

/// The `elem_size` argument handed to the array runtime: a byte stride, or the
/// sentinel 0 for a bit-packed `bool` array.
fn runtime_elem_size(elem: &ConcreteType, structs: &HashMap<String, TypeDef>) -> i64 {
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
    builder.ins().band_imm_u(arr_ptr, !(ARR_TAG_MASK as i64))
}

/// Load the length of an array (any repr) in Cranelift IR.  Strips the tag
/// before reading, because tagged pointers are not valid addresses.
/// Resolve the receiver of an in-place array mutation (`push`, `extend`) to its
/// stack slot, its live type cell, whether static analysis proved it unaliased,
/// and its current element type. `what` names the method in the diagnostics.
///
/// The receiver has to be a `mut` array *variable* by name: the whole point of
/// the writeback form is storing the grown array back into that binding's slot,
/// and there is no slot to write back to for anything else. Every other call
/// position was already rewritten by mono into this one on a fresh local.
fn mut_array_receiver(
    env: &Env,
    receiver: &Expr,
    what: &str,
) -> Result<(StackSlot, Rc<RefCell<ConcreteType>>, bool, ConcreteType), Error> {
    let ExprKind::Ident(var) = &receiver.kind else {
        return Err(Error::at(
            format!("\"{what}\" must be called on a mutable array variable, e.g. \"xs.{what}(x)\""),
            receiver.span.clone(),
        ));
    };
    let (slot, ty_cell, exclusive) = match env.get(var) {
        Some(EnvBinding::Mut(slot, cell, excl)) => (*slot, cell.clone(), *excl),
        Some(EnvBinding::Immut(_, _)) => {
            return Err(Error::at(
                format!("cannot \"{what}\" to immutable binding {var:?}; declare it with \"mut\""),
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
        ConcreteType::Array(inner) => (**inner).clone(),
        other => {
            return Err(Error::at(
                format!("\"{what}\" requires an array, got {}", type_name(other)),
                receiver.span.clone(),
            ));
        }
    };
    Ok((slot, ty_cell, exclusive, elem_ty))
}

fn load_arr_len(builder: &mut FunctionBuilder, arr_ptr: Value) -> Value {
    let u = arr_base(builder, arr_ptr);
    builder.ins().load(
        types::I64,
        MemFlagsData::trusted(),
        u,
        ARR_LEN_OFFSET as i32,
    )
}

fn load_array_elem<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    arr_ptr: Value,
    idx: Value,
    elem: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) -> Value {
    // Inline tag check: fast heap path, slow extern path for non-heap reprs.
    // Values are passed through a stack slot (not block params) so that the
    // two-path merge stays compatible with Cranelift's block-arg API.
    let val_slot = i64_slot(builder);
    let tag = builder.ins().band_imm_u(arr_ptr, ARR_TAG_MASK as i64);
    let is_heap = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, tag, ARR_HEAP_TAG as i64);
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
    let base = builder.ins().iadd_imm_s(untagged, ARR_ELEMS_OFFSET as i64);
    let heap_val = if is_bit_packed(elem) {
        let byte_off = builder.ins().ushr_imm_u(idx, 3);
        let byte_addr = builder.ins().iadd(base, byte_off);
        let byte = builder
            .ins()
            .load(types::I8, MemFlagsData::trusted(), byte_addr, 0);
        let byte = builder.ins().uextend(types::I64, byte);
        let bit_idx = builder.ins().band_imm_u(idx, 7);
        let shifted = builder.ins().ushr(byte, bit_idx);
        builder.ins().band_imm_u(shifted, 1)
    } else {
        let stride = elem_size_of(elem, structs);
        let off = builder.ins().imul_imm_s(idx, stride);
        let addr = builder.ins().iadd(base, off);
        if is_str_shaped(elem) {
            // A **snapshot**, for the reason `lookup_ident` copies a `mut`
            // binding: the address would alias the array's own storage, and the
            // in-place `map`/`filter`/`zip` bodies overwrite the very slot they
            // are reading. The element handle would then name the value just
            // written — so the loop's end-of-iteration release hit the new value
            // instead of the old one and freed it out from under the array.
            //
            // A tagged element is loaded, which was already a snapshot: the word
            // is copied out.
            copy_str_value(builder, addr)
        } else if is_composite(elem, structs) {
            addr
        } else {
            builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), addr, 0)
        }
    };
    builder.ins().stack_store(types::I64, heap_val, val_slot, 0);
    builder.ins().jump(merge, &[]);

    // Slow path: non-heap repr — call runtime dispatch.
    builder.switch_to_block(slow_block);
    builder.seal_block(slow_block);
    let slow_val = if is_bit_packed(elem) {
        builtins.call(module, builder, "aipl_arr_load_bit", &[arr_ptr, idx])
    } else {
        let stride_v = builder
            .ins()
            .iconst(types::I64, elem_size_of(elem, structs));
        let addr = builtins.call(
            module,
            builder,
            "aipl_arr_elem_ptr",
            &[arr_ptr, idx, stride_v],
        );
        if is_composite(elem, structs) {
            addr
        } else {
            builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), addr, 0)
        }
    };
    builder.ins().stack_store(types::I64, slow_val, val_slot, 0);
    builder.ins().jump(merge, &[]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    builder
        .ins()
        .stack_load(types::I64, types::I64, val_slot, 0)
}

/// Byte `idx` of a str-shaped `char[]` (see `is_char_array`), without consuming
/// it — `aipl_char_at` borrows, so there is nothing to balance. Returns the raw
/// byte (`idx` is trusted in-bounds; no optional wrapping) for callers that
/// already know the index is valid, like a fold over every element. Mirrors
/// `emit_char_at`.
fn load_char_array_byte<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    arr_ptr: Value,
    idx: Value,
) -> Value {
    builtins.call(module, builder, "aipl_char_at", &[arr_ptr, idx])
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
    ty: &ConcreteType,
) -> Value {
    if is_char_array(ty) {
        builtins.call(module, builder, "aipl_str_len", &[ptr])
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
    arr_ty: &ConcreteType,
) -> Value {
    if is_char_array(arr_ty) {
        load_char_array_byte(module, builder, builtins, ptr, idx)
    } else if let ConcreteType::Array(elem) = arr_ty {
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
    ty: &ConcreteType,
    op: RcOp,
) {
    emit_rc_w(builder, module, builtins, structs, v, ty, op, None);
}

/// [`emit_rc`] with a *weak context*: when `weak_scc` is `Some(g)`, the value
/// being retained/dropped is (about to be / was) held by a field of a boxed
/// value of recursion group `g`, so a reference to a boxed value of that same
/// group is internal and counts on the `weak` counter instead of `strong`.
/// Only boxed-value construction and the generated per-type drop helpers pass
/// `Some`; everything else goes through `emit_rc`.
#[allow(clippy::too_many_arguments)]
fn emit_rc_w<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    ty: &ConcreteType,
    op: RcOp,
    weak_scc: Option<u32>,
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
            // `active_sym` routes these to their `aipl_*` counterparts when the
            // switch is on, where `v` is an address rather than a tagged pointer.
            let sym = match op {
                RcOp::Retain => "aipl_inc",
                RcOp::Drop => "aipl_dec",
            };
            builtins.call_void(module, builder, sym, &[v]);
        }
        // Other primitives own no heap (and `needs_drop` gated them out above).
        ConcreteType::Primitive(_) | ConcreteType::Unit => {}
        ConcreteType::Array(_) | ConcreteType::Set(_) | ConcreteType::Dict(_, _) => {
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
            builtins.call_void(module, builder, sym, &[v]);
        }
        ConcreteType::Named(n) => match structs.get(n) {
            // A boxed (recursive) value counts as one reference to its heap
            // block — never recursed into here; releasing the payload is the
            // job of the block's stored drop-fn when the counts reach zero.
            // Which counter depends on who holds the reference: a field of a
            // boxed value of the *same* recursion group (`weak_scc` matches)
            // is internal → weak; everything else is external → strong.
            Some(d) if d.boxed() => {
                let internal = weak_scc == Some(d.scc());
                let sym = match (op, internal) {
                    (RcOp::Retain, false) => "aipl_rec_inc_strong",
                    (RcOp::Drop, false) => "aipl_rec_dec_strong",
                    (RcOp::Retain, true) => "aipl_rec_inc_weak",
                    (RcOp::Drop, true) => "aipl_rec_dec_weak",
                };
                builtins.call_void(module, builder, sym, &[v]);
            }
            Some(TypeDef::Struct(_)) => {
                // Recurse over the struct's heap-bearing fields. Clone the field
                // list so we don't borrow `structs` across the recursive calls.
                let fields: Vec<(u32, ConcreteType)> = structs[n]
                    .as_struct()
                    .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                    .unwrap_or_default();
                for (offset, fty) in fields {
                    if needs_drop(&fty, structs) {
                        let fv = component(builder, v, offset, &fty, structs);
                        emit_rc_w(builder, module, builtins, structs, fv, &fty, op, weak_scc);
                    }
                }
            }
            // A variant: dispatch on the runtime tag, then recurse over the
            // active case's heap fields (only that case's payload is live).
            Some(TypeDef::Variant(_)) => {
                emit_variant_rc(builder, module, builtins, structs, v, n, op, weak_scc);
            }
            None => {}
        },
        ConcreteType::Optional(_) => {
            // The flattened slot holds {tag, core}; the core heap is owned only
            // when the whole chain is `some` (tag == depth). `some^k(none)` for
            // k < depth carries no heap, so its garbage value is never touched.
            let depth = opt_depth(ty) as i64;
            let core = opt_core(ty);
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, 0);
            let full = builder.ins().icmp_imm_s(IntCC::Equal, tag, depth);
            let then_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(full, then_b, &[], merge, &[]);
            builder.switch_to_block(then_b);
            builder.seal_block(then_b);
            let core_v = component(builder, v, OPT_VALUE_OFFSET, core, structs);
            emit_rc_w(
                builder, module, builtins, structs, core_v, core, op, weak_scc,
            );
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
        }
        ConcreteType::Result(ok_ty, err_ty) => {
            // tag 1 = Ok, 0 = Err; the 8-byte value at `OPT_VALUE_OFFSET` holds
            // the active payload. Release/retain whichever side is live (and only
            // when that side carries heap).
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, 0);
            let rc_side =
                |b: &mut FunctionBuilder, m: &mut M, want_tag: i64, side: &ConcreteType| {
                    if !needs_drop(side, structs) {
                        return;
                    }
                    let is_side = b.ins().icmp_imm_s(IntCC::Equal, tag, want_tag);
                    let then_b = b.create_block();
                    let merge = b.create_block();
                    b.ins().brif(is_side, then_b, &[], merge, &[]);
                    b.switch_to_block(then_b);
                    b.seal_block(then_b);
                    let sv = component(b, v, OPT_VALUE_OFFSET, side, structs);
                    emit_rc_w(b, m, builtins, structs, sv, side, op, weak_scc);
                    b.ins().jump(merge, &[]);
                    b.switch_to_block(merge);
                    b.seal_block(merge);
                };
            rc_side(builder, module, 1, ok_ty);
            rc_side(builder, module, 0, err_ty);
        }
        // Unreachable: `needs_drop` returns false for function types (erased by
        // monomorphization), so this arm is guarded out above.
        ConcreteType::Fn(_, _) => {}
        // Tuple type annotations are lowered to Named by lower_tuples before codegen.
        // Already handled by the `is_str_repr` guard above.
        ConcreteType::ConcatStr => unreachable!(),
        // `needs_drop` panics on these (resolved away by monomorphization), so
        // the guard above already returned.
        ConcreteType::NoneInner | ConcreteType::EmptyArrayArg | ConcreteType::NoneLiteralArg => {
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
    weak_scc: Option<u32>,
) {
    // Clone (tag, heap fields) per case so we don't borrow `structs` across the
    // recursive `emit_rc` calls. Skip cases with no heap payload.
    let cases: Vec<(usize, Vec<(u32, ConcreteType)>)> = structs[name]
        .as_variant()
        .map(|vl| {
            vl.cases
                .iter()
                .enumerate()
                .filter_map(|(tag, c)| {
                    let hf: Vec<(u32, ConcreteType)> = c
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
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), v, 0);
    let done = builder.create_block();
    for (k, fields) in cases {
        let case_b = builder.create_block();
        let next_b = builder.create_block();
        let is_k = builder.ins().icmp_imm_s(IntCC::Equal, tag, k as i64);
        builder.ins().brif(is_k, case_b, &[], next_b, &[]);
        builder.switch_to_block(case_b);
        builder.seal_block(case_b);
        for (offset, fty) in fields {
            let fv = component(builder, v, offset, &fty, structs);
            emit_rc_w(builder, module, builtins, structs, fv, &fty, op, weak_scc);
        }
        builder.ins().jump(done, &[]);
        builder.switch_to_block(next_b);
        builder.seal_block(next_b);
    }
    builder.ins().jump(done, &[]);
    builder.switch_to_block(done);
    builder.seal_block(done);
}

/// Drop the *contents* of a boxed (recursive) value's payload at `payload_ptr`
/// — the struct's fields, or a variant's active case's fields — without
/// touching the box itself (the runtime is already freeing it). Same-group
/// boxed references are weak-dec'd (the internal-reference discipline);
/// everything else drops normally. This is the body of a boxed type's
/// `__rec_drop_<n>` helper.
fn emit_boxed_payload_drop<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    payload_ptr: Value,
    name: &str,
) {
    let scc = Some(structs[name].scc());
    match &structs[name] {
        TypeDef::Struct(_) => {
            let fields: Vec<(u32, ConcreteType)> = structs[name]
                .as_struct()
                .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                .unwrap_or_default();
            for (offset, fty) in fields {
                if needs_drop(&fty, structs) {
                    let fv = component(builder, payload_ptr, offset, &fty, structs);
                    emit_rc_w(
                        builder,
                        module,
                        builtins,
                        structs,
                        fv,
                        &fty,
                        RcOp::Drop,
                        scc,
                    );
                }
            }
        }
        TypeDef::Variant(_) => {
            emit_variant_rc(
                builder,
                module,
                builtins,
                structs,
                payload_ptr,
                name,
                RcOp::Drop,
                scc,
            );
        }
    }
}

/// Declare (once, cached) the `__rec_drop_<n>(payload_ptr)` helper for boxed
/// type `name`; the body is defined later from `rec_drop_pending`.
fn rec_drop_func<M: Module>(module: &mut M, elem_rc: &RefCell<ElemRc>, name: &str) -> FuncId {
    let mut er = elem_rc.borrow_mut();
    if let Some(id) = er.rec_drop_fns.get(name) {
        return *id;
    }
    let sym = er.symbol("__rec_drop_", name);
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // payload ptr
    let id = module
        .declare_function(&sym, Linkage::Local, &sig)
        .expect("declare rec-drop helper");
    er.rec_drop_fns.insert(name.to_string(), id);
    er.rec_drop_pending.push((name.to_string(), id));
    id
}

/// The drop-fn address (as an i64 value) to bake into a boxed value's block
/// header, declaring the per-type helper on first use.
fn rec_drop_fn_addr<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    elem_rc: &RefCell<ElemRc>,
    name: &str,
) -> Value {
    let id = rec_drop_func(module, elem_rc, name);
    let fref = module.declare_func_in_func(id, builder.func);
    builder.ins().func_addr(types::I64, fref)
}

/// Define a generated `__rec_drop_<n>(payload_ptr)` helper: drop the boxed
/// type's payload contents (see [`emit_boxed_payload_drop`]) and return.
#[allow(clippy::too_many_arguments)]
fn define_rec_drop_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    id: FuncId,
    name: &str,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // payload ptr
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let payload = builder.block_params(entry)[0];
        emit_boxed_payload_drop(&mut builder, module, builtins, structs, payload, name);
        builder.ins().return_(&[]);
        builder.finalize(module.target_config());
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    // These helpers carry no `instructions executed` instrumentation (their work
    // is refcount traffic, already visible in the builtin counts), but they are
    // real functions with real code in the object, so they are counted like any
    // other in the per-function breakdown.
    if instrument {
        let call_fn = builtins.id(module, "aipl_count_call");
        instrument_call_count(module, &mut ctx.func, id, call_fn)?;
    }
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define rec drop fn: {e}")))?;
    module.clear_context(ctx);
    Ok(())
}

/// A fresh 8-byte, 8-aligned stack slot — used by `emit_eq` to carry a running
/// 0/1 result (and a loop index) across the blocks its composite branches make.
fn i64_slot(builder: &mut FunctionBuilder) -> StackSlot {
    builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3))
}

/// A stack slot holding one AIPL value of type `ty`, **by value** — sized from
/// [`elem_size_of`] rather than assumed to be a machine word.
///
/// Use this wherever a value is spilled so its address can be handed to
/// something that reads it (the container runtime takes elements and keys
/// through pointers), and [`i64_slot`] only for machine words: loop counters,
/// tags, accumulated results, and the *addresses* composites travel as.
///
/// The distinction is invisible today, since every non-composite value is one
/// i64 — and it is exactly what a 24-byte `str` (`STR_REPR.md`) needs, because
/// then "one value" and "one word" stop being the same thing.
fn value_slot(
    builder: &mut FunctionBuilder,
    ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) -> StackSlot {
    let size = elem_size_of(ty, structs).max(8) as u32;
    builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 3))
}

/// Emit `self.starts_with(other)` / `self.starts_with_at(other, at)` /
/// `self.ends_with(other)` for two arrays of element type `elem`, returning an
/// `i64` 0/1. True iff `other`'s elements equal a contiguous run of `self`'s at
/// the offset `end` selects — 0, `at`, or the aligned tail — with `other` no
/// longer than what remains from there. Borrows both arrays (`emit_eq` balances
/// its own per-element refs, like the array branch of `==`).
fn emit_arr_starts_ends<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    self_ptr: Value,
    other_ptr: Value,
    elem: &ConcreteType,
    end: SeEnd,
    at: Option<Value>,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let la = load_arr_len(builder, self_ptr);
    let lb = load_arr_len(builder, other_ptr);
    // Where a `starts_with_at` comparison begins: `at` clamped to `[0, la]`
    // exactly as a slice bound is, so an offset past the end leaves nothing to
    // match but the empty pattern.
    let lo = at.map(|at| {
        let z = builder.ins().iconst(types::I64, 0);
        let lo = builder.ins().smax(at, z);
        builder.ins().smin(lo, la)
    });
    // A pattern longer than what remains from the offset can't match there.
    // `starts_with`/`ends_with` start from 0, so all of the source remains.
    let avail = match lo {
        Some(lo) => builder.ins().isub(la, lo),
        None => la,
    };
    let fits = builder.ins().icmp(IntCC::SignedLessThanOrEqual, lb, avail);
    // Both arrays are the untyped empty literal (`[].starts_with([])`): there's
    // no element type to compare, so length-fits is the whole answer (and `lb`
    // is 0, so it's `true`). Skip the element loop — `emit_eq` can't lower a
    // `__none__` element.
    if is_none_inner(elem) {
        return Ok(builder.ins().uextend(types::I64, fits));
    }
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(types::I64, zero, res, 0);
    let pre = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(fits, pre, &[], merge, &[]);

    builder.switch_to_block(pre);
    builder.seal_block(pre);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(types::I64, one, res, 0); // optimistic: all matched
                                                        // `ends_with` compares `self[la - lb + i]` to `other[i]`; `starts_with`
                                                        // uses offset 0, and `starts_with_at` the clamped `at`.
    let offset = match end {
        SeEnd::Starts => zero,
        SeEnd::At => lo.expect("a `starts_with_at` call supplies its offset"),
        SeEnd::Ends => builder.ins().isub(la, lb),
    };
    let idx = i64_slot(builder);
    builder.ins().stack_store(types::I64, zero, idx, 0);
    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, lb);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    let si = builder.ins().iadd(offset, i);
    let el = load_array_elem(module, builder, builtins, self_ptr, si, elem, structs);
    let er = load_array_elem(module, builder, builtins, other_ptr, i, elem, structs);
    let ee = emit_eq(module, builder, cx, el, er, elem)?;
    let cont = builder.create_block();
    let neq = builder.create_block();
    builder.ins().brif(ee, cont, &[], neq, &[]);
    builder.switch_to_block(neq);
    builder.seal_block(neq);
    builder.ins().stack_store(types::I64, zero, res, 0);
    builder.ins().jump(exit, &[]);
    builder.switch_to_block(cont);
    builder.seal_block(cont);
    let next = builder.ins().iadd_imm_s(i, 1);
    builder.ins().stack_store(types::I64, next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, types::I64, res, 0))
}

/// Whether `ty`'s structural equality is emitted through a synthesized per-type
/// `__eq_<n>` helper (composites) rather than inline (scalars and `str`-shaped
/// values — `str`/`Error`/`char[]` — which are a single `icmp`/`aipl_str_eq`).
fn uses_eq_helper(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    if is_str_shaped(ty) {
        return false;
    }
    match ty {
        ConcreteType::Optional(_)
        | ConcreteType::Array(_)
        | ConcreteType::Set(_)
        | ConcreteType::Dict(_, _)
        | ConcreteType::Result(_, _) => true,
        ConcreteType::Named(n) => structs
            .get(n)
            .is_some_and(|d| d.as_struct().is_some() || d.as_variant().is_some()),
        _ => false,
    }
}

/// Emit a structural equality test for two values of type `ty`, returning an
/// `i64` 0/1. A composite type calls its shared per-type `__eq_<n>(lv, rv)`
/// helper (generated once — see [`define_eq_fn`]) instead of inlining the whole
/// comparison at every `==`/`!=` and nested site; a scalar or `str`-shaped value
/// stays inline via [`emit_eq_body`]. Both operands are borrowed.
fn emit_eq<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    lv: Value,
    rv: Value,
    ty: &ConcreteType,
) -> Result<Value, Error> {
    if uses_eq_helper(ty, cx.structs) {
        let id = eq_func(module, cx, ty);
        let fref = module.declare_func_in_func(id, builder.func);
        let inst = builder.ins().call(fref, &[lv, rv]);
        Ok(builder.inst_results(inst)[0])
    } else {
        emit_eq_body(module, builder, cx, lv, rv, ty)
    }
}

/// Declare (once, cached) the per-type `__eq_<n>(lv, rv) -> i64` helper for `ty`.
/// Returns its id; the body is defined later from `eq_pending`.
fn eq_func<M: Module>(module: &mut M, cx: Cx, ty: &ConcreteType) -> FuncId {
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(id) = er.eq_fns.get(ty) {
        return *id;
    }
    let sym = er.symbol("__eq_", &type_symbol(ty));
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // lv
    sig.params.push(AbiParam::new(types::I64)); // rv
    sig.returns.push(AbiParam::new(types::I64)); // 0/1
    let id = module
        .declare_function(&sym, Linkage::Local, &sig)
        .expect("declare eq helper");
    er.eq_fns.insert(ty.clone(), id);
    er.eq_pending.push((ty.clone(), id));
    id
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
/// Composite recursions route back through [`emit_eq`], so a nested composite
/// calls its own helper rather than being re-inlined here. Both operands are
/// borrowed; `str_eq`'s consumed refs are balanced by the incs, so no scope
/// tracking is needed.
fn emit_eq_body<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    lv: Value,
    rv: Value,
    ty: &ConcreteType,
) -> Result<Value, Error> {
    let builtins = cx.builtins;
    let structs = cx.structs;
    Ok(match ty {
        // All integer widths (and bool/char) compare by their canonical i64
        // register value — distinct values have distinct canonical reps.
        ConcreteType::Primitive(p) if !matches!(p, Primitive::Str) => {
            let b = builder.ins().icmp(IntCC::Equal, lv, rv);
            builder.ins().uextend(types::I64, b)
        }
        // `str` (and `Error`, and `char[]` — see `is_str_shaped`) compares by
        // its byte content.
        _ if is_str_shaped(ty) => {
            // `str_eq` borrows both inputs — it reads bytes and keeps nothing,
            // and the caller holds a reference across the call — so there is no
            // retain/release pair to pay (see `emit_char_at`).
            builtins.call(module, builder, "aipl_str_eq", &[lv, rv])
        }
        ConcreteType::Optional(_) => {
            let depth = opt_depth(ty) as i64;
            let core = opt_core(ty);
            let tl = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), lv, 0);
            let tr = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), rv, 0);
            let tags_eq = builder.ins().icmp(IntCC::Equal, tl, tr);
            let res = i64_slot(builder);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(types::I64, zero, res, 0);
            let then_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(tags_eq, then_b, &[], merge, &[]);
            builder.switch_to_block(then_b);
            builder.seal_block(then_b);
            // Tags equal. The cores matter only when the chain is fully `some`
            // (tag == depth); a `none` at some layer makes tag equality decisive.
            let is_full = builder.ins().icmp_imm_s(IntCC::Equal, tl, depth);
            let core_b = builder.create_block();
            let one_b = builder.create_block();
            builder.ins().brif(is_full, core_b, &[], one_b, &[]);
            builder.switch_to_block(one_b);
            builder.seal_block(one_b);
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(types::I64, one, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(core_b);
            builder.seal_block(core_b);
            let cl = component(builder, lv, OPT_VALUE_OFFSET, core, structs);
            let cr = component(builder, rv, OPT_VALUE_OFFSET, core, structs);
            let ce = emit_eq(module, builder, cx, cl, cr, core)?;
            builder.ins().stack_store(types::I64, ce, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, types::I64, res, 0)
        }
        ConcreteType::Array(elem) => {
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
                builder.ins().stack_store(types::I64, zero, res, 0);
                let pre = builder.create_block();
                let merge = builder.create_block();
                builder.ins().brif(len_eq, pre, &[], merge, &[]);
                builder.switch_to_block(pre);
                builder.seal_block(pre);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().stack_store(types::I64, one, res, 0); // optimistic: all-equal
                let idx = i64_slot(builder);
                builder.ins().stack_store(types::I64, zero, idx, 0);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, ll);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                let el = load_array_elem(module, builder, builtins, lv, i, elem, structs);
                let er = load_array_elem(module, builder, builtins, rv, i, elem, structs);
                let ee = emit_eq(module, builder, cx, el, er, elem)?;
                let cont = builder.create_block();
                let neq = builder.create_block();
                builder.ins().brif(ee, cont, &[], neq, &[]);
                builder.switch_to_block(neq);
                builder.seal_block(neq);
                builder.ins().stack_store(types::I64, zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cont);
                builder.seal_block(cont);
                let next = builder.ins().iadd_imm_s(i, 1);
                builder.ins().stack_store(types::I64, next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(merge);
                builder.seal_block(merge);
                builder.ins().stack_load(types::I64, types::I64, res, 0)
            }
        }
        ConcreteType::Set(elem) => {
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
                builder.ins().stack_store(types::I64, zero, res, 0);
                let pre = builder.create_block();
                let merge = builder.create_block();
                builder.ins().brif(len_eq, pre, &[], merge, &[]);
                builder.switch_to_block(pre);
                builder.seal_block(pre);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().stack_store(types::I64, one, res, 0);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(elem, structs));
                let idx = i64_slot(builder);
                builder.ins().stack_store(types::I64, zero, idx, 0);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, ll);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                let el = load_array_elem(module, builder, builtins, lv, i, elem, structs);
                let xslot = value_slot(builder, elem, structs);
                let xptr = builder.ins().stack_addr(types::I64, xslot, 0);
                store_array_elem(builder, xptr, el, elem, structs);
                let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&elem));
                let c = builtins.call(
                    module,
                    builder,
                    "aipl_set_contains",
                    &[rv, xptr, esz, str_cmp],
                );
                let cont = builder.create_block();
                let missing = builder.create_block();
                builder.ins().brif(c, cont, &[], missing, &[]);
                builder.switch_to_block(missing);
                builder.seal_block(missing);
                builder.ins().stack_store(types::I64, zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cont);
                builder.seal_block(cont);
                let next = builder.ins().iadd_imm_s(i, 1);
                builder.ins().stack_store(types::I64, next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(merge);
                builder.seal_block(merge);
                builder.ins().stack_load(types::I64, types::I64, res, 0)
            }
        }
        ConcreteType::Named(n) if structs.get(n).and_then(TypeDef::as_struct).is_some() => {
            // Clone field (offset, type) pairs so we don't borrow `structs` across
            // the recursive calls. AND-fold every field's equality.
            let fields: Vec<(u32, ConcreteType)> = structs[n]
                .as_struct()
                .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                .unwrap_or_default();
            let res = i64_slot(builder);
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(types::I64, one, res, 0);
            for (offset, fty) in fields {
                let fl = component(builder, lv, offset, &fty, structs);
                let fr = component(builder, rv, offset, &fty, structs);
                let fe = emit_eq(module, builder, cx, fl, fr, &fty)?;
                let cur = builder.ins().stack_load(types::I64, types::I64, res, 0);
                let new = builder.ins().band(cur, fe);
                builder.ins().stack_store(types::I64, new, res, 0);
            }
            builder.ins().stack_load(types::I64, types::I64, res, 0)
        }
        ConcreteType::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            // Clone each case's (tag, payload fields) so we don't borrow `structs`
            // across the recursive calls.
            let cases: Vec<(usize, Vec<(u32, ConcreteType)>)> = structs[n]
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
            let tl = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), lv, 0);
            let tr = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), rv, 0);
            let tags_eq = builder.ins().icmp(IntCC::Equal, tl, tr);
            let res = i64_slot(builder);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(types::I64, zero, res, 0);
            let then_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(tags_eq, then_b, &[], merge, &[]);
            builder.switch_to_block(then_b);
            builder.seal_block(then_b);
            // Same tag ⇒ equal unless the active case carries a payload to compare;
            // a nullary/no-field case is already equal (res stays 1).
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(types::I64, one, res, 0);
            for (k, fields) in cases {
                if fields.is_empty() {
                    continue;
                }
                let case_b = builder.create_block();
                let next_b = builder.create_block();
                let is_k = builder.ins().icmp_imm_s(IntCC::Equal, tl, k as i64);
                builder.ins().brif(is_k, case_b, &[], next_b, &[]);
                builder.switch_to_block(case_b);
                builder.seal_block(case_b);
                for (offset, fty) in fields {
                    let fl = component(builder, lv, offset, &fty, structs);
                    let fr = component(builder, rv, offset, &fty, structs);
                    let fe = emit_eq(module, builder, cx, fl, fr, &fty)?;
                    let cur = builder.ins().stack_load(types::I64, types::I64, res, 0);
                    let new = builder.ins().band(cur, fe);
                    builder.ins().stack_store(types::I64, new, res, 0);
                }
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(next_b);
                builder.seal_block(next_b);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, types::I64, res, 0)
        }
        ConcreteType::Dict(k, v) => {
            // Equal iff same length and every left pair's key is bound in the
            // right to an equal value (distinct keys + equal sizes ⇒ equal maps).
            let ll = load_arr_len(builder, lv);
            let rl = load_arr_len(builder, rv);
            let len_eq = builder.ins().icmp(IntCC::Equal, ll, rl);
            if is_none_inner(k) {
                builder.ins().uextend(types::I64, len_eq)
            } else {
                let pair_size = dict_pair_size(k, v, structs);
                let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(k));
                let psz = builder.ins().iconst(types::I64, pair_size);
                let res = i64_slot(builder);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(types::I64, zero, res, 0);
                let pre = builder.create_block();
                let merge = builder.create_block();
                builder.ins().brif(len_eq, pre, &[], merge, &[]);
                builder.switch_to_block(pre);
                builder.seal_block(pre);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().stack_store(types::I64, one, res, 0); // optimistic
                let idx = i64_slot(builder);
                builder.ins().stack_store(types::I64, zero, idx, 0);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, ll);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                // Address of left pair `i`, its key (offset 0) and value (8).
                let lv_base = arr_base(builder, lv);
                let lelems = builder.ins().iadd_imm_s(lv_base, ARR_ELEMS_OFFSET as i64);
                let off = builder.ins().imul_imm_s(i, pair_size);
                let lpair = builder.ins().iadd(lelems, off);
                let key = component(builder, lpair, 0, k, structs);
                let kslot = value_slot(builder, k, structs);
                let kptr = builder.ins().stack_addr(types::I64, kslot, 0);
                store_array_elem(builder, kptr, key, k, structs);
                let rslot =
                    builtins.call(module, builder, "aipl_dict_get", &[rv, kptr, psz, str_cmp]);
                let found = builder.ins().icmp_imm_s(IntCC::NotEqual, rslot, 0);
                let cmp_b = builder.create_block();
                let missing = builder.create_block();
                builder.ins().brif(found, cmp_b, &[], missing, &[]);
                builder.switch_to_block(missing);
                builder.seal_block(missing);
                builder.ins().stack_store(types::I64, zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cmp_b);
                builder.seal_block(cmp_b);
                // Right value is at the slot's offset 0; left at the pair's key
                // width, which is 8 for every key but a wide `str`
                // (`dict_key_size` — the same rule the runtime's `dict_get` uses
                // to find the value it returns).
                let lval = component(builder, lpair, dict_key_size(k, structs) as u32, v, structs);
                let rval = component(builder, rslot, 0, v, structs);
                let ve = emit_eq(module, builder, cx, lval, rval, v)?;
                let cont = builder.create_block();
                let neq = builder.create_block();
                builder.ins().brif(ve, cont, &[], neq, &[]);
                builder.switch_to_block(neq);
                builder.seal_block(neq);
                builder.ins().stack_store(types::I64, zero, res, 0);
                builder.ins().jump(exit, &[]);
                builder.switch_to_block(cont);
                builder.seal_block(cont);
                let next = builder.ins().iadd_imm_s(i, 1);
                builder.ins().stack_store(types::I64, next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(merge);
                builder.seal_block(merge);
                builder.ins().stack_load(types::I64, types::I64, res, 0)
            }
        }
        ConcreteType::Result(ok_ty, err_ty) => {
            // Equal iff same tag and the active payload (by that side's type)
            // is equal. tag 1 = Ok, 0 = Err; both payloads live at the value
            // offset.
            let tl = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), lv, 0);
            let tr = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), rv, 0);
            let tags_eq = builder.ins().icmp(IntCC::Equal, tl, tr);
            let res = i64_slot(builder);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(types::I64, zero, res, 0);
            let cmp_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(tags_eq, cmp_b, &[], merge, &[]);
            builder.switch_to_block(cmp_b);
            builder.seal_block(cmp_b);
            // Tags equal: compare the live payload by the matching side's type.
            let is_ok = builder.ins().icmp_imm_s(IntCC::Equal, tl, 1);
            let ok_b = builder.create_block();
            let err_b = builder.create_block();
            builder.ins().brif(is_ok, ok_b, &[], err_b, &[]);
            // A side that is unit (void-Ok `!E`) carries no payload, and a
            // `__none__` side is unconstructible (so its branch is dead) — either
            // way equal tags suffice, so compare the payload only for real types.
            let payload_trivial = |t: &ConcreteType| is_unit(t) || is_none_inner(t);
            builder.switch_to_block(ok_b);
            builder.seal_block(ok_b);
            let e = if payload_trivial(ok_ty) {
                builder.ins().iconst(types::I64, 1)
            } else {
                let lo = component(builder, lv, OPT_VALUE_OFFSET, ok_ty, structs);
                let ro = component(builder, rv, OPT_VALUE_OFFSET, ok_ty, structs);
                emit_eq(module, builder, cx, lo, ro, ok_ty)?
            };
            builder.ins().stack_store(types::I64, e, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(err_b);
            builder.seal_block(err_b);
            let e = if payload_trivial(err_ty) {
                builder.ins().iconst(types::I64, 1)
            } else {
                let le = component(builder, lv, OPT_VALUE_OFFSET, err_ty, structs);
                let re = component(builder, rv, OPT_VALUE_OFFSET, err_ty, structs);
                emit_eq(module, builder, cx, le, re, err_ty)?
            };
            builder.ins().stack_store(types::I64, e, res, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, types::I64, res, 0)
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
        fields: Vec<(u32, ConcreteType)>,
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
) -> Result<Option<(Value, ConcreteType)>, Error> {
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
            let ConcreteType::Result(ok_ty, err_ty) = &ot else {
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
            if !matches!(&ot, ConcreteType::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some())
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
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), ov, 0);
    let tag_matches = builder.ins().icmp_imm_s(IntCC::Equal, tag, expect_tag);
    // A tagless match (void `ok()`, or a nullary variant case) carries nothing
    // to compare — matching tags alone means equal.
    let eq = if fields.is_empty() {
        builder.ins().uextend(types::I64, tag_matches)
    } else {
        let res = i64_slot(builder);
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().stack_store(types::I64, zero, res, 0);
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
            let feq = emit_eq(module, builder, cx, other_field, cv, fty)?;
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
            types::I64,
            fields_eq.expect("fields is non-empty in this branch"),
            res,
            0,
        );
        builder.ins().jump(merge, &[]);
        builder.switch_to_block(merge);
        builder.seal_block(merge);
        builder.ins().stack_load(types::I64, types::I64, res, 0)
    };
    let result = if op == 'N' {
        builder.ins().bxor_imm_u(eq, 1)
    } else {
        eq
    };
    Ok(Some((result, ConcreteType::Primitive(Primitive::Bool))))
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
    let s = builder.ins().ushr_imm_u(x, 30);
    let x = builder.ins().bxor(x, s);
    let x = mul(builder, x, 0xbf58_476d_1ce4_e5b9);
    let s = builder.ins().ushr_imm_u(x, 27);
    let x = builder.ins().bxor(x, s);
    let x = mul(builder, x, 0x94d0_49bb_1331_11eb);
    let s = builder.ins().ushr_imm_u(x, 31);
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
    elem: &ConcreteType,
    seed: Value,
    commutative: bool,
) -> Result<Value, Error> {
    let len = load_arr_len(builder, arr);
    let acc = i64_slot(builder);
    builder.ins().stack_store(types::I64, seed, acc, 0);
    let idx = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(types::I64, zero, idx, 0);
    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);
    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder.ins().brif(more, body, &[], exit, &[]);
    builder.switch_to_block(body);
    builder.seal_block(body);
    let el = load_array_elem(module, builder, builtins, arr, i, elem, structs);
    let h = emit_hash(module, builder, builtins, structs, el, elem)?;
    let cur = builder.ins().stack_load(types::I64, types::I64, acc, 0);
    let new = if commutative {
        builder.ins().iadd(cur, h)
    } else {
        emit_hash_combine(builder, cur, h)
    };
    builder.ins().stack_store(types::I64, new, acc, 0);
    let next = builder.ins().iadd_imm_s(i, 1);
    builder.ins().stack_store(types::I64, next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);
    builder.switch_to_block(exit);
    builder.seal_block(exit);
    Ok(builder.ins().stack_load(types::I64, types::I64, acc, 0))
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
    ty: &ConcreteType,
) -> Result<Value, Error> {
    Ok(match ty {
        ConcreteType::Primitive(p) if !matches!(p, Primitive::Str) => emit_scalar_hash(builder, v),
        // `str` (and `Error`, and `char[]` — see `is_str_shaped`) hashes by
        // its byte content.
        _ if is_str_shaped(ty) => builtins.call(module, builder, "aipl_str_hash", &[v]),
        ConcreteType::Optional(_) => {
            // hash(tag), combined with the core's hash only when fully `some`
            // (tag == depth) — so `none`/`some^k(none)` hash by tag alone.
            let depth = opt_depth(ty) as i64;
            let core = opt_core(ty);
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, 0);
            let base = emit_scalar_hash(builder, tag);
            let acc = i64_slot(builder);
            builder.ins().stack_store(types::I64, base, acc, 0);
            let is_full = builder.ins().icmp_imm_s(IntCC::Equal, tag, depth);
            let core_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(is_full, core_b, &[], merge, &[]);
            builder.switch_to_block(core_b);
            builder.seal_block(core_b);
            let cv = component(builder, v, OPT_VALUE_OFFSET, core, structs);
            let ch = emit_hash(module, builder, builtins, structs, cv, core)?;
            let combined = emit_hash_combine(builder, base, ch);
            builder.ins().stack_store(types::I64, combined, acc, 0);
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, types::I64, acc, 0)
        }
        ConcreteType::Array(elem) => {
            let len = load_arr_len(builder, v);
            let seed = emit_scalar_hash(builder, len);
            if is_none_inner(elem) {
                seed
            } else {
                emit_seq_hash(module, builder, builtins, structs, v, elem, seed, false)?
            }
        }
        ConcreteType::Set(elem) => {
            let len = load_arr_len(builder, v);
            let seed = emit_scalar_hash(builder, len);
            if is_none_inner(elem) {
                seed
            } else {
                emit_seq_hash(module, builder, builtins, structs, v, elem, seed, true)?
            }
        }
        // Hashing inlines the structure, which can't terminate on a recursive
        // type; a per-type hash helper (like `to_str`/`eq`) would be needed.
        // Recursive types aren't valid set/dict keys yet, so reject rather than
        // loop the compiler.
        ConcreteType::Named(_) if is_boxed(ty, structs) => {
            return Err(Error::msg(format!(
                "hash: hashing recursive type {} is not yet supported",
                type_name(ty)
            )));
        }
        ConcreteType::Named(n) if structs.get(n).and_then(TypeDef::as_struct).is_some() => {
            let fields: Vec<(u32, ConcreteType)> = structs[n]
                .as_struct()
                .map(|l| l.fields.iter().map(|f| (f.offset, f.ty.clone())).collect())
                .unwrap_or_default();
            let acc = i64_slot(builder);
            let seed = builder.ins().iconst(types::I64, HASH_SEED);
            builder.ins().stack_store(types::I64, seed, acc, 0);
            for (offset, fty) in fields {
                let fv = component(builder, v, offset, &fty, structs);
                let h = emit_hash(module, builder, builtins, structs, fv, &fty)?;
                let cur = builder.ins().stack_load(types::I64, types::I64, acc, 0);
                let new = emit_hash_combine(builder, cur, h);
                builder.ins().stack_store(types::I64, new, acc, 0);
            }
            builder.ins().stack_load(types::I64, types::I64, acc, 0)
        }
        ConcreteType::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            let cases: Vec<(usize, Vec<(u32, ConcreteType)>)> = structs[n]
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
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, 0);
            let acc = i64_slot(builder);
            let base = emit_scalar_hash(builder, tag);
            builder.ins().stack_store(types::I64, base, acc, 0);
            let merge = builder.create_block();
            for (k, fields) in cases {
                if fields.is_empty() {
                    continue; // tag alone already hashed
                }
                let case_b = builder.create_block();
                let next_b = builder.create_block();
                let is_k = builder.ins().icmp_imm_s(IntCC::Equal, tag, k as i64);
                builder.ins().brif(is_k, case_b, &[], next_b, &[]);
                builder.switch_to_block(case_b);
                builder.seal_block(case_b);
                for (offset, fty) in fields {
                    let fv = component(builder, v, offset, &fty, structs);
                    let h = emit_hash(module, builder, builtins, structs, fv, &fty)?;
                    let cur = builder.ins().stack_load(types::I64, types::I64, acc, 0);
                    let new = emit_hash_combine(builder, cur, h);
                    builder.ins().stack_store(types::I64, new, acc, 0);
                }
                builder.ins().jump(merge, &[]);
                builder.switch_to_block(next_b);
                builder.seal_block(next_b);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, types::I64, acc, 0)
        }
        ConcreteType::Dict(key_ty, val_ty) => {
            // Order-independent over pairs (matching dict `==`): fold each pair's
            // (key, value) combine commutatively. Within a pair the combine is
            // order-sensitive so `{1: 2}` and `{2: 1}` differ.
            let len = load_arr_len(builder, v);
            let seed = emit_scalar_hash(builder, len);
            if is_none_inner(key_ty) {
                seed
            } else {
                let pair_size = dict_pair_size(key_ty, val_ty, structs);
                let acc = i64_slot(builder);
                builder.ins().stack_store(types::I64, seed, acc, 0);
                let idx = i64_slot(builder);
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(types::I64, zero, idx, 0);
                let v_base = arr_base(builder, v);
                let elems = builder.ins().iadd_imm_s(v_base, ARR_ELEMS_OFFSET as i64);
                let header = builder.create_block();
                let body = builder.create_block();
                let exit = builder.create_block();
                builder.ins().jump(header, &[]);
                builder.switch_to_block(header);
                let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
                let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
                builder.ins().brif(more, body, &[], exit, &[]);
                builder.switch_to_block(body);
                builder.seal_block(body);
                let off = builder.ins().imul_imm_s(i, pair_size);
                let pair = builder.ins().iadd(elems, off);
                let kv = component(builder, pair, 0, key_ty, structs);
                let kh = emit_hash(module, builder, builtins, structs, kv, key_ty)?;
                // Value at the key's width — see `dict_key_size`.
                let vv = component(
                    builder,
                    pair,
                    dict_key_size(key_ty, structs) as u32,
                    val_ty,
                    structs,
                );
                let vh = emit_hash(module, builder, builtins, structs, vv, val_ty)?;
                let pseed = builder.ins().iconst(types::I64, HASH_SEED);
                let pk = emit_hash_combine(builder, pseed, kh);
                let ph = emit_hash_combine(builder, pk, vh);
                let cur = builder.ins().stack_load(types::I64, types::I64, acc, 0);
                let new = builder.ins().iadd(cur, ph); // commutative over pairs
                builder.ins().stack_store(types::I64, new, acc, 0);
                let next = builder.ins().iadd_imm_s(i, 1);
                builder.ins().stack_store(types::I64, next, idx, 0);
                builder.ins().jump(header, &[]);
                builder.seal_block(header);
                builder.switch_to_block(exit);
                builder.seal_block(exit);
                builder.ins().stack_load(types::I64, types::I64, acc, 0)
            }
        }
        ConcreteType::Result(ok_ty, err_ty) => {
            // hash(tag) combined with the active payload's hash (by its type).
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, 0);
            let base = emit_scalar_hash(builder, tag);
            let acc = i64_slot(builder);
            builder.ins().stack_store(types::I64, base, acc, 0);
            let is_ok = builder.ins().icmp_imm_s(IntCC::Equal, tag, 1);
            let ok_b = builder.create_block();
            let err_b = builder.create_block();
            let merge = builder.create_block();
            builder.ins().brif(is_ok, ok_b, &[], err_b, &[]);
            // A unit (void-Ok) or `__none__` (unconstructible) side carries no
            // payload — its hash is just the tag's.
            let payload_trivial = |t: &ConcreteType| is_unit(t) || is_none_inner(t);
            builder.switch_to_block(ok_b);
            builder.seal_block(ok_b);
            if !payload_trivial(ok_ty) {
                let okv = component(builder, v, OPT_VALUE_OFFSET, ok_ty, structs);
                let h = emit_hash(module, builder, builtins, structs, okv, ok_ty)?;
                let c = emit_hash_combine(builder, base, h);
                builder.ins().stack_store(types::I64, c, acc, 0);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(err_b);
            builder.seal_block(err_b);
            if !payload_trivial(err_ty) {
                let errv = component(builder, v, OPT_VALUE_OFFSET, err_ty, structs);
                let h = emit_hash(module, builder, builtins, structs, errv, err_ty)?;
                let c = emit_hash_combine(builder, base, h);
                builder.ins().stack_store(types::I64, c, acc, 0);
            }
            builder.ins().jump(merge, &[]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            builder.ins().stack_load(types::I64, types::I64, acc, 0)
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
    ty: &ConcreteType,
) {
    emit_rc(builder, module, builtins, structs, v, ty, RcOp::Drop);
}

fn emit_retain<M: Module>(
    builder: &mut FunctionBuilder,
    module: &mut M,
    builtins: &Builtins,
    structs: &HashMap<String, TypeDef>,
    v: Value,
    ty: &ConcreteType,
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
    ty: &ConcreteType,
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
fn opt_core(ty: &ConcreteType) -> &ConcreteType {
    match ty {
        ConcreteType::Optional(inner) => opt_core(inner),
        _ => ty,
    }
}

/// The number of `Optional` layers wrapping `ty` (0 if not an optional). This is
/// the maximum tag of the flattened representation — the tag equals the depth
/// exactly when the core value is present.
fn opt_depth(ty: &ConcreteType) -> u32 {
    match ty {
        ConcreteType::Optional(inner) => 1 + opt_depth(inner),
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
fn elem_size_of(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> i64 {
    abi_elem_size(Abi::active(), ty, structs)
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
    elem: &ConcreteType,
) -> Value {
    let b = cx.builtins;
    // These are *stored* in the array header and called by the runtime, not
    // emitted as calls, so they never pass through `active_sym` — the wide
    // counterpart has to be named here. It is picked per element type rather than
    // per symbol because `aipl_arr_retain_ptr` below serves `str` *and* array
    // elements, and only the `str` use changes representation.
    let drop_str = "aipl_arr_drop_str";
    let id = match elem {
        // `is_str_repr`, not the exact `Primitive::Str`: `Error` and the
        // internal concat-str share the representation, and under the wide ABI
        // they must use the wide element helper rather than falling through to a
        // generated per-type one that walks 8-byte slots. `map(|s| s +++ x)`
        // produces concat-str elements, which is how this surfaced.
        _ if is_str_repr(elem) => Some(b.id(module, drop_str)),
        ConcreteType::Primitive(Primitive::Str) => Some(b.id(module, drop_str)),
        // `char[]` shares `str`'s representation (see `is_char_array`), so a
        // nested `char[]` element (e.g. in `char[][]`) is freed the same way
        // a `str` element is, not via the generic array-element drop-fn.
        ConcreteType::Array(_) if is_char_array(elem) => Some(b.id(module, drop_str)),
        ConcreteType::Array(_) => Some(b.id(module, "aipl_arr_drop_arr")),
        ConcreteType::Optional(inner)
            if matches!(inner.as_ref(), ConcreteType::Primitive(Primitive::Str)) =>
        {
            Some(b.id(module, "aipl_arr_drop_opt_str"))
        }
        ConcreteType::Optional(inner) if matches!(inner.as_ref(), ConcreteType::Array(_)) => {
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
    elem: &ConcreteType,
) -> Value {
    let b = cx.builtins;
    let id = match elem {
        // See `array_drop_fn_addr` for why this is chosen per element type: the
        // array case below keeps the 8-byte-pointer helper either way.
        _ if is_str_repr(elem) => Some(b.id(module, "aipl_arr_retain_str")),
        ConcreteType::Array(_) if is_char_array(elem) => Some(b.id(module, "aipl_arr_retain_str")),
        ConcreteType::Primitive(Primitive::Str) => Some(b.id(module, "aipl_arr_retain_ptr")),
        ConcreteType::Array(_) => Some(b.id(module, "aipl_arr_retain_ptr")),
        ConcreteType::Optional(inner)
            if matches!(inner.as_ref(), ConcreteType::Primitive(Primitive::Str)) =>
        {
            // The wide form cannot share `aipl_arr_retain_opt` with `T[]?[]`
            // below: an array element is still an 8-byte pointer, so only the
            // `str?` case changes shape.
            Some(b.id(module, "aipl_arr_retain_opt_str"))
        }
        ConcreteType::Optional(inner) if matches!(inner.as_ref(), ConcreteType::Array(_)) => {
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
fn elem_rc_ids<M: Module>(module: &mut M, cx: Cx, elem: &ConcreteType) -> (FuncId, FuncId) {
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(ids) = er.fns.get(elem) {
        return *ids;
    }
    let elem_sym = type_symbol(elem);
    let drop_sym = er.symbol("__arr_drop_", &elem_sym);
    let retain_sym = er.symbol("__arr_retain_", &elem_sym);
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // elems
    sig.params.push(AbiParam::new(types::I64)); // len
    let drop_id = module
        .declare_function(&drop_sym, Linkage::Local, &sig)
        .expect("declare elem drop");
    let retain_id = module
        .declare_function(&retain_sym, Linkage::Local, &sig)
        .expect("declare elem retain");
    er.fns.insert(elem.clone(), (drop_id, retain_id));
    er.pending.push((elem.clone(), drop_id, retain_id));
    (drop_id, retain_id)
}

/// Declare (once, cached) the `(drop, retain)` helpers for a dict's pair-array
/// elements, where each element is `[key: K][value: V]`. Mirrors `elem_rc_ids`
/// but the generated body releases/retains both halves of every pair.
fn pair_rc_ids<M: Module>(
    module: &mut M,
    cx: Cx,
    k: &ConcreteType,
    v: &ConcreteType,
) -> (FuncId, FuncId) {
    let key = (k.clone(), v.clone());
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(ids) = er.pair_fns.get(&key) {
        return *ids;
    }
    let pair_sym = format!("{}${}", type_symbol(k), type_symbol(v));
    let drop_sym = er.symbol("__dict_drop_", &pair_sym);
    let retain_sym = er.symbol("__dict_retain_", &pair_sym);
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // elems
    sig.params.push(AbiParam::new(types::I64)); // len
    let drop_id = module
        .declare_function(&drop_sym, Linkage::Local, &sig)
        .expect("declare pair drop");
    let retain_id = module
        .declare_function(&retain_sym, Linkage::Local, &sig)
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
    k: &ConcreteType,
    v: &ConcreteType,
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
    key_ty: &ConcreteType,
    val_ty: &ConcreteType,
    op: RcOp,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // elems
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // len
    let pair_size = dict_pair_size(key_ty, val_ty, structs);
    let key_size = dict_key_size(key_ty, structs);
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
        builder.ins().stack_store(types::I64, zero, slot, 0);
        let header = builder.create_block();
        let body = builder.create_block();
        let exit = builder.create_block();
        builder.ins().jump(header, &[]);

        builder.switch_to_block(header);
        let i = builder.ins().stack_load(types::I64, types::I64, slot, 0);
        let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
        builder.ins().brif(more, body, &[], exit, &[]);

        builder.switch_to_block(body);
        builder.seal_block(body);
        let off = builder.ins().imul_imm_s(i, pair_size);
        let pair = builder.ins().iadd(elems, off);
        // Key at offset 0, value immediately after it — at `key_size`, which is
        // 8 for every key but a wide `str` (`dict_key_size`). `component` loads a
        // scalar and addresses a composite.
        let kv = component(&mut builder, pair, 0, key_ty, structs);
        emit_rc(&mut builder, module, builtins, structs, kv, key_ty, op);
        let vv = component(&mut builder, pair, key_size as u32, val_ty, structs);
        emit_rc(&mut builder, module, builtins, structs, vv, val_ty, op);
        let next = builder.ins().iadd_imm_s(i, 1);
        builder.ins().stack_store(types::I64, next, slot, 0);
        builder.ins().jump(header, &[]);
        builder.seal_block(header);

        builder.switch_to_block(exit);
        builder.seal_block(exit);
        builder.ins().return_(&[]);
        builder.finalize(module.target_config());
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    if instrument {
        let call_fn = builtins.id(module, "aipl_count_call");
        instrument_call_count(module, &mut ctx.func, id, call_fn)?;
    }
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
    elem: &ConcreteType,
    op: RcOp,
    ir_out: &mut String,
    instrument: bool,
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
        builder.ins().stack_store(types::I64, zero, slot, 0);
        let header = builder.create_block();
        let body = builder.create_block();
        let exit = builder.create_block();
        builder.ins().jump(header, &[]);

        builder.switch_to_block(header);
        let i = builder.ins().stack_load(types::I64, types::I64, slot, 0);
        let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
        builder.ins().brif(more, body, &[], exit, &[]);

        builder.switch_to_block(body);
        builder.seal_block(body);
        let off = builder.ins().imul_imm_s(i, esz);
        let addr = builder.ins().iadd(elems, off);
        let elem_val = component(&mut builder, addr, 0, elem, structs);
        emit_rc(&mut builder, module, builtins, structs, elem_val, elem, op);
        let next = builder.ins().iadd_imm_s(i, 1);
        builder.ins().stack_store(types::I64, next, slot, 0);
        builder.ins().jump(header, &[]);
        builder.seal_block(header);

        builder.switch_to_block(exit);
        builder.seal_block(exit);
        builder.ins().return_(&[]);
        builder.finalize(module.target_config());
    }
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    ir_out.push_str(&fix_data_ref_names(
        &ctx.func,
        &format!("{}\n", ctx.func.display()),
    ));
    if instrument {
        let call_fn = builtins.id(module, "aipl_count_call");
        instrument_call_count(module, &mut ctx.func, id, call_fn)?;
    }
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define elem rc fn: {e}")))?;
    module.clear_context(ctx);
    Ok(())
}

/// Emit a call to a shimmable *operation* — one of the builtins listed in
/// `aipl_syntax::SHIMMABLE_EFFECTS`. Reads the operation's shim slot and calls
/// the installed shim through it when one is present, falling back to the real
/// runtime symbol otherwise:
///
/// ```text
///     cur = aipl_shim_get(slot)
///     brif cur != 0 -> shim(cur) : real()
/// ```
///
/// The branch is what makes a shim reach every call at any depth: a callee
/// compiled long before any shim existed still asks the slot each time. Only
/// zero-argument, single-`i64`-result operations are supported so far, which is
/// all the clock operations need; widening it means threading args and the sret
/// ABI through, exactly as `compile_indirect_call` already does.
fn emit_shimmable_call<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    builtins: &Builtins,
    op: &str,
    real_sym: &'static str,
) -> Value {
    let slot = aipl_syntax::shim_slot_index(op).expect("a shimmable operation has a slot") as i64;
    let idx = builder.ins().iconst(types::I64, slot);
    let cur = builtins.call(module, builder, "aipl_shim_get", &[idx]);

    let shim_b = builder.create_block();
    let real_b = builder.create_block();
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    let installed = builder.ins().icmp_imm_s(IntCC::NotEqual, cur, 0);
    builder.ins().brif(installed, shim_b, &[], real_b, &[]);

    // A shim has the operation's own signature, which for these is `() -> i64`.
    builder.switch_to_block(shim_b);
    builder.seal_block(shim_b);
    let mut sig = module.make_signature();
    sig.returns.push(AbiParam::new(types::I64));
    let sigref = builder.import_signature(sig);
    let inst = builder.ins().call_indirect(sigref, cur, &[]);
    let shimmed = builder.inst_results(inst)[0];
    builder.ins().jump(merge, &[BlockArg::Value(shimmed)]);

    builder.switch_to_block(real_b);
    builder.seal_block(real_b);
    let actual = builtins.call(module, builder, real_sym, &[]);
    builder.ins().jump(merge, &[BlockArg::Value(actual)]);

    builder.switch_to_block(merge);
    builder.seal_block(merge);
    builder.block_params(merge)[0]
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

/// Copy the 24-byte `str` at `src` into a fresh stack slot and return its
/// address — a snapshot that later writes to `src` cannot disturb.
///
/// Word-by-word rather than through `copy_composite` so it needs no `structs`
/// map: a `str`'s size is fixed by the representation, not by a layout table.
fn copy_str_value(builder: &mut FunctionBuilder, src: Value) -> Value {
    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        str24::STR_SIZE as u32,
        3,
    ));
    let dst = builder.ins().stack_addr(types::I64, slot, 0);
    let flags = MemFlagsData::trusted();
    for off in (0..str24::STR_SIZE as i32).step_by(8) {
        let w = builder.ins().load(types::I64, flags, src, off);
        builder.ins().store(flags, w, dst, off);
    }
    dst
}

/// The handle for the value living in `slot` — what the rest of codegen expects
/// to be given for a binding or a slot-track.
///
/// The counterpart of [`store_binding_str`], and wrong in the same way if
/// skipped: a tagged value is one word *in* the slot, so it is loaded; a wide
/// `str`/`char[]` *is* the slot's 24 bytes, so the handle is its address.
/// Loading a word from a wide slot yields the value's first word, which then
/// reads as a `Str` — the failure that surfaced as an overlapping
/// `copy_nonoverlapping`, and again as `misaligned pointer dereference ... is
/// 0x636261` (the bytes `abc`).
///
/// **A wide handle is not a snapshot.** A tagged load copies the pointer out, so
/// capturing an old value and then overwriting the slot was safe in either
/// order; a wide handle keeps naming whatever the slot holds *now*. Anything
/// that releases an old value around a writeback has to release it first — see
/// the `push`/`extend` char paths.
fn slot_value(builder: &mut FunctionBuilder, slot: StackSlot, ty: &ConcreteType) -> Value {
    if is_str_shaped(ty) {
        builder.ins().stack_addr(types::I64, slot, 0)
    } else {
        builder.ins().stack_load(types::I64, types::I64, slot, 0)
    }
}

/// [`slot_value`] where the binding is known to be `str`-shaped.
fn load_binding_str(builder: &mut FunctionBuilder, slot: StackSlot) -> Value {
    slot_value(builder, slot, &ConcreteType::Primitive(Primitive::Str))
}

/// Write a freshly built `str` back into the stack slot of the `mut` binding it
/// belongs to.
///
/// A tagged `str` is one word, so this was a plain `stack_store`. A wide one is
/// the whole 24-byte value, and `v` addresses it — storing `v` itself would put
/// a pointer where the binding's value lives.
fn store_binding_str(
    builder: &mut FunctionBuilder,
    cx: Cx,
    slot: StackSlot,
    v: Value,
    structs: &HashMap<String, TypeDef>,
) {
    store_binding(
        builder,
        cx,
        slot,
        v,
        &ConcreteType::Primitive(Primitive::Str),
        structs,
    );
}

/// [`store_binding_str`] for a binding of any type: the value's 24 bytes when it
/// is a wide `str`/`char[]`, one word otherwise. Mirrors [`slot_value`], and the
/// two must agree — a slot written one way and read the other is a silent
/// miscompile rather than a crash.
fn store_binding(
    builder: &mut FunctionBuilder,
    cx: Cx,
    slot: StackSlot,
    v: Value,
    ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) {
    let _ = cx;
    if !is_str_shaped(ty) {
        builder.ins().stack_store(types::I64, v, slot, 0);
        return;
    }
    let dst = builder.ins().stack_addr(types::I64, slot, 0);
    copy_composite(builder, dst, v, ty, structs);
}

/// Allocate a `str` of exactly `len` bytes and return `(value, write_ptr)` —
/// what to store, and where to put the bytes.
///
/// The two are the *same* under the tagged representation, where a `str` value
/// is its content pointer, and different under the wide one, where the value is
/// 24 bytes describing a buffer that has to be asked for its write pointer.
/// Every "build a fresh buffer and copy into it" site went straight to
/// `aipl_str_alloc` and used its result as both, which is exactly the conflation
/// that breaks — so they go through here instead.
///
/// Pair every call with [`emit_str_grew`]: wide allocation gives *capacity*, and
/// the value's length is only recorded once the bytes are in.
fn emit_str_alloc<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    len: Value,
) -> (Value, Value) {
    let built = cx.builtins.call(module, builder, "aipl_str_alloc", &[len]);
    let start = cx
        .builtins
        .call(module, builder, "aipl_str_write_ptr", &[built]);
    (built, start)
}

/// Record that a buffer from [`emit_str_alloc`] now holds `len` bytes. A no-op
/// under the tagged representation, where `aipl_str_alloc` already set the
/// length it was asked for.
fn emit_str_grew<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    value: Value,
    len: Value,
) {
    cx.builtins
        .call_void(module, builder, "aipl_str_grew", &[value, len]);
}

/// The contiguous content pointer for a `str` *value*, which is what the
/// byte-copying sinks need. Neither representation stores one directly for every
/// case — a tagged value may be inline or a rope, a wide one may be inline — so
/// this always goes through `aipl_str_data`, which returns the in-place pointer
/// where there is one and spills into `scratch` where there isn't.
///
/// The only thing that differs between the ABIs is how big that spill can get:
/// a tagged inline value holds 7 bytes, a wide one [`str24::INLINE_CAP`]. Sizing
/// the slot for the wrong ABI is a stack overwrite, which is why every copying
/// path routes through this one function rather than opening its own slot.
fn str_bytes_ptr<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    value: Value,
) -> Value {
    let spill = str24::INLINE_CAP as u32;
    let scratch =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, spill, 3));
    let scratch_addr = builder.ins().stack_addr(types::I64, scratch, 0);
    cx.builtins
        .call(module, builder, "aipl_str_data", &[value, scratch_addr])
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
        let dst = builder.ins().stack_load(types::I64, types::I64, cur, 0);
        let adv = cx
            .builtins
            .call(module, builder, "aipl_write_bytes", &[dst, src, n]);
        builder.ins().stack_store(types::I64, adv, cur, 0);
    }
}

/// In `Write` mode, write one byte (low 8 bits of `byte`) and advance the cursor.
fn sink_byte(builder: &mut FunctionBuilder, sink: Sink, byte: Value) {
    if let Sink::Write(cur) = sink {
        let dst = builder.ins().stack_load(types::I64, types::I64, cur, 0);
        builder.ins().istore8(MemFlagsData::trusted(), byte, dst, 0);
        let adv = builder.ins().iadd_imm_s(dst, 1);
        builder.ins().stack_store(types::I64, adv, cur, 0);
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
        // `emit_str_literal` yields the *content* pointer already, not a `str`
        // value, so it feeds the sink directly under either representation.
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
    ty: &ConcreteType,
    sink: Sink,
) -> Result<Value, Error> {
    let b = cx.builtins;
    Ok(match ty {
        // Signed integers (i8/i16/i32/i64) render via the signed formatter — the
        // canonical i64 register value is the signed value. Unsigned ones use the
        // unsigned formatter (u64 can exceed i64's range).
        ConcreteType::Primitive(p) if p.is_int() => {
            let (len_fn, write_fn) = if p.int_signed() {
                ("aipl_i64_len", "aipl_write_i64")
            } else {
                ("aipl_u64_len", "aipl_write_u64")
            };
            let len = { b.call(module, builder, len_fn, &[value]) };
            if let Sink::Write(cur) = sink {
                let dst = builder.ins().stack_load(types::I64, types::I64, cur, 0);
                let adv = b.call(module, builder, write_fn, &[dst, value]);
                builder.ins().stack_store(types::I64, adv, cur, 0);
            }
            len
        }
        ConcreteType::Primitive(Primitive::Bool) => {
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
        ConcreteType::Primitive(Primitive::Char) => {
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
            let content = { b.call(module, builder, "aipl_str_len", &[value]) };
            if let Sink::Write(_) = sink {
                let quote = builder.ins().iconst(types::I64, b'"' as i64);
                sink_byte(builder, sink, quote);
                // Resolve a contiguous content pointer for any representation
                // (inline/owned/view). (`content`, the length, already handles
                // all three — `aipl_str_len`.)
                let src = str_bytes_ptr(module, builder, cx, value);
                sink_bytes(module, builder, cx, sink, src, content);
                sink_byte(builder, sink, quote);
            }
            builder.ins().iadd_imm_s(content, 2)
        }
        // `char[]` is str-shaped (see `is_char_array`) but keeps its own
        // array-bracket rendering (`['a', 'b', 'c']`, not `str`'s `"abc"`) —
        // read via the same byte cursor `for`-loop iteration uses, since
        // `value` is a real `str` underneath, not a generic array block.
        _ if is_char_array(ty) => emit_render_char_array(module, builder, cx, value, sink)?,
        ConcreteType::Array(elem) => {
            emit_render_seq(module, builder, cx, value, elem, sink, b'[', b']')?
        }
        ConcreteType::Set(elem) => {
            emit_render_seq(module, builder, cx, value, elem, sink, b'{', b'}')?
        }
        ConcreteType::Dict(k, v) => emit_render_dict(module, builder, cx, value, k, v, sink)?,
        ConcreteType::Optional(_) => emit_render_optional(module, builder, cx, value, ty, sink)?,
        ConcreteType::Result(ok, err) => {
            emit_render_result(module, builder, cx, value, ok, err, sink)?
        }
        // A boxed (recursive) type is rendered by calling its own `to_str`
        // helper and splicing the result — so the recursion runs through
        // function calls (terminating at a base case) instead of inlining the
        // structure into itself forever. The helper's own body renders one
        // level inline (`define_tostr_fn` enters `emit_render_named` directly).
        ConcreteType::Named(_) if is_boxed(ty, cx.structs) => {
            emit_render_boxed(module, builder, cx, value, ty, sink)?
        }
        ConcreteType::Named(_) => emit_render_named(module, builder, cx, value, ty, sink)?,
        other => {
            return Err(Error::msg(format!(
                "to_str: rendering {} is not yet supported",
                type_name(other)
            )));
        }
    })
}

/// Render a declared `Named` type (struct or variant) *inline*, dispatching on
/// which it is. `value` addresses the `{tag, payload}`/fields layout (a stack
/// slot for an inline value, or the heap payload pointer for a boxed one — both
/// read identically). Used for a non-boxed type directly by `emit_render`, and
/// as the entry level of a boxed type's `to_str` helper.
fn emit_render_named<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    value: Value,
    ty: &ConcreteType,
    sink: Sink,
) -> Result<Value, Error> {
    let ConcreteType::Named(n) = ty else {
        unreachable!("emit_render_named called with a non-Named type");
    };
    if cx.structs.get(n).and_then(TypeDef::as_struct).is_some() {
        emit_render_struct(module, builder, cx, value, n, sink)
    } else {
        emit_render_variant(module, builder, cx, value, n, sink)
    }
}

/// Render a boxed (recursive) value by calling its cached `to_str` helper and
/// splicing the resulting string's bytes (unquoted) into the sink. The helper
/// recurses through nested boxed children via further helper calls, so a
/// linked structure renders without inlining itself. Called in both the
/// measure and write passes; it rebuilds the string each pass (rendering isn't
/// on any hot path). The temporary string is dropped after use.
fn emit_render_boxed<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    value: Value,
    ty: &ConcreteType,
    sink: Sink,
) -> Result<Value, Error> {
    let b = cx.builtins;
    let id = tostr_func(module, cx, ty);
    let fref = module.declare_func_in_func(id, builder.func);
    let s = {
        let slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            str24::STR_SIZE as u32,
            3,
        ));
        let out = builder.ins().stack_addr(types::I64, slot, 0);
        builder.ins().call(fref, &[out, value]);
        out
    };
    let len = { b.call(module, builder, "aipl_str_len", &[s]) };
    if let Sink::Write(_) = sink {
        let src = str_bytes_ptr(module, builder, cx, s);
        sink_bytes(module, builder, cx, sink, src, len);
    }
    emit_drop(
        builder,
        module,
        b,
        cx.structs,
        s,
        &ConcreteType::Primitive(Primitive::Str),
    );
    Ok(len)
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
    Ok(builder.ins().iadd_imm_s(base, STR_HEADER_SIZE as i64))
}

fn emit_const_str<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    content: &[u8],
) -> Result<(Value, bool), Error> {
    // A `str` is a 24-byte composite (`STR_REPR.md`), so a literal is three
    // words materialized into a stack slot, and what flows is that slot's
    // address — exactly like a struct constant.
    //
    // Neither form needs scope tracking: an inline literal owns no allocation,
    // and a static one carries `STATIC_REFCOUNT`, so the release at scope exit
    // would be a no-op either way.
    let slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        str24::STR_SIZE as u32,
        3,
    ));
    let addr = builder.ins().stack_addr(types::I64, slot, 0);
    let flags = MemFlagsData::trusted();

    if content.len() <= str24::INLINE_CAP {
        // Small string optimization, now 22 bytes rather than 7: the content is
        // the value, with no data object, allocation, or refcount.
        for (i, word) in str24::inline_words(content).into_iter().enumerate() {
            let v = builder.ins().iconst(types::I64, word as i64);
            builder.ins().store(flags, v, addr, (i * 8) as i32);
        }
        return Ok((addr, false));
    }

    // Static literal: `[cap][refcount = STATIC][bytes]` in the data section, with
    // `base` past both header words. Interned by content (see `StrLiterals`), so
    // a repeated literal shares one object and one content-hash symbol.
    //
    // No NUL: the new representation is length-delimited everywhere, and a
    // buffer no longer promises a terminator.
    let data_id = cx.str_data.borrow_mut().intern(module, content, || {
        let mut bytes = Vec::with_capacity(str24::BUF_HEADER + content.len());
        bytes.extend_from_slice(&(content.len() as i64).to_le_bytes()); // cap
        bytes.extend_from_slice(&str24::STATIC_REFCOUNT.to_le_bytes());
        bytes.extend_from_slice(content);
        bytes.into_boxed_slice()
    })?;
    let gv = module.declare_data_in_func(data_id, builder.func);
    let symbol = builder.ins().symbol_value(types::I64, gv);
    let base = builder.ins().iadd_imm_s(symbol, str24::BUF_HEADER as i64);
    let meta = builder
        .ins()
        .iconst(types::I64, str24::buffer_meta(content.len()) as i64);
    builder.ins().store(flags, base, addr, 0); // base
    builder.ins().store(flags, base, addr, 8); // data — the whole buffer
    builder.ins().store(flags, meta, addr, 16); // len | tag
    Ok((addr, false))
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
    ok_ty: Option<ConcreteType>,
    err_msg: &[u8],
) -> Result<Value, Error> {
    // The tagged runtime signals failure with a null return, so the value *is*
    // the success test. See `emit_file_result_split` for the wide form, where a
    // `str` payload arrives through an out pointer that is never null and the
    // flag has to be carried separately.
    let is_ok = builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0);
    let payload = ok_ty.map(|t| (raw, t));
    emit_file_result_split(module, builder, cx, is_ok, payload, err_msg)
}

/// `str!Error` from an explicit success flag and an optional payload.
///
/// Both the slot size and how the payload is written follow the payload's
/// representation: a tagged `str` is one word stored at `OPT_VALUE_OFFSET`, a
/// wide one is 24 bytes copied there, making the whole result 32 rather than 16.
/// The error side is a literal either way — and note it goes through
/// `emit_const_str`, which yields a *value*, not `emit_str_literal`, which
/// yields raw content bytes for a sink to copy.
fn emit_file_result_split<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    is_ok: Value,
    payload: Option<(Value, ConcreteType)>,
    err_msg: &[u8],
) -> Result<Value, Error> {
    let str_ty = ConcreteType::Primitive(Primitive::Str);
    // The payload region is sized to the *wider* side, as any result is. That
    // only started to matter here when a `str` stopped being one word: the err
    // side is always an `Error` (a `str`), so for `str[]!Error` — where the ok
    // side is an 8-byte array pointer — the err side is now the larger one, and
    // sizing from the ok side would put the message past the end of the slot.
    let ok_size = payload
        .as_ref()
        .map_or(8, |(_, t)| elem_size_of(t, cx.structs));
    let size = OPT_VALUE_OFFSET + ok_size.max(elem_size_of(&str_ty, cx.structs)) as u32;
    let slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 3));
    let ptr = builder.ins().stack_addr(types::I64, slot, 0);
    let val_at = builder.ins().iadd_imm_s(ptr, OPT_VALUE_OFFSET as i64);
    let ok_b = builder.create_block();
    let err_b = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(is_ok, ok_b, &[], err_b, &[]);

    builder.switch_to_block(ok_b);
    builder.seal_block(ok_b);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().store(MemFlagsData::trusted(), one, ptr, 0);
    match &payload {
        Some((v, t)) => store_array_elem(builder, val_at, *v, t, cx.structs),
        None => {
            let zero = builder.ins().iconst(types::I64, 0);
            builder
                .ins()
                .store(MemFlagsData::trusted(), zero, val_at, 0);
        }
    }
    builder.ins().jump(merge, &[]);

    builder.switch_to_block(err_b);
    builder.seal_block(err_b);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().store(MemFlagsData::trusted(), zero, ptr, 0);
    let (msg, _) = emit_const_str(module, builder, cx, err_msg)?;
    store_array_elem(builder, val_at, msg, &str_ty, cx.structs);
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
        .load(types::I64, MemFlagsData::trusted(), result_ptr, 0);
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
        MemFlagsData::trusted(),
        result_ptr,
        OPT_VALUE_OFFSET as i32,
    );
    builtins.call_void(module, builder, "aipl_print_error", &[msg]);
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
    opt_ty: &ConcreteType,
    sink: Sink,
) -> Result<Value, Error> {
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), slot_ptr, 0);
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
    core: &ConcreteType,
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
        let dec = builder.ins().iadd_imm_s(tag, -1);
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
    ok_ty: &ConcreteType,
    err_ty: &ConcreteType,
    sink: Sink,
) -> Result<Value, Error> {
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), slot_ptr, 0);
    let ok_b = builder.create_block();
    let err_b = builder.create_block();
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    builder.ins().brif(tag, ok_b, &[], err_b, &[]); // tag 1 = Ok, 0 = Err

    // A unit (void-Ok `!E`) side renders as `ok()`; a `__none__` side is
    // unconstructible (dead branch) — render the bare ctor either way.
    let payload_trivial = |t: &ConcreteType| is_unit(t) || is_none_inner(t);
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
    let fields: Vec<(String, u32, ConcreteType)> = layout
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
    let cases: Vec<(String, Vec<(u32, ConcreteType)>)> = cx.structs[name]
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
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), base, 0);
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    for (k, (ctor, fields)) in cases.into_iter().enumerate() {
        let case_b = builder.create_block();
        let next_b = builder.create_block();
        let is_k = builder.ins().icmp_imm_s(IntCC::Equal, tag, k as i64);
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
    let cur = builder.ins().stack_load(types::I64, types::I64, slot, 0);
    let sum = builder.ins().iadd(cur, v);
    builder.ins().stack_store(types::I64, sum, slot, 0);
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
    builder.ins().stack_store(types::I64, zero, len_slot, 0);
    let open_len = emit_lit(module, builder, cx, sink, b"[")?;
    add_len(builder, len_slot, open_len);

    let cur = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        iter_state_size(),
        3,
    ));
    let cur_addr = builder.ins().stack_addr(types::I64, cur, 0);
    b.call_void(module, builder, "aipl_str_iter_init", &[cur_addr, arr]);

    // 1 until the first element has been rendered, then 0 — controls the
    // leading ", " separator (mirrors `emit_render_seq`'s `is_first` check,
    // but this cursor has no index to compare against 0).
    let first_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(types::I64, one, first_slot, 0);

    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let byte_i64 = emit_str_iter_next(module, builder, b, cur_addr);
    let more = builder
        .ins()
        .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, byte_i64, 0);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    let is_first = builder
        .ins()
        .stack_load(types::I64, types::I64, first_slot, 0);
    let is_first_b = builder.ins().icmp_imm_s(IntCC::NotEqual, is_first, 0);
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
    builder
        .ins()
        .stack_store(types::I64, zero_flag, first_slot, 0);

    let elem_len = emit_render(
        module,
        builder,
        cx,
        byte_i64,
        &ConcreteType::Primitive(Primitive::Char),
        sink,
    )?;
    add_len(builder, len_slot, elem_len);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    let close_len = emit_lit(module, builder, cx, sink, b"]")?;
    add_len(builder, len_slot, close_len);
    Ok(builder
        .ins()
        .stack_load(types::I64, types::I64, len_slot, 0))
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
    elem_ty: &ConcreteType,
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
    builder.ins().stack_store(types::I64, zero, len_slot, 0);
    let open_len = emit_lit(module, builder, cx, sink, &[open])?;
    add_len(builder, len_slot, open_len);

    let idx =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    builder.ins().stack_store(types::I64, zero, idx, 0);
    let count = load_arr_len(builder, arr);

    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, count);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    // ", " before every element but the first.
    let is_first = builder.ins().icmp_imm_s(IntCC::Equal, i, 0);
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

    let next = builder.ins().iadd_imm_s(i, 1);
    builder.ins().stack_store(types::I64, next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    let close_len = emit_lit(module, builder, cx, sink, &[close])?;
    add_len(builder, len_slot, close_len);
    Ok(builder
        .ins()
        .stack_load(types::I64, types::I64, len_slot, 0))
}

/// Render a dict as `{k0: v0, k1: v1, ...}` (empty: `{}`). Mirrors
/// `emit_render_seq`, but each pair-array element renders as its key, `": "`,
/// then its value. The key is at the pair's offset 0, the value at 8.
fn emit_render_dict<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    dict: Value,
    key_ty: &ConcreteType,
    val_ty: &ConcreteType,
    sink: Sink,
) -> Result<Value, Error> {
    // An untyped empty `#{:}` has no key/value renderer to recurse into.
    if is_none_inner(key_ty) {
        return emit_lit(module, builder, cx, sink, b"{}");
    }
    let pair_size = dict_pair_size(key_ty, val_ty, cx.structs);
    let key_size = dict_key_size(key_ty, cx.structs);
    let len_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(types::I64, zero, len_slot, 0);
    let open_len = emit_lit(module, builder, cx, sink, b"{")?;
    add_len(builder, len_slot, open_len);

    let idx =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
    builder.ins().stack_store(types::I64, zero, idx, 0);
    let count = load_arr_len(builder, dict);
    let dict_base = arr_base(builder, dict);
    let elems = builder.ins().iadd_imm_s(dict_base, ARR_ELEMS_OFFSET as i64);

    let header = builder.create_block();
    let body = builder.create_block();
    let exit = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, count);
    builder.ins().brif(more, body, &[], exit, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    // ", " before every pair but the first.
    let is_first = builder.ins().icmp_imm_s(IntCC::Equal, i, 0);
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

    let off = builder.ins().imul_imm_s(i, pair_size);
    let pair = builder.ins().iadd(elems, off);
    let kv = component(builder, pair, 0, key_ty, cx.structs);
    let klen = emit_render(module, builder, cx, kv, key_ty, sink)?;
    add_len(builder, len_slot, klen);
    let colon = emit_lit(module, builder, cx, sink, b": ")?;
    add_len(builder, len_slot, colon);
    let vv = component(builder, pair, key_size as u32, val_ty, cx.structs);
    let vlen = emit_render(module, builder, cx, vv, val_ty, sink)?;
    add_len(builder, len_slot, vlen);

    let next = builder.ins().iadd_imm_s(i, 1);
    builder.ins().stack_store(types::I64, next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(exit);
    builder.seal_block(exit);
    let close_len = emit_lit(module, builder, cx, sink, b"}")?;
    add_len(builder, len_slot, close_len);
    Ok(builder
        .ins()
        .stack_load(types::I64, types::I64, len_slot, 0))
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
    ty: &ConcreteType,
) -> Result<Value, Error> {
    let id = tostr_func(module, cx, ty);
    let fref = module.declare_func_in_func(id, builder.func);
    // The helper borrows `value` (renders without consuming it) and produces a
    // freshly built `str` with one reference, owned by us — tracked below for
    // drop at scope exit (an inline result no-ops; a buffer is freed via the
    // refcount). Under the wide representation the result comes back through a
    // caller-provided slot rather than a register.
    let result = {
        let slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            str24::STR_SIZE as u32,
            3,
        ));
        let out = builder.ins().stack_addr(types::I64, slot, 0);
        builder.ins().call(fref, &[out, value]);
        out
    };
    scopes.last_mut().expect("scope").push(Tracked::new(
        result,
        &ConcreteType::Primitive(Primitive::Str),
    ));
    Ok(result)
}

/// Declare (once, cached) the per-type `__to_str_<n>(value) -> str` rendering
/// helper for `ty`, recording it to be defined after the main function loop
/// (when the build context is free). Returns its function id.
fn tostr_func<M: Module>(module: &mut M, cx: Cx, ty: &ConcreteType) -> FuncId {
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(id) = er.tostr_fns.get(ty) {
        return *id;
    }
    let sym = er.symbol("__to_str_", &type_symbol(ty));
    let mut sig = module.make_signature();
    // A 24-byte `str` is a composite, so the result is written through a
    // hidden leading pointer and nothing is returned — the same shape
    // `build_signature` gives any composite-returning function.
    sig.params.push(AbiParam::new(types::I64)); // sret
    sig.params.push(AbiParam::new(types::I64)); // value
    let id = module
        .declare_function(&sym, Linkage::Local, &sig)
        .expect("declare to_str helper");
    er.tostr_fns.insert(ty.clone(), id);
    er.tostr_pending.push((ty.clone(), id));
    id
}

/// Declare (once, cached) the per-err-type `__test_try_fail_<n>(payload)` helper
/// for `err_ty` — the shared body a test `?` calls on an err. Returns its id; the
/// body is defined later from `test_fail_pending`.
fn test_fail_func<M: Module>(module: &mut M, cx: Cx, err_ty: &ConcreteType) -> FuncId {
    let mut er = cx.elem_rc.borrow_mut();
    if let Some(id) = er.test_fail_fns.get(err_ty) {
        return *id;
    }
    let sym = er.symbol("__test_try_fail_", &type_symbol(err_ty));
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // err payload (scalar/str value, or struct ptr)
    let id = module
        .declare_function(&sym, Linkage::Local, &sig)
        .expect("declare test-fail helper");
    er.test_fail_fns.insert(err_ty.clone(), id);
    er.test_fail_pending.push((err_ty.clone(), id));
    id
}

/// Define a generated `__test_try_fail_<n>(payload)` helper: render the err
/// `payload` (of type `err_ty`) to a `str`, report it via `aipl_test_fail`, and
/// drop the rendered string. Centralizes the "error printing stuff" so each `?`
/// site in a test only reads the payload and calls this.
#[allow(clippy::too_many_arguments)]
fn define_test_fail_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    funcs: &HashMap<String, FuncInfo>,
    structs: &HashMap<String, TypeDef>,
    builtins: &Builtins,
    lit_ctr: &Cell<u32>,
    str_data: &RefCell<StrLiterals>,
    elem_rc: &RefCell<ElemRc>,
    id: FuncId,
    err_ty: &ConcreteType,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // payload
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let payload = builder.block_params(entry)[0];

        let env: Env = HashMap::new();
        let owned_params: HashSet<String> = HashSet::new();
        let unit = ConcreteType::Unit;
        let no_bindings: RefCell<Vec<(String, String)>> = RefCell::new(Vec::new());
        let mut scopes: Vec<Vec<Tracked>> = vec![Vec::new()];
        let cx = Cx {
            env: &env,
            funcs,
            structs,
            builtins,
            effects: &[],
            owned_params: &owned_params,
            lit_ctr,
            str_data,
            elem_rc,
            ret_ty: &unit,
            sret: None,
            error_main: false,
            in_test: false,
            bindings: &no_bindings,
            // A generated helper is plain C-convention and never a tail-call
            // participant, so nothing inside it is ever in tail position.
            can_tail: false,
            tail: false,
        };

        // Render the borrowed payload to a fresh `str`, report it, then release
        // the rendered string (its sole reference).
        let msg = emit_to_str(module, &mut builder, cx, &mut scopes, payload, err_ty)?;
        builtins.call_void(module, &mut builder, "aipl_test_fail", &[msg]);
        emit_drop(
            &mut builder,
            module,
            builtins,
            structs,
            msg,
            &ConcreteType::Primitive(Primitive::Str),
        );
        builder.ins().return_(&[]);
        builder.finalize(module.target_config());
    }

    ctx.func.name = UserFuncName::user(0, id.as_u32());
    run_ir_passes(module, builtins, &mut ctx.func, DebugOptions::OFF);
    ir_out.push_str(&format!("{}\n", ctx.func.display()));
    if instrument {
        let count_fn = builtins.id(module, "aipl_count_insns");
        instrument_insn_count(module, &mut ctx.func, count_fn);
        let call_fn = builtins.id(module, "aipl_count_call");
        instrument_call_count(module, &mut ctx.func, id, call_fn)?;
    }
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define test-fail helper: {e:?}")))?;
    ctx.clear();
    Ok(())
}

/// Define a generated `__eq_<n>(lv, rv) -> i64` helper: the structural equality
/// of two borrowed `ty` values (see [`emit_eq_body`]). Nested composites in the
/// body call *their* helpers (via [`emit_eq`]), so defining one can request more
/// eq helpers — the drain loop handles that.
#[allow(clippy::too_many_arguments)]
fn define_eq_fn<M: Module>(
    module: &mut M,
    ctx: &mut Context,
    fbc: &mut FunctionBuilderContext,
    funcs: &HashMap<String, FuncInfo>,
    structs: &HashMap<String, TypeDef>,
    builtins: &Builtins,
    lit_ctr: &Cell<u32>,
    str_data: &RefCell<StrLiterals>,
    elem_rc: &RefCell<ElemRc>,
    id: FuncId,
    ty: &ConcreteType,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // lv
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // rv
    ctx.func.signature.returns.push(AbiParam::new(types::I64)); // 0/1
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let lv = builder.block_params(entry)[0];
        let rv = builder.block_params(entry)[1];

        // `emit_eq_body` only reads `builtins`/`structs`/`elem_rc`; the rest of
        // `Cx` is irrelevant to a borrow-only comparison, so feed trivial values.
        let env: Env = HashMap::new();
        let owned_params: HashSet<String> = HashSet::new();
        let unit = ConcreteType::Unit;
        let no_bindings: RefCell<Vec<(String, String)>> = RefCell::new(Vec::new());
        let cx = Cx {
            env: &env,
            funcs,
            structs,
            builtins,
            effects: &[],
            owned_params: &owned_params,
            lit_ctr,
            str_data,
            elem_rc,
            ret_ty: &unit,
            sret: None,
            error_main: false,
            in_test: false,
            bindings: &no_bindings,
            // A generated helper is plain C-convention and never a tail-call
            // participant, so nothing inside it is ever in tail position.
            can_tail: false,
            tail: false,
        };

        let res = emit_eq_body(module, &mut builder, cx, lv, rv, ty)?;
        builder.ins().return_(&[res]);
        builder.finalize(module.target_config());
    }

    ctx.func.name = UserFuncName::user(0, id.as_u32());
    run_ir_passes(module, builtins, &mut ctx.func, DebugOptions::OFF);
    ir_out.push_str(&format!("{}\n", ctx.func.display()));
    if instrument {
        let count_fn = builtins.id(module, "aipl_count_insns");
        instrument_insn_count(module, &mut ctx.func, count_fn);
        let call_fn = builtins.id(module, "aipl_count_call");
        instrument_call_count(module, &mut ctx.func, id, call_fn)?;
    }
    module
        .define_function(id, ctx)
        .map_err(|e| Error::msg(format!("define eq helper: {e:?}")))?;
    ctx.clear();
    Ok(())
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
    str_data: &RefCell<StrLiterals>,
    elem_rc: &RefCell<ElemRc>,
    id: FuncId,
    ty: &ConcreteType,
    ir_out: &mut String,
    instrument: bool,
) -> Result<(), Error> {
    builtins.clear_func_cache();
    // Must match `tostr_func`'s declaration exactly: under the wide
    // representation a `str` result is a composite, written through a leading
    // sret pointer with nothing returned.
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // sret
    ctx.func.signature.params.push(AbiParam::new(types::I64)); // value
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let value = *builder
            .block_params(entry)
            .last()
            .expect("the rendered value is the final parameter");

        // `emit_render` only reads `builtins`/`structs`/`lit_ctr`/`elem_rc`; the
        // rest of `Cx` is irrelevant to rendering, so feed it trivial values.
        let env: Env = HashMap::new();
        let owned_params: HashSet<String> = HashSet::new();
        let unit = ConcreteType::Unit;
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
            str_data,
            elem_rc,
            ret_ty: &unit,
            sret: None,
            error_main: false,
            in_test: false,
            bindings: &no_bindings,
            // A generated helper is plain C-convention and never a tail-call
            // participant, so nothing inside it is ever in tail position.
            can_tail: false,
            tail: false,
        };

        // A boxed (recursive) type's helper renders its top level *inline* (via
        // `emit_render_named`) so that nested boxed children — rendered by
        // `emit_render` — route back through *this same* helper by call rather
        // than inlining the structure into itself. A non-boxed type renders
        // straight through `emit_render`.
        let render = |module: &mut M, builder: &mut FunctionBuilder, sink| {
            if is_boxed(ty, structs) {
                emit_render_named(module, builder, cx, value, ty, sink)
            } else {
                emit_render(module, builder, cx, value, ty, sink)
            }
        };

        // Pass 1: measure the total length.
        let len = render(module, &mut builder, Sink::Measure)?;

        // Wide representation: one shape, no small/large split. The value is
        // written into the caller's slot (`sret`, block param 0) and the
        // bytes into a fresh buffer — allocation gives *capacity*, so the
        // length is recorded with `aipl_str_grew` once the write pass is
        // done.
        //
        // No SSO here yet: a short result gets a buffer rather than being
        // packed into the value's 22 inline bytes. Correct, one allocation
        // more than necessary for short strings, and the obvious place to
        // optimize once the switch is the only path.
        let sret = builder.block_params(entry)[0];
        let built = builtins.call(module, &mut builder, "aipl_str_alloc", &[len]);
        let start = builtins.call(module, &mut builder, "aipl_str_write_ptr", &[built]);
        let cursor =
            builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        builder.ins().stack_store(types::I64, start, cursor, 0);
        render(module, &mut builder, Sink::Write(cursor))?;
        builtins.call_void(module, &mut builder, "aipl_str_grew", &[built, len]);
        copy_composite(
            &mut builder,
            sret,
            built,
            &ConcreteType::Primitive(Primitive::Str),
            structs,
        );
        builder.ins().return_(&[]);
        builder.finalize(module.target_config());
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        run_ir_passes(module, builtins, &mut ctx.func, DebugOptions::OFF);
        ir_out.push_str(&fix_data_ref_names(
            &ctx.func,
            &format!("{}\n", ctx.func.display()),
        ));
        if instrument {
            let count_fn = builtins.id(module, "aipl_count_insns");
            instrument_insn_count(module, &mut ctx.func, count_fn);
            let call_fn = builtins.id(module, "aipl_count_call");
            instrument_call_count(module, &mut ctx.func, id, call_fn)?;
        }
        module
            .define_function(id, ctx)
            .map_err(|e| Error::msg(format!("define to_str helper: {e:?}")))?;
        module.clear_context(ctx);
    }
    Ok(())
}

/// FNV-1a 64-bit hash of `bytes`, computed at compile time (the runtime
/// [`aipl_str_hash`] is the same fold over a live string's bytes). Used to name
/// static string-literal data symbols by content — see [`StrLiterals`].
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // offset basis
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
    }
    h
}

/// Interns static string-literal data objects by content across a whole
/// compilation. Identical literals — the same struct-field default materialized
/// at many construction sites, or the same text repeated anywhere — share one
/// data object (so the binary carries each distinct literal once). The symbol
/// name is a content hash (`__str_<hash>`), so a literal keeps its name when
/// unrelated source above it changes; the old span-based `__str_<start>_<end>`
/// name shifted on every earlier edit, churning the whole data section (and the
/// checked-in dogfood IR) for a change that touched none of the literals.
///
/// `used_names` guards the astronomically rare case of two *different* contents
/// hashing to the same name: the second is disambiguated with a numeric suffix
/// so it can never silently alias the first literal's bytes.
#[derive(Default)]
struct StrLiterals {
    by_content: HashMap<Box<[u8]>, DataId>,
    used_names: HashSet<String>,
}

impl StrLiterals {
    /// The data object for `content`, declaring and defining it on first sight
    /// and reusing it thereafter. `define` builds the static string bytes for a
    /// freshly-declared object; it is not called on a cache hit.
    fn intern<M: Module>(
        &mut self,
        module: &mut M,
        content: &[u8],
        define: impl FnOnce() -> Box<[u8]>,
    ) -> Result<DataId, Error> {
        if let Some(&id) = self.by_content.get(content) {
            return Ok(id);
        }
        // Distinct content, not yet interned. Pick a content-hash name unique to
        // it: on a hash collision with a different literal, extend the name until
        // free so `declare_data` mints a new object rather than aliasing.
        let base = format!("__str_{:016x}", fnv1a_64(content));
        let mut name = base.clone();
        let mut n: u32 = 0;
        while self.used_names.contains(&name) {
            n += 1;
            name = format!("{base}_{n}");
        }
        let id = module
            .declare_data(&name, Linkage::Local, false, false)
            .map_err(|e| Error::msg(format!("declare data: {e}")))?;
        let mut desc = DataDescription::new();
        // 8-byte align so the i64 header words read safely.
        desc.set_align(8);
        desc.define(define());
        module
            .define_data(id, &desc)
            .map_err(|e| Error::msg(format!("define data: {e}")))?;
        self.by_content.insert(content.into(), id);
        self.used_names.insert(name);
        Ok(id)
    }
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
    /// Content-interned static string-literal data objects (see [`StrLiterals`]),
    /// shared across every function in the compilation.
    str_data: &'a RefCell<StrLiterals>,
    /// On-demand cache of per-element-type array drop/retain helper functions
    /// (for element types the fixed runtime helpers don't cover — structs and
    /// struct/optional combinations). Declared here when first needed and
    /// *defined* after the main function loop (when the build context is free).
    elem_rc: &'a RefCell<ElemRc>,
    /// The enclosing function's ABI return type. Used by the `__map_result`
    /// intrinsic to reinterpret an in-place-mapped buffer as the result element
    /// type (the buffer's static type is the *input* element type).
    ret_ty: &'a ConcreteType,
    /// The enclosing function's hidden struct-return pointer, when it returns a
    /// composite (struct/optional/result). The `?` operator's early Err return
    /// copies the propagated error into it. `None` for a scalar/unit return.
    sret: Option<Value>,
    /// Whether the enclosing function is an `fn main() -> !Error` (its ABI return
    /// is the `i64` exit code, not a result). The `?` operator's early Err return
    /// then prints `error: <msg>` and returns exit code 1 instead of an sret copy.
    error_main: bool,
    /// Whether the enclosing function is a synthesized `.test` body (name prefixed
    /// `__test$`). The `?` operator then treats an Err by failing the current test
    /// (via `aipl_test_fail`) and returning from the unit test function, rather
    /// than propagating — so a test `?` needs no result/`!Error` enclosing return.
    in_test: bool,
    /// Sink for a source-variable legend: each named binding (param, `let`,
    /// `let mut`, `for` variable, `match` payload) records `(source name, its CLIF
    /// repr — `v<n>` for a value, `ss<n>` for a `mut` stack slot)`. Emitted as
    /// trailing `;` comments after the function so the printed IR is readable;
    /// cranelift's reader ignores the comments, so checked-in `.clif` still loads.
    /// Shared across the function's nested scopes (env clones spread `..cx`).
    bindings: &'a RefCell<Vec<(String, String)>>,
    /// Whether this body is the `tail`-convention `$tail` one — cranelift
    /// refuses a `return_call` from any other convention — *and* its ABI return
    /// is the body's own value (so handing the callee's result straight back is
    /// the whole epilogue). False disables tail calls in this function outright.
    /// See the "Tail calls" section.
    can_tail: bool,
    /// Whether the expression being compiled is in *tail position*: its value is
    /// the function's value, and nothing follows it but scope cleanup. Only ever
    /// true when `can_tail` is. [`compile_expr`] clears it before recursing, so
    /// each arm that genuinely passes tail position on (a `Seq`/`Let` body, both
    /// `if` branches, every `match` arm, a `return` operand) has to say so.
    tail: bool,
}

/// Per-element-type array drop/retain helpers, generated on demand. `fns` maps a
/// type name to its `(drop, retain)` function ids; `pending` lists the ones
/// still to be defined (with the element type to loop over).
#[derive(Default)]
struct ElemRc {
    fns: HashMap<ConcreteType, (FuncId, FuncId)>,
    pending: Vec<(ConcreteType, FuncId, FuncId)>,
    // Per-`(key, value)` drop/retain helpers for a dict's pair-array elements (a
    // pair is `[key][value]`, so its cleanup releases the key *and* the value).
    pair_fns: HashMap<(ConcreteType, ConcreteType), (FuncId, FuncId)>,
    pair_pending: Vec<(ConcreteType, ConcreteType, FuncId, FuncId)>,
    // Per-type `to_str` rendering helpers: `__to_str_<n>(value) -> str`. Maps a
    // type to its function id; `tostr_pending` lists the ones still to be
    // defined (with the type to render). One function per type, so the rendering
    // IR is generated once instead of inlined at every `to_str` site.
    tostr_fns: HashMap<ConcreteType, FuncId>,
    tostr_pending: Vec<(ConcreteType, FuncId)>,
    // Per-error-type test-fail helpers: `__test_try_fail_<n>(payload)`. A `?` on
    // an err inside a `.test` body calls this (passing the err payload) instead
    // of inlining the render + `aipl_test_fail` + rendered-str drop at every `?`
    // site — so each site is just "read payload, call helper". Keyed by the err
    // type; `test_fail_pending` lists the ones still to be defined.
    test_fail_fns: HashMap<ConcreteType, FuncId>,
    test_fail_pending: Vec<(ConcreteType, FuncId)>,
    // Per-type structural-equality helpers: `__eq_<n>(lv, rv) -> i64`. A `==`/`!=`
    // (or a nested comparison) on a composite type calls its helper instead of
    // inlining the whole structural comparison at every site. Keyed by the type
    // itself; `eq_pending` lists the ones still to be defined. Defining one can
    // request further helpers (its composite fields/elements), so the drain
    // loops until `eq_pending` is empty.
    eq_fns: HashMap<ConcreteType, FuncId>,
    eq_pending: Vec<(ConcreteType, FuncId)>,
    // Per-boxed-type payload drop helpers: `__rec_drop_<n>(payload_ptr)`. Stored
    // in each boxed block's header and called by the runtime when the block is
    // freed; it releases the payload's contained values (weak-dec'ing same-group
    // children, normal-dropping everything else). Keyed by the boxed type's
    // name. Defining one recurses only through function calls (never re-inlining
    // a recursive type), so a single drain suffices — but a body may request
    // *other* generated helpers (an array element helper for a `List[]` field),
    // so it's drained before those.
    rec_drop_fns: HashMap<String, FuncId>,
    rec_drop_pending: Vec<(String, FuncId)>,
    ctr: u32,
    // Every generated helper symbol handed out so far. The names carry the type
    // they serve (`__to_str_Point`), which reads far better in a disassembly or
    // a `--- performance ---` section than a bare counter — but two distinct
    // types can mangle to one fragment (`arr[]` and the generic instance
    // `arr<arr>` both give `arr$arr`), and `Module::declare_function` hands back
    // the *same* `FuncId` for a repeated name+signature, which would silently
    // fuse two types' helpers. So every name is checked here and disambiguated
    // with the counter if it's taken.
    used_symbols: HashSet<String>,
}

/// A symbol-safe fragment naming `ty`, for the generated per-type helpers
/// (`__to_str_<ty>`, `__eq_<ty>`, `__arr_drop_<ty>`, ...). Object-file symbols
/// can't carry the punctuation `type_name` uses, so each shape is spelled with
/// `$` — the separator mono already uses for generic instances (`Foo$i64`).
///
/// Built by recursing on the *type*, not by sanitizing `type_name`'s string:
/// stripping punctuation alone would erase the difference between a set and its
/// element (`#{str}` and `str` both reduce to `str`), and between a dict and a
/// two-field shape. Spelling the containers out keeps distinct types distinct
/// and reads better besides — `dict$str$i64$arr` over `str$i64$arr`.
fn type_symbol(ty: &ConcreteType) -> String {
    match ty {
        ConcreteType::Primitive(p) => p.name().to_string(),
        ConcreteType::Named(n) => sanitize_symbol(n),
        ConcreteType::Optional(inner) => format!("{}$opt", type_symbol(inner)),
        ConcreteType::Array(inner) => format!("{}$arr", type_symbol(inner)),
        ConcreteType::Set(inner) => format!("set${}", type_symbol(inner)),
        ConcreteType::Dict(k, v) => format!("dict${}${}", type_symbol(k), type_symbol(v)),
        ConcreteType::Result(ok, err) => format!("{}$err${}", type_symbol(ok), type_symbol(err)),
        ConcreteType::Fn(params, ret) => {
            let ps = params.iter().map(type_symbol).collect::<Vec<_>>().join("$");
            format!("fn${ps}$to${}", type_symbol(ret))
        }
        // Unit and the compiler pseudo-types. None of these reaches a generated
        // helper today, but naming them keeps this exhaustive, so a new `ConcreteType`
        // variant has to make a decision here rather than silently collapsing
        // onto another type's symbol.
        ConcreteType::Unit => "unit".to_string(),
        ConcreteType::NoneInner => "none".to_string(),
        ConcreteType::EmptyArrayArg => "empty_arr".to_string(),
        ConcreteType::NoneLiteralArg => "none_lit".to_string(),
        ConcreteType::ConcatStr => "concat_str".to_string(),
    }
}

/// A declared type's name, with anything an object-file symbol can't carry
/// replaced by `$`. Mono's synthetic names already use `$` (`Foo$i64`) and the
/// loader's per-file mangling uses `_` (`__m1__Point`), so in practice this
/// passes both through untouched.
fn sanitize_symbol(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '$'
            }
        })
        .collect()
}

impl ElemRc {
    /// A unique symbol for a generated helper: `<prefix><ty>`, falling back to
    /// `<prefix><ty>$<n>` when that name is already spoken for (see
    /// `used_symbols`).
    fn symbol(&mut self, prefix: &str, ty: &str) -> String {
        let mut name = format!("{prefix}{ty}");
        if !self.used_symbols.insert(name.clone()) {
            let n = self.ctr;
            self.ctr += 1;
            name = format!("{prefix}{ty}${n}");
            self.used_symbols.insert(name.clone());
        }
        name
    }
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
        inner: ConcreteType,
        some: usize,
        none: usize,
    },
    /// Variant: arm `i` is selected when the tag equals `arm_tags[i]`, and binds
    /// the case's payload `(offset, type)` fields.
    Variant {
        arm_tags: Vec<usize>,
        payloads: Vec<Vec<(u32, ConcreteType)>>,
    },
}

/// Validate a `match`'s arms against the scrutinee type and resolve each arm's
/// tag + payload layout. Mirrors the checker's exhaustiveness/arity rules (it's
/// a backstop, and codegen runs on monomorphized output the checker already saw).
fn plan_match(
    scrut_ty: &ConcreteType,
    arms: &[MatchArm],
    structs: &HashMap<String, TypeDef>,
    scrut_span: Span,
) -> Result<MatchPlan, Error> {
    match scrut_ty {
        ConcreteType::Optional(inner) => {
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
        ConcreteType::Result(ok, err) => {
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
        ConcreteType::Named(n) if structs.get(n).and_then(TypeDef::as_variant).is_some() => {
            let vl = structs[n].as_variant().expect("variant layout");
            let mut arm_tags = Vec::with_capacity(arms.len());
            let mut payloads = Vec::with_capacity(arms.len());
            let mut seen = HashSet::new();
            for arm in arms {
                // A pattern constructor may be variant-qualified (`Case@Variant`);
                // the scrutinee's type fixes the variant, so match on the bare case.
                let name = arm
                    .pattern
                    .ctor_name()
                    .unwrap_or("")
                    .split('@')
                    .next()
                    .unwrap_or("");
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
) -> Vec<(String, Value, ConcreteType)> {
    match plan {
        MatchPlan::Optional { inner, some, .. } => {
            if i != *some {
                return Vec::new(); // the `none` arm binds nothing
            }
            // Unwrap one optional layer: a non-optional core is read in place; a
            // nested optional is materialized in a fresh slot with `tag - 1`
            // (sharing the core value) — see "Optional representation".
            let value = if matches!(inner, ConcreteType::Optional(_)) {
                let core = opt_core(inner);
                let islot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    elem_size_of(inner, structs) as u32,
                    3,
                ));
                let ibase = builder.ins().stack_addr(types::I64, islot, 0);
                let dec = builder.ins().iadd_imm_s(tag, -1);
                builder.ins().store(MemFlagsData::trusted(), dec, ibase, 0);
                let core_val = component(builder, ptr, OPT_VALUE_OFFSET, core, structs);
                let va = builder.ins().iadd_imm_s(ibase, OPT_VALUE_OFFSET as i64);
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
/// fields)`. Every constructor arrives variant-qualified as `<ctor>@<variant>` —
/// the loader rewrites each in-scope reference to that form (`A@Shape`), and the
/// monomorphizer rewrites a generic constructor to its instance (`Some@Opt$i64`).
/// A bare name never resolves: it was never brought into scope.
fn variant_ctor(
    structs: &HashMap<String, TypeDef>,
    name: &str,
) -> Option<(String, usize, Vec<(u32, ConcreteType)>)> {
    let (ctor, inst) = name.split_once('@')?;
    let vl = structs.get(inst)?.as_variant()?;
    let (tag, case) = vl.case(ctor)?;
    let fields = case
        .fields
        .iter()
        .map(|f| (f.offset, f.ty.clone()))
        .collect();
    Some((inst.to_string(), tag, fields))
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
    fields: &[(u32, ConcreteType)],
    args: &[Expr],
    span: Span,
) -> Result<(Value, ConcreteType), Error> {
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
    let vty = ConcreteType::Named(vname.to_string());
    let size = cx.structs[vname].size();
    // A boxed (recursive) variant lives on the heap behind a refcounted block;
    // a normal one lives in a fresh stack slot. Either way `base` addresses the
    // `{tag, payload}` layout, so tag/field stores are identical below.
    let boxed = cx.structs[vname].boxed();
    let base = if boxed {
        let size_v = builder.ins().iconst(types::I64, size as i64);
        let drop_fn = rec_drop_fn_addr(builder, module, cx.elem_rc, vname);
        cx.builtins
            .call(module, builder, "aipl_rec_alloc", &[size_v, drop_fn])
    } else {
        let slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        builder.ins().stack_addr(types::I64, slot, 0)
    };
    let scc = boxed.then(|| cx.structs[vname].scc());
    let tag_v = builder.ins().iconst(types::I64, tag as i64);
    builder.ins().store(MemFlagsData::trusted(), tag_v, base, 0);
    for ((offset, fty), arg) in fields.iter().zip(args) {
        let before = scope_depth(scopes);
        let (v, actual) = compile_expr(module, builder, cx, scopes, arg)?;
        // A bare literal takes the payload field's int type.
        let actual = flex_int_ty(arg, &actual, fty);
        expect_type(&actual, fty, "constructor argument", arg.span.clone())?;
        let dst = builder.ins().iadd_imm_s(base, *offset as i64);
        store_array_elem(builder, dst, v, fty, cx.structs);
        // An *internal* field — one that (through optional/result layers) refers
        // to a boxed value of this same recursion group — is a weak reference:
        // retain it weakly and keep its strong-drop tracking, so the net at scope
        // exit converts the argument's external (strong) reference into the
        // parent's internal (weak) one. The move optimization would cancel both
        // and leave the child strong-pinned, so it's disabled for these.
        let internal = scc.is_some_and(|g| contains_scc_ref(fty, g, cx.structs));
        if internal {
            emit_rc_w(
                builder,
                module,
                cx.builtins,
                cx.structs,
                v,
                fty,
                RcOp::Retain,
                scc,
            );
        } else if !move_owned_temp(scopes, before, v) {
            // The variant co-owns each external heap payload field. A fresh temp
            // is moved in (skip retain, untrack); a borrow is co-owned via retain.
            emit_retain(builder, module, cx.builtins, cx.structs, v, fty);
        }
    }
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

/// Which end a `starts_with`/`ends_with`/`starts_with_at` call matches against,
/// and whether it carries an explicit offset. `Starts` and `At` are the same
/// comparison — `At` just takes its start from a trailing argument instead of 0,
/// which is exactly what makes it the fused form of `xs[at..].starts_with(p)`.
#[derive(Clone, Copy, PartialEq)]
enum SeEnd {
    Starts,
    At,
    Ends,
}

impl SeEnd {
    /// Argument count: the receiver and pattern, plus `At`'s offset.
    fn arity(self) -> usize {
        if self == SeEnd::At {
            3
        } else {
            2
        }
    }
}

/// Parse a (possibly shape-suffixed) `starts_with`/`starts_with_at`/`ends_with`
/// builtin name into `(end, shape)`, or `None` if it isn't one. Mono appends
/// `$ve`/`$vo` for the element/optional monomorphizations; the bare name is the
/// sequence form.
fn starts_ends_variant(name: &str) -> Option<(SeEnd, SeShape)> {
    let (base, shape) = if let Some(b) = name.strip_suffix("$ve") {
        (b, SeShape::Elem)
    } else if let Some(b) = name.strip_suffix("$vo") {
        (b, SeShape::Opt)
    } else {
        (name, SeShape::Seq)
    };
    match base {
        "__builtin_starts_with" => Some((SeEnd::Starts, shape)),
        "__builtin_starts_with_at" => Some((SeEnd::At, shape)),
        "__builtin_ends_with" => Some((SeEnd::Ends, shape)),
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
fn emit_char_to_str<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    c: Value,
) -> Value {
    // A wide inline value is spread across all three words, so it is built by
    // the runtime rather than open-coded here; `Builtins::call` allocates the
    // out slot and hands back its address.
    return cx.builtins.call(module, builder, "aipl_char_to_str", &[c]);
}

/// `arr.starts_with(x)` / `arr.starts_with_at(x, at)` / `arr.ends_with(x)` for a
/// single element `x` of type `elem` — the `$ve` (element) monomorphization.
/// True iff the element `end` selects — the first, the one at `at`, or the last
/// — exists and structurally equals `x`, with no intermediate array built.
/// Borrows `arr`; `emit_eq` balances its own per-element refs.
fn emit_arr_starts_ends_elem<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    arr: Value,
    elem_val: Value,
    elem: &ConcreteType,
    end: SeEnd,
    at: Option<Value>,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let len = load_arr_len(builder, arr);
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(types::I64, zero, res, 0);
    // `at` clamps to 0 like a slice bound (a negative offset slices from the
    // start); the upper end is the in-range test below.
    let lo = at.map(|at| builder.ins().smax(at, zero));
    let inrange = match end {
        // The first element exists iff the array is non-empty — and so does the
        // last, whose index is `len - 1`.
        SeEnd::Starts | SeEnd::Ends => builder.ins().icmp(IntCC::SignedGreaterThan, len, zero),
        // An offset at or past the end has no element there, so a one-element
        // pattern can't match — the same answer `self[at..]` being empty gives.
        SeEnd::At => {
            let lo = lo.expect("a `starts_with_at` call supplies its offset");
            builder.ins().icmp(IntCC::SignedLessThan, lo, len)
        }
    };
    let chk = builder.create_block();
    let merge = builder.create_block();
    builder.ins().brif(inrange, chk, &[], merge, &[]);
    builder.switch_to_block(chk);
    builder.seal_block(chk);
    let idx = match end {
        SeEnd::Starts => zero,
        SeEnd::At => lo.expect("a `starts_with_at` call supplies its offset"),
        SeEnd::Ends => builder.ins().iadd_imm_s(len, -1),
    };
    let e = load_array_elem(module, builder, builtins, arr, idx, elem, structs);
    let eq = emit_eq(module, builder, cx, e, elem_val, elem)?;
    builder.ins().stack_store(types::I64, eq, res, 0);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, types::I64, res, 0))
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
    elem: &ConcreteType,
) -> Result<Value, Error> {
    let Cx {
        structs, builtins, ..
    } = cx;
    let len = load_arr_len(builder, arr);
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(types::I64, zero, res, 0);
    let idx = i64_slot(builder);
    builder.ins().stack_store(types::I64, zero, idx, 0);
    let header = builder.create_block();
    let body = builder.create_block();
    let found = builder.create_block();
    let merge = builder.create_block();
    builder.ins().jump(header, &[]);

    builder.switch_to_block(header);
    let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
    let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
    builder.ins().brif(more, body, &[], merge, &[]);

    builder.switch_to_block(body);
    builder.seal_block(body);
    let e = load_array_elem(module, builder, builtins, arr, i, elem, structs);
    let eq = emit_eq(module, builder, cx, e, elem_val, elem)?;
    let cont = builder.create_block();
    builder.ins().brif(eq, found, &[], cont, &[]);
    builder.switch_to_block(cont);
    builder.seal_block(cont);
    let next = builder.ins().iadd_imm_s(i, 1);
    builder.ins().stack_store(types::I64, next, idx, 0);
    builder.ins().jump(header, &[]);
    builder.seal_block(header);

    builder.switch_to_block(found);
    builder.seal_block(found);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(types::I64, one, res, 0);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, types::I64, res, 0))
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
    elem: &ConcreteType,
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
        let empty = builder.ins().icmp_imm_s(IntCC::Equal, lb, 0);
        return Ok(builder.ins().uextend(types::I64, empty));
    }
    let res = i64_slot(builder);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().stack_store(types::I64, zero, res, 0);
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
    builder.ins().stack_store(types::I64, zero, start, 0);
    let idx = i64_slot(builder);
    let outer_header = builder.create_block();
    let outer_body = builder.create_block();
    let inner_header = builder.create_block();
    let inner_body = builder.create_block();
    let next_start = builder.create_block();
    let found = builder.create_block();
    builder.ins().jump(outer_header, &[]);

    builder.switch_to_block(outer_header);
    let s = builder.ins().stack_load(types::I64, types::I64, start, 0);
    let more_windows = builder.ins().icmp(IntCC::SignedLessThanOrEqual, s, limit);
    builder
        .ins()
        .brif(more_windows, outer_body, &[], merge, &[]);

    builder.switch_to_block(outer_body);
    builder.seal_block(outer_body);
    builder.ins().stack_store(types::I64, zero, idx, 0);
    builder.ins().jump(inner_header, &[]);

    builder.switch_to_block(inner_header);
    let i = builder.ins().stack_load(types::I64, types::I64, idx, 0);
    let more_elems = builder.ins().icmp(IntCC::SignedLessThan, i, lb);
    // The whole needle matched at this window — found.
    builder.ins().brif(more_elems, inner_body, &[], found, &[]);

    builder.switch_to_block(inner_body);
    builder.seal_block(inner_body);
    let si = builder.ins().iadd(s, i);
    let el = load_array_elem(module, builder, builtins, self_ptr, si, elem, structs);
    let er = load_array_elem(module, builder, builtins, other_ptr, i, elem, structs);
    let ee = emit_eq(module, builder, cx, el, er, elem)?;
    let inner_cont = builder.create_block();
    builder.ins().brif(ee, inner_cont, &[], next_start, &[]);
    builder.switch_to_block(inner_cont);
    builder.seal_block(inner_cont);
    let next_i = builder.ins().iadd_imm_s(i, 1);
    builder.ins().stack_store(types::I64, next_i, idx, 0);
    builder.ins().jump(inner_header, &[]);
    builder.seal_block(inner_header);

    builder.switch_to_block(next_start);
    builder.seal_block(next_start);
    let next_s = builder.ins().iadd_imm_s(s, 1);
    builder.ins().stack_store(types::I64, next_s, start, 0);
    builder.ins().jump(outer_header, &[]);
    builder.seal_block(outer_header);

    builder.switch_to_block(found);
    builder.seal_block(found);
    let one = builder.ins().iconst(types::I64, 1);
    builder.ins().stack_store(types::I64, one, res, 0);
    builder.ins().jump(merge, &[]);
    builder.switch_to_block(merge);
    builder.seal_block(merge);
    Ok(builder.ins().stack_load(types::I64, types::I64, res, 0))
}

/// Compile an *indirect* call `f(args)` where `f` is a local holding a runtime
/// function value (a `(ptys) -> ret` code address). Mirrors [`compile_call`]'s
/// ABI — borrow-retain each heap arg, sret a composite result — but dispatches
/// through `call_indirect` on the loaded address rather than a known callee. No
/// effect check: only effect-free functions can become values (enforced by the
/// checker), so an indirect call performs none.
fn compile_indirect_call<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    name: &str,
    ptys: &[ConcreteType],
    ret: &ConcreteType,
    args: &[Expr],
    span: Span,
) -> Result<(Value, ConcreteType), Error> {
    let Cx {
        env,
        structs,
        builtins,
        ..
    } = cx;
    if ptys.len() != args.len() {
        return Err(Error::at(
            format!(
                "function value {name:?} expects {} arg(s), got {}",
                ptys.len(),
                args.len()
            ),
            span.clone(),
        ));
    }
    let (callee_addr, _) = env_load(builder, name, env, span.clone())?;
    let mut arg_values = Vec::with_capacity(args.len());
    let mut arg_fresh = Vec::with_capacity(args.len());
    for (idx, (arg, expected)) in args.iter().zip(ptys).enumerate() {
        let before = scope_depth(scopes);
        let (v, actual) = compile_expr(module, builder, cx, scopes, arg)?;
        let actual = flex_int_ty(arg, &actual, expected);
        expect_type(
            &actual,
            expected,
            &format!("function value {name:?} arg {idx}"),
            arg.span.clone(),
        )?;
        let v = coerce_empty_to_char_array(builder, module, builtins, scopes, v, &actual, expected);
        arg_fresh.push(owned_temp_since(scopes, before, v));
        arg_values.push(v);
    }
    // Borrow semantics: retain each heap arg so refcounts stay balanced (the
    // callee decrements it on return, like a borrowed direct-call parameter) —
    // unless the arg is a fresh temporary we own, which we move in instead
    // (transfer our sole ref, drop its tracking; the callee's return-dec frees it).
    for (idx, (v, expected)) in arg_values.iter().zip(ptys).enumerate() {
        if is_heap(expected) {
            hand_off_arg(
                builder,
                module,
                builtins,
                structs,
                scopes,
                *v,
                expected,
                arg_fresh[idx],
            );
        }
    }
    let sret = sret_size(ret, structs).map(|size| {
        let slot = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        builder.ins().stack_addr(types::I64, slot, 0)
    });
    let call_args: Vec<Value> = sret.iter().copied().chain(arg_values).collect();
    let sig = fn_value_signature(module, ptys, ret, structs);
    let sigref = builder.import_signature(sig);
    let inst = builder.ins().call_indirect(sigref, callee_addr, &call_args);
    let ret_v = if let Some(s) = sret {
        s
    } else if is_unit(ret) {
        builder.ins().iconst(types::I64, 0)
    } else {
        builder.inst_results(inst)[0]
    };
    if needs_drop(ret, structs) {
        scopes
            .last_mut()
            .expect("scope")
            .push(Tracked::new(ret_v, ret));
    }
    Ok((ret_v, ret.clone()))
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
) -> Result<(Value, ConcreteType), Error> {
    let Cx {
        structs,
        builtins,
        effects: current_effects,
        ..
    } = cx;
    // Tail position belongs to *this* call, never to its arguments — an argument
    // is evaluated and then handed over, so a call inside one has the whole
    // hand-off still ahead of it. Take the flag and clear it from the context
    // every sub-expression below sees, so nothing can inherit it by accident
    // (`1 + f(x)` in tail position is exactly this trap: `f(x)` is an argument of
    // the `+`, and eliding the `+` by tail-calling `f` silently drops the add).
    let tail = cx.tail;
    let cx = Cx { tail: false, ..cx };
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
    let mut arg_fresh = Vec::with_capacity(args.len());
    for (idx, (arg, p)) in args.iter().zip(info.params.iter()).enumerate() {
        let expected = &p.ty;
        let before = scope_depth(scopes);
        let (v, actual) = compile_expr(module, builder, cx, scopes, arg)?;
        // A bare literal argument flexes to a narrow-int parameter (its
        // i64-register value is already canonical when it fits — checker-verified).
        let actual = flex_int_ty(arg, &actual, expected);
        expect_type(
            &actual,
            expected,
            &format!("fn {disp:?} arg {idx}"),
            arg.span.clone(),
        )?;
        let v = coerce_empty_to_char_array(builder, module, builtins, scopes, v, &actual, expected);
        // Whether this arg is a fresh temporary we exclusively own, captured now
        // before later args grow the scope past its tracking entry (the hand-off
        // below runs after all args are evaluated).
        arg_fresh.push(owned_temp_since(scopes, before, v));
        arg_values.push(v);
    }
    // Hand each heap (str/array) arg to the callee. A borrowed param is retained
    // (the callee decs it on return, the caller keeps its own ref). An *owned*
    // param, or any arg that is a fresh temporary we own, is instead moved: we
    // transfer our sole reference (no inc) and stop tracking it, since the callee
    // accounts for that ref — an owned param via the local it's moved into, a
    // borrowed param via its return-dec. A fresh temp is anonymous, so it's dead
    // in the caller after the call.
    //
    // The fresh-temp move is restricted to *user* callees: their heap params obey
    // the uniform borrow protocol (exactly one epilogue dec), so passing our sole
    // ref is balanced. A builtin's per-argument contract varies (many consume via
    // "caller pre-incs"; a str concat/rope builds a view that keeps referencing
    // its operands), so it isn't guaranteed to free exactly the one ref we'd hand
    // it — retain into builtins as before. (Owned params never apply to builtins.)
    for (idx, (v, p)) in arg_values.iter().zip(info.params.iter()).enumerate() {
        // Not retained: either the parameter owns no heap, or it is a pure
        // borrow — an inspect-only heap parameter, or a boxed one outside a
        // tail-call participant — that the callee will not release. Hand it over
        // untouched: our own reference (a live binding, an owning container, or
        // a fresh temporary still tracked by this scope) keeps it alive for the
        // whole call, and the retain/release pair we'd otherwise emit cancels. A
        // fresh temporary deliberately keeps its tracking entry here (it is
        // *not* moved), since nothing in the callee will free it.
        if !p.retained() && !p.owned {
            continue;
        }
        let is_builtin = matches!(info.link, FuncLink::Builtin(_));
        let moved = p.owned || (arg_fresh[idx] && !is_builtin);
        hand_off_arg(builder, module, builtins, structs, scopes, *v, &p.ty, moved);
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
    // Every call to a participant targets its `$tail` body (see `callee_id`
    // below), not just the tail ones — so a wide `str` argument is split into
    // its three words here, once, rather than at the `return_call`.
    let call_args: Vec<Value> = if info.tail_id.is_some() {
        let lead = call_args.len() - info.params.len();
        let mut split: Vec<Value> = call_args[..lead].to_vec();
        for (p, a) in info.params.iter().zip(&call_args[lead..]) {
            if tail_passes_str_by_value(true, &p.ty) {
                split.extend(str_value_words(builder, *a));
            } else {
                split.push(*a);
            }
        }
        split
    } else {
        call_args
    };
    // A user function is already declared; a builtin's import is declared lazily
    // here on first reference. A tail-call participant's real body lives in its
    // `$tail` declaration — call that directly and leave the exported trampoline
    // to the FFI and to `func_addr`.
    let callee_id = match info.link {
        FuncLink::User(id) => info.tail_id.unwrap_or(id),
        // Through `active_sym` like any other runtime call: this path builds the
        // call itself rather than going via `Builtins::call`, so without it a
        // builtin would keep the old entry point while its argument became an
        // address.
        FuncLink::Builtin(sym) => builtins.id(module, sym),
    };
    let local_callee = module.declare_func_in_func(callee_id, builder.func);

    // Tail call: this call's value *is* the enclosing function's value, and both
    // ends carry `CallConv::Tail`, so hand the frame over instead of stacking a
    // new one. `cx.tail` is only ever set inside a `$tail` body (see `can_tail`),
    // and the two return-shape checks re-derive at the site what
    // `tail_call_plan` decided for the pair — cranelift requires a
    // `return_call`'s results to match the caller's signature exactly.
    //
    // The scope release has to happen *before* the transfer, since the frame is
    // gone afterwards. That is safe here and only here: every argument that owns
    // heap has been retained or moved above (`ParamInfo::retained`, which a
    // participant's parameters are all forced into), so it holds a reference of
    // its own, and releasing any *other* reference can never take it — that is
    // exactly the invariant a refcount maintains. Arguments that were moved in
    // are no longer tracked, so they are not released here at all.
    if tail
        && info.tail_id.is_some()
        && sret.is_none()
        && sret_size(cx.ret_ty, structs).is_none()
        && is_unit(cx.ret_ty) == is_unit(&info.return_ty)
    {
        for scope in scopes.iter() {
            for t in scope {
                let v = match t.owned {
                    Owned::Value(v) => v,
                    Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
                };
                emit_drop(builder, module, builtins, structs, v, &t.ty);
            }
        }
        builder.ins().return_call(local_callee, &call_args);
        // Unreachable continuation: the enclosing `if`/`match` arm still emits
        // its merge jump (and the epilogue its return), which compile into here
        // and are dropped as dead code — the same shape `ExprKind::Return` uses.
        let dead = builder.create_block();
        builder.switch_to_block(dead);
        builder.seal_block(dead);
        let placeholder = builder.ins().iconst(types::I64, 0);
        return Ok((placeholder, info.return_ty.clone()));
    }

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
) -> Result<(Value, ConcreteType), Error> {
    let Cx {
        env,
        funcs,
        structs,
        builtins,
        effects: _,
        owned_params: _,
        lit_ctr: _,
        str_data: _,
        elem_rc: _,
        ret_ty: _,
        sret: _,
        error_main: _,
        in_test: _,
        bindings: _,
        can_tail: _,
        tail,
    } = cx;
    // Only the ordinary-call arm at the bottom can become a tail call: the
    // intrinsic arms below compile sub-expressions and then do further work with
    // the result, so tail position must not reach them. Clear it here and put it
    // back on the one hand-off that can honour it.
    let cx = Cx { tail: false, ..cx };
    // A free call whose callee is a local holding a function value is an
    // indirect call (mono resolved HOF-parameter calls to concrete callees, so
    // any surviving call through a `ConcreteType::Fn` binding is a runtime one).
    if !style {
        if let Some(ConcreteType::Fn(ptys, ret)) = env.get(name).map(env_binding_type) {
            return compile_indirect_call(
                module, builder, cx, scopes, name, &ptys, &ret, args, span,
            );
        }
    }
    Ok(match name {
        "__builtin_wrapping_add"
        | "__builtin_saturating_add"
        | "__builtin_wrapping_sub"
        | "__builtin_saturating_sub"
        | "__builtin_wrapping_mul" => {
            // `a + b` / `a - b` / `a * b` resolved (in the loader) to their bound
            // integer arithmetic builtin. Both operands are the same integer type
            // (checker-verified); a bare literal flexes to the other's width. The
            // flavor (wrapping/saturating) and operation (add/sub/mul) are the only
            // differences — see `emit_int_addsub` (multiply is wrapping-only).
            // Scalar ints carry no refcount, so there's nothing to track.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("{name:?} expects 2 arguments, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (lv, lt) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (rv, rt) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let lt = flex_int_ty(&args[0], &lt, &rt);
            let rt = flex_int_ty(&args[1], &rt, &lt);
            let p = match (&lt, &rt) {
                (ConcreteType::Primitive(a), ConcreteType::Primitive(b))
                    if a.is_int() && a == b =>
                {
                    *a
                }
                _ => {
                    expect_type(
                        &lt,
                        &ConcreteType::Primitive(Primitive::I64),
                        "arithmetic operand",
                        args[0].span.clone(),
                    )?;
                    expect_type(
                        &rt,
                        &ConcreteType::Primitive(Primitive::I64),
                        "arithmetic operand",
                        args[1].span.clone(),
                    )?;
                    Primitive::I64
                }
            };
            let out = if name.ends_with("_mul") {
                // Wrapping multiply: compute in i64 and re-canonicalize to the
                // width (dropping any out-of-range bits), like wrapping add/sub.
                let raw = builder.ins().imul(lv, rv);
                canon_int(builder, raw, p)
            } else {
                let sub = name.ends_with("_sub");
                let saturating = name.starts_with("__builtin_saturating_");
                emit_int_addsub(builder, lv, rv, p, sub, saturating)
            };
            (out, ConcreteType::Primitive(p))
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
            (s, ConcreteType::Primitive(Primitive::Str))
        }
        "__template_interp" => {
            // Template-literal interpolation: pass `str` through as-is, widen a
            // `char` to the one-char `str` holding it, and convert any other
            // type via `to_str`. Both special cases exist for the same reason —
            // `to_str` renders a text scalar Debug-style (`"s"`, `'c'`), and an
            // interpolation wants the text, not its literal form.
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
            let s = if is_str_repr(&t) {
                v
            } else if t == ConcreteType::Primitive(Primitive::Char) {
                // An inline one-char `str`: no allocation and no refcount, so
                // there is nothing to track for release.
                emit_char_to_str(module, builder, cx, v)
            } else {
                emit_to_str(module, builder, cx, scopes, v, &t)?
            };
            (s, ConcreteType::Primitive(Primitive::Str))
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
            (h, ConcreteType::Primitive(Primitive::I64))
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
                ConcreteType::Array(e) => (**e).clone(),
                _ => {
                    return Err(Error::at(
                        format!("{:?} of one argument expects an array", display_name(name)),
                        args[0].span.clone(),
                    ))
                }
            };
            let opt_ty = ConcreteType::Optional(Box::new(elem.clone()));
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
                .store(MemFlagsData::trusted(), zero, result_ptr, 0);
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
            builder.ins().stack_store(types::I64, acc0, acc_slot, 0);
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().stack_store(types::I64, one, i_slot, 0);
            let header = builder.create_block();
            let body = builder.create_block();
            let after = builder.create_block();
            builder.ins().jump(header, &[]);

            builder.switch_to_block(header);
            let i = builder.ins().stack_load(types::I64, types::I64, i_slot, 0);
            let more = builder.ins().icmp(IntCC::SignedLessThan, i, len);
            builder.ins().brif(more, body, &[], after, &[]);

            builder.switch_to_block(body);
            builder.seal_block(body);
            let e = seq_elem(module, builder, builtins, structs, arr_ptr, i, &arr_ty);
            let acc = builder
                .ins()
                .stack_load(types::I64, types::I64, acc_slot, 0);
            let cc = if is_min {
                IntCC::SignedLessThan
            } else {
                IntCC::SignedGreaterThan
            };
            let take = builder.ins().icmp(cc, e, acc);
            let new_acc = builder.ins().select(take, e, acc);
            builder.ins().stack_store(types::I64, new_acc, acc_slot, 0);
            let inext = builder.ins().iadd_imm_s(i, 1);
            builder.ins().stack_store(types::I64, inext, i_slot, 0);
            builder.ins().jump(header, &[]);
            builder.seal_block(header);

            builder.switch_to_block(after);
            builder.seal_block(after);
            let acc = builder
                .ins()
                .stack_load(types::I64, types::I64, acc_slot, 0);
            emit_build_some(builder, result_ptr, acc, &elem, structs);
            builder.ins().jump(done, &[]);

            builder.switch_to_block(done);
            builder.seal_block(done);
            (result_ptr, opt_ty)
        }
        "__builtin_min" | "__builtin_max" => {
            // `min(a, b)` / `max(a, b)` over any `ord` type — every integer
            // width, `char`, or `str` — comparing and selecting the smaller or
            // larger. Scalars compare with `icmp` under the operand type's own
            // signedness; strings order lexicographically through the runtime
            // `aipl_str_cmp`, exactly as `<`/`>` do.
            let disp = display_name(name);
            if args.len() != 2 {
                return Err(Error::at(
                    format!("{disp:?} expects 2 arguments, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (a, at) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (b, bt) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // A bare integer literal flexes to the other side's width, so
            // `n.max(1)` needs no conversion on the `1` whatever `n` is.
            let at = flex_int_ty(&args[0], &at, &bt);
            let bt = flex_int_ty(&args[1], &bt, &at);
            let want = if name == "__builtin_min" {
                "min"
            } else {
                "max"
            };
            let is_str = is_str_repr(&at) && is_str_repr(&bt);
            if !is_str {
                if at != bt {
                    return Err(Error::at(
                        format!(
                            "{want:?} between {} and {}: both sides must be the same type",
                            type_name(&at),
                            type_name(&bt),
                        ),
                        span.clone(),
                    ));
                }
                if !matches!(&at, ConcreteType::Primitive(p) if p.is_int() || *p == Primitive::Char)
                {
                    return Err(Error::at(
                        format!(
                            "{want:?} compares integers, chars or strs, not {}",
                            type_name(&at)
                        ),
                        span.clone(),
                    ));
                }
            }
            // `min`: keep `a` when `a < b`; `max`: keep `a` when `a > b`.
            let want_min = name == "__builtin_min";
            let cond = if is_str {
                // `aipl_str_cmp` returns a sign; test it against zero.
                let c = builtins.call(module, builder, "aipl_str_cmp", &[a, b]);
                let cc = if want_min {
                    IntCC::SignedLessThan
                } else {
                    IntCC::SignedGreaterThan
                };
                builder.ins().icmp_imm_s(cc, c, 0)
            } else {
                let signed = match &at {
                    ConcreteType::Primitive(p) if p.is_int() => p.int_signed(),
                    _ => false, // `char` is a byte — unsigned
                };
                let cc = match (want_min, signed) {
                    (true, true) => IntCC::SignedLessThan,
                    (true, false) => IntCC::UnsignedLessThan,
                    (false, true) => IntCC::SignedGreaterThan,
                    (false, false) => IntCC::UnsignedGreaterThan,
                };
                builder.ins().icmp(cc, a, b)
            };
            let r = builder.ins().select(cond, a, b);
            let result_ty = at;
            // The result aliases whichever operand won, and both are tracked by
            // the enclosing scope; take an independently-owned ref and track it,
            // as `value_or` does for the same select-one-of-two shape. Scalars
            // own nothing, so this is a no-op for them.
            if needs_drop(&result_ty, structs) {
                emit_retain(builder, module, builtins, structs, r, &result_ty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(r, &result_ty));
            }
            (r, result_ty)
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
                &ConcreteType::Primitive(Primitive::Str),
                "split receiver",
                args[0].span.clone(),
            )?;
            let (sep_v, sep_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            expect_type(
                &sep_t,
                &ConcreteType::Primitive(Primitive::Str),
                "split separator",
                args[1].span.clone(),
            )?;
            // Tagged `aipl_str_split` consumes both; `aipl_str_split` borrows.
            let result = builtins.call(module, builder, "aipl_str_split", &[s_v, sep_v]);
            let ty = ConcreteType::Array(Box::new(ConcreteType::Primitive(Primitive::Str)));
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
                &ConcreteType::Primitive(Primitive::Str),
                "read_file_to_string filename",
                args[0].span.clone(),
            )?;
            // The runtime consumes (decs) the filename ref, so inc to keep our
            // scope-tracked ref alive.
            let result_ty = ConcreteType::Result(
                Box::new(ConcreteType::Primitive(Primitive::Str)),
                Box::new(error_ty()),
            );
            // The contents come back through an out pointer this call site
            // allocates, and the success flag is the return value — there is no
            // null `str` to test. Borrows the filename, so no compensating inc.
            let ptr = {
                let out = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    str24::STR_SIZE as u32,
                    3,
                ));
                let out_addr = builder.ins().stack_addr(types::I64, out, 0);
                let ok = builtins.call(
                    module,
                    builder,
                    "aipl_read_file_to_string",
                    &[out_addr, name_v],
                );
                emit_file_result_split(
                    module,
                    builder,
                    cx,
                    ok,
                    Some((out_addr, ConcreteType::Primitive(Primitive::Str))),
                    b"could not read file",
                )?
            };
            // The ok payload is a fresh, owned str (the err is a static literal):
            // track so it's released at scope exit (drop decs it only on tag 1).
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &result_ty));
            (ptr, result_ty)
        }
        "__builtin_now_nanos" | "__builtin_monotonic_now" => {
            // `() -> u64`: the runtime reads a clock (wall or monotonic) and
            // returns the nanosecond count on the shared i64 ABI. Nothing is
            // allocated or consumed, so there is no refcount traffic and nothing
            // to track for scope release. The `!clock` effect is
            // checker-enforced.
            let (sym, pretty) = if name == "__builtin_now_nanos" {
                ("aipl_now_nanos", "now_nanos")
            } else {
                ("aipl_monotonic_now", "monotonic_now")
            };
            if !args.is_empty() {
                return Err(Error::at(
                    format!("{pretty} expects no args, got {}", args.len()),
                    span.clone(),
                ));
            }
            // Both are shimmable operations, so the call routes through the
            // shim slot: an installed shim wins, otherwise the real clock.
            let v = emit_shimmable_call(module, builder, builtins, pretty, sym);
            (v, ConcreteType::Primitive(Primitive::U64))
        }
        "__builtin_list_files" => {
            // `(str) -> str[]!str`: ok(paths) on success, err(message) on any
            // failure. The runtime returns a fresh array pointer or null (an
            // empty listing is still a real, non-null array block, so it reads
            // back as `ok([])`); codegen wraps it into the Result with a static
            // error message. The `!list_files` effect is checker-enforced.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("list_files expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (dir_v, dir_t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            expect_type(
                &dir_t,
                &ConcreteType::Primitive(Primitive::Str),
                "list_files directory",
                args[0].span.clone(),
            )?;
            // The tagged runtime consumes (decs) the directory ref, so inc to
            // keep our scope-tracked ref alive; `aipl_list_files` borrows.
            let raw = builtins.call(module, builder, "aipl_list_files", &[dir_v]);
            let result_ty = ConcreteType::Result(
                Box::new(ConcreteType::Array(Box::new(ConcreteType::Primitive(
                    Primitive::Str,
                )))),
                Box::new(error_ty()),
            );
            let ptr = emit_file_result(
                module,
                builder,
                cx,
                raw,
                // A `str[]` payload: an 8-byte array pointer under either
                // representation — it is the *err* side that widened.
                Some(ConcreteType::Array(Box::new(ConcreteType::Primitive(
                    Primitive::Str,
                )))),
                b"could not list files",
            )?;
            // The ok payload is a fresh, owned array (the err is a static
            // literal): track so it's released at scope exit (drop decs the
            // array — and cascades to its strs — only on tag 1).
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
                &ConcreteType::Primitive(Primitive::Str),
                "write_string_to_file path",
                args[0].span.clone(),
            )?;
            let (data_v, data_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            expect_type(
                &data_t,
                &ConcreteType::Primitive(Primitive::Str),
                "write_string_to_file contents",
                args[1].span.clone(),
            )?;
            // The runtime consumes (decs) both str args, so inc to keep ours alive.
            // Tagged consumes both refs; `aipl_write_string_to_file` borrows.
            let code = builtins.call(
                module,
                builder,
                "aipl_write_string_to_file",
                &[path_v, data_v],
            );
            let result_ty =
                ConcreteType::Result(Box::new(ConcreteType::Unit), Box::new(error_ty()));
            let ptr = emit_file_result(module, builder, cx, code, None, b"could not write file")?;
            // Neither payload needs freeing (ok is unit, err is a static literal),
            // so no scope tracking is required.
            (ptr, result_ty)
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
            if !matches!(&recv_ty, ConcreteType::Optional(_)) {
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
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), ptr, 0);
            let nz = builder.ins().icmp_imm_s(IntCC::NotEqual, tag, 0);
            let b = builder.ins().uextend(types::I64, nz);
            (b, ConcreteType::Primitive(Primitive::Bool))
        }
        "__builtin_same_case" => {
            // `a.same_case(b)`: do these share a constructor, payloads ignored?
            //
            // The `variant` bound guarantees both are named variants, and a
            // variant's tag is the leading `i64` of its value — the same field
            // `emit_render_variant` reads to pick a case. So this is two loads
            // and a compare, and it never touches the payload: that is the whole
            // point, since a generic driver cannot name the payload's type.
            //
            // Both operands are borrowed, like `hash` — nothing is consumed, so
            // their scope-tracking is untouched.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"same_case\" expects 2 arguments, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (a, a_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (b, b_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // The bound is enforced by the checker; this catches a caller that
            // reached codegen another way (a synthesized call, say) rather than
            // silently reading a tag off something that has none.
            for (t, e) in [(&a_ty, &args[0]), (&b_ty, &args[1])] {
                let is_variant = matches!(t, ConcreteType::Named(n)
                    if cx.structs.get(n).and_then(TypeDef::as_variant).is_some());
                if !is_variant {
                    return Err(Error::at(
                        format!(
                            "\"same_case\" is only callable on a variant, got {}",
                            type_name(t)
                        ),
                        e.span.clone(),
                    ));
                }
            }
            let ta = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), a, 0);
            let tb = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), b, 0);
            let eq = builder.ins().icmp(IntCC::Equal, ta, tb);
            let out = builder.ins().uextend(types::I64, eq);
            (out, ConcreteType::Primitive(Primitive::Bool))
        }
        "__builtin_case_name" => {
            // `v.case_name()`: the name of the case `v` was built with, as a
            // `str`. Like `same_case`, this reads only the tag — the leading
            // `i64` of a variant value, the same field `emit_render_variant`
            // switches on — so it never touches the payload, which is what lets
            // one implementation serve every variant.
            //
            // It is `to_str` with the payload left off, and that is the point:
            // `to_str(v)` renders `Name("x")`, so callers wanting just the case
            // had to render the payload and then cut the string back at the
            // `(`. Here the tag picks a constant directly.
            //
            // The receiver is borrowed, like `same_case` — nothing is consumed,
            // so its scope tracking is untouched.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"case_name\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (v, v_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            // The bound is enforced by the checker; this catches a caller that
            // reached codegen another way (a synthesized call, say) rather than
            // silently reading a tag off something that has none.
            let ConcreteType::Named(vname) = &v_ty else {
                return Err(Error::at(
                    format!(
                        "\"case_name\" is only callable on a variant, got {}",
                        type_name(&v_ty)
                    ),
                    args[0].span.clone(),
                ));
            };
            let Some(cases) = cx
                .structs
                .get(vname)
                .and_then(TypeDef::as_variant)
                .map(|v| v.cases.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
            else {
                return Err(Error::at(
                    format!(
                        "\"case_name\" is only callable on a variant, got {}",
                        type_name(&v_ty)
                    ),
                    args[0].span.clone(),
                ));
            };
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), v, 0);
            let merge = builder.create_block();
            builder.append_block_param(merge, types::I64);
            for (k, ctor) in cases.iter().enumerate() {
                let case_b = builder.create_block();
                let next_b = builder.create_block();
                let is_k = builder.ins().icmp_imm_s(IntCC::Equal, tag, k as i64);
                builder.ins().brif(is_k, case_b, &[], next_b, &[]);
                builder.switch_to_block(case_b);
                builder.seal_block(case_b);
                let (name_v, _) = emit_const_str(module, builder, cx, ctor.as_bytes())?;
                builder.ins().jump(merge, &[BlockArg::Value(name_v)]);
                builder.switch_to_block(next_b);
                builder.seal_block(next_b);
            }
            // Unreachable at runtime (the tag always names a case), but the
            // fallthrough block must still produce a value for `merge`. An
            // empty inline str is the harmless one.
            let (empty, _) = emit_const_str(module, builder, cx, b"")?;
            builder.ins().jump(merge, &[BlockArg::Value(empty)]);
            builder.switch_to_block(merge);
            builder.seal_block(merge);
            let out = builder.block_params(merge)[0];
            // Every arm produced an inline or STATIC-refcount `str`, both of
            // which no-op on drop — but track the merged value anyway, so a
            // `case_name` result is scoped exactly like the `str` literal it
            // could have been written as.
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(out, &ConcreteType::Primitive(Primitive::Str)));
            (out, ConcreteType::Primitive(Primitive::Str))
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
            if !matches!(&recv_ty, ConcreteType::Primitive(Primitive::Char)) {
                return Err(Error::at(
                    format!(
                        "\"is_space\" is only callable on a char, got {}",
                        type_name(&recv_ty)
                    ),
                    args[0].span.clone(),
                ));
            }
            // Check if c is ' ', '\t', '\n', or '\r' (32, 9, 10, 13).
            let sp = builder.ins().icmp_imm_s(IntCC::Equal, c, 32);
            let tab = builder.ins().icmp_imm_s(IntCC::Equal, c, 9);
            let lf = builder.ins().icmp_imm_s(IntCC::Equal, c, 10);
            let cr = builder.ins().icmp_imm_s(IntCC::Equal, c, 13);
            let or1 = builder.ins().bor(sp, tab);
            let or2 = builder.ins().bor(lf, cr);
            let result = builder.ins().bor(or1, or2);
            let b = builder.ins().uextend(types::I64, result);
            (b, ConcreteType::Primitive(Primitive::Bool))
        }
        "__builtin_is_whitespace" => {
            // `c.is_whitespace() -> bool` — the *full* ASCII whitespace set:
            // `is_space`'s four plus vertical tab (11) and form feed (12).
            //
            // It exists because those two cannot be written in AIPL source —
            // the language has `\n`, `\t` and `\r` and no numeric escape — so a
            // predicate covering them has to come from the compiler. That is
            // what lets `is_all_whitespace` be written in AIPL without narrowing
            // what it accepts (see `builtin_is_all_whitespace.aipl`).
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"is_whitespace\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (c, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if recv_ty != ConcreteType::Primitive(Primitive::Char) {
                return Err(Error::at(
                    format!(
                        "\"is_whitespace\" is only callable on a char, got {}",
                        type_name(&recv_ty)
                    ),
                    args[0].span.clone(),
                ));
            }
            let mut acc = builder.ins().icmp_imm_s(IntCC::Equal, c, 32);
            for byte in [9i64, 10, 13, 11, 12] {
                let hit = builder.ins().icmp_imm_s(IntCC::Equal, c, byte);
                acc = builder.ins().bor(acc, hit);
            }
            let b = builder.ins().uextend(types::I64, acc);
            (b, ConcreteType::Primitive(Primitive::Bool))
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
            if !matches!(&recv_ty, ConcreteType::Primitive(Primitive::Char)) {
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
                .icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, c, 48);
            let le_9 = builder
                .ins()
                .icmp_imm_s(IntCC::UnsignedLessThanOrEqual, c, 57);
            let result = builder.ins().band(ge_0, le_9);
            let b = builder.ins().uextend(types::I64, result);
            (b, ConcreteType::Primitive(Primitive::Bool))
        }
        "__builtin_to_digit" => {
            // `c.to_digit() -> i64?` — an ASCII digit '0'..'9' to its 0..9
            // value (`c - '0'`), `none` for any other char.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"to_digit\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (c, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            if !matches!(&recv_ty, ConcreteType::Primitive(Primitive::Char)) {
                return Err(Error::at(
                    format!(
                        "\"to_digit\" is only callable on a char, got {}",
                        type_name(&recv_ty)
                    ),
                    args[0].span.clone(),
                ));
            }
            // Branchless: the `i64?` core is a plain scalar (no heap, no
            // refcount), so store the present-tag and the value unconditionally
            // — the value at `OPT_VALUE_OFFSET` is simply ignored when the tag is
            // 0 (none). Tag = is_digit('0' (48) <= c <= '9' (57)); value = c - 48.
            let ge_0 = builder
                .ins()
                .icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, c, 48);
            let le_9 = builder
                .ins()
                .icmp_imm_s(IntCC::UnsignedLessThanOrEqual, c, 57);
            let is_digit = builder.ins().band(ge_0, le_9);
            let tag = builder.ins().uextend(types::I64, is_digit);
            let value = builder.ins().iadd_imm_s(c, -48);
            let opt_ty = ConcreteType::Optional(Box::new(ConcreteType::Primitive(Primitive::I64)));
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&opt_ty, structs) as u32,
                3,
            ));
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            builder.ins().store(MemFlagsData::trusted(), tag, ptr, 0);
            builder
                .ins()
                .store(MemFlagsData::trusted(), value, ptr, OPT_VALUE_OFFSET as i32);
            (ptr, opt_ty)
        }
        "__builtin_len" | "__builtin_is_nonempty" => {
            // `len(a) -> u64` — element/byte count — and `is_nonempty(a) -> bool`,
            // which is that count compared against zero. One arm because they take
            // the same receivers and read them the same way: neither consumes `a`
            // (it stays live in the caller's scope), so no inc/dec.
            let what = if name == "__builtin_len" {
                "len"
            } else {
                "is_nonempty"
            };
            if args.len() != 1 {
                return Err(Error::at(
                    format!("fn \"{what}\" expects 1 arg, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (ptr, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let len = if is_str_shaped(&t) {
                // A `str` (or a str-shaped `char[]`, see `is_str_shaped`) stores
                // no length field (it can be inline/owned/view); `aipl_str_len`
                // computes the byte length for any representation.
                builtins.call(module, builder, "aipl_str_len", &[ptr])
            } else if matches!(
                t,
                ConcreteType::Array(_) | ConcreteType::Set(_) | ConcreteType::Dict(_, _)
            ) {
                // A set/dict shares the array layout, so its element/pair count is
                // the same `len` field.
                load_arr_len(builder, ptr)
            } else {
                return Err(Error::at(
                    format!(
                        "\"{what}\" expects a str, array, set, or dict, got {}",
                        type_name(&t)
                    ),
                    args[0].span.clone(),
                ));
            };
            if name == "__builtin_len" {
                (len, ConcreteType::Primitive(Primitive::U64))
            } else {
                // `bool` is an i64 0/1 here like every other AIPL bool, so the
                // comparison result is extended rather than kept 1-bit.
                let nonzero = builder.ins().icmp_imm_s(IntCC::NotEqual, len, 0);
                let out = builder.ins().uextend(types::I64, nonzero);
                (out, ConcreteType::Primitive(Primitive::Bool))
            }
        }
        "__builtin_sort" => {
            // `xs.sort() -> T[]` — a fresh array, elements ascending. Consumes
            // `self` (callers pre-inc). The `ord` bound has already restricted the
            // element type to an integer, `char`, or `str`; which of those decides
            // how the runtime reads each 8-byte word.
            if args.len() != 1 {
                return Err(Error::at(
                    format!("\"sort\" expects 1 argument, got {}", args.len()),
                    span.clone(),
                ));
            }
            let (ptr, t) = compile_expr(module, builder, cx, scopes, &args[0])?;
            // A `str` (or a str-shaped `char[]`) is not an array block — it has no
            // length field and its elements are bytes — so it sorts through the
            // str path, exactly as `reverse` splits these cases. A `str` receiver
            // keeps `str`; a `char[]` keeps its nominal type.
            if is_str_repr(&t) || is_char_array(&t) {
                // The tagged entry point consumes its receiver, so the caller incs to
                // balance its own track. The wide one borrows (`active_sym`), so the
                // pair is dropped rather than mirrored — the same shape as concat.
                let out = builtins.call(module, builder, "aipl_str_sort", &[ptr]);
                let out_ty = if is_char_array(&t) {
                    t.clone()
                } else {
                    ConcreteType::Primitive(Primitive::Str)
                };
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(out, &out_ty));
                return Ok((out, out_ty));
            }
            let ConcreteType::Array(elem) = &t else {
                return Err(Error::at(
                    format!("\"sort\" expects an array, got {}", type_name(&t)),
                    args[0].span.clone(),
                ));
            };
            let elem = (**elem).clone();
            // `char[]` took the str path above; every other element type that
            // satisfies the `ord` bound reaches here. Elements of a narrow width
            // are stored canonicalized in their 8-byte slots (sign-extended for
            // `i*`, zero-extended for `u*`), so the same full-width signed and
            // unsigned comparisons order them correctly — only the choice
            // between the two arms depends on the width's signedness.
            let kind = match &elem {
                ConcreteType::Primitive(Primitive::Str) => SORT_KIND_STR,
                // A `char` is a byte value, so it orders as an unsigned word.
                ConcreteType::Primitive(Primitive::Char) => SORT_KIND_UNSIGNED,
                ConcreteType::Primitive(p) if p.is_int() => {
                    if p.int_signed() {
                        SORT_KIND_SIGNED
                    } else {
                        SORT_KIND_UNSIGNED
                    }
                }
                // An untyped empty literal (`[].sort()`) never has an element to
                // compare, so any kind does; it stays empty either way.
                _ if is_none_inner(&elem) => SORT_KIND_SIGNED,
                other => {
                    return Err(Error::at(
                        format!(
                            "\"sort\" needs comparable elements (an integer, char, or str), got {}",
                            type_name(other)
                        ),
                        args[0].span.clone(),
                    ));
                }
            };
            // See the note at `reverse`'s array arm: an array retains through
            // `aipl_arr_inc`, never the str-tagged `aipl_inc`.
            builtins.call_void(module, builder, "aipl_arr_inc", &[ptr]);
            let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
            let esz = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(&elem, structs));
            let kind_v = builder.ins().iconst(types::I64, kind);
            let out = builtins.call(
                module,
                builder,
                "aipl_arr_sort",
                &[ptr, drop_fn, retain_fn, esz, kind_v],
            );
            let arr_ty = ConcreteType::Array(Box::new(elem));
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(out, &arr_ty));
            (out, arr_ty)
        }
        "__builtin_join" => {
            // `parts.join(sep=s) -> T[]` — the parts flattened with `s` between
            // consecutive ones. Generic in the element type `T`, so the receiver
            // is a `T[][]`; for `T = char` that is a `str[]` and the whole thing
            // is the string join, which keeps its own runtime fast path.
            //
            // Native rather than an AIPL loop because the output length is known
            // before anything is written, so both paths allocate the result
            // exactly once instead of growing it.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"join\" expects 1 argument, got {}", args.len() - 1),
                    span.clone(),
                ));
            }
            let (parts, pt) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (sep, st) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // Both runtime entry points consume their arguments; the callers here
            // are borrowing, so each gets a compensating pre-inc.
            let inner = match &pt {
                ConcreteType::Array(e) => (**e).clone(),
                other => {
                    return Err(Error::at(
                        format!(
                            "\"join\" expects an array of arrays, got {}",
                            type_name(other)
                        ),
                        args[0].span.clone(),
                    ));
                }
            };
            // `join([], sep=", ")`: an empty parts array carries only the
            // `__none__` placeholder element, so it pins no `T`. The separator
            // is the one other place `T` appears, so it is what decides — the
            // result is empty either way, and what is really being chosen is
            // which representation that empty result has.
            let inner = if is_none_inner(&inner) {
                match &st {
                    t if is_str_repr(t) || is_char_array(t) => {
                        ConcreteType::Primitive(Primitive::Str)
                    }
                    ConcreteType::Primitive(Primitive::Char) => {
                        ConcreteType::Primitive(Primitive::Str)
                    }
                    // Both sides empty: nothing anywhere names `T`.
                    ConcreteType::Array(e) if is_none_inner(e) => {
                        return Err(Error::at(
                            "\"join\" cannot tell what it is joining: the parts and the \
                             separator are both empty, so neither names an element type"
                                .to_string(),
                            args[0].span.clone(),
                        ));
                    }
                    // A `T[]` separator is already the parts' element type; a
                    // bare `T` is one level down.
                    ConcreteType::Array(_) => st.clone(),
                    other => ConcreteType::Array(Box::new(other.clone())),
                }
            } else {
                inner
            };
            if is_str_repr(&inner) || is_char_array(&inner) {
                // `str[]` (i.e. `char[][]`): the string join, whose separator is
                // itself a `str`.
                //
                // The tagged `aipl_str_join` consumes both the array and the
                // separator, so both are retained first to balance the caller's
                // own tracks. `aipl_str_join` borrows them, so under the wide
                // ABI both retains are dropped rather than mirrored.
                // A variadic separator arrives in whichever shape the call site
                // wrote. For an AIPL-bodied function `specialize_variadic`
                // normalizes a bare element into a one-item sequence with a
                // prologue; a *native* builtin has no body to prepend one to, so
                // the wrapping happens here instead.
                let sep = if matches!(st, ConcreteType::Primitive(Primitive::Char)) {
                    let one = builder.ins().iconst(types::I64, 1);
                    let buf = builtins.call(module, builder, "aipl_str_alloc", &[one]);
                    builder.ins().istore8(MemFlagsData::trusted(), sep, buf, 0);
                    scopes
                        .last_mut()
                        .expect("scope")
                        .push(Tracked::new(buf, &ConcreteType::Primitive(Primitive::Str)));
                    buf
                } else {
                    sep
                };
                let out = builtins.call(module, builder, "aipl_str_join", &[parts, sep]);
                let out_ty = ConcreteType::Primitive(Primitive::Str);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(out, &out_ty));
                (out, out_ty)
            } else {
                let elem = match &inner {
                    ConcreteType::Array(e) => (**e).clone(),
                    other => {
                        return Err(Error::at(
                            format!(
                                "\"join\" expects an array of arrays, got an array of {}",
                                type_name(other)
                            ),
                            args[0].span.clone(),
                        ));
                    }
                };
                let out_ty = ConcreteType::Array(Box::new(elem.clone()));
                emit_retain(builder, module, builtins, structs, parts, &pt);
                // The element helpers describe `T`, not the parts: the runtime
                // copies and retains the innermost elements.
                let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
                let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(&elem, structs));
                // See the str path: a native builtin gets no `specialize_variadic`
                // prologue, so a bare element is wrapped into a one-item sequence
                // here. An `Array` argument is already the sequence.
                let sep = if matches!(st, ConcreteType::Array(_)) {
                    emit_retain(builder, module, builtins, structs, sep, &out_ty);
                    sep
                } else {
                    let slot = if is_composite(&elem, structs) {
                        sep
                    } else {
                        let sl = builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            8,
                            3,
                        ));
                        builder.ins().stack_store(types::I64, sep, sl, 0);
                        builder.ins().stack_addr(types::I64, sl, 0)
                    };
                    let zero = builder.ins().iconst(types::I64, 0);
                    let empty =
                        builtins.call(module, builder, "aipl_array_new", &[zero, drop_fn, esz]);
                    let one = builtins.call(
                        module,
                        builder,
                        "aipl_array_push",
                        &[empty, slot, drop_fn, retain_fn, esz],
                    );
                    scopes
                        .last_mut()
                        .expect("scope")
                        .push(Tracked::new(one, &out_ty));
                    emit_retain(builder, module, builtins, structs, one, &out_ty);
                    one
                };
                let out = builtins.call(
                    module,
                    builder,
                    "aipl_arr_join",
                    &[parts, sep, drop_fn, retain_fn, esz],
                );
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(out, &out_ty));
                (out, out_ty)
            }
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
                // The tagged entry point consumes its receiver, so the caller incs to
                // balance its own track. The wide one borrows (`active_sym`), so the
                // pair is dropped rather than mirrored — the same shape as concat.
                let result = builtins.call(module, builder, "aipl_str_reverse", &[ptr]);
                scopes.last_mut().expect("scope").push(Tracked::new(
                    result,
                    &ConcreteType::Primitive(Primitive::Str),
                ));
                (result, ConcreteType::Primitive(Primitive::Str))
            } else if is_char_array(&t) {
                // Str-shaped (see `is_char_array`), but — unlike a bare `str`
                // receiver above — keeps its nominal `char[]` type.
                // The tagged entry point consumes its receiver, so the caller incs to
                // balance its own track. The wide one borrows (`active_sym`), so the
                // pair is dropped rather than mirrored — the same shape as concat.
                let result = builtins.call(module, builder, "aipl_str_reverse", &[ptr]);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(result, &t));
                (result, t)
            } else if let ConcreteType::Array(elem) = &t {
                let elem = (**elem).clone();
                // Array receiver: `aipl_arr_inc`, not `emit_inc`. `aipl_inc` dispatches on the
                // *string* tag scheme (its own doc says so), which reads an array's
                // representation tag as a str's. The two coincide for an untagged
                // array — both find the refcount at `ptr - 8` — which is why this was
                // survivable; under the wide `str` it reads 24 bytes of array header
                // as a value instead.
                builtins.call_void(module, builder, "aipl_arr_inc", &[ptr]);
                let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
                let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(&elem, structs));
                let view = builtins.call(
                    module,
                    builder,
                    "aipl_arr_reverse",
                    &[ptr, drop_fn, retain_fn, esz],
                );
                let arr_ty = ConcreteType::Array(Box::new(elem));
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
            // `s.starts_with(p)` / `s.starts_with_at(p, i)` / `s.ends_with(p)
            // -> bool` over a `str` (byte compare) or `T[]` (element-wise
            // structural compare). The pattern is variadic; monomorphization has
            // already resolved its shape into the name suffix (`$ve` element,
            // `$vo` optional, none = the sequence), so each shape is implemented
            // directly here — its own monomorphization. The empty pattern always
            // matches; a pattern longer than what remains from the offset never
            // does.
            let (end, shape) = starts_ends_variant(name).unwrap();
            if args.len() != end.arity() {
                return Err(Error::at(
                    format!(
                        "{:?} expects {} args, got {}",
                        display_name(name),
                        end.arity(),
                        args.len()
                    ),
                    span.clone(),
                ));
            }
            let (recv, recv_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (pat_v, pat_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
            // `starts_with_at`'s offset, compiled once and shared by every shape.
            let at = match args.get(2) {
                Some(a) => Some(compile_expr(module, builder, cx, scopes, a)?.0),
                None => None,
            };
            let result = if is_str_shaped(&recv_ty) {
                // `char*` pattern. These runtimes *borrow* both refs — they read
                // bytes and keep nothing, and the caller holds its own references
                // across the call — so no pre-inc is paid (see `emit_char_at`).
                // For the optional shape `none` matches (a `""` prefix/suffix);
                // `some(c)` compares the 1-char string, materialized inline with
                // no allocation.
                let sym = match end {
                    SeEnd::Starts => "aipl_str_starts_with",
                    SeEnd::At => "aipl_str_starts_with_at",
                    SeEnd::Ends => "aipl_str_ends_with",
                };
                // The 1-char-or-whole `str` pattern to compare directly, or
                // `None` for the optional shape (handled with a tag branch).
                let pat: Option<Value> = match shape {
                    SeShape::Seq => Some(pat_v),
                    SeShape::Elem => Some(emit_char_to_str(module, builder, cx, pat_v)),
                    SeShape::Opt => None,
                };
                if let Some(pat) = pat {
                    let mut call_args = vec![recv, pat];
                    call_args.extend(at);
                    builtins.call(module, builder, sym, &call_args)
                } else {
                    // Optional `char?`: `none` → true; `some(c)` → str compare.
                    let res = i64_slot(builder);
                    let tag = builder
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), pat_v, 0);
                    let is_some = builder.ins().icmp_imm_s(IntCC::NotEqual, tag, 0);
                    let some_b = builder.create_block();
                    let none_b = builder.create_block();
                    let merge = builder.create_block();
                    builder.ins().brif(is_some, some_b, &[], none_b, &[]);
                    builder.switch_to_block(none_b);
                    builder.seal_block(none_b);
                    let one = builder.ins().iconst(types::I64, 1);
                    builder.ins().stack_store(types::I64, one, res, 0);
                    builder.ins().jump(merge, &[]);
                    builder.switch_to_block(some_b);
                    builder.seal_block(some_b);
                    let cv = builder.ins().load(
                        types::I64,
                        MemFlagsData::trusted(),
                        pat_v,
                        OPT_VALUE_OFFSET as i32,
                    );
                    let s = emit_char_to_str(module, builder, cx, cv);
                    let mut call_args = vec![recv, s];
                    call_args.extend(at);
                    let r = builtins.call(module, builder, sym, &call_args);
                    builder.ins().stack_store(types::I64, r, res, 0);
                    builder.ins().jump(merge, &[]);
                    builder.switch_to_block(merge);
                    builder.seal_block(merge);
                    builder.ins().stack_load(types::I64, types::I64, res, 0)
                }
            } else {
                // `T[]` pattern — element-wise structural compare. Borrows both
                // arrays; `emit_eq` balances its own per-element refs.
                let self_elem = match &recv_ty {
                    ConcreteType::Array(e) => (**e).clone(),
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
                        (_, ConcreteType::Array(e) | ConcreteType::Optional(e)) => (**e).clone(),
                        (_, t) => t.clone(),
                    }
                };
                match shape {
                    SeShape::Seq => {
                        emit_arr_starts_ends(module, builder, cx, recv, pat_v, &elem, end, at)?
                    }
                    SeShape::Elem => {
                        emit_arr_starts_ends_elem(module, builder, cx, recv, pat_v, &elem, end, at)?
                    }
                    SeShape::Opt => {
                        // `none` → true; `some(v)` → single-element compare.
                        let res = i64_slot(builder);
                        let tag = builder
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), pat_v, 0);
                        let is_some = builder.ins().icmp_imm_s(IntCC::NotEqual, tag, 0);
                        let some_b = builder.create_block();
                        let none_b = builder.create_block();
                        let merge = builder.create_block();
                        builder.ins().brif(is_some, some_b, &[], none_b, &[]);
                        builder.switch_to_block(none_b);
                        builder.seal_block(none_b);
                        let one = builder.ins().iconst(types::I64, 1);
                        builder.ins().stack_store(types::I64, one, res, 0);
                        builder.ins().jump(merge, &[]);
                        builder.switch_to_block(some_b);
                        builder.seal_block(some_b);
                        let cv = component(builder, pat_v, OPT_VALUE_OFFSET, &elem, structs);
                        let r = emit_arr_starts_ends_elem(
                            module, builder, cx, recv, cv, &elem, end, at,
                        )?;
                        builder.ins().stack_store(types::I64, r, res, 0);
                        builder.ins().jump(merge, &[]);
                        builder.switch_to_block(merge);
                        builder.seal_block(merge);
                        builder.ins().stack_load(types::I64, types::I64, res, 0)
                    }
                }
            };
            (result, ConcreteType::Primitive(Primitive::Bool))
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
                    SeShape::Elem => Some(emit_char_to_str(module, builder, cx, ndl_v)),
                    SeShape::Opt => None,
                };
                if let Some(ndl) = ndl {
                    // Both borrowed (see `emit_char_at`). `ndl` may be a fresh
                    // inline string from `emit_char_to_str`, whose inc/dec are
                    // no-ops either way.
                    builtins.call(module, builder, "aipl_str_contains", &[recv, ndl])
                } else {
                    // Optional `char?`: `none` → false; `some(c)` → window scan.
                    let res = i64_slot(builder);
                    let zero = builder.ins().iconst(types::I64, 0);
                    builder.ins().stack_store(types::I64, zero, res, 0);
                    let tag = builder
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), ndl_v, 0);
                    let is_some = builder.ins().icmp_imm_s(IntCC::NotEqual, tag, 0);
                    let some_b = builder.create_block();
                    let merge = builder.create_block();
                    builder.ins().brif(is_some, some_b, &[], merge, &[]);
                    builder.switch_to_block(some_b);
                    builder.seal_block(some_b);
                    let cv = builder.ins().load(
                        types::I64,
                        MemFlagsData::trusted(),
                        ndl_v,
                        OPT_VALUE_OFFSET as i32,
                    );
                    let s = emit_char_to_str(module, builder, cx, cv);
                    // Borrowed, like the sibling site above. `s` is an inline
                    // one-char string, so it owns no allocation to release.
                    let r = builtins.call(module, builder, "aipl_str_contains", &[recv, s]);
                    builder.ins().stack_store(types::I64, r, res, 0);
                    builder.ins().jump(merge, &[]);
                    builder.switch_to_block(merge);
                    builder.seal_block(merge);
                    builder.ins().stack_load(types::I64, types::I64, res, 0)
                }
            } else {
                // `T*` needle — element-wise structural compare. Borrows both
                // arrays; `emit_eq` balances its own per-element refs.
                let self_elem = match &recv_ty {
                    ConcreteType::Array(e) => (**e).clone(),
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
                        (_, ConcreteType::Array(e) | ConcreteType::Optional(e)) => (**e).clone(),
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
                        builder.ins().stack_store(types::I64, zero, res, 0);
                        let tag = builder
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), ndl_v, 0);
                        let is_some = builder.ins().icmp_imm_s(IntCC::NotEqual, tag, 0);
                        let some_b = builder.create_block();
                        let merge = builder.create_block();
                        builder.ins().brif(is_some, some_b, &[], merge, &[]);
                        builder.switch_to_block(some_b);
                        builder.seal_block(some_b);
                        let cv = component(builder, ndl_v, OPT_VALUE_OFFSET, &elem, structs);
                        let r = emit_arr_contains_elem(module, builder, cx, recv, cv, &elem)?;
                        builder.ins().stack_store(types::I64, r, res, 0);
                        builder.ins().jump(merge, &[]);
                        builder.switch_to_block(merge);
                        builder.seal_block(merge);
                        builder.ins().stack_load(types::I64, types::I64, res, 0)
                    }
                }
            };
            (result, ConcreteType::Primitive(Primitive::Bool))
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
                emit_char_to_str(module, builder, cx, c),
                ConcreteType::Primitive(Primitive::Str),
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
                ConcreteType::Set(inner) => (**inner).clone(),
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
                    ConcreteType::Primitive(Primitive::Bool),
                )
            } else {
                // A bare literal queried against a narrow-element set flexes,
                // so `#{u8(1)}.has(1)` needs no conversion on the `1`.
                let x_ty = flex_int_ty(&args[1], &x_ty, &elem);
                expect_type(&x_ty, &elem, "has element", args[1].span.clone())?;
                // The runtime reads the queried value through a pointer; spill
                // it and pass its address. Sized and written by element type
                // (`value_slot`/`store_array_elem`) rather than as a bare word:
                // a scalar is one store, but a composite element — a wide `str`
                // among them — is a whole 24-byte value to copy, and storing its
                // *address* into an 8-byte slot is what the runtime would then
                // read back as the value.
                let s = value_slot(builder, &elem, structs);
                let x_ptr = builder.ins().stack_addr(types::I64, s, 0);
                store_array_elem(builder, x_ptr, x_v, &elem, structs);
                let esz = builder
                    .ins()
                    .iconst(types::I64, runtime_elem_size(&elem, structs));
                let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&elem));
                let found = builtins.call(
                    module,
                    builder,
                    "aipl_set_contains",
                    &[set_ptr, x_ptr, esz, str_cmp],
                );
                (found, ConcreteType::Primitive(Primitive::Bool))
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
            let Some(result_ty @ ConcreteType::Set(_)) = merged else {
                return Err(Error::at(
                    format!(
                        "\"union\" expects two sets of the same type, got {} and {}",
                        type_name(&a_ty),
                        type_name(&b_ty)
                    ),
                    span.clone(),
                ));
            };
            let ConcreteType::Set(elem) = &result_ty else {
                unreachable!()
            };
            // Sets are array blocks — see the note at `reverse`'s array arm.
            builtins.call_void(module, builder, "aipl_arr_inc", &[a_ptr]);
            builtins.call_void(module, builder, "aipl_arr_inc", &[b_ptr]);
            let drop_fn = array_drop_fn_addr(builder, module, cx, elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, elem);
            let esz = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(elem, structs));
            let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&elem));
            let res = builtins.call(
                module,
                builder,
                "aipl_set_union",
                &[a_ptr, b_ptr, drop_fn, retain_fn, esz, str_cmp],
            );
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
                ConcreteType::Dict(k, v) => ((**k).clone(), (**v).clone()),
                other => {
                    return Err(Error::at(
                        format!("\"get\" expects a dict, got {}", type_name(other)),
                        args[0].span.clone(),
                    ));
                }
            };
            let (key_v, key_t) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let result_ty = ConcreteType::Optional(Box::new(val_ty.clone()));
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&result_ty, structs) as u32,
                3,
            ));
            let sbase = builder.ins().stack_addr(types::I64, slot, 0);
            // An empty dict (`__none__` key/value) holds nothing — always `none`.
            if is_none_inner(&key_ty) {
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().stack_store(types::I64, zero, slot, 0);
                (sbase, result_ty)
            } else {
                let key_t = flex_int_ty(&args[1], &key_t, &key_ty);
                expect_type(&key_t, &key_ty, "get key", args[1].span.clone())?;
                // Spill the key and pass its address (a `bool` reads back as i64,
                // a `str` as its pointer).
                // Spilled by key type, not as a bare word — see `has` for why
                // storing a composite key's *address* into an 8-byte slot is the
                // bug that reads back as a value.
                let ks = value_slot(builder, &key_ty, structs);
                let key_ptr = builder.ins().stack_addr(types::I64, ks, 0);
                store_array_elem(builder, key_ptr, key_v, &key_ty, structs);
                let pair_size = dict_pair_size(&key_ty, &val_ty, structs);
                let psz = builder.ins().iconst(types::I64, pair_size);
                let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&key_ty));
                // The value's address, or 0 when the key is absent.
                let vslot = builtins.call(
                    module,
                    builder,
                    "aipl_dict_get",
                    &[dict_ptr, key_ptr, psz, str_cmp],
                );
                let found = builder.ins().icmp_imm_s(IntCC::NotEqual, vslot, 0);
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
                builder.ins().stack_store(types::I64, zero, slot, 0);
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
                ConcreteType::Dict(k, v) => ((**k).clone(), (**v).clone()),
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
                    ConcreteType::Primitive(Primitive::Bool),
                )
            } else {
                let key_t = flex_int_ty(&args[1], &key_t, &key_ty);
                expect_type(&key_t, &key_ty, "contains_key key", args[1].span.clone())?;
                // Spilled by key type, not as a bare word — see `has` for why
                // storing a composite key's *address* into an 8-byte slot is the
                // bug that reads back as a value.
                let ks = value_slot(builder, &key_ty, structs);
                let key_ptr = builder.ins().stack_addr(types::I64, ks, 0);
                store_array_elem(builder, key_ptr, key_v, &key_ty, structs);
                let pair_size = dict_pair_size(&key_ty, &val_ty, structs);
                let psz = builder.ins().iconst(types::I64, pair_size);
                let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&key_ty));
                let found = builtins.call(
                    module,
                    builder,
                    "aipl_dict_contains_key",
                    &[dict_ptr, key_ptr, psz, str_cmp],
                );
                (found, ConcreteType::Primitive(Primitive::Bool))
            }
        }
        "__filter_keep" => {
            // Internal (in-place `filter`): `__filter_keep(arr, w, e)` stores
            // element `e` at slot `w` with a raw pointer copy — no refcount
            // change, since ownership relocates from `e`'s read slot to slot `w`.
            let (a_ptr, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (w, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
            let (e, ety) = compile_expr(module, builder, cx, scopes, &args[2])?;
            let base = builder.ins().iadd_imm_s(a_ptr, ARR_ELEMS_OFFSET as i64);
            // Stride and store both follow the element type. They were a fixed 8
            // and a word store, on the in-place gate's assumption that an element
            // is never composite — true until a wide `str` became one. A
            // composite store is still a bit copy, so the "ownership relocates"
            // reasoning above is unchanged.
            let off = builder.ins().imul_imm_s(w, elem_size_of(&ety, structs));
            let addr = builder.ins().iadd(base, off);
            store_array_elem(builder, addr, e, &ety, structs);
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
        }
        "__filter_drop" => {
            // Internal (in-place `filter`): release a filtered-out element. The
            // surrounding `for` loop retains/releases `e` each iteration (a
            // no-op net), so this single drop removes the array's ownership of
            // it. A no-op for scalar elements (`needs_drop` is false).
            let (e, ety) = compile_expr(module, builder, cx, scopes, &args[0])?;
            emit_drop(builder, module, builtins, structs, e, &ety);
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
        }
        "__filter_truncate" => {
            // Internal (in-place `filter`): set the array's length to `w`. The
            // dead tail `[w, old_len)` holds relocated/stale pointers and is
            // never released (the block is later freed by its capacity).
            let (a_ptr, _) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let (w, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
            builder
                .ins()
                .store(MemFlagsData::trusted(), w, a_ptr, ARR_LEN_OFFSET as i32);
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
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
            let base = builder.ins().iadd_imm_s(a_ptr, ARR_ELEMS_OFFSET as i64);
            // The block's stride is the *old* element type's — that is what it
            // was allocated with — while the value written is the new one. Both
            // were a fixed 8 until a wide `str` made an element composite.
            let off = builder
                .ins()
                .imul_imm_s(i_val, elem_size_of(&old_ty, structs));
            let addr = builder.ins().iadd(base, off);
            // Retain the new value, release the old, *then* write the slot.
            //
            // The old order was store-then-release, which is fine when `old_val`
            // is a word copied out of the slot. A wide `str` element is not
            // copied out — `component` hands back the slot's own address — so
            // releasing after the store releases the value just written.
            // Retaining first is what makes the release safe: `map(|s| s +++ x)`
            // builds its result from the old element, and the result already
            // holds its own reference by then.
            emit_retain(builder, module, builtins, structs, new_val, &new_ty);
            emit_drop(builder, module, builtins, structs, old_val, &old_ty);
            store_array_elem(builder, addr, new_val, &new_ty, structs);
            let new_drop = array_drop_fn_addr(builder, module, cx, &new_ty);
            builder.ins().store(
                MemFlagsData::trusted(),
                new_drop,
                a_ptr,
                ARR_DROPFN_OFFSET as i32,
            );
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
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
            // A capacity is a count: `map`'s desugar feeds it `len()` (`u64`),
            // while hand-written calls pass an `i64` literal.
            expect_len_operand(&t, "with_capacity capacity", args[0].span.clone())?;
            // Element type unknown here (`__none__`); the drop-fn and element
            // size are settled by the first `push`. `map`/`filter` only pre-size
            // 8-byte-element outputs (optional outputs use a plain `[]`).
            let zero = builder.ins().iconst(types::I64, 0); // drop_fn
            let esz = builder.ins().iconst(types::I64, 8); // elem_size
            let ptr = builtins.call(module, builder, "aipl_array_with_cap", &[cap, zero, esz]);
            let arr_ty = ConcreteType::Array(Box::new(ConcreteType::NoneInner));
            scopes
                .last_mut()
                .expect("scope")
                .push(Tracked::new(ptr, &arr_ty));
            (ptr, arr_ty)
        }
        // Array-literal spread. `[..a, b, ..c]` lowers to
        //   mut $s = __aipl_arr_reserve(a, 1 + len(c));
        //   set $s = __aipl_arr_append($s, b);
        //   set $s = __aipl_arr_concat($s, c);
        // `reserve` returns a *uniquely owned* block already sized for the whole
        // literal, so the appends after it are plain in-place writes that never
        // reallocate and never consult the static exclusivity analysis. All
        // three consume their array argument, so a borrowed one is retained
        // first (the same compensating pre-inc `aipl_array_push` needs).
        //
        // Only the generic 8-byte-element representation takes this path.
        // `char[]` is str-shaped and `bool[]` is bit-packed; both keep the
        // ordinary push lowering, which `arr_reserve` degrades to by handing
        // its argument straight back (see the `is_char_array`/`is_bit_packed`
        // guards below).
        "__aipl_arr_reserve" | "__aipl_arr_append" | "__aipl_arr_concat" => {
            let before = scope_depth(scopes);
            let (arr_ptr, arr_ty) = compile_expr(module, builder, cx, scopes, &args[0])?;
            let arr_owned = owned_temp_since(scopes, before, arr_ptr);
            let elem = match &arr_ty {
                ConcreteType::Array(inner) => (**inner).clone(),
                _ => ConcreteType::NoneInner,
            };
            // Representations with their own append lowering opt out: hand the
            // array back untouched and let the plain `push` path handle them.
            // Representation dispatch. `char[]` is str-shaped and `bool[]` is
            // bit-packed; neither pre-sizes, so `reserve` hands the array back
            // untouched and their appends stay on the existing lowering.
            if is_char_array(&arr_ty) {
                if name == "__aipl_arr_reserve" {
                    return Ok((arr_ptr, arr_ty));
                }
                // `str` has no in-place growable form: build a fresh buffer of
                // the combined length and copy both sides in. `aipl_str_len` /
                // `aipl_str_data` only *borrow*, so neither side is retained.
                let old_len = builtins.call(module, builder, "aipl_str_len", &[arr_ptr]);
                let (tail, add_len) = if name == "__aipl_arr_concat" {
                    let (src, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
                    let src_len = builtins.call(module, builder, "aipl_str_len", &[src]);
                    (Some(src), src_len)
                } else {
                    let (x_v, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
                    (None, x_v)
                };
                let new_len = if tail.is_some() {
                    builder.ins().iadd(old_len, add_len)
                } else {
                    builder.ins().iadd_imm_s(old_len, 1)
                };
                let (buf, dst) = emit_str_alloc(module, builder, cx, new_len);
                let src0 = str_bytes_ptr(module, builder, cx, arr_ptr);
                // The advanced cursor is not needed — each copy's destination is
                // computed from `dst` directly.
                let _ = builtins.call(module, builder, "aipl_write_bytes", &[dst, src0, old_len]);
                let at = builder.ins().iadd(dst, old_len);
                match tail {
                    Some(src) => {
                        let src1 = str_bytes_ptr(module, builder, cx, src);
                        builtins.call_void(
                            module,
                            builder,
                            "aipl_write_bytes",
                            &[at, src1, add_len],
                        );
                    }
                    None => {
                        builder
                            .ins()
                            .istore8(MemFlagsData::trusted(), add_len, at, 0);
                    }
                }
                emit_str_grew(module, builder, cx, buf, new_len);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(buf, &arr_ty));
                return Ok((buf, arr_ty));
            }
            let packed = is_bit_packed(&elem);
            if packed && name == "__aipl_arr_reserve" {
                return Ok((arr_ptr, arr_ty));
            }
            // All three consume their array argument. A borrowed one needs a
            // compensating pre-inc (the same one `aipl_array_push` needs); an
            // owned temporary is *moved* in instead — its scope track has to go,
            // or the block is dropped twice once the result's track drops it.
            let mut moved: Vec<Value> = Vec::new();
            if arr_owned {
                moved.push(arr_ptr);
            } else {
                emit_retain(builder, module, builtins, structs, arr_ptr, &arr_ty);
            }
            let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
            let esz = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(&elem, structs));
            let out = if name == "__aipl_arr_reserve" {
                let (extra, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
                builtins.call(
                    module,
                    builder,
                    "aipl_arr_reserve",
                    &[arr_ptr, extra, drop_fn, retain_fn, esz],
                )
            } else if name == "__aipl_arr_concat" {
                let mark = scope_depth(scopes);
                let (src, src_ty) = compile_expr(module, builder, cx, scopes, &args[1])?;
                if owned_temp_since(scopes, mark, src) {
                    moved.push(src);
                } else {
                    emit_retain(builder, module, builtins, structs, src, &src_ty);
                }
                builtins.call(
                    module,
                    builder,
                    "aipl_arr_extend",
                    &[arr_ptr, src, drop_fn, retain_fn, esz],
                )
            } else {
                // One element. After `reserve` the block is uniquely owned with
                // spare capacity, so `push_mut` is a plain write; a bit-packed
                // array never reserved, so it takes the copying push.
                let (x_v, _) = compile_expr(module, builder, cx, scopes, &args[1])?;
                let sym = if packed {
                    "aipl_array_push"
                } else {
                    "aipl_array_push_mut"
                };
                // The runtime reads `elem_size` bytes from an address: a
                // composite element already is one, a scalar/pointer is spilled.
                let x_slot = if is_composite(&elem, structs) {
                    x_v
                } else {
                    let sl = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        3,
                    ));
                    builder.ins().stack_store(types::I64, x_v, sl, 0);
                    builder.ins().stack_addr(types::I64, sl, 0)
                };
                builtins.call(
                    module,
                    builder,
                    sym,
                    &[arr_ptr, x_slot, drop_fn, retain_fn, esz],
                )
            };
            let scope = scopes.last_mut().expect("scope");
            scope.retain(|t| !matches!(t.owned, Owned::Value(x) if moved.contains(&x)));
            scope.push(Tracked::new(out, &arr_ty));
            (out, arr_ty)
        }
        "__builtin_push" => {
            // The in-place writeback form: the receiver is `args[0]`, a mutable
            // array variable, and the grown array is stored back into its slot.
            // Value semantics are kept — a possibly-shared array is copied
            // first (the old block, still referenced elsewhere, is untouched);
            // an exclusive one is grown in place.
            //
            // This is the only shape that reaches codegen. `push` is a mutating
            // method, so every *other* position — the free call `push(xs, x)`
            // and any expression-position `xs.push(x)` — was already rewritten
            // by mono into `{ mut t = xs; set t.push(x); t }`, i.e. back into
            // this form on a fresh local.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"push\" expects 1 argument, got {}", args.len() - 1),
                    span.clone(),
                ));
            }
            let receiver = &args[0];
            let value = &args[1];
            let (slot, ty_cell, exclusive, elem_ty) = mut_array_receiver(env, receiver, "push")?;
            let arr_ptr = builder.ins().stack_load(types::I64, types::I64, slot, 0);
            let (x_v, x_ty) = compile_expr(module, builder, cx, scopes, value)?;
            let elem_was_none = is_none_inner(&elem_ty);
            // An empty array (`__none__` element) takes its element type from
            // the first pushed value; otherwise the value must match.
            let result_elem = if elem_was_none {
                let ok = is_array_elem(&x_ty)
                    || matches!(&x_ty, ConcreteType::Optional(_))
                    || matches!(&x_ty, ConcreteType::Named(n) if structs.contains_key(n));
                if !ok {
                    return Err(Error::at(
                        format!(
                            "\"push\" element must be an integer (i8..i64, u8..u64), bool, char, str, or an array, got {}",
                            type_name(&x_ty)
                        ),
                        value.span.clone(),
                    ));
                }
                x_ty
            } else {
                // A bare literal pushed onto a narrow-element array flexes.
                let x_ty = flex_int_ty(value, &x_ty, &elem_ty);
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
                builder.ins().stack_store(types::I64, x_v, s, 0);
                builder.ins().stack_addr(types::I64, s, 0)
            };
            let new_arr_ty = ConcreteType::Array(Box::new(result_elem));
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
                // The receiver's handle has to be re-derived here: the generic
                // load above reads one word from the slot, which is the whole
                // value only under the tagged representation.
                let arr_ptr = load_binding_str(builder, slot);
                if exclusive {
                    // Statically proven unaliased, so the append may write into
                    // the value's own allocation: `aipl_str_push_byte` fills
                    // spare capacity when there is some, grows the block under
                    // itself when there isn't, and only copies when the dynamic
                    // refcount says the static proof isn't enough on its own
                    // (`STR_REPR.md`). It takes over the slot's reference and
                    // writes the result back, so — as in the in-place `+++` —
                    // there is nothing to release and no new track to add.
                    builtins.call_void(module, builder, "aipl_str_push_byte", &[arr_ptr, x_v]);
                    *ty_cell.borrow_mut() = new_arr_ty;
                    return Ok((builder.ins().iconst(types::I64, 0), ConcreteType::Unit));
                }
                let old_len = builtins.call(module, builder, "aipl_str_len", &[arr_ptr]);
                let new_len = builder.ins().iadd_imm_s(old_len, 1);
                let (buf, dst) = emit_str_alloc(module, builder, cx, new_len);
                let src = str_bytes_ptr(module, builder, cx, arr_ptr);
                // The advanced cursor is not needed here — the copy is the point.
                let _ = builtins.call(module, builder, "aipl_write_bytes", &[dst, src, old_len]);
                let dst_addr = builder.ins().iadd(dst, old_len);
                builder
                    .ins()
                    .istore8(MemFlagsData::trusted(), x_v, dst_addr, 0);
                emit_str_grew(module, builder, cx, buf, new_len);
                // The slot owns exactly one reference to its current value
                // (`LetMut`'s str-shaped branch), so the rebuild keeps that
                // invariant: release the old value, then let the slot own the
                // fresh `buf` outright. `buf` gets no value-track of its own —
                // one in the *current* scope would be wrong inside a loop, where
                // it frees the buffer the binding still names at the end of the
                // iteration.
                //
                // The release comes *before* the writeback, not after. A tagged
                // `arr_ptr` was a snapshot — a pointer loaded out of the slot —
                // so the order did not matter. A wide `arr_ptr` *is* the slot's
                // address, so once the new value is stored there the "old" handle
                // names the new one, and dropping it frees the string that was
                // just built. The copy above is already done, so nothing reads
                // the old bytes after this point.
                emit_rc(
                    builder,
                    module,
                    builtins,
                    structs,
                    arr_ptr,
                    &new_arr_ty,
                    RcOp::Drop,
                );
                store_binding_str(builder, cx, slot, buf, structs);
            } else if exclusive {
                // Statically proven unaliased: mutate in place. No pre-inc and
                // no new value-track — the binding's slot-track (added at
                // `LetMut`) already owns the block, even after a relocating grow.
                let new_ptr = builtins.call(
                    module,
                    builder,
                    "aipl_array_push_mut",
                    &[arr_ptr, x_ptr, drop_fn, retain_fn, esz],
                );
                builder.ins().stack_store(types::I64, new_ptr, slot, 0);
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
                let new_ptr = builtins.call(
                    module,
                    builder,
                    "aipl_array_push",
                    &[arr_ptr, x_ptr, drop_fn, retain_fn, esz],
                );
                builder.ins().stack_store(types::I64, new_ptr, slot, 0);
                emit_retain(builder, module, builtins, structs, new_ptr, &new_arr_ty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(new_ptr, &new_arr_ty));
            }
            // Refine the binding's element type (e.g. `mut a = []` → `i64[]`).
            *ty_cell.borrow_mut() = new_arr_ty;
            // `push` mutates; it produces no value.
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
        }
        "__builtin_extend" => {
            // `push` for a whole array, and the same in-place writeback form:
            // receiver in `args[0]`, source array in `args[1]`, result stored
            // back into the receiver's slot. Every other call position was
            // rewritten into this one by mono, exactly as for `push`.
            //
            // The reason this is a builtin rather than an AIPL loop over `push`
            // is the sizing: `aipl_arr_reserve` grows the destination once, to
            // its final length, so appending N elements costs at most one
            // reallocation instead of the log N a repeated `push` pays — and the
            // elements then move as a single `memcpy` plus one retain pass.
            if args.len() != 2 {
                return Err(Error::at(
                    format!("\"extend\" expects 1 argument, got {}", args.len() - 1),
                    span.clone(),
                ));
            }
            let receiver = &args[0];
            let source = &args[1];
            let (slot, ty_cell, exclusive, elem_ty) = mut_array_receiver(env, receiver, "extend")?;
            let arr_ptr = builder.ins().stack_load(types::I64, types::I64, slot, 0);
            let mark = scope_depth(scopes);
            let (src_ptr, src_ty) = compile_expr(module, builder, cx, scopes, source)?;
            let src_owned = owned_temp_since(scopes, mark, src_ptr);
            let src_elem = match &src_ty {
                ConcreteType::Array(inner) => (**inner).clone(),
                // A `str` *is* the `char` sequence (see `is_char_array`), so it
                // is a valid source for a `char[]` receiver — and the only shape
                // a `char*` variadic ever arrives as.
                _ if is_str_repr(&src_ty) => ConcreteType::Primitive(Primitive::Char),
                other => {
                    return Err(Error::at(
                        format!("\"extend\" requires an array, got {}", type_name(other)),
                        source.span.clone(),
                    ));
                }
            };
            // The receiver takes its element type from the source when it is
            // still the untyped empty literal, and otherwise the two must agree.
            // An empty source pins nothing either way.
            let elem_was_none = is_none_inner(&elem_ty);
            let result_elem = if elem_was_none {
                src_elem
            } else {
                if !is_none_inner(&src_elem) {
                    expect_type(
                        &src_ty,
                        &ConcreteType::Array(Box::new(elem_ty.clone())),
                        "extend source",
                        source.span.clone(),
                    )?;
                }
                elem_ty
            };
            let new_arr_ty = ConcreteType::Array(Box::new(result_elem.clone()));
            if is_char_array(&new_arr_ty) {
                // `char[]` is str-shaped, and `str` has no in-place growable
                // form: build a fresh buffer of the combined length and copy
                // both sides in, mirroring the `push` char path (and, for the
                // same reason it does, refusing the untyped-empty transition —
                // a loop body compiles once but runs many times, so "this is
                // the first push" cannot be decided here).
                if elem_was_none {
                    return Err(Error::at(
                        "cannot extend a char array that started as an untyped empty literal \
                         (`mut cs = []`) — initialize it with a char first, e.g. \
                         \"mut cs = ['a']\", since the first append can't be proven to run \
                         only once"
                            .to_string(),
                        receiver.span.clone(),
                    ));
                }
                // See `push`: the generic slot load is a word, the wide value
                // is the slot itself.
                let arr_ptr = load_binding_str(builder, slot);
                if exclusive {
                    // In place, exactly as the `push` char path above — and with
                    // the same contract, so the source is only borrowed. A source
                    // that *is* this receiver (`cs.extend(cs)`) is handled inside
                    // the runtime, which takes the copy path rather than reading
                    // bytes out of a block it is about to grow.
                    builtins.call_void(module, builder, "aipl_str_append", &[arr_ptr, src_ptr]);
                    *ty_cell.borrow_mut() = new_arr_ty;
                    return Ok((builder.ins().iconst(types::I64, 0), ConcreteType::Unit));
                }
                let old_len = builtins.call(module, builder, "aipl_str_len", &[arr_ptr]);
                let add_len = builtins.call(module, builder, "aipl_str_len", &[src_ptr]);
                let new_len = builder.ins().iadd(old_len, add_len);
                let (buf, dst) = emit_str_alloc(module, builder, cx, new_len);
                let src0 = str_bytes_ptr(module, builder, cx, arr_ptr);
                // The advanced cursor is not needed — each copy's destination is
                // computed from `dst` directly.
                let _ = builtins.call(module, builder, "aipl_write_bytes", &[dst, src0, old_len]);
                let at = builder.ins().iadd(dst, old_len);
                let src1 = str_bytes_ptr(module, builder, cx, src_ptr);
                let _ = builtins.call(module, builder, "aipl_write_bytes", &[at, src1, add_len]);
                emit_str_grew(module, builder, cx, buf, new_len);
                // Same ownership handover as the `push` char path, for the same
                // reasons — including the release coming before the writeback,
                // since a wide handle is the slot rather than a snapshot of it.
                emit_rc(
                    builder,
                    module,
                    builtins,
                    structs,
                    arr_ptr,
                    &new_arr_ty,
                    RcOp::Drop,
                );
                store_binding_str(builder, cx, slot, buf, structs);
                *ty_cell.borrow_mut() = new_arr_ty;
                return Ok((builder.ins().iconst(types::I64, 0), ConcreteType::Unit));
            }
            let drop_fn = array_drop_fn_addr(builder, module, cx, &result_elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, &result_elem);
            let esz = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(&result_elem, structs));
            // `aipl_arr_extend` consumes `src`. An owned temporary is moved in
            // (its scope track has to go, or the block is dropped twice); a
            // borrowed one gets the compensating pre-inc.
            if src_owned {
                let scope = scopes.last_mut().expect("scope");
                scope.retain(|t| !matches!(t.owned, Owned::Value(x) if x == src_ptr));
            } else {
                emit_retain(builder, module, builtins, structs, src_ptr, &src_ty);
            }
            // `aipl_arr_reserve` has `aipl_array_push`'s ownership contract: it
            // consumes one reference to the array and hands back one owned
            // reference, growing in place when the block is uniquely owned and
            // copying when it isn't. So the bookkeeping below is `push`'s,
            // unchanged — including the aliasing case `a.extend(a)`, where the
            // source's own retain is what makes the block shared and so forces
            // the copy before anything is written.
            //
            // A bit-packed `bool[]` has no pre-sized form (`reserve` measures in
            // whole elements), so it skips straight to `aipl_arr_extend`, which
            // appends bit by bit through the copying push.
            let grown = if is_bit_packed(&result_elem) {
                arr_ptr
            } else {
                let len_v = load_arr_len(builder, src_ptr);
                builtins.call(
                    module,
                    builder,
                    "aipl_arr_reserve",
                    &[arr_ptr, len_v, drop_fn, retain_fn, esz],
                )
            };
            let new_ptr = builtins.call(
                module,
                builder,
                "aipl_arr_extend",
                &[grown, src_ptr, drop_fn, retain_fn, esz],
            );
            builder.ins().stack_store(types::I64, new_ptr, slot, 0);
            if !exclusive {
                // Possibly shared: the slot's own reference was consumed by the
                // reserve above (as `push` has it consumed by `aipl_array_push`);
                // the extra retain plus value-track is the new version's region
                // track, keeping it borrowable to this scope's exit.
                emit_retain(builder, module, builtins, structs, new_ptr, &new_arr_ty);
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(new_ptr, &new_arr_ty));
            }
            *ty_cell.borrow_mut() = new_arr_ty;
            // `extend` mutates; it produces no value.
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
        }
        // The `i8(x)`/`u32(x)`/… conversion builtins were removed in favour of a
        // typed binding (`let n: u8 = x;`), which is where `canon_int` now runs.
        // The checker rejects these first with the same advice; this mirrors it
        // so a direct codegen entry point can't fall through to "unknown call".
        _ if Primitive::from_name(name).is_some_and(Primitive::is_int) => {
            return Err(Error::at(
                format!(
                    "{name}(..) conversions were removed — bind with the type instead \
                     (`let n: {name} = ..;`), or drop the conversion entirely if the \
                     argument is a literal, which takes the type its context expects"
                ),
                span.clone(),
            ));
        }
        "ok" | "err" => {
            // A Result `{ tag, value }` (tag 1 = ok, 0 = err). The unbound side is
            // `__none__`, resolved by coercion at the use site (like bare `none`).
            let is_ok = name == "ok";
            // `ok()` with no argument is the void success of a `!E` result: tag 1
            // with an unused (zeroed) value region, Ok side `unit`.
            if is_ok && args.is_empty() {
                let res_ty = ConcreteType::Result(
                    Box::new(ConcreteType::Unit),
                    Box::new(ConcreteType::NoneInner),
                );
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    elem_size_of(&res_ty, structs) as u32,
                    3,
                ));
                let ptr = builder.ins().stack_addr(types::I64, slot, 0);
                let one = builder.ins().iconst(types::I64, 1);
                builder.ins().store(MemFlagsData::trusted(), one, ptr, 0);
                let zero = builder.ins().iconst(types::I64, 0);
                builder
                    .ins()
                    .store(MemFlagsData::trusted(), zero, ptr, OPT_VALUE_OFFSET as i32);
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
            // Any *value* can be a payload. The machinery a payload rides on is
            // already generic over the type: `elem_size_of` sizes it (an inline
            // composite — struct, variant, optional, nested result — by its
            // layout; everything else as one 8-byte word),
            // `store_array_elem`/`copy_composite` writes it, and
            // `emit_retain`/`needs_drop` refcount whatever heap it owns. So the
            // only things rejected here are the two types that have no runtime
            // value to store.
            match &t {
                // `ok()` (no argument) is how a void success is written; `ok(x)`
                // needs an `x` that exists.
                ConcreteType::Unit => {
                    return Err(Error::at(
                        format!("{name:?} payload cannot be () — write `ok()` for a void success"),
                        args[0].span.clone(),
                    ));
                }
                // Function types are erased by monomorphization: a function is
                // never a runtime value, so it can't be carried in one.
                ConcreteType::Fn(_, _) => {
                    return Err(Error::at(
                        format!("{name:?} payload cannot be a function ({})", type_name(&t)),
                        args[0].span.clone(),
                    ));
                }
                _ => {}
            }
            let res_ty = if is_ok {
                ConcreteType::Result(Box::new(t.clone()), Box::new(ConcreteType::NoneInner))
            } else {
                ConcreteType::Result(Box::new(ConcreteType::NoneInner), Box::new(t.clone()))
            };
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                elem_size_of(&res_ty, structs) as u32,
                3,
            ));
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            let tag = builder.ins().iconst(types::I64, if is_ok { 1 } else { 0 });
            builder.ins().store(MemFlagsData::trusted(), tag, ptr, 0);
            let val_addr = builder.ins().iadd_imm_s(ptr, OPT_VALUE_OFFSET as i64);
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
                // Every scalar: any integer width, `bool`, `char`, `str`.
                _ if is_set_elem(&t) => {}
                // A struct (`Point?`) is stored inline, as is a nested optional
                // (`some(some(..))` → `T??`) and an array.
                ConcreteType::Named(n) if structs.contains_key(n) => {}
                ConcreteType::Array(_) | ConcreteType::Optional(_) => {}
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
            let opt_ty = ConcreteType::Optional(Box::new(t.clone()));
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
            let old = if mut_binding_owns_slot_ref(&old_ty, structs) {
                Some(builder.ins().stack_load(types::I64, types::I64, slot, 0))
            } else {
                None
            };
            // `args` is already the effective list `[receiver, method args..]`;
            // its result is the mutated self.
            let (new_self, _) =
                compile_call(module, builder, cx, scopes, name, &info, args, span.clone())?;
            // Store the mutated receiver back, and refine the variable's type
            // to it (e.g. a `mut a = []` receiver pinned by the method's self).
            builder.ins().stack_store(types::I64, new_self, slot, 0);
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
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
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
            // by monomorphization; `compile_call` moves the owned args in. The
            // only path that may emit a `return_call`, so the only one that gets
            // tail position back.
            compile_call(
                module,
                builder,
                Cx { tail, ..cx },
                scopes,
                name,
                &info,
                args,
                span.clone(),
            )?
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
    recv_ty: &ConcreteType,
    a_v: Value,
    b_v: Option<Value>,
    recv_span: &Span,
) -> Result<(Value, ConcreteType), Error> {
    let structs = cx.structs;
    let builtins = cx.builtins;
    // Exact `str` plus `char[]`, not the broader `is_str_shaped` — matching
    // the char-at scope in `ExprKind::Index` (`Error`/concat-str receivers
    // aren't part of the slice surface).
    if *recv_ty == ConcreteType::Primitive(Primitive::Str) || is_char_array(recv_ty) {
        let b_v = match b_v {
            Some(b) => b,
            None => builtins.call(module, builder, "aipl_str_len", &[recv_v]),
        };
        let result = builtins.call(module, builder, "aipl_str_slice", &[recv_v, a_v, b_v]);
        scopes
            .last_mut()
            .expect("scope")
            .push(Tracked::new(result, recv_ty));
        return Ok((result, recv_ty.clone()));
    }
    if let ConcreteType::Array(elem) = recv_ty {
        let elem = (**elem).clone();
        let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
        let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
        let esz = builder
            .ins()
            .iconst(types::I64, runtime_elem_size(&elem, structs));
        let b_v = b_v.unwrap_or_else(|| builder.ins().iconst(types::I64, i64::MAX));
        let result = builtins.call(
            module,
            builder,
            "aipl_arr_slice",
            &[recv_v, a_v, b_v, drop_fn, retain_fn, esz],
        );
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

/// Compile `expr`, preferring the type the checker recorded on it over the one
/// derived here.
///
/// Codegen derives types from the AST just as the checker and monomorphizer do,
/// so a context-dependent expression — a bare `none`, an empty `[]` — derives a
/// `__none__`-style placeholder once whatever pinned it is out of view. Inlining
/// puts expressions in exactly that position, which is why the answer is
/// recorded at check time (see `Expr::ty`) and read back here.
///
/// Only a *placeholder* derivation is replaced: everywhere else what is derived
/// here is at least as good, and a recorded type is not a licence to override a
/// real one.
fn compile_expr<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    expr: &Expr,
) -> Result<(Value, ConcreteType), Error> {
    let (v, derived) = compile_expr_inner(module, builder, cx, scopes, expr)?;
    let locked = expr.ty.as_ref().and_then(|t| t.to_concrete());
    let ty = match &locked {
        Some(locked) if has_placeholder_ty(&derived) && !has_placeholder_ty(locked) => {
            locked.clone()
        }
        _ => derived,
    };
    Ok((v, ty))
}

/// Whether `t` still contains a "decided by context" placeholder.
fn has_placeholder_ty(t: &ConcreteType) -> bool {
    match t {
        ConcreteType::NoneInner | ConcreteType::EmptyArrayArg | ConcreteType::NoneLiteralArg => {
            true
        }
        ConcreteType::Optional(i) | ConcreteType::Array(i) | ConcreteType::Set(i) => {
            has_placeholder_ty(i)
        }
        ConcreteType::Dict(k, v) => has_placeholder_ty(k) || has_placeholder_ty(v),
        ConcreteType::Result(a, b) => has_placeholder_ty(a) || has_placeholder_ty(b),
        ConcreteType::Fn(ps, r) => ps.iter().any(has_placeholder_ty) || has_placeholder_ty(r),
        _ => false,
    }
}

fn compile_expr_inner<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    cx: Cx,
    scopes: &mut Vec<Vec<Tracked>>,
    expr: &Expr,
) -> Result<(Value, ConcreteType), Error> {
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
        str_data: _,
        elem_rc: _,
        ret_ty: _,
        sret: _,
        error_main: _,
        in_test: _,
        bindings: _,
        can_tail: _,
        tail,
    } = cx;
    // Tail position belongs to *this* expression, never automatically to what it
    // evaluates: clear it before any recursion, so a sub-expression is in tail
    // position only where an arm below deliberately puts it back with
    // `Cx { tail, ..cx }`. The polarity matters — an expression form added later
    // is non-tail by default rather than silently inheriting a wrong `true`.
    let cx = Cx { tail: false, ..cx };
    let span = expr.span.clone();
    Ok(match &expr.kind {
        ExprKind::KwArg(..) => unreachable!("keyword arguments are expanded by the loader"),
        ExprKind::Spread(..) => unreachable!("array spreads are desugared by the loader"),
        // Unit carries no value; hand back a placeholder i64 the unit type
        // forbids anyone from consuming, mirroring the unit-call result.
        ExprKind::Unit => (builder.ins().iconst(types::I64, 0), ConcreteType::Unit),
        ExprKind::Shim(_, bindings, body) => {
            // Install each bound function's address into its operation's slot
            // for the dynamic extent of `body`, then put back whatever was
            // there. Saving the previous occupant (rather than clearing) is what
            // makes shims nest and what lets an inner shim shadow an outer one.
            // The checker has already verified coverage and signatures.
            let mut saved: Vec<(Value, Value)> = Vec::with_capacity(bindings.len());
            for (op, f) in bindings {
                let slot = aipl_syntax::shim_slot_index(op)
                    .expect("checker verified this is a shimmable operation");
                let idx = builder.ins().iconst(types::I64, slot as i64);
                let old = builtins.call(module, builder, "aipl_shim_get", &[idx]);
                let Some(info) = cx.funcs.get(f) else {
                    return Err(Error::at(
                        format!("unknown shim function {:?}", display_name(f)),
                        expr.span.clone(),
                    ));
                };
                let FuncLink::User(id) = info.link else {
                    return Err(Error::at(
                        format!(
                            "shim function {:?} is a runtime builtin, which has no address to \
                             install",
                            display_name(f)
                        ),
                        expr.span.clone(),
                    ));
                };
                let addr = fn_addr_or_zero(builder, module, Some(id));
                builtins.call_void(module, builder, "aipl_shim_set", &[idx, addr]);
                saved.push((idx, old));
            }
            let (v, t) = compile_expr(module, builder, cx, scopes, body)?;
            for (idx, old) in saved {
                builtins.call_void(module, builder, "aipl_shim_set", &[idx, old]);
            }
            (v, t)
        }
        ExprKind::Return(value) => {
            // Early return: evaluate the value, hand the caller a reference,
            // release *every* live scope (we're leaving the function), then return
            // per the ABI — mirroring the function epilogue. Whatever follows in
            // the block is unreachable, so a fresh (dead) block receives it.
            let mark = scope_depth(scopes);
            // `return e` puts `e` in tail position however deep the `return`
            // sits — the epilogue below is exactly the cleanup a tail call
            // hoists ahead of the transfer.
            let (rv, rty) = compile_expr(
                module,
                builder,
                Cx {
                    tail: cx.can_tail,
                    ..cx
                },
                scopes,
                value,
            )?;
            let ret_val = if cx.error_main {
                // `fn main() -> !Error`: derive the exit code (printing
                // `error: <msg>` on the err side) before releasing the scopes.
                emit_error_main_exit_code(builder, module, builtins, rv)
            } else {
                rv
            };
            // Hand the caller a ref on the returned value (retaining nested heap
            // for composites). If `rv` is a fresh temporary we own, move it: skip
            // the retain and untrack it so the scope drop below won't free it.
            // (For `error_main`, `ret_val` is the derived exit code — not a tracked
            // value — so this never fires and the `rv` result is still dropped.)
            if needs_drop(cx.ret_ty, structs) && !move_owned_temp(scopes, mark, ret_val) {
                emit_retain(builder, module, builtins, structs, ret_val, cx.ret_ty);
            }
            for scope in scopes.iter() {
                for t in scope {
                    let v = match t.owned {
                        Owned::Value(v) => v,
                        Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
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
            (builder.ins().iconst(types::I64, 0), ConcreteType::Unit)
        }
        ExprKind::Num(n) => (
            builder.ins().iconst(types::I64, *n),
            ConcreteType::Primitive(Primitive::I64),
        ),
        ExprKind::Bool(b) => (
            builder.ins().iconst(types::I64, if *b { 1 } else { 0 }),
            ConcreteType::Primitive(Primitive::Bool),
        ),
        ExprKind::Char(c) => (
            builder.ins().iconst(types::I64, i64::from(*c)),
            ConcreteType::Primitive(Primitive::Char),
        ),
        ExprKind::Str(s) => {
            let (v, tracked) = emit_const_str(module, builder, cx, s.as_bytes())?;
            if tracked {
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(v, &ConcreteType::Primitive(Primitive::Str)));
            }
            (v, ConcreteType::Primitive(Primitive::Str))
        }
        ExprKind::Ident(name) => {
            // A local binding shadows everything; an unbound name may be a
            // nullary variant constructor (e.g. `Empty`), or a function used as
            // a value (`let f = inc;`) — its value is the function's code
            // address, materialized with `func_addr`.
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
            } else if let Some(info) = cx
                .funcs
                .get(name)
                .filter(|i| matches!(i.link, FuncLink::User(_)))
            {
                let FuncLink::User(id) = info.link else {
                    unreachable!("filtered to User links")
                };
                let fref = module.declare_func_in_func(id, builder.func);
                let addr = builder.ins().func_addr(types::I64, fref);
                let ty = ConcreteType::Fn(
                    info.param_types().cloned().collect(),
                    Box::new(info.return_ty.clone()),
                );
                (addr, ty)
            } else {
                env_load(builder, name, env, span.clone())?
            }
        }
        ExprKind::None => {
            // Allocate a 16-byte slot with tag = 0. Value field stays
            // undefined (callers must check is_some before touching it).
            // ConcreteType is Optional(__none__) — implicitly converts to any
            // Optional(T) via expect_type.
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                16,
                3,
            ));
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(types::I64, zero, slot, 0);
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            (
                ptr,
                ConcreteType::Optional(Box::new(ConcreteType::NoneInner)),
            )
        }
        // The only expression that can *become* a tail call: `tail` rides down
        // to `compile_call`, which decides whether this particular callee
        // qualifies.
        ExprKind::Call(name, args, style) => compile_call_expr(
            module,
            builder,
            Cx { tail, ..cx },
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
            // A boxed (recursive) struct lives on a refcounted heap block,
            // addressed by the returned pointer; a normal one in a fresh stack
            // slot. Only the store target and the return value differ.
            let boxed = structs[name].boxed();
            let scc = boxed.then(|| structs[name].scc());
            let slot = (!boxed).then(|| alloc_struct_slot(builder, layout));
            let heap = boxed.then(|| {
                let size_v = builder.ins().iconst(types::I64, layout.size as i64);
                let drop_fn = rec_drop_fn_addr(builder, module, cx.elem_rc, name);
                builtins.call(module, builder, "aipl_rec_alloc", &[size_v, drop_fn])
            });
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
                let before = scope_depth(scopes);
                let (v, actual) = compile_expr(module, builder, cx, scopes, &init.value)?;
                // `expect_type` (not `==`) so a `none` / empty `[]` value
                // coerces into an optional / array field.
                let ctx = format!("struct {:?} field {:?}", display_name(name), init.name);
                // `start..end` desugars to a `__builtin_Span` construction, so its
                // two fields are slice bounds by another name — accept either
                // signedness there, exactly as `xs[a..b]` does. Both are the same
                // 64-bit register, so the store is unaffected.
                if name == "__builtin_Span" {
                    expect_len_operand(&actual, &ctx, init.value.span.clone())?;
                } else {
                    // A bare literal takes the field's int type.
                    let actual = flex_int_ty(&init.value, &actual, &fty);
                    expect_type(&actual, &fty, &ctx, init.value.span.clone())?;
                }
                match (slot, heap) {
                    // Boxed: store into the heap payload via the block pointer.
                    (_, Some(base)) => {
                        let dst = builder.ins().iadd_imm_s(base, offset as i64);
                        store_array_elem(builder, dst, v, &fty, structs);
                    }
                    // Non-boxed: store into the stack slot. A scalar/heap field is
                    // an 8-byte value; an optional field is a 16-byte inline
                    // composite, so copy its bytes from the source slot.
                    (Some(slot), _) => {
                        if is_composite(&fty, structs) {
                            let size = field_size(&fty, structs);
                            let mut o = 0u32;
                            while o < size {
                                let chunk = builder.ins().load(
                                    types::I64,
                                    MemFlagsData::trusted(),
                                    v,
                                    o as i32,
                                );
                                builder.ins().stack_store(
                                    types::I64,
                                    chunk,
                                    slot,
                                    (offset + o) as i32,
                                );
                                o += 8;
                            }
                        } else {
                            builder
                                .ins()
                                .stack_store(types::I64, v, slot, offset as i32);
                        }
                    }
                    (None, None) => unreachable!("a struct is either boxed or slot-backed"),
                }
                // The struct co-owns each heap field. An *internal* field (one
                // referring to a boxed value of this same recursion group) is a
                // weak reference — retain it weakly, keep its strong-drop
                // tracking, and disable the move (see `compile_variant`); an
                // external field is moved-in when fresh, else co-owned via retain.
                // `scc` is `None` for a non-boxed struct, so `internal` is always
                // false there and the original move/retain path is unchanged.
                let internal = scc.is_some_and(|g| contains_scc_ref(&fty, g, structs));
                if internal {
                    emit_rc_w(
                        builder,
                        module,
                        builtins,
                        structs,
                        v,
                        &fty,
                        RcOp::Retain,
                        scc,
                    );
                } else if !move_owned_temp(scopes, before, v) {
                    emit_retain(builder, module, builtins, structs, v, &fty);
                }
            }
            let sty = ConcreteType::Named(name.clone());
            let ptr = match (slot, heap) {
                (_, Some(base)) => base,
                (Some(slot), _) => builder.ins().stack_addr(types::I64, slot, 0),
                (None, None) => unreachable!("a struct is either boxed or slot-backed"),
            };
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
            let ConcreteType::Named(ref struct_name) = obj_ty else {
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
                &ConcreteType::Primitive(Primitive::I64),
                "unary \"-\"",
                inner.span.clone(),
            )?;
            (
                builder.ins().ineg(v),
                ConcreteType::Primitive(Primitive::I64),
            )
        }
        ExprKind::Not(inner) => {
            let (v, t) = compile_expr(module, builder, cx, scopes, inner)?;
            expect_type(
                &t,
                &ConcreteType::Primitive(Primitive::Bool),
                "unary \"!\"",
                inner.span.clone(),
            )?;
            (
                builder.ins().bxor_imm_u(v, 1),
                ConcreteType::Primitive(Primitive::Bool),
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
            let lt = flex_int_ty(l, &lt, &rt);
            let rt = flex_int_ty(r, &rt, &lt);
            match op {
                // `+` is integer add only. User `+` resolves to a call to its
                // bound `wrapping_add`/`saturating_add` (intrinsified above), so a
                // primitive `+` Binop here is the increment sugar (`set n++`) or
                // mono's own index arithmetic — always wrapping. String concat is
                // `+++` (`'C'`).
                '+' => {
                    if is_int_ty(&lt) && lt == rt {
                        let ConcreteType::Primitive(p) = &lt else {
                            unreachable!()
                        };
                        (
                            emit_int_addsub(builder, lv, rv, *p, false, false),
                            lt.clone(),
                        )
                    } else {
                        expect_type(
                            &lt,
                            &ConcreteType::Primitive(Primitive::I64),
                            "arithmetic operand",
                            l.span.clone(),
                        )?;
                        expect_type(
                            &rt,
                            &ConcreteType::Primitive(Primitive::I64),
                            "arithmetic operand",
                            r.span.clone(),
                        )?;
                        (
                            builder.ins().iadd(lv, rv),
                            ConcreteType::Primitive(Primitive::I64),
                        )
                    }
                }
                // `+++` string concatenation. `Error` is str-represented, so it
                // concatenates like a `str`. Builds a *lazy concat node* (see
                // `aipl_concat_lazy`) rather than copying eagerly — the result is
                // still a `str` to the source, in the concat representation.
                'C' => {
                    if is_str_repr(&lt) && is_str_repr(&rt) {
                        // The tagged concat node takes ownership of its inputs, so
                        // inc both before the call to balance our local refs. The
                        // wide entry point retains for itself (`aipl_concat`), so
                        // there the pair is dropped rather than mirrored.
                        let ret = builtins.call(module, builder, "aipl_concat", &[lv, rv]);
                        scopes
                            .last_mut()
                            .expect("scope")
                            .push(Tracked::new(ret, &ConcreteType::Primitive(Primitive::Str)));
                        (ret, ConcreteType::Primitive(Primitive::Str))
                    } else {
                        return Err(Error::at(
                            "\"+++\" concatenates strings: both sides must be str".to_string(),
                            span.clone(),
                        ));
                    }
                }
                '-' | '*' | '/' | '%' => {
                    if is_int_ty(&lt) && lt == rt {
                        let ConcreteType::Primitive(p) = &lt else {
                            unreachable!()
                        };
                        let signed = p.int_signed();
                        let raw = match op {
                            '-' => builder.ins().isub(lv, rv),
                            '*' => builder.ins().imul(lv, rv),
                            '/' => saturating_div(builder, lv, rv, *p),
                            '%' if signed => builder.ins().srem(lv, rv),
                            '%' => builder.ins().urem(lv, rv),
                            _ => unreachable!(),
                        };
                        (canon_int(builder, raw, *p), lt.clone())
                    } else {
                        expect_type(
                            &lt,
                            &ConcreteType::Primitive(Primitive::I64),
                            "arithmetic operand",
                            l.span.clone(),
                        )?;
                        expect_type(
                            &rt,
                            &ConcreteType::Primitive(Primitive::I64),
                            "arithmetic operand",
                            r.span.clone(),
                        )?;
                        let v = match op {
                            '-' => builder.ins().isub(lv, rv),
                            '*' => builder.ins().imul(lv, rv),
                            '/' => saturating_div(builder, lv, rv, Primitive::I64),
                            '%' => builder.ins().srem(lv, rv),
                            _ => unreachable!(),
                        };
                        (v, ConcreteType::Primitive(Primitive::I64))
                    }
                }
                'E' | 'N' => {
                    // Structural equality for any two values of the same type
                    // (the checker already verified compatibility). Compute the
                    // common, fully-concrete type — `merge_types` resolves a
                    // `none`/`[]`/`#{}` operand against the other side — then walk
                    // it with `emit_eq`. `!=` is the bitwise negation of `==`.
                    let opn = if *op == 'E' { "==" } else { "!=" };
                    if matches!(lt, ConcreteType::Fn(_, _)) || matches!(rt, ConcreteType::Fn(_, _))
                    {
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
                    let eq = emit_eq(module, builder, cx, lv, rv, &cmp_ty)?;
                    let result = if *op == 'N' {
                        builder.ins().bxor_imm_u(eq, 1)
                    } else {
                        eq
                    };
                    (result, ConcreteType::Primitive(Primitive::Bool))
                }
                '<' | '>' | 'L' | 'G' => {
                    // `str` orders lexicographically by bytes — the same order
                    // `sort` gives a `str[]` — via a runtime compare whose
                    // sign is then tested against zero.
                    if is_str_repr(&lt) && is_str_repr(&rt) {
                        let c = builtins.call(module, builder, "aipl_str_cmp", &[lv, rv]);
                        let zero = builder.ins().iconst(types::I64, 0);
                        let cc = match op {
                            '<' => IntCC::SignedLessThan,
                            '>' => IntCC::SignedGreaterThan,
                            'L' => IntCC::SignedLessThanOrEqual,
                            _ => IntCC::SignedGreaterThanOrEqual,
                        };
                        let b = builder.ins().icmp(cc, c, zero);
                        return Ok((
                            builder.ins().uextend(types::I64, b),
                            ConcreteType::Primitive(Primitive::Bool),
                        ));
                    }
                    // Unsigned integers compare with the unsigned predicates;
                    // signed ones (and i64) with the signed predicates. Operands
                    // are kept canonically sign-/zero-extended, so an i64-register
                    // comparison is correct either way.
                    let signed = match &lt {
                        ConcreteType::Primitive(p) if is_int_ty(&lt) && lt == rt => p.int_signed(),
                        _ => {
                            expect_type(
                                &lt,
                                &ConcreteType::Primitive(Primitive::I64),
                                "comparison operand",
                                l.span.clone(),
                            )?;
                            expect_type(
                                &rt,
                                &ConcreteType::Primitive(Primitive::I64),
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
                        ConcreteType::Primitive(Primitive::Bool),
                    )
                }
                'A' | 'O' => {
                    expect_type(
                        &lt,
                        &ConcreteType::Primitive(Primitive::Bool),
                        "logical operand",
                        l.span.clone(),
                    )?;
                    expect_type(
                        &rt,
                        &ConcreteType::Primitive(Primitive::Bool),
                        "logical operand",
                        r.span.clone(),
                    )?;
                    let v = match op {
                        'A' => builder.ins().band(lv, rv),
                        'O' => builder.ins().bor(lv, rv),
                        _ => unreachable!(),
                    };
                    (v, ConcreteType::Primitive(Primitive::Bool))
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
                &ConcreteType::Primitive(Primitive::Bool),
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
            // Both branches are in tail position when the `if` is: whichever one
            // runs, its value is the `if`'s value.
            let (then_v, then_ty) =
                compile_expr(module, builder, Cx { tail, ..cx }, scopes, then_e)?;
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
            let (else_v, else_ty) =
                compile_expr(module, builder, Cx { tail, ..cx }, scopes, else_e)?;
            // Branch types must agree, with one twist: if one branch is
            // bare `none` (Optional(__none__)) and the other is a concrete
            // Optional(T), the result type is the concrete one.
            //
            // A bare integer literal in one branch also takes the other's int
            // type (`if (b) { u8_val } else { 9 }`), matching the checker.
            let then_ty = flex_int_ty(then_e, &then_ty, &else_ty);
            let else_ty = flex_int_ty(else_e, &else_ty, &then_ty);
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
            compile_expr(module, builder, Cx { tail, ..cx }, scopes, rest)?
        }
        // Monomorphization lifts lambdas into synthesized functions before
        // codegen; the checker rejects them otherwise. So none reach here.
        ExprKind::Lambda(_, _) => unreachable!("lambda reached codegen"),
        // Monomorphization lowers TupleLit to Construct before codegen.
        ExprKind::TupleLit(_) => unreachable!("TupleLit must be lowered before codegen"),
        ExprKind::Let(name, ty, value, body) => {
            // Evaluate the binding's value in the current scope (so any
            // string refcounts allocated by the value are already tracked
            // for dec at scope exit). Then extend the env with the new
            // name and compile the body.
            let (v, actual) = compile_expr(module, builder, cx, scopes, value)?;
            // The `let`'s annotation rides on `Expr`, which both sides of
            // monomorphization share, so it is abstract; by the time codegen
            // reads it every variable in it has been substituted.
            let declared = ty.as_ref().and_then(|t| t.to_concrete());
            let (v, t) = binding_ty(builder, value, v, &actual, declared.as_ref(), name)?;
            // An annotated empty `[]` bound at `char[]` must become the
            // *str-shaped* empty, not the array-shaped placeholder: a `char[]`
            // and a `str` are the same representation, and every `char[]`
            // operation (`push`, `extend`, `len`) reads it through the string
            // runtime. Without this the binding held an array block that
            // `aipl_str_len` then read as a string — a silent miscompile that
            // aborted on the first `push`. Call sites already coerced this way
            // (see `coerce_empty_to_char_array`); bindings did not.
            let v = coerce_empty_to_char_array(builder, module, builtins, scopes, v, &actual, &t);
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
                    tail,
                    ..cx
                },
                scopes,
                body,
            )?
        }
        ExprKind::LetMut(name, ty, value, body) => {
            let (v, actual) = compile_expr(module, builder, cx, scopes, value)?;
            // The `let`'s annotation rides on `Expr`, which both sides of
            // monomorphization share, so it is abstract; by the time codegen
            // reads it every variable in it has been substituted.
            let declared = ty.as_ref().and_then(|t| t.to_concrete());
            let (v, t) = binding_ty(builder, value, v, &actual, declared.as_ref(), name)?;
            // An annotated empty `[]` bound at `char[]` must become the
            // *str-shaped* empty, not the array-shaped placeholder: a `char[]`
            // and a `str` are the same representation, and every `char[]`
            // operation (`push`, `extend`, `len`) reads it through the string
            // runtime. Without this the binding held an array block that
            // `aipl_str_len` then read as a string — a silent miscompile that
            // aborted on the first `push`. Call sites already coerced this way
            // (see `coerce_empty_to_char_array`); bindings did not.
            let v = coerce_empty_to_char_array(builder, module, builtins, scopes, v, &actual, &t);
            reject_unit_binding(&t, name, value.span.clone())?;
            // 8-byte slot, 8-byte aligned: fits any i64/bool/char, and any heap
            // value the binding holds a *pointer* to — which is every composite
            // (a struct binding deliberately points at its value; see the `set`
            // arm's sret-buffer note).
            //
            // The one exception is a wide `str`/`char[]`, which lives *in* the
            // slot as its whole 24 bytes rather than being pointed at. Only that
            // case widens: broadening it to every composite silently switched
            // struct bindings from pointer-holding to value-holding, and the
            // rest of the code still read them the old way.
            let slot = if is_str_shaped(&t) {
                value_slot(builder, &t, structs)
            } else {
                builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ))
            };
            store_binding(builder, cx, slot, v, &t, structs);
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
                ConcreteType::Array(_) => {
                    matches!(&value.kind, ExprKind::ArrayLit(_))
                        || matches!(&value.kind, ExprKind::Call(n, _, _) if n == "__builtin_with_capacity")
                }
                ConcreteType::Primitive(Primitive::Str) => matches!(&value.kind, ExprKind::Str(_)),
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
            if is_str_shaped(&t) {
                // A `str` binding's slot owns exactly one reference to its current
                // value, released once at scope exit by this slot-track. `set`
                // preserves the invariant (drop the old value, take ownership of
                // the new), so the binding can be reassigned — even across a nested
                // scope, e.g. `set s = s[..]` in a loop body — without leaking or
                // freeing a value the slot still points at.
                //
                // `is_str_shaped`, so a `char[]` binding is owned this way too.
                // It used to fall through to the array branches, which give the
                // slot no reference at all and let the *value* track own it — and
                // a `push`/`extend` inside a loop then tracked each rebuilt value
                // in the loop-body scope, which freed it at the end of the
                // iteration while the binding still named it. The next iteration's
                // allocation reused the block, so the copy read from the buffer it
                // was writing into (`extend/extend_char_in_place.aipl`).
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
            } else if mut_binding_owns_slot_ref(&t, structs) {
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
            } else if is_composite(&t, structs) && !is_str_shaped(&t) {
                // The `!is_str_shaped` guard matters only under the wide `str`,
                // where `is_composite` starts answering *true* for `str`/`char[]`
                // — they travel by address like any other composite. Their
                // ownership model is not the composite one, though: the branch
                // above deliberately excludes `char[]` from
                // `mut_binding_owns_slot_ref` because the `push` path assumes the
                // slot holds no reference of its own. Without this guard a wide
                // `char[]` binding fell in here and took one anyway, so the slot
                // retained the *old* value and then released whatever the slot
                // held at scope exit — the value `push` had replaced it with.
                // Net: the old value leaked and the new one was released twice.
                //
                // A composite (inline struct / optional / result) `mut` binding:
                // the slot takes its own reference on the value's heap-bearing
                // fields, released by this slot-track at scope exit or by the
                // `set` that replaces it — the same ownership the array arm above
                // gives, which `set` relies on to release the outgoing value
                // without double-freeing one the value-track still owns.
                //
                // The slot keeps pointing at the value's *own* storage rather
                // than a copy, so an alias taken before a later `set` still sees
                // the value it was bound to.
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
                    tail,
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
                        && (aipl_mono::builtin_is_mutating(f)
                            || funcs.get(f).is_some_and(|i| i.is_mutating))
            );
            if is_writeback_call {
                compile_expr(module, builder, cx, scopes, value)?;
                return compile_expr(module, builder, Cx { tail, ..cx }, scopes, body);
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
            // In-place concat: `set s = s +++ r` on an exclusive `str` binding
            // appends `r` into `s`'s own buffer instead of building a fresh
            // string (or a rope node) each time. The binding is slot-tracked, so
            // the — possibly relocated — buffer is still dropped exactly once.
            //
            // The operator is `'C'` (`+++`), not `'+'` — `'+'` is integer
            // addition and never has `str` operands, so the arm that spelled it
            // that way could not fire, and neither could the matching one in
            // `mono::aliases_or_unsafe` that decides `exclusive`. Both are fixed
            // together: either alone leaves the optimization off.
            if exclusive && expected_ty == ConcreteType::Primitive(Primitive::Str) {
                let appends_self = match &value.kind {
                    ExprKind::Binop(l, 'C', r) if matches!(&l.kind, ExprKind::Ident(n) if n == name) => {
                        Some(r)
                    }
                    _ => None,
                };
                if let Some(r) = appends_self {
                    let s_ptr = load_binding_str(builder, slot);
                    let (rv, rt) = compile_expr(module, builder, cx, scopes, r)?;
                    expect_type(
                        &rt,
                        &ConcreteType::Primitive(Primitive::Str),
                        "concat operand",
                        r.span.clone(),
                    )?;
                    // `aipl_str_append` borrows `r` and takes over the reference
                    // the slot was holding, writing the result back into the same
                    // slot — so there is no inc, no dec and no new value-track:
                    // `r`'s own track still releases it, and the binding's
                    // slot-track already owns whatever the slot now names.
                    builtins.call_void(module, builder, "aipl_str_append", &[s_ptr, rv]);
                    return compile_expr(module, builder, Cx { tail, ..cx }, scopes, body);
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
                    // Same slot discipline as the concat branch above, and for
                    // the same reason: a wide `str` binding *is* its slot's 24
                    // bytes, so it is read with `load_binding_str` and written
                    // with `store_binding_str`. Reading one word out of the slot
                    // and calling `aipl_trim` on it dereferenced `w0` — a buffer's
                    // base, or an inline value's first eight content bytes — as
                    // if it were a `Str`, which bus-errors on any inline
                    // receiver.
                    let s_ptr = load_binding_str(builder, slot);
                    // `aipl_trim` borrows `s` and hands back a *retained* window
                    // into the same buffer, so the trim allocates nothing and the
                    // result keeps the block alive on its own. That makes the
                    // slot's own reference surplus: release it — after the call,
                    // and before the store, since `s_ptr` names the slot rather
                    // than a snapshot of what was in it.
                    let new_ptr = builtins.call(module, builder, "aipl_trim", &[s_ptr]);
                    emit_rc(
                        builder,
                        module,
                        builtins,
                        structs,
                        s_ptr,
                        &ConcreteType::Primitive(Primitive::Str),
                        RcOp::Drop,
                    );
                    store_binding_str(builder, cx, slot, new_ptr, structs);
                    // No new track — the binding's slot-track owns the result.
                    return compile_expr(module, builder, Cx { tail, ..cx }, scopes, body);
                }
            }
            // In-place union: `set a = a.union(b)` on an exclusive set binding
            // extends `a`'s allocation with `b`'s elements rather than building a
            // fresh set (mirrors the in-place `+`/`trim` above). Skipped when
            // `a`'s element type is still `__none__` (a `mut a = #{}`); that falls
            // through to the copy path, which merges to `b`'s concrete type.
            if exclusive {
                if let ConcreteType::Set(elem) = &expected_ty {
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
                        let a_ptr = builder.ins().stack_load(types::I64, types::I64, slot, 0);
                        let (b_ptr, b_ty) = compile_expr(module, builder, cx, scopes, other)?;
                        expect_type(&b_ty, &expected_ty, "union operand", other.span.clone())?;
                        // `aipl_set_union_mut` decs `b`; inc first so b's own track
                        // balances. `a` is reused, not dec'd.
                        builtins.call_void(module, builder, "aipl_arr_inc", &[b_ptr]);
                        let drop_fn = array_drop_fn_addr(builder, module, cx, elem);
                        let retain_fn = array_retain_fn_addr(builder, module, cx, elem);
                        let esz = builder
                            .ins()
                            .iconst(types::I64, runtime_elem_size(elem, structs));
                        let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&elem));
                        let new_ptr = builtins.call(
                            module,
                            builder,
                            "aipl_set_union_mut",
                            &[a_ptr, b_ptr, drop_fn, retain_fn, esz, str_cmp],
                        );
                        builder.ins().stack_store(types::I64, new_ptr, slot, 0);
                        // No new track — the binding's slot-track owns the result.
                        return compile_expr(module, builder, Cx { tail, ..cx }, scopes, body);
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
            let arr_slot_ref = mut_binding_owns_slot_ref(&expected_ty, structs);
            // `is_str_shaped`, matching `LetMut`: a `char[]` binding's slot owns
            // one reference like a `str`'s, so `set` has to maintain that here
            // too. Testing `is_str_repr` let a `char[]` skip both halves — it
            // neither took ownership of the incoming value nor released the
            // outgoing one — so every reassignment leaked the value it replaced.
            let old = if is_str_shaped(&expected_ty) || arr_slot_ref {
                Some(slot_value(builder, slot, &expected_ty))
            } else {
                None
            };
            let (v, t) = compile_expr(module, builder, cx, scopes, value)?;
            // A bare literal takes the binding's int type.
            let t = flex_int_ty(value, &t, &expected_ty);
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
                    builder.ins().stack_store(types::I64, v, slot, 0);
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
                    // Release before the store, not after. A tagged `old` is a
                    // pointer copied out of the slot, so either order worked; a
                    // wide `old` is the slot's own address, and once the new
                    // value is written there it names *that* — so a release
                    // afterwards frees what was just assigned. The new value has
                    // already taken its own reference above, so anything it
                    // shares with the old one survives.
                    //
                    // (The composite arm below reasons its way to the same order
                    // from a different direction: its buffer is reused across
                    // loop iterations, so the outgoing value lives in the very
                    // storage being overwritten.)
                    emit_drop(builder, module, builtins, structs, old, &expected_ty);
                    store_binding_str(builder, cx, slot, v, structs);
                }
            } else if is_composite(&expected_ty, structs) && !is_str_shaped(&expected_ty) {
                // The `!is_str_shaped` guard is the same one `LetMut` needs, for
                // the same reason: under the wide `str`, `is_composite` starts
                // answering *true* for `str`/`char[]`, and this arm's ownership
                // model is not theirs. A wide `char[]` binding fell in here and
                // had its old value released as if the slot held a pointer to a
                // composite buffer, aborting on the value's own first word.
                //
                // Copy the incoming composite into a buffer belonging to this
                // `set`, and point the slot there.
                //
                // The value is usually the hidden-sret buffer of the call that
                // produced it, and a call site inside a loop reuses one buffer
                // every iteration — so pointing the binding straight at it makes
                // the next `set recv = f(recv)` read its argument out of the very
                // buffer the call is writing into. The copy breaks that: the
                // binding's storage is distinct from any callee's scratch.
                //
                // Ownership mirrors the array arm: retain the incoming fields
                // before releasing the outgoing ones, since `set s = f(s)` hands
                // back a value built from `s`'s own fields and dropping first
                // could free one the new value still holds.
                let size = field_size(&expected_ty, structs);
                let buf = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    size,
                    3,
                ));
                let buf_addr = builder.ins().stack_addr(types::I64, buf, 0);
                let old_ptr = builder.ins().stack_load(types::I64, types::I64, slot, 0);
                // Retain, then release, then overwrite — in that order. Retain
                // first because `set s = f(s)` hands back a value built from
                // `s`'s own fields, so releasing first could free one the new
                // value still holds. Release *before* the copy because on the
                // second and later trips through a loop this buffer already *is*
                // the outgoing value's storage (one buffer per `set` site), and
                // copying first would leave the drop reading — and freeing — the
                // fields just written.
                emit_retain(builder, module, builtins, structs, v, &expected_ty);
                emit_drop(builder, module, builtins, structs, old_ptr, &expected_ty);
                copy_composite(builder, buf_addr, v, &expected_ty, structs);
                builder.ins().stack_store(types::I64, buf_addr, slot, 0);
            } else {
                // `store_binding`, not a bare word store: a wide `str`/`char[]`
                // lives in the slot as its whole value. Ownership is unchanged
                // from the tagged path — this arm takes none.
                store_binding(builder, cx, slot, v, &expected_ty, structs);
            }
            // Body uses the unchanged env; the slot has been updated in-place
            // so subsequent Ident lookups will load the new value.
            compile_expr(module, builder, Cx { tail, ..cx }, scopes, body)?
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
            let str_cursor =
                if it_ty == ConcreteType::Primitive(Primitive::Str) || is_char_array(&it_ty) {
                    let cur = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        iter_state_size(),
                        3,
                    ));
                    let cur_addr = builder.ins().stack_addr(types::I64, cur, 0);
                    builtins.call_void(module, builder, "aipl_str_iter_init", &[cur_addr, it_ptr]);
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
            builder.ins().stack_store(types::I64, zero, slot, 0);

            let header = builder.create_block();
            let body_block = builder.create_block();
            let exit = builder.create_block();
            builder.ins().jump(header, &[]);

            // Header: load i, decide whether to continue, and (for arrays)
            // fetch the element. `var_value`/`var_ty` are what the body's
            // loop variable binds to.
            builder.switch_to_block(header);
            let i = builder.ins().stack_load(types::I64, types::I64, slot, 0);
            let (var_value, var_ty) = match &it_ty {
                t if *t == ConcreteType::Primitive(Primitive::Str) || is_char_array(t) => {
                    // Pull the next byte from the cursor; `-1` signals the end (so
                    // a rope is walked leaf-by-leaf, never flattened, and we never
                    // index out of bounds).
                    let byte_i64 = emit_str_iter_next(module, builder, builtins, str_cursor);
                    let more =
                        builder
                            .ins()
                            .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, byte_i64, 0);
                    builder.ins().brif(more, body_block, &[], exit, &[]);
                    (byte_i64, ConcreteType::Primitive(Primitive::Char))
                }
                ConcreteType::Array(inner) => {
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
            if it_ty == ConcreteType::Primitive(Primitive::Str) || is_char_array(&it_ty) {
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
            let next = builder.ins().iadd_imm_s(i, 1);
            builder.ins().stack_store(types::I64, next, slot, 0);
            builder.ins().jump(header, &[]);
            builder.seal_block(header);

            builder.switch_to_block(exit);
            builder.seal_block(exit);
            (
                builder.ins().iconst(types::I64, 0),
                ConcreteType::Primitive(Primitive::I64),
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
                &ConcreteType::Primitive(Primitive::Bool),
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
                ConcreteType::Primitive(Primitive::I64),
            )
        }
        ExprKind::Match(scrutinee, arms) => {
            let (ptr, scrut_ty) = compile_expr(module, builder, cx, scopes, scrutinee)?;

            let merge = builder.create_block();
            builder.append_block_param(merge, types::I64);
            let arm_blocks: Vec<_> = arms.iter().map(|_| builder.create_block()).collect();

            // A `str` or array scrutinee dispatches by *value* — no tag word — so
            // `plan` is `None`; its arms' binders (array-pattern identifier
            // elements) are read from the scrutinee per-arm below. Any other
            // matchable type dispatches on its tag via a `MatchPlan`.
            // A `char` scrutinee is a bare scalar: dispatch is one integer
            // comparison per arm, with the `_` arm as the fallthrough. No length,
            // no elements and no refcounting, so it gets its own short path
            // rather than riding the sequence machinery below.
            let plan = if matches!(&scrut_ty, ConcreteType::Primitive(Primitive::Char)) {
                for (i, arm) in arms.iter().enumerate() {
                    let Pattern::Char(c) = &arm.pattern else {
                        continue; // the `_` arm — the fallthrough jump below
                    };
                    let hit = builder.ins().icmp_imm_s(IntCC::Equal, ptr, *c as i64);
                    let next = builder.create_block();
                    builder.ins().brif(hit, arm_blocks[i], &[], next, &[]);
                    builder.switch_to_block(next);
                    builder.seal_block(next);
                }
                let wildcard = arms
                    .iter()
                    .position(|a| matches!(a.pattern, Pattern::Wildcard))
                    .expect("a char match has a `_` arm (checked)");
                builder.ins().jump(arm_blocks[wildcard], &[]);
                None
            } else if is_str_repr(&scrut_ty) || matches!(&scrut_ty, ConcreteType::Array(_)) {
                // Route each arm to its block: a `str`-literal arm (`"foo"`, str
                // scrutinee) by content equality, an array pattern (`[a, 'x', b]`)
                // by exact length then elementwise equality of its *literal*
                // elements (its identifier elements are binders — no comparison),
                // and the `_` arm as the final fallthrough. A `str` is matched as
                // its bit-identical `char[]` view, so `seq_len`/`seq_elem` take a
                // `char[]` type. The scrutinee is *borrowed* (each comparison
                // balances the refs it consumes); its one real drop is left to
                // enclosing-scope tracking.
                let seq_ty = match &scrut_ty {
                    ConcreteType::Array(_) => scrut_ty.clone(),
                    _ => ConcreteType::Array(Box::new(ConcreteType::Primitive(Primitive::Char))),
                };
                let elem_ty: ConcreteType = match &seq_ty {
                    ConcreteType::Array(e) => (**e).clone(),
                    _ => unreachable!("seq_ty is always an array"),
                };
                let wildcard = arms
                    .iter()
                    .position(|a| matches!(a.pattern, Pattern::Wildcard))
                    .expect("a str/array match has a `_` arm (checked)");
                // Compile each array pattern's *literal* elements up front in this
                // (dominating) block, so a tracked literal (a heap str / nested
                // array) is defined where it dominates both the per-arm comparison
                // and the enclosing-scope drop. Binder (identifier) elements carry
                // no value here — they're loaded when their arm is taken. Each entry
                // is `(element index, value)`.
                let arm_lits: Vec<Vec<(usize, Value)>> = arms
                    .iter()
                    .map(|arm| {
                        let mut vals = Vec::new();
                        if let Pattern::Array(elems) = &arm.pattern {
                            for (j, e) in elems.iter().enumerate() {
                                if matches!(e.kind, ExprKind::Ident(_)) {
                                    continue; // a binder, not a comparison
                                }
                                let (v, _) = compile_expr(module, builder, cx, scopes, e)?;
                                vals.push((j, v));
                            }
                        }
                        Ok(vals)
                    })
                    .collect::<Result<_, Error>>()?;
                // `seq_len` is only needed by array-pattern arms; a pure
                // string-literal match (no array arms) skips it, so it compiles
                // exactly as before this path was unified.
                let scrut_len = if arms.iter().any(|a| matches!(a.pattern, Pattern::Array(_))) {
                    Some(seq_len(module, builder, builtins, ptr, &seq_ty))
                } else {
                    None
                };
                for (i, arm) in arms.iter().enumerate() {
                    match &arm.pattern {
                        // A char literal never reaches here: it only matches a
                        // `char` scrutinee, which took the scalar path above.
                        Pattern::Char(_) => unreachable!("char arm on a str/array match"),
                        Pattern::Str(lit) => {
                            let lit_expr =
                                Expr::new(ExprKind::Str(lit.clone()), scrutinee.span.clone());
                            let (lit_val, _) =
                                compile_expr(module, builder, cx, scopes, &lit_expr)?;
                            // Both borrowed: `str_eq` keeps neither.
                            let eq = builtins.call(module, builder, "aipl_str_eq", &[ptr, lit_val]);
                            let next = builder.create_block();
                            builder.ins().brif(eq, arm_blocks[i], &[], next, &[]);
                            builder.switch_to_block(next);
                            builder.seal_block(next);
                        }
                        Pattern::Array(elems) => {
                            // Length must match before any element load (else out of
                            // bounds).
                            let len_ok = builder.ins().icmp_imm_s(
                                IntCC::Equal,
                                scrut_len.expect("an array arm implies scrut_len is computed"),
                                elems.len() as i64,
                            );
                            let check = builder.create_block();
                            let next = builder.create_block();
                            builder.ins().brif(len_ok, check, &[], next, &[]);
                            builder.switch_to_block(check);
                            builder.seal_block(check);
                            // AND together the literal elements' comparisons; binder
                            // elements impose no test. The scrutinee elements are
                            // borrowed; `emit_eq` balances any ref it consumes.
                            let mut matched = builder.ins().iconst(types::I64, 1);
                            for (j, lit_val) in &arm_lits[i] {
                                let idx = builder.ins().iconst(types::I64, *j as i64);
                                let scrut_elem =
                                    seq_elem(module, builder, builtins, structs, ptr, idx, &seq_ty);
                                let eq =
                                    emit_eq(module, builder, cx, scrut_elem, *lit_val, &elem_ty)?;
                                matched = builder.ins().band(matched, eq);
                            }
                            builder.ins().brif(matched, arm_blocks[i], &[], next, &[]);
                            builder.switch_to_block(next);
                            builder.seal_block(next);
                        }
                        // The `_` arm is the fallthrough; a ctor can't reach a
                        // str/array match (checker).
                        Pattern::Wildcard | Pattern::Ctor { .. } => {}
                    }
                }
                builder.ins().jump(arm_blocks[wildcard], &[]);
                None
            } else {
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), ptr, 0);
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
                            let hit =
                                builder
                                    .ins()
                                    .icmp_imm_s(IntCC::Equal, tag, arm_tags[i] as i64);
                            builder.ins().brif(hit, arm_blocks[i], &[], next, &[]);
                            builder.switch_to_block(next);
                            builder.seal_block(next);
                        }
                        builder.ins().jump(arm_blocks[arm_blocks.len() - 1], &[]);
                    }
                }
                Some((plan, tag))
            };

            let mut merged_ty: Option<ConcreteType> = None;
            // The arm that produced `merged_ty`, so an earlier literal arm can
            // still flex to a later narrow-int one.
            let mut merged_body: Option<&Expr> = None;
            for (i, arm) in arms.iter().enumerate() {
                builder.switch_to_block(arm_blocks[i]);
                builder.seal_block(arm_blocks[i]);
                scopes.push(Vec::new());
                // Read this arm's payload bindings (borrowed from the scrutinee). A
                // tag-dispatched arm reads its variant/optional payload; a str/array
                // arm (`plan` is `None`) reads each array-pattern binder (identifier
                // element) from the scrutinee at its position.
                let binds = match &plan {
                    Some((plan, tag)) => bind_match_arm(builder, plan, arm, i, ptr, *tag, structs),
                    None => {
                        if let Pattern::Array(elems) = &arm.pattern {
                            // A `str` scrutinee is read as its `char[]` view.
                            let seq_ty = match &scrut_ty {
                                ConcreteType::Array(_) => scrut_ty.clone(),
                                _ => ConcreteType::Array(Box::new(ConcreteType::Primitive(
                                    Primitive::Char,
                                ))),
                            };
                            let elem_ty: ConcreteType = match &seq_ty {
                                ConcreteType::Array(e) => (**e).clone(),
                                _ => unreachable!("seq_ty is always an array"),
                            };
                            elems
                                .iter()
                                .enumerate()
                                .filter_map(|(j, e)| match &e.kind {
                                    ExprKind::Ident(name) => {
                                        let idx = builder.ins().iconst(types::I64, j as i64);
                                        let val = seq_elem(
                                            module, builder, builtins, structs, ptr, idx, &seq_ty,
                                        );
                                        Some((name.clone(), val, elem_ty.clone()))
                                    }
                                    _ => None,
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    }
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
                // Every arm is in tail position when the `match` is: whichever
                // one runs, its value is the `match`'s value.
                let (av, at) = compile_expr(
                    module,
                    builder,
                    Cx {
                        env: &arm_env,
                        tail,
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
                // Mirrors the checker: a bare-literal arm flexes to a narrow-int
                // arm in either order. The emitted value is the same 64-bit
                // constant either way — only its static type moves — so retyping
                // after the fact is sound.
                merged_ty = Some(match merged_ty.take() {
                    None => {
                        merged_body = Some(&arm.body);
                        at
                    }
                    Some(prev) => {
                        let at = flex_int_ty(&arm.body, &at, &prev);
                        let prev = match merged_body {
                            Some(b) => flex_int_ty(b, &prev, &at),
                            None => prev,
                        };
                        let m = merge_types(&prev, &at).ok_or_else(|| {
                            Error::at(
                                format!(
                                    "match arms have mismatched types: {} vs {}",
                                    type_name(&prev),
                                    type_name(&at),
                                ),
                                span.clone(),
                            )
                        })?;
                        merged_body = Some(&arm.body);
                        m
                    }
                });
            }

            builder.switch_to_block(merge);
            builder.seal_block(merge);
            let result = builder.block_params(merge)[0];
            let merged_ty = merged_ty.unwrap_or(ConcreteType::Primitive(Primitive::I64));
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
            let mut elem_ty: Option<ConcreteType> = None;
            // Each element carries whether it's a fresh temporary we exclusively
            // own: true when compiling it grew the scope and left its result as the
            // last-tracked entry (a call result, constructor, …). The store loop
            // below then *moves* such an element into the array (no retain, no
            // separate scope-exit drop) instead of co-owning it. A borrowed place
            // (a bare variable) tracks nothing new, so it's co-owned as before.
            let mut vals: Vec<(Value, ConcreteType, bool)> = Vec::with_capacity(elems.len());
            for el in elems {
                let before = scope_depth(scopes);
                let (v, mut t) = compile_expr(module, builder, cx, scopes, el)?;
                match &elem_ty {
                    None => {
                        let ok = is_array_elem(&t)
                            || matches!(&t, ConcreteType::Optional(_))
                            || matches!(&t, ConcreteType::Named(n) if structs.contains_key(n));
                        if !ok {
                            return Err(Error::at(
                                format!(
                                    "array elements must be an integer (i8..i64, u8..u64), bool, char, str, or an array, got {}",
                                    type_name(&t)
                                ),
                                el.span.clone(),
                            ));
                        }
                        elem_ty = Some(t.clone());
                    }
                    // A bare literal element flexes to the element type the
                    // first element established, so `[u64_max(), 1]` is `u64[]`.
                    Some(expected) => {
                        t = flex_int_ty(el, &t, expected);
                        expect_type(&t, expected, "array element", el.span.clone())?;
                    }
                }
                let owned_temp = owned_temp_since(scopes, before, v);
                vals.push((v, t, owned_temp));
            }
            let elem = elem_ty.unwrap_or(ConcreteType::NoneInner);
            let arr_ty = ConcreteType::Array(Box::new(elem.clone()));
            if is_char_array(&arr_ty) {
                // `char[]` is str-shaped (see `is_char_array`): build a heap
                // `str` buffer and write each element's byte directly, rather
                // than a generic array block. `vals` is non-empty here — an
                // empty `[]` never infers `elem == char` (it stays the
                // untyped `NoneInner` element and takes the generic path
                // below), so there's no empty/SSO case to special-case.
                // Always heap-allocates (no small-string inlining yet).
                let len = builder.ins().iconst(types::I64, vals.len() as i64);
                // Two representations, two shapes. The old one *is* the content
                // pointer, so the bytes go straight where `aipl_str_alloc` hands
                // back. The new one is a value that merely *points* at its
                // buffer, so the write cursor comes from `aipl_str_write_ptr`
                // and the length is recorded afterwards with `aipl_str_grew`
                // (allocation gives capacity, not length). Getting this wrong is
                // what made `['c', 'a', 'b'].len()` read content bytes as a
                // pointer.
                let value = {
                    let v = builtins.call(module, builder, "aipl_str_alloc", &[len]);
                    let cursor = builtins.call(module, builder, "aipl_str_write_ptr", &[v]);
                    for (i, (b, _, _)) in vals.into_iter().enumerate() {
                        let addr = builder.ins().iadd_imm_s(cursor, i as i64);
                        builder.ins().istore8(MemFlagsData::trusted(), b, addr, 0);
                    }
                    builtins.call_void(module, builder, "aipl_str_grew", &[v, len]);
                    v
                };
                scopes
                    .last_mut()
                    .expect("scope")
                    .push(Tracked::new(value, &arr_ty));
                return Ok((value, arr_ty));
            }
            let len = builder.ins().iconst(types::I64, elems.len() as i64);
            let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
            let esz_v = builder
                .ins()
                .iconst(types::I64, runtime_elem_size(&elem, structs));
            let ptr = builtins.call(module, builder, "aipl_array_new", &[len, drop_fn, esz_v]);
            let elems_base = builder.ins().iadd_imm_s(ptr, ARR_ELEMS_OFFSET as i64);
            if is_bit_packed(&elem) {
                // Pack 8 bools per byte: build each data byte from its (up to 8)
                // element bits and store it. Bools carry no heap (no retain).
                for (j, chunk) in vals.chunks(8).enumerate() {
                    let mut byte = builder.ins().iconst(types::I64, 0);
                    for (k, (v, _, _)) in chunk.iter().enumerate() {
                        let bit = builder.ins().ishl_imm_u(*v, k as i64);
                        byte = builder.ins().bor(byte, bit);
                    }
                    let addr = builder.ins().iadd_imm_s(elems_base, j as i64);
                    builder
                        .ins()
                        .istore8(MemFlagsData::trusted(), byte, addr, 0);
                }
            } else {
                let esz = elem_size_of(&elem, structs);
                let mut moved: Vec<Value> = Vec::new();
                for (i, (v, src_ty, owned_temp)) in vals.into_iter().enumerate() {
                    let slot = builder.ins().iadd_imm_s(elems_base, i as i64 * esz);
                    // Copy the element's own size (a `none` is narrower than a
                    // wider optional element slot — its unread tail is don't-care).
                    store_array_elem(builder, slot, v, &src_ty, structs);
                    if owned_temp {
                        // Move: the array inherits the temporary's ref, so no retain
                        // — and untrack it below so it isn't dropped again at scope
                        // exit (the array's drop-fn releases it).
                        moved.push(v);
                    } else {
                        // Borrowed element: the array co-owns it — retain on store.
                        emit_retain(builder, module, builtins, structs, v, &elem);
                    }
                }
                if !moved.is_empty() {
                    let scope = scopes.last_mut().expect("scope");
                    scope.retain(|t| !matches!(t.owned, Owned::Value(x) if moved.contains(&x)));
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
            let mut elem_ty: Option<ConcreteType> = None;
            let mut vals = Vec::with_capacity(elems.len());
            for el in elems {
                let (v, t) = compile_expr(module, builder, cx, scopes, el)?;
                match &elem_ty {
                    None => {
                        if !is_set_elem(&t) {
                            return Err(Error::at(
                                format!(
                                    "set elements must be an integer (i8..i64, u8..u64), bool, char, or str, got {}",
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
            let elem = elem_ty.unwrap_or(ConcreteType::NoneInner);
            let esz = runtime_elem_size(&elem, structs);
            let esz_v = builder.ins().iconst(types::I64, esz);
            // `str` elements are heap: store the array `str` drop/retain helpers
            // so the set frees/retains them, and compare membership by content.
            let drop_fn = array_drop_fn_addr(builder, module, cx, &elem);
            let retain_fn = array_retain_fn_addr(builder, module, cx, &elem);
            let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&elem));
            let cap = builder.ins().iconst(types::I64, elems.len() as i64);
            let mut ptr = builtins.call(
                module,
                builder,
                "aipl_array_with_cap",
                &[cap, drop_fn, esz_v],
            );
            for v in vals {
                // `aipl_set_insert` reads the element through a pointer; spill
                // the value (a `bool` is read back as i64, a `str` as its
                // pointer) and pass its address.
                let s = value_slot(builder, &elem, structs);
                let x_ptr = builder.ins().stack_addr(types::I64, s, 0);
                store_array_elem(builder, x_ptr, v, &elem, structs);
                ptr = builtins.call(
                    module,
                    builder,
                    "aipl_set_insert",
                    &[ptr, x_ptr, drop_fn, retain_fn, esz_v, str_cmp],
                );
            }
            let set_ty = ConcreteType::Set(Box::new(elem));
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
            let mut key_ty: Option<ConcreteType> = None;
            let mut val_ty: Option<ConcreteType> = None;
            let mut vals = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                let (kv, kt) = compile_expr(module, builder, cx, scopes, k)?;
                let (vv, vt) = compile_expr(module, builder, cx, scopes, v)?;
                match &key_ty {
                    None => {
                        if !is_dict_key(&kt) {
                            return Err(Error::at(
                                format!(
                                    "dict keys must be an integer (i8..i64, u8..u64), bool, char, or str, got {}",
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
            let key = key_ty.unwrap_or(ConcreteType::NoneInner);
            let val = val_ty.unwrap_or(ConcreteType::NoneInner);
            let pair_size = dict_pair_size(&key, &val, structs);
            let psz = builder.ins().iconst(types::I64, pair_size);
            let (drop_fn, retain_fn) = pair_rc_fn_addrs(builder, module, cx, &key, &val);
            let str_cmp = builder.ins().iconst(types::I64, str_cmp_width(&key));
            let cap = builder.ins().iconst(types::I64, pairs.len() as i64);
            let mut ptr =
                builtins.call(module, builder, "aipl_array_with_cap", &[cap, drop_fn, psz]);
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
                let vaddr = builder
                    .ins()
                    .iadd_imm_s(pbase, dict_key_size(&key, structs));
                store_array_elem(builder, vaddr, vv, &val, structs);
                ptr = builtins.call(
                    module,
                    builder,
                    "aipl_dict_insert",
                    &[ptr, pbase, drop_fn, retain_fn, psz, str_cmp],
                );
            }
            let dict_ty = ConcreteType::Dict(Box::new(key), Box::new(val));
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
            if matches!(&idx_t, ConcreteType::Named(n) if n == "__builtin_Span") {
                let layout = structs
                    .get("__builtin_Span")
                    .and_then(TypeDef::as_struct)
                    .ok_or_else(|| {
                        Error::at("Span struct layout missing (compiler bug)", span.clone())
                    })?;
                // `Span`'s bounds are `u64` (a byte offset is never negative);
                // both signednesses read the same 8-byte scalar here.
                let bound_ty = ConcreteType::Primitive(Primitive::U64);
                let mut bound = |field: &str| -> Result<Value, Error> {
                    let f = layout.field(field).ok_or_else(|| {
                        Error::at(
                            format!("Span struct has no field {field:?} (compiler bug)"),
                            span.clone(),
                        )
                    })?;
                    Ok(component(builder, idx_v, f.offset, &bound_ty, structs))
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

            expect_len_operand(&idx_t, "index", index.span.clone())?;

            // `s[i]` on a `str` (or a str-shaped `char[]`, see `is_char_array`)
            // yields `char?` — the byte at `i`, via the runtime `aipl_char_at`.
            // (Exact-`str` plus `char[]`, not the broader `is_str_shaped`: the
            // original check was an exact `Str` match, not `is_str_repr` — kept
            // that scope, since `Error`/concat-str indexing wasn't audited.)
            if recv_ty == ConcreteType::Primitive(Primitive::Str) || is_char_array(&recv_ty) {
                let ptr = emit_char_at(builder, module, builtins, recv_v, idx_v);
                return Ok((
                    ptr,
                    ConcreteType::Optional(Box::new(ConcreteType::Primitive(Primitive::Char))),
                ));
            }

            let arr_ptr = recv_v;
            let elem_ty = match &recv_ty {
                ConcreteType::Array(inner) => (**inner).clone(),
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
            let result_ty = ConcreteType::Optional(Box::new(elem_ty.clone()));
            // Guard the load behind a branch so an out-of-bounds index
            // never dereferences past the allocation.
            let len = load_arr_len(builder, arr_ptr);
            let ge0 = builder
                .ins()
                .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, idx_v, 0);
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
            builder.ins().stack_store(types::I64, zero, slot, 0);
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
            expect_len_operand(&a_t, "slice start", start.span.clone())?;
            let b_v = match end {
                Some(end) => {
                    let (b_v, b_t) = compile_expr(module, builder, cx, scopes, end)?;
                    expect_len_operand(&b_t, "slice end", end.span.clone())?;
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
            // Snapshot the current scope's tracking depth first: if evaluating the
            // scrutinee grows it and leaves *its* result as the last-tracked entry,
            // the scrutinee is a fresh temporary we exclusively own — see the Ok
            // arm, which then consumes it instead of retaining + deferring its drop.
            let scope_len_before = scope_depth(scopes);
            let (rptr, rty) = compile_expr(module, builder, cx, scopes, inner)?;

            // An optional operand: `expr?` unwraps `some`, or early-returns `none`
            // from the enclosing (optional-returning) function — the value analog
            // of the `?`-propagates-`err` behaviour below. Optionals share the
            // result's `{tag, value@8}` layout (tag != 0 = some, 0 = none), so the
            // branch structure mirrors the Ok/Err split; only the early-returned
            // husk differs (a fresh `none`, tag 0, carries no payload).
            if let ConcreteType::Optional(inner_ty) = &rty {
                let val_ty = (**inner_ty).clone();
                // A `.test` body absorbs the `none` by failing the test (below),
                // exactly as it absorbs an `err` — so it needs no optional return.
                if !cx.in_test && !matches!(cx.ret_ty, ConcreteType::Optional(_)) {
                    return Err(Error::at(
                        "\"?\" on an optional can only be used in a function that returns an \
                         optional, or in a test"
                            .to_string(),
                        span.clone(),
                    ));
                }
                let tag = builder
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), rptr, 0);
                let some_block = builder.create_block();
                let none_block = builder.create_block();
                // tag != 0 = some → continue; tag 0 = none → early return.
                builder.ins().brif(tag, some_block, &[], none_block, &[]);

                // --- none: drop live scopes, then either fail the test or return
                // a fresh `none`. ---
                builder.switch_to_block(none_block);
                builder.seal_block(none_block);
                // In a test, record the failure *before* the scope drop, mirroring
                // the err arm (which reads its payload first). There is no payload
                // here, so the hook takes no argument.
                if cx.in_test {
                    builtins.call_void(module, builder, "aipl_test_fail_none", &[]);
                }
                for scope in scopes.iter() {
                    for t in scope {
                        let v = match t.owned {
                            Owned::Value(v) => v,
                            Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
                        };
                        emit_drop(builder, module, builtins, structs, v, &t.ty);
                    }
                }
                if cx.in_test {
                    // The synthesized test body returns unit — nothing to store.
                    builder.ins().return_(&[]);
                } else {
                    let sret = cx.sret.expect("optional-returning fn has an sret pointer");
                    let zero = builder.ins().iconst(types::I64, 0);
                    builder.ins().store(MemFlagsData::trusted(), zero, sret, 0);
                    builder.ins().return_(&[]);
                }

                // --- some: unwrap the value and carry on (mirrors the Ok arm). ---
                builder.switch_to_block(some_block);
                builder.seal_block(some_block);
                let owned_temp = move_owned_temp(scopes, scope_len_before, rptr);
                let val = component(builder, rptr, OPT_VALUE_OFFSET, &val_ty, structs);
                if needs_drop(&val_ty, structs) {
                    if !owned_temp {
                        emit_retain(builder, module, builtins, structs, val, &val_ty);
                    }
                    scopes
                        .last_mut()
                        .expect("scope")
                        .push(Tracked::new(val, &val_ty));
                }
                return Ok((val, val_ty));
            }

            let ConcreteType::Result(ok_in, err_in) = &rty else {
                return Err(Error::at(
                    format!(
                        "\"?\" operand must be a result or an optional, got {}",
                        type_name(&rty)
                    ),
                    inner.span.clone(),
                ));
            };
            // The enclosing context must be able to receive the propagated error:
            // a result-returning function (early-return its Err via sret), an
            // `fn main() -> !Error` (print `error: <msg>` and exit 1), or a
            // `.test` body (fail the test with the error's message). A test `?`
            // never propagates a value out, so it places no constraint on the
            // err type — skip the coercibility check for it.
            if !cx.in_test {
                let ret_err = match cx.ret_ty {
                    ConcreteType::Result(_, ret_err) => Some(ret_err),
                    _ if cx.error_main => None, // err type is `Error`
                    _ => {
                        return Err(Error::at(
                            "\"?\" can only be used in a function that returns a result, \
                             or in a test",
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
            }
            let ok_ty = (**ok_in).clone();
            let err_in_ty = (**err_in).clone();
            let tag = builder
                .ins()
                .load(types::I64, MemFlagsData::trusted(), rptr, 0);
            let ok_block = builder.create_block();
            let err_block = builder.create_block();
            // tag 1 = ok → continue; tag 0 = err → early return.
            builder.ins().brif(tag, ok_block, &[], err_block, &[]);

            // --- Err: early return. ---
            builder.switch_to_block(err_block);
            builder.seal_block(err_block);
            if cx.in_test {
                // `?` on an err inside a `.test` body: read the err payload
                // (borrowed) and hand it to the shared per-err-type helper, which
                // renders it and records the current test as failed — keeping this
                // site to just "read payload, call helper". Then drop the live
                // scopes and leave the (unit-returning) test function.
                let err_val = component(builder, rptr, OPT_VALUE_OFFSET, &err_in_ty, structs);
                let fail_id = test_fail_func(module, cx, &err_in_ty);
                let fref = module.declare_func_in_func(fail_id, builder.func);
                builder.ins().call(fref, &[err_val]);
                for scope in scopes.iter() {
                    for t in scope {
                        let v = match t.owned {
                            Owned::Value(v) => v,
                            Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
                        };
                        emit_drop(builder, module, builtins, structs, v, &t.ty);
                    }
                }
                builder.ins().return_(&[]);
            } else if cx.error_main {
                // `?` in `fn main() -> !Error`: print `error: <msg>` and exit 1.
                // Read the err payload (borrowed) before the scope drop frees it.
                let msg = component(builder, rptr, OPT_VALUE_OFFSET, &err_in_ty, structs);
                builtins.call_void(module, builder, "aipl_print_error", &[msg]);
                for scope in scopes.iter() {
                    for t in scope {
                        let v = match t.owned {
                            Owned::Value(v) => v,
                            Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
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
                            Owned::Slot(slot) => slot_value(builder, slot, &t.ty),
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
            // Consuming: when the scrutinee is a fresh temporary we own, move it —
            // drop its tracking here rather than leaving it to be re-dropped
            // (conditionally, on tag) at every later early-return and at scope exit,
            // the source of the quadratic drop-code growth with chained `?`. On the
            // Ok path its Err side is absent and its Ok payload moves into `val`, so
            // there's nothing left to free (the husk is a function-lifetime stack
            // slot). The Err block, emitted above while the entry was still tracked,
            // still drops it on that path.
            let owned_temp = move_owned_temp(scopes, scope_len_before, rptr);
            // A void-Ok (`!E`) unwraps to unit — there's no payload to read.
            if is_unit(&ok_ty) {
                return Ok((builder.ins().iconst(types::I64, 0), ConcreteType::Unit));
            }
            let val = component(builder, rptr, OPT_VALUE_OFFSET, &ok_ty, structs);
            if needs_drop(&ok_ty, structs) {
                // A consumed temporary transfers ownership of its Ok payload to
                // `val` (no retain — `val` inherits the ref the husk held). A
                // borrowed scrutinee still owns its payload, so co-own via retain.
                if !owned_temp {
                    emit_retain(builder, module, builtins, structs, val, &ok_ty);
                }
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

/// Byte size of a struct field of type `ty` — [`elem_size_of`] as a `u32`.
///
/// An optional is stored inline as `8 (tag) + sizeof(Core)` (a nested `T??` is
/// no wider than `T?` — see "Optional representation"); a nested struct is
/// stored inline at its own size; a `str` is whichever width the selected
/// representation gives it; everything else is one word. The nested struct's
/// layout must already be resolved.
fn field_size(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> u32 {
    // Deliberately just `elem_size_of`: a struct field, a variant payload slot
    // and an array element are the same question asked three times, and this
    // used to answer it with its own `match` that agreed by coincidence. It
    // stopped agreeing the moment a `str` was not 8 bytes — the duplicate's
    // catch-all said 8, so every `str` field overlapped the one after it — which
    // is exactly the kind of drift a second copy of a layout rule invites.
    elem_size_of(ty, structs) as u32
}
/// A dict is an array of `[key][value]` pairs laid out back to back, so the
/// value's offset within a pair *is* the key's width and the pair's size is the
/// two widths summed.
///
/// Both were the literal `8` everywhere until the 24-byte `str` arrived: every
/// key had been one word. They are derived here so the eight sites that build,
/// scan, hash, render, free and look up pairs cannot disagree about the layout —
/// a disagreement reads a key's bytes as a value's, which is how a wide `str`
/// key surfaced as `misaligned pointer dereference: ... is 0x63` (the letter
/// `c`).
fn dict_key_size(key_ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> i64 {
    elem_size_of(key_ty, structs).max(8)
}

/// The size of a whole `[key][value]` pair — see [`dict_key_size`].
fn dict_pair_size(
    key_ty: &ConcreteType,
    val_ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) -> i64 {
    dict_key_size(key_ty, structs) + elem_size_of(val_ty, structs)
}

/// The `str_cmp` argument the set/dict runtime helpers take: **0 when the element
/// (or dict key) is not a `str`, otherwise its width in bytes.**
///
/// It used to be a plain 0/1 flag, which was enough while every `str` was an
/// 8-byte pointer. It no longer is: the helpers have to know both *how* to
/// compare (by content, not by word) and *how wide the thing is* — a set strides
/// over elements, and a dict's value sits immediately after its key, so the
/// value offset is the key's width. Carrying the width answers both questions
/// with the argument that was already there, and keeps the wide arm keyed on a
/// real quantity rather than a second magic flag.
///
/// `char[]` deliberately does not count: the existing helpers compare it as an
/// opaque word (identity, not content), and widening it here would change
/// tagged-path semantics rather than port them.
fn str_cmp_width(ty: &ConcreteType) -> i64 {
    if *ty != ConcreteType::Primitive(Primitive::Str) {
        return 0;
    }
    Abi::active().str_size()
}

/// Every representation choice a body of compiled code has made — the whole ABI
/// it speaks, as one value to thread.
///
/// **It has no fields, and that is the point.** A type may have more than one
/// runtime representation, and anything computing layout or ownership has to be
/// told which one is in force — so every such question comes in two forms, one
/// taking an `Abi` explicitly and a shorthand for the ABI this compilation
/// selected ([`Abi::active`]). The shorthands are defined *in terms of* the
/// explicit forms, so each rule has one implementation and no second copy to
/// drift.
///
/// `str` was the first type to have a choice and, for now, the last: it settled
/// on the 24-byte value, so today there is exactly one ABI and this carries no
/// information. The threading stayed anyway, because **the threading is the
/// expensive part and the field is not** — when the next type gains a
/// representation choice it becomes a field here, and nothing downstream is
/// re-plumbed.
///
/// The shape is load-bearing history, not speculation: while `str` had two
/// representations, the FFI genuinely met both at once — the compiler calls its
/// dogfooded engines, compiled earlier, through the same marshaling code it uses
/// to call a program it just compiled. "Is a `str` composite?" had two answers
/// simultaneously, and the marshaling layer had to ask with the callee in hand.
/// That is why the explicit form is primary and the global one is a convenience.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Abi {}

impl Abi {
    /// The ABI this compilation selected, for the majority of callers asking
    /// about the code they are emitting right now rather than about some other
    /// callee.
    fn active() -> Abi {
        Abi {}
    }

    /// Bytes one `str` value occupies under this ABI.
    fn str_size(self) -> i64 {
        str24::STR_SIZE as i64
    }

    /// Whether a `str` travels by address under this ABI. The knob that had two
    /// answers while `str` had two representations, and the one that grows an
    /// answer again if it ever gets a second.
    fn str_is_composite(self) -> bool {
        true
    }
}

/// Whether a value of `ty` travels by address for a callee speaking `abi`.
///
/// The single implementation of the rule; [`is_composite`] is this at
/// [`Abi::active`]. A `str` is the only type whose answer depends on the
/// representation — under `Tagged` it is a plain word, under `Wide` it is 24
/// bytes that live in memory, which is what "composite" already meant here.
fn abi_is_composite(abi: Abi, ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> bool {
    if is_str_shaped(ty) {
        return abi.str_is_composite();
    }
    matches!(ty, ConcreteType::Optional(_) | ConcreteType::Result(_, _))
        || matches!(ty, ConcreteType::Named(n) if structs.get(n).is_some_and(|d| !d.boxed()))
}

/// Bytes one value of `ty` occupies inline for a callee speaking `abi`.
///
/// The single implementation of the rule; [`elem_size_of`] is this at
/// [`Abi::active`], and [`field_size`] is the same answer as a `u32` — a
/// struct field, a variant payload slot and an array element are one question
/// asked three times, and used to be three `match`es that agreed by coincidence.
///
/// Recurses through optionals and results so one carrying a `str` is sized for
/// the right representation: `str?` is 16 bytes under `Tagged` and 32 under
/// `Wide`.
fn abi_elem_size(abi: Abi, ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> i64 {
    match ty {
        _ if is_str_shaped(ty) => abi.str_size(),
        ConcreteType::Optional(_) => {
            OPT_VALUE_OFFSET as i64 + abi_elem_size(abi, opt_core(ty), structs)
        }
        // A result is `{ tag, value }` where `value` is sized to the wider
        // payload.
        ConcreteType::Result(ok, err) => {
            OPT_VALUE_OFFSET as i64
                + abi_elem_size(abi, ok, structs).max(abi_elem_size(abi, err, structs))
        }
        // A declared type's size comes from its layout table — which for an
        // artifact is read straight out of the manifest, so it is already right
        // for that artifact's representation. A boxed (recursive) one is an
        // 8-byte pointer.
        ConcreteType::Named(n) => {
            structs
                .get(n)
                .map_or(8, |t| if t.boxed() { 8 } else { t.size() as i64 })
        }
        _ => 8,
    }
}

/// The size of a value returned through a hidden pointer for a callee speaking
/// `abi`, or `None` if it comes back in a register. Composite and inline-size
/// are the same two questions, so this is just their conjunction.
fn abi_sret_size(abi: Abi, ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> Option<u32> {
    abi_is_composite(abi, ty, structs).then(|| abi_elem_size(abi, ty, structs) as u32)
}

fn sret_size(ty: &ConcreteType, structs: &HashMap<String, TypeDef>) -> Option<u32> {
    abi_sret_size(Abi::active(), ty, structs)
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
    ty: &ConcreteType,
    structs: &HashMap<String, TypeDef>,
) {
    let size = sret_size(ty, structs).unwrap_or(8);
    let mut o = 0u32;
    while o < size {
        let chunk = builder
            .ins()
            .load(types::I64, MemFlagsData::trusted(), src, o as i32);
        builder
            .ins()
            .store(MemFlagsData::trusted(), chunk, dst, o as i32);
        o += 8;
    }
}

/// True for heap-allocated, refcounted value types (str, arrays, and sets —
/// sets share the array heap block). These get tracked for `dec` at scope exit
/// and `inc`'d when handed to a callee or returned.
fn is_heap(t: &ConcreteType) -> bool {
    *t == ConcreteType::Primitive(Primitive::Str)
        || is_error(t)
        || matches!(t, ConcreteType::Array(_) | ConcreteType::Set(_))
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
