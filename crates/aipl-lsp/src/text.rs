//! The two translations between what a compiler says and what an editor
//! understands: byte offsets to line/character positions, and file paths to
//! `file://` URIs.
//!
//! Both are places where a language server is quietly wrong rather than
//! visibly broken — a position off by one lands the cursor on the wrong word,
//! and a URI encoded differently than the client spells it silently resolves
//! to nothing — so both are unit-tested against the cases that actually differ
//! (multi-byte text, astral-plane characters, percent-escapes) rather than the
//! ASCII ones that cannot tell the implementations apart.

use std::path::{Path, PathBuf};

use aipl_syntax::Span;
use serde_json::{json, Value};

/// Where each line of a document starts, so a byte offset can be turned into a
/// position without re-scanning the file for every symbol in it.
pub struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset of the first character of each line. Always starts with 0,
    /// so it is never empty and the lookup below always finds a line.
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> LineIndex<'a> {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        LineIndex { text, starts }
    }

    /// The LSP position of a byte offset: a zero-based line, and a character
    /// counted in **UTF-16 code units** from the start of that line.
    ///
    /// UTF-16 is the protocol's default encoding and the only one this server
    /// claims, so the unit is neither bytes (what the compiler has) nor
    /// characters (what it looks like): an emoji counts 2, and a `é` counts 1
    /// while occupying 2 bytes.
    pub fn position(&self, offset: usize) -> Value {
        let offset = self.clamp(offset);
        // `partition_point` gives the number of line starts at or before the
        // offset; the last of them is the line the offset is on.
        let line = self.starts.partition_point(|&start| start <= offset) - 1;
        let character = self.text[self.starts[line]..offset]
            .chars()
            .map(char::len_utf16)
            .sum::<usize>();
        json!({ "line": line, "character": character })
    }

    /// A compiler span as an LSP range.
    pub fn range(&self, span: &Span) -> Value {
        json!({ "start": self.position(span.start), "end": self.position(span.end) })
    }

    /// A zero-length range at one offset — what a diagnostic with no span of
    /// its own gets, so it still has somewhere to appear.
    pub fn empty_range_at(&self, offset: usize) -> Value {
        let position = self.position(offset);
        json!({ "start": position, "end": position })
    }

    /// The byte offset of an LSP position. A position past the end of its line
    /// clamps to the line's end, and a line past the end of the document to
    /// the document's end: an editor sends a position it computed before the
    /// edit it is telling us about, so out of range is ordinary rather than
    /// exceptional.
    pub fn offset(&self, line: u32, character: u32) -> usize {
        let Some(&start) = self.starts.get(line as usize) else {
            return self.text.len();
        };
        let mut offset = start;
        let mut remaining = character as usize;
        for c in self.text[start..].chars() {
            // The line's own newline is past its end, so a character count
            // that would run through it stops here instead.
            if c == '\n' || remaining < c.len_utf16() {
                break;
            }
            remaining -= c.len_utf16();
            offset += c.len_utf8();
        }
        offset
    }

    /// Pull an offset back to the nearest character boundary at or before it,
    /// and inside the document. Spans from the lexer are already boundaries;
    /// this is for the ones arithmetic on a client-supplied position produced.
    fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }
}

/// Characters a `file://` URI may carry literally — [RFC 3986's unreserved
/// set][rfc], plus the `/` that separates path segments.
///
/// [rfc]: https://www.rfc-editor.org/rfc/rfc3986#section-2.3
fn is_literal_in_uri(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/')
}

