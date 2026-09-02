//! The JIT runtime's half of the 24-byte `str` (`STR_REPR.md`): the pieces that
//! genuinely differ between the two runtimes, so the layout itself can be shared
//! verbatim (see `str24.rs`).
//!
//! That is I/O, and only I/O — `std::io`/`std::fs` here, `libc` in the AOT
//! runtime.
#![allow(dead_code)] // staged: wired up by the Stage 1 switch

use super::str24::{for_each_chunk, from_bytes, Str, INLINE_CAP};

// ---------- I/O ----------

/// Write the value's bytes to `out`, streaming — a rope never flattens, so
/// printing or writing a built-up string costs no extra allocation.
pub(super) fn write_to(s: Str, out: &mut impl std::io::Write) -> std::io::Result<()> {
    let mut err = None;
    for_each_chunk(s, &mut |chunk| match out.write_all(chunk) {
        Ok(()) => true,
        Err(e) => {
            err = Some(e);
            false
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `print(s)` — the value's bytes then a newline, streamed to stdout.
pub(super) fn print(s: Str) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = write_to(s, &mut out);
    let _ = out.write_all(b"\n");
}

/// `error: <msg>` on stderr, streamed.
pub(super) fn print_error(msg: Str) {
    use std::io::Write;
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = out.write_all(b"error: ");
    let _ = write_to(msg, &mut out);
    let _ = out.write_all(b"\n");
}

/// A value's bytes as a `&str` path. Materializes into `scratch` when the value
/// has no contiguous bytes of its own; **note there is no NUL involved** — a
/// buffer is a window and carries no terminator, and every path consumer here is
/// length-delimited.
fn path_of<'a>(s: &'a Str, scratch: &'a mut [u8; INLINE_CAP]) -> Option<&'a str> {
    std::str::from_utf8(s.bytes(scratch)).ok()
}

/// `read_file_to_string(path)`. `None` on any failure, matching the runtime's
/// "null means error" convention at this boundary.
pub(super) fn read_file_to_string(path: Str) -> Option<Str> {
    let mut scratch = [0u8; INLINE_CAP];
    let name = path_of(&path, &mut scratch)?;
    let bytes = std::fs::read(name).ok()?;
    Some(from_bytes(&bytes))
}

/// `write_string_to_file(path, contents)` — the contents stream out chunk by
/// chunk, so writing a rope never builds it flat first.
pub(super) fn write_string_to_file(path: Str, contents: Str) -> bool {
    let mut scratch = [0u8; INLINE_CAP];
    let Some(name) = path_of(&path, &mut scratch) else {
        return false;
    };
    let Ok(file) = std::fs::File::create(name) else {
        return false;
    };
    let mut file = std::io::BufWriter::new(file);
    if write_to(contents, &mut file).is_err() {
        return false;
    }
    std::io::Write::flush(&mut file).is_ok()
}

/// `print(s)` under the new ABI — the `aipl2_*` counterpart of `aipl_print`.
#[no_mangle]
pub(crate) extern "C" fn aipl2_print(s: *const Str) {
    print(unsafe { *s });
}

/// `error: <msg>` under the new ABI.
#[no_mangle]
pub(crate) extern "C" fn aipl2_print_error(s: *const Str) {
    print_error(unsafe { *s });
}

/// `read_file_to_string(path)` under the wide ABI: contents through `out`, and
/// **1/0 as the return value**.
///
/// The tagged entry point signals failure by returning a null `str`. That is not
/// available here — a `str` result travels through an out pointer the caller
/// already allocated, so there is no null to return. The success flag is
/// therefore explicit, which is also clearer: "did it work" and "what did it
/// produce" stop sharing one channel. Borrows `path`.
#[no_mangle]
pub(crate) extern "C" fn aipl2_read_file_to_string(out: *mut Str, path: *const Str) -> i64 {
    match read_file_to_string(unsafe { *path }) {
        Some(s) => {
            unsafe { *out = s };
            1
        }
        None => {
            unsafe { *out = Str::empty() };
            0
        }
    }
}

/// `write_string_to_file(path, contents)` under the wide ABI. Borrows both.
#[no_mangle]
pub(crate) extern "C" fn aipl2_write_string_to_file(path: *const Str, contents: *const Str) -> i64 {
    i64::from(write_string_to_file(unsafe { *path }, unsafe { *contents }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::str24::{concat, TAG_BUFFER, TAG_ROPE};

    fn text(s: Str) -> String {
        let mut scratch = [0u8; INLINE_CAP];
        String::from_utf8(s.bytes(&mut scratch).to_vec()).unwrap()
    }

    /// The same content as a buffer and as a rope, so streaming is exercised on
    /// a representation that has no contiguous bytes of its own.
    fn both(src: &str) -> [(&'static str, Str); 2] {
        [
            ("buffer", from_bytes(src.as_bytes())),
            (
                "rope",
                concat(
                    from_bytes(&src.as_bytes()[..10]),
                    from_bytes(&src.as_bytes()[10..]),
                ),
            ),
        ]
    }

    #[test]
    fn writing_streams_every_representation() {
        let src = "written content, long enough to be a buffer of its own";
        for (what, s) in both(src) {
            let mut out = Vec::new();
            write_to(s, &mut out).unwrap();
            assert_eq!(out, src.as_bytes(), "{what}");
            s.release();
        }
    }

    #[test]
    fn files_round_trip_through_windows_and_ropes() {
        let dir = std::env::temp_dir();
        let path_text = dir
            .join(format!("aipl_str24_{}.txt", std::process::id()))
            .to_str()
            .unwrap()
            .to_string();
        // The path itself is a *window* into a larger string — a buffer carries
        // no NUL, so this is the case that would break a C-string assumption.
        let padded = from_bytes(format!("<{path_text}>").as_bytes());
        let path = padded.slice(1, 1 + path_text.len());
        assert_eq!(text(path), path_text);

        // The contents are a rope, so the write has to stream them.
        let contents = concat(
            from_bytes(b"first half of the file, long enough to be its own buffer\n"),
            from_bytes(b"second half, also long enough to live in a buffer\n"),
        );
        assert_eq!(contents.tag(), TAG_ROPE);
        assert!(write_string_to_file(path, contents));

        let back = read_file_to_string(path).expect("file reads back");
        assert_eq!(text(back), text(contents));
        assert_eq!(back.tag(), TAG_BUFFER);

        assert!(read_file_to_string(from_bytes(b"/nonexistent/aipl/str24")).is_none());
        let _ = std::fs::remove_file(&path_text);
        padded.release();
        contents.release();
        back.release();
    }
}
