// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The transport behind `http_get`: the system libcurl, declared here rather than included from a
// header (the runtime has no C headers). Shared by the JIT and the AOT runtime, so the two make
// the same request the same way and report the same failures; the layout of what they hand back
// is `http_response.rs`'s. The option and info numbers are libcurl's own, stable across releases.
// Only `core` and libc are used, which is all the AOT runtime has.

use super::http_response::{write_http_error, write_http_response, RawResponse};
use core::ffi::{c_char, c_int, c_long, c_void};

extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn strlen(s: *const c_char) -> usize;
}

#[link(name = "curl")]
extern "C" {
    fn curl_easy_init() -> *mut c_void;
    fn curl_easy_cleanup(handle: *mut c_void);
    fn curl_easy_setopt(handle: *mut c_void, option: c_int, ...) -> c_int;
    fn curl_easy_perform(handle: *mut c_void) -> c_int;
    fn curl_easy_getinfo(handle: *mut c_void, info: c_int, ...) -> c_int;
    fn curl_easy_strerror(code: c_int) -> *const c_char;
}

const CURLOPT_WRITEDATA: c_int = 10001;
const CURLOPT_URL: c_int = 10002;
const CURLOPT_TIMEOUT: c_int = 13;
const CURLOPT_ERRORBUFFER: c_int = 10010;
const CURLOPT_FOLLOWLOCATION: c_int = 52;
const CURLOPT_MAXREDIRS: c_int = 68;
const CURLOPT_NOSIGNAL: c_int = 99;
const CURLOPT_HEADERDATA: c_int = 10029;
const CURLOPT_ACCEPT_ENCODING: c_int = 10102;
const CURLOPT_WRITEFUNCTION: c_int = 20011;
const CURLOPT_HEADERFUNCTION: c_int = 20079;
const CURLOPT_PROTOCOLS_STR: c_int = 10318;
const CURLOPT_REDIR_PROTOCOLS_STR: c_int = 10319;
const CURLINFO_RESPONSE_CODE: c_int = 0x20_0002;

/// libcurl's `CURL_ERROR_SIZE`.
const ERROR_CAP: usize = 256;
/// How many redirects a request follows before it fails.
const MAX_REDIRECTS: c_long = 20;

/// A growing byte buffer on libc's heap. The transient copies of a response are
/// not AIPL values, so they stay out of the runtime's allocation tallies.
struct Buffer {
    data: *mut u8,
    len: usize,
    cap: usize,
}

impl Buffer {
    const fn new() -> Buffer {
        Buffer {
            data: core::ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    /// Append `n` bytes from `src`; false if memory ran out.
    unsafe fn push(&mut self, src: *const u8, n: usize) -> bool {
        if self.len + n > self.cap {
            let cap = (self.len + n).max(self.cap * 2).max(256);
            let grown = unsafe { realloc(self.data as *mut c_void, cap) } as *mut u8;
            if grown.is_null() {
                return false;
            }
            self.data = grown;
            self.cap = cap;
        }
        unsafe {
            memcpy(
                self.data.add(self.len) as *mut c_void,
                src as *const c_void,
                n,
            )
        };
        self.len += n;
        true
    }

    fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.data, self.len) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { free(self.data as *mut c_void) };
    }
}

/// libcurl hands the body over in pieces; returning less than it gave aborts the
/// transfer.
extern "C" fn on_body(data: *const u8, size: usize, count: usize, into: *mut c_void) -> usize {
    let n = size * count;
    if unsafe { (*(into as *mut Buffer)).push(data, n) } {
        n
    } else {
        0
    }
}

/// One header line at a time, the status line and the blank line included. A
/// status line starts a new response (a redirect's, then the one it led to), so
/// only the last response's lines are kept.
extern "C" fn on_header(line: *const u8, size: usize, count: usize, into: *mut c_void) -> usize {
    let n = size * count;
    let headers = unsafe { &mut *(into as *mut Buffer) };
    if n >= 5 && unsafe { core::slice::from_raw_parts(line, 5) } == b"HTTP/" {
        headers.len = 0;
    }
    if unsafe { headers.push(line, n) } {
        n
    } else {
        0
    }
}

