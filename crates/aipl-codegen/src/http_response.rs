// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The `HttpResponse!Error` that `http_get` hands back, written by hand because the
// runtime has no layout table, and the checks that decide whether a response can be
// one. Shared by the JIT and the AOT runtime so the two cannot disagree about the
// layout or about what counts as text; each supplies only the transport.
//
// The layout is what `build_struct_layout` computes for the declarations in
// `BUILTIN_SIGNATURES`, field after field with every slot sized by `elem_size_of`:
//
// ```text
// HttpResponse!Error   [ tag : 8 ][ payload ]                       tag 1 = ok, 0 = err
//   err payload        [ message : STR_SIZE ]
//   ok payload         [ status : 8 ][ headers : ARR_SIZE ][ body : STR_SIZE ]
// HttpHeader           [ name : STR_SIZE ][ value : STR_SIZE ]      HEADER_SIZE bytes
// ```
//
// `status` is a `u16` held in a whole word, as every scalar field is.

/// Where the payload starts, after the tag.
const PAYLOAD_OFFSET: usize = 8;

/// One `HttpHeader` element of the `headers` array: a name and a value.
const HEADER_SIZE: usize = 2 * super::str24::STR_SIZE;

/// Release each of `len` `HttpHeader` elements in a run: the array header's
/// element drop-fn for the headers array this file builds.
pub(crate) extern "C" fn aipl_arr_drop_http_header(elems: *const u8, len: i64) {
    for i in 0..len.max(0) as usize {
        let header = unsafe { elems.add(i * HEADER_SIZE) };
        unsafe {
            core::ptr::read(header as *const super::str24::Str).release();
            core::ptr::read(header.add(super::str24::STR_SIZE) as *const super::str24::Str).release();
        }
    }
}

/// Write `err(message)` to `out`.
pub(crate) fn write_http_error(out: *mut u8, message: &[u8]) {
    unsafe {
        *(out as *mut i64) = 0;
        core::ptr::write(
            out.add(PAYLOAD_OFFSET) as *mut super::str24::Str,
            super::str24::from_bytes(message),
        );
    }
}

/// What the transport saw: the final status, the header lines of the final
/// response (the status line first, `\r\n` after each), and the body.
pub(crate) struct RawResponse<'a> {
    pub status: i64,
    pub header_lines: &'a [u8],
    pub body: &'a [u8],
}

/// Whether `bytes` can be a `str`: valid UTF-8 holding no NUL byte.
fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && core::str::from_utf8(bytes).is_ok()
}

/// `bytes` without the spaces and tabs at either end.
fn trim_blanks(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = bytes {
        bytes = rest;
    }
    bytes
}

/// The `(name, value)` of a header line (without its line ending), or `None` for a
/// line that is not one: the status line, a blank line, a folded continuation.
fn header_of(line: &[u8]) -> Option<(&[u8], &[u8])> {
    if line.starts_with(b"HTTP/") {
        return None;
    }
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    (!name.is_empty()).then(|| (name, trim_blanks(&line[colon + 1..])))
}

/// Write the response to `out` as `ok(HttpResponse)`, or `err(message)` when it
/// cannot be one: a body or a header that is not text.
pub(crate) fn write_http_response(out: *mut u8, response: &RawResponse) {
    if !is_text(response.body) {
        return write_http_error(
            out,
            b"the response body is not text (it holds a NUL byte or invalid UTF-8)",
        );
    }
    let mut headers = super::aipl_array_with_cap(
        0,
        aipl_arr_drop_http_header as *const () as usize as i64,
        HEADER_SIZE as i64,
    );
    let mut count = 0usize;
    for raw_line in response.header_lines.split(|&b| b == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        let Some((name, value)) = header_of(line) else {
            continue;
        };
        if !is_text(name) || !is_text(value) {
            super::aipl_array_dec(headers);
            return write_http_error(out, b"a response header is not text");
        }
        // The two fresh strings' references move into the array.
        let element = [super::str24::from_bytes(name), super::str24::from_bytes(value)];
        headers = super::array_ops::push_mut(
            headers,
            count,
            element.as_ptr() as *const u8,
            aipl_arr_drop_http_header as *const () as usize as i64,
            0,
            HEADER_SIZE as i64,
        );
        count += 1;
    }
    unsafe {
        *(out as *mut i64) = 1;
        let payload = out.add(PAYLOAD_OFFSET);
        *(payload as *mut i64) = response.status;
        core::ptr::write(
            payload.add(8) as *mut super::arr24::ArrValue,
            super::arr24::ArrValue::whole_of(headers, count),
        );
        core::ptr::write(
            payload.add(8 + super::arr24::ARR_SIZE) as *mut super::str24::Str,
            super::str24::from_bytes(response.body),
        );
    }
}
