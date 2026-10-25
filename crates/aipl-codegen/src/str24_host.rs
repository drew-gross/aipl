//! The JIT runtime's half of the 24-byte `str`: the pieces that genuinely differ
//! between the two runtimes, so the layout itself can be shared verbatim (see
//! `str24.rs`).
//!
//! That is I/O, and only I/O — `std::io`/`std::fs` here, `libc` in the AOT
//! runtime.

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

/// `read_line()` under the wide ABI: the next line of standard input, without its
/// `\n` or `\r\n`, through `out`, and **1/0 as the return value** (0 at end of
/// input or on a read error, with `out` left empty).
#[no_mangle]
pub(crate) extern "C" fn aipl_read_line(out: *mut Str) -> i64 {
    use std::io::BufRead;
    let mut line = Vec::new();
    let read = std::io::stdin().lock().read_until(b'\n', &mut line);
    if !matches!(read, Ok(n) if n > 0) {
        unsafe { *out = Str::empty() };
        return 0;
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    unsafe { *out = from_bytes(&line) };
    1
}

/// Standard input taken out of line mode and stopped from echoing, for as long as
/// the value lives. Standard input that is not a terminal is left as it is.
struct UnbufferedTerminal(Option<libc::termios>);

impl UnbufferedTerminal {
    fn enter() -> Self {
        let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Self(None);
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
        Self(Some(saved))
    }
}

impl Drop for UnbufferedTerminal {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
        }
    }
}

/// `read_byte()`: the next byte of standard input as it is typed, or -1 at end of
/// input or on a read error. Reads through the same buffer as `aipl_read_line`.
#[no_mangle]
pub(crate) extern "C" fn aipl_read_byte() -> i64 {
    use std::io::Read;
    let _terminal = UnbufferedTerminal::enter();
    let mut byte = [0u8; 1];
    match std::io::stdin().lock().read(&mut byte) {
        Ok(1) => i64::from(byte[0]),
        _ => -1,
    }
}

/// Sends the terminal a control sequence. Flushed, because standard output is
/// line buffered and a sequence has no newline to release it.
fn write_terminal_sequence(sequence: &[u8]) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(sequence);
    let _ = out.flush();
}

/// `clear_terminal()`: erase the display (`ESC [ 2 J`), then home the cursor
/// (`ESC [ H`).
#[no_mangle]
pub(crate) extern "C" fn aipl_clear_terminal() {
    write_terminal_sequence(b"\x1b[2J\x1b[H");
}

/// `move_to(row, column)`: put the cursor at a cell counted from 0 at the top
/// left, which the terminal counts from 1 (`ESC [ row ; column H`). Saturating
/// keeps `u64::MAX` an answer rather than a wrap to 0.
#[no_mangle]
pub(crate) extern "C" fn aipl_move_to(row: u64, column: u64) {
    let sequence = format!(
        "\x1b[{};{}H",
        row.saturating_add(1),
        column.saturating_add(1)
    );
    write_terminal_sequence(sequence.as_bytes());
}

/// `print(s)` under the new ABI — the `aipl_*` counterpart of `aipl_print`.
#[no_mangle]
pub(crate) extern "C" fn aipl_print(s: *const Str) {
    print(unsafe { *s });
}

/// `error: <msg>` under the new ABI.
#[no_mangle]
pub(crate) extern "C" fn aipl_print_error(s: *const Str) {
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
pub(crate) extern "C" fn aipl_read_file_to_string(out: *mut Str, path: *const Str) -> i64 {
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
pub(crate) extern "C" fn aipl_write_string_to_file(path: *const Str, contents: *const Str) -> i64 {
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
