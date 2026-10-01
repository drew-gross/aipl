//! The LSP base protocol: `Content-Length`-framed JSON-RPC over a byte stream.
//!
//! This is the whole transport. It is hand-written rather than taken from a
//! crate because there is nothing to it — a header block, a blank line, and
//! exactly that many bytes of JSON — and because the alternative is a
//! dependency tree carrying typed definitions for the hundred requests this
//! server does not answer.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// Read one message, or `None` at end of input — which is how an editor says
/// it is done: it closes the pipe rather than always sending `exit`.
///
/// Headers other than `Content-Length` are skipped. `Content-Type` is the only
/// other one the protocol defines and its sole legal value is the default, so
/// there is nothing to learn from it; anything else is a client's invention and
/// not ours to interpret.
pub fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            // EOF *between* messages is an orderly end. EOF in the middle of
            // one is not, and `read_exact` below reports it as such.
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let Some(length) = length else {
        return Err(io::Error::other("message header has no Content-Length"));
    };
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

/// Write one message, framed, and flush it.
///
/// The flush is not optional: the client is blocked on a pipe waiting for this
/// reply, so a response left in a buffer reads to it as a server that hung.
pub fn write_message(output: &mut impl Write, message: &Value) -> io::Result<()> {
    // Serializing a `Value` cannot fail — every variant it holds is
    // representable, which is what makes it a `Value` — so a failure here is a
    // broken invariant rather than a condition to propagate.
    let body = serde_json::to_vec(message).expect("serialize a JSON value");
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_framed_message() {
        let raw = b"Content-Length: 17\r\n\r\n{\"method\":\"ping\"}";
        let mut input = &raw[..];
        let message = read_message(&mut input).expect("read").expect("a message");
        assert_eq!(message, json!({"method": "ping"}));
        assert_eq!(read_message(&mut input).expect("read"), None, "then EOF");
    }

    #[test]
    fn skips_other_headers() {
        let raw = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
                    Content-Length: 17\r\n\r\n{\"method\":\"ping\"}";
        let mut input = &raw[..];
        let message = read_message(&mut input).expect("read").expect("a message");
        assert_eq!(message, json!({"method": "ping"}));
    }

    #[test]
    fn reads_back_what_it_wrote() {
        let message = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let mut buffer = Vec::new();
        write_message(&mut buffer, &message).expect("write");
        assert!(buffer.starts_with(b"Content-Length: "), "framed");
        let mut written = &buffer[..];
        assert_eq!(read_message(&mut written).expect("read"), Some(message));
    }

    /// A body is counted in *bytes*, not characters — the one way a
    /// hand-written framer goes wrong, and it goes wrong only on input with
    /// multi-byte text in it.
    #[test]
    fn frames_multibyte_text_by_byte_length() {
        let message = json!({"doc": "— ünïcödé —"});
        let mut buffer = Vec::new();
        write_message(&mut buffer, &message).expect("write");
        let mut written = &buffer[..];
        assert_eq!(read_message(&mut written).expect("read"), Some(message));
    }

    #[test]
    fn a_missing_length_is_an_error() {
        let mut input = &b"Content-Type: whatever\r\n\r\n{}"[..];
        assert!(read_message(&mut input).is_err());
    }
}