/// The text of libcurl's message for a failed transfer: the detail it wrote to the
/// error buffer, or its generic description of `code`.
unsafe fn failure<'a>(code: c_int, detail: &'a [u8; ERROR_CAP]) -> &'a [u8] {
    let written = detail.iter().position(|&b| b == 0).unwrap_or(ERROR_CAP);
    if written > 0 {
        return &detail[..written];
    }
    let generic = unsafe { curl_easy_strerror(code) };
    unsafe { core::slice::from_raw_parts(generic as *const u8, strlen(generic)) }
}

/// Request `url` and write the outcome to `out`.
pub fn get(out: *mut u8, url: &[u8], timeout_seconds: i64) {
    // libcurl wants a NUL-terminated URL, and the runtime's strings are not.
    let c_url = unsafe { malloc(url.len() + 1) } as *mut u8;
    if c_url.is_null() {
        return write_http_error(out, b"out of memory");
    }
    unsafe {
        memcpy(
            c_url as *mut c_void,
            url.as_ptr() as *const c_void,
            url.len(),
        );
        *c_url.add(url.len()) = 0;
    }
    let handle = unsafe { curl_easy_init() };
    if handle.is_null() {
        unsafe { free(c_url as *mut c_void) };
        return write_http_error(out, b"could not start the request");
    }
    let mut body = Buffer::new();
    let mut headers = Buffer::new();
    let mut detail = [0u8; ERROR_CAP];
    // A timeout of 0 would mean "none" to libcurl, which is what 0 seconds means
    // here too.
    let timeout = timeout_seconds.clamp(0, i32::MAX as i64) as c_long;
    let code = unsafe {
        curl_easy_setopt(handle, CURLOPT_URL, c_url as *const c_char);
        curl_easy_setopt(handle, CURLOPT_ERRORBUFFER, detail.as_mut_ptr());
        curl_easy_setopt(handle, CURLOPT_NOSIGNAL, 1 as c_long);
        curl_easy_setopt(handle, CURLOPT_TIMEOUT, timeout);
        curl_easy_setopt(handle, CURLOPT_FOLLOWLOCATION, 1 as c_long);
        curl_easy_setopt(handle, CURLOPT_MAXREDIRS, MAX_REDIRECTS);
        // Only the web: not `file://`, `ftp://` and the rest libcurl also speaks.
        curl_easy_setopt(handle, CURLOPT_PROTOCOLS_STR, b"http,https\0".as_ptr());
        curl_easy_setopt(
            handle,
            CURLOPT_REDIR_PROTOCOLS_STR,
            b"http,https\0".as_ptr(),
        );
        // The empty string turns on every encoding libcurl can undo.
        curl_easy_setopt(handle, CURLOPT_ACCEPT_ENCODING, b"\0".as_ptr());
        curl_easy_setopt(
            handle,
            CURLOPT_WRITEFUNCTION,
            on_body as extern "C" fn(*const u8, usize, usize, *mut c_void) -> usize,
        );
        curl_easy_setopt(handle, CURLOPT_WRITEDATA, &mut body as *mut Buffer);
        curl_easy_setopt(
            handle,
            CURLOPT_HEADERFUNCTION,
            on_header as extern "C" fn(*const u8, usize, usize, *mut c_void) -> usize,
        );
        curl_easy_setopt(handle, CURLOPT_HEADERDATA, &mut headers as *mut Buffer);
        curl_easy_perform(handle)
    };
    if code != 0 {
        write_http_error(out, unsafe { failure(code, &detail) });
    } else {
        let mut status: c_long = 0;
        unsafe { curl_easy_getinfo(handle, CURLINFO_RESPONSE_CODE, &mut status as *mut c_long) };
        write_http_response(
            out,
            &RawResponse {
                status: status as i64,
                header_lines: headers.bytes(),
                body: body.bytes(),
            },
        );
    }
    unsafe {
        curl_easy_cleanup(handle);
        free(c_url as *mut c_void);
    }
}