/// A path as a `file://` URI.
///
/// Windows is why this is not a `format!`: its paths are separated by `\` and
/// start with a drive letter rather than a `/`, and a URI has neither — so the
/// separators are rewritten and the root slash the drive letter lacks is
/// supplied, giving the `file:///c%3A/...` form every editor speaks.
pub fn path_to_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file://");
    if !path.starts_with('/') {
        uri.push('/');
    }
    for byte in path.bytes() {
        if is_literal_in_uri(byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// The path a `file://` URI names, or `None` for a URI this server has no file
/// for — an `untitled:` buffer, or a `file://host/share` naming someone else's
/// machine.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    // `file:` + an empty authority + an absolute path. Anything between the
    // `//` and the first `/` is a host, and a host is not a local file.
    let path = uri.strip_prefix("file://")?;
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut rest = path.as_bytes();
    while let [first, tail @ ..] = rest {
        match (first, tail) {
            (b'%', [hi, lo, tail @ ..]) => {
                match u8::from_str_radix(std::str::from_utf8(&[*hi, *lo]).ok()?, 16) {
                    Ok(byte) => {
                        bytes.push(byte);
                        rest = tail;
                    }
                    // A stray `%` is not an escape. Keep it rather than failing:
                    // it is a legal character in a filename.
                    Err(_) => {
                        bytes.push(b'%');
                        rest = tail;
                    }
                }
            }
            _ => {
                bytes.push(*first);
                rest = tail;
            }
        }
    }
    let decoded = String::from_utf8(bytes).ok()?;
    // `/c:/src/x.aipl` is a Windows path wearing the URI's root slash. Strip
    // it; `C:/src/x.aipl` is a path Windows understands, slashes and all.
    let decoded = match decoded.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => &decoded[1..],
        _ => decoded.as_str(),
    };
    Some(PathBuf::from(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position_of(text: &str, offset: usize) -> (u64, u64) {
        let index = LineIndex::new(text);
        let position = index.position(offset);
        (
            position["line"].as_u64().expect("line"),
            position["character"].as_u64().expect("character"),
        )
    }

    #[test]
    fn counts_lines_and_ascii_columns() {
        let text = "fn a() {}\nfn b() {}\n";
        assert_eq!(position_of(text, 3), (0, 3));
        assert_eq!(position_of(text, 10), (1, 0));
        assert_eq!(position_of(text, 13), (1, 3));
        // The offset one past the last newline is the empty final line.
        assert_eq!(position_of(text, 20), (2, 0));
    }

    /// A column is UTF-16 code units: `é` is two bytes and one unit, and an
    /// emoji is four bytes and *two* units. A byte count or a character count
    /// would each get one of these wrong.
    #[test]
    fn counts_columns_in_utf16_units() {
        assert_eq!(position_of("// é x", 5), (0, 4), "é is 2 bytes, 1 unit");
        assert_eq!(
            position_of("// 😀 x", 8),
            (0, 6),
            "emoji is 4 bytes, 2 units"
        );
    }

    #[test]
    fn round_trips_a_position_through_an_offset() {
        let text = "fn a() {}\n// é 😀 x\nfn b() {}\n";
        let index = LineIndex::new(text);
        for offset in (0..text.len()).filter(|o| text.is_char_boundary(*o)) {
            let position = index.position(offset);
            let line = position["line"].as_u64().expect("line") as u32;
            let character = position["character"].as_u64().expect("character") as u32;
            assert_eq!(index.offset(line, character), offset, "at byte {offset}");
        }
    }

    #[test]
    fn clamps_a_position_past_the_end() {
        let index = LineIndex::new("abc\n");
        assert_eq!(index.offset(0, 99), 3, "stops at the line's newline");
        assert_eq!(index.offset(99, 0), 4, "past the last line is the end");
    }

    /// Offsets inside a multi-byte character arise from arithmetic on a
    /// client-supplied position, and must not panic on the slice.
    #[test]
    fn clamps_an_offset_inside_a_character() {
        assert_eq!(
            position_of("// é", 4),
            (0, 3),
            "mid-é falls back to before it"
        );
        assert_eq!(position_of("abc", 99), (0, 3), "past the end is the end");
    }

    #[test]
    fn round_trips_a_posix_path() {
        let path = Path::new("/Users/me/src/thing.aipl");
        assert_eq!(path_to_uri(path), "file:///Users/me/src/thing.aipl");
        assert_eq!(uri_to_path(&path_to_uri(path)).as_deref(), Some(path));
    }

    #[test]
    fn escapes_and_unescapes_what_a_uri_cannot_hold() {
        let path = Path::new("/tmp/a b/é.aipl");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%A9.aipl");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    /// What VS Code itself sends on Windows: the drive's colon escaped, the
    /// drive letter lowercased, and a root slash the path does not have.
    #[test]
    fn reads_the_windows_uri_an_editor_sends() {
        assert_eq!(
            uri_to_path("file:///c%3A/src/thing.aipl").as_deref(),
            Some(Path::new("c:/src/thing.aipl"))
        );
    }

    #[test]
    fn declines_a_uri_with_no_local_file() {
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
        assert_eq!(uri_to_path("file://host/share/x.aipl"), None);
    }
}
